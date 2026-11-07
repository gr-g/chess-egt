use std::collections::{BTreeMap, HashMap};

use crate::egt::Egt;
use crate::error::{EgtError, EgtResult};
use crate::storage::{Location, block_on};
use crate::table_id::{TableId, position_table_name};
use crate::{DtcOutcome, PackedOutcome};
#[cfg(test)]
use crate::ConversionType;
use object_store::{GetOptions, GetRange, ObjectStoreExt, path::Path as ObjectPath};
#[cfg(test)]
use shakmaty::CastlingMode;
#[cfg(test)]
use shakmaty::Position;
use shakmaty::{Chess, Color};
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock, Weak};

use crate::storage::FRAME_SIZE;

// Compression level used when storing data on disk.
pub(crate) const COMPRESSION_LEVEL: i32 = 19;

/// Attempts to load an existing table, looking it up first in `input_path`
/// (if set) and then in `base_path`, falling back only when a file is missing.
fn load_existing_file(
    base_path: &Location,
    input_path: Option<&Location>,
    endgame: &str,
) -> EgtResult<EgtFileReader> {
    if let Some(input) = input_path {
        match EgtFileReader::open_in(input, endgame) {
            Ok(f) => return Ok(f),
            Err(EgtError::FileNotFound(_)) => {}
            Err(e) => return Err(e),
        }
    }
    EgtFileReader::open_in(base_path, endgame)
}

/// Logical extent of an unpadded, table-local transposed Zstd frame.
#[derive(Debug, Clone, Copy)]
struct FrameDescriptor {
    start: usize,
    len: usize,
}

/// Read-only storage and frame cache for one existing canonical table.
#[derive(Debug)]
pub struct EgtFileReader {
    egt: Egt,
    location: Location,
    key: ObjectPath,
    seek_table: zeekstd::SeekTable,
    /// `None` means the frame has not been loaded or has been evicted.
    frames: Vec<Option<Vec<PackedOutcome>>>,
    frame_layout: Vec<FrameDescriptor>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FrameKey {
    file: usize,
    frame: usize,
}

#[derive(Debug)]
struct CachedFrame {
    file: Weak<RwLock<EgtFileReader>>,
    bytes: usize,
    last_used: u64,
}

#[derive(Debug, Default)]
struct CacheMemory {
    assigned: Option<usize>,
    resident: usize,
    clock: u64,
    frames: HashMap<FrameKey, CachedFrame>,
}

/// Shared files and their uncompressed frames for probing and verification.
///
/// The assigned memory bounds retained frames between operations. Concurrent
/// cold loads, copied chunks, metadata, and decompression buffers are excluded.
#[derive(Debug)]
pub(crate) struct SharedFileCache {
    files: RwLock<HashMap<String, Arc<RwLock<EgtFileReader>>>>,
    base_path: Location,
    input_path: Option<Location>,
    cache: Mutex<CacheMemory>,
}

impl SharedFileCache {
    pub(crate) fn new(
        base_path: &Location,
        input_path: Option<&Location>,
        cache_size: Option<usize>,
    ) -> Self {
        Self {
            files: RwLock::new(HashMap::new()),
            base_path: base_path.clone(),
            input_path: input_path.cloned(),
            cache: Mutex::new(CacheMemory {
                assigned: cache_size,
                ..CacheMemory::default()
            }),
        }
    }

    pub(crate) fn set_cache_size(&self, assigned: usize) -> EgtResult<()> {
        self.cache
            .lock()
            .map_err(|_| EgtError::Internal("cache memory lock poisoned"))?
            .assigned = Some(assigned);
        self.enforce_limit()
    }

    pub(crate) fn set_input_path(&mut self, input: Option<Location>) -> EgtResult<()> {
        self.clear()?;
        self.input_path = input;
        Ok(())
    }

    pub(crate) fn file(&self, endgame: &str) -> EgtResult<Arc<RwLock<EgtFileReader>>> {
        let cached_file = {
            let files = self
                .files
                .read()
                .map_err(|_| EgtError::Internal("dependency registry lock poisoned"))?;
            files.get(endgame).map(Arc::clone)
        };
        let file = if let Some(file) = cached_file {
            file
        } else {
            let mut files = self
                .files
                .write()
                .map_err(|_| EgtError::Internal("dependency registry lock poisoned"))?;
            // Recheck after releasing the read lock: another worker may have loaded it.
            if !files.contains_key(endgame) {
                // Serialize loading so racing workers never allocate duplicate files.
                let file = load_existing_file(&self.base_path, self.input_path.as_ref(), endgame)?;
                files.insert(endgame.to_string(), Arc::new(RwLock::new(file)));
            }
            Arc::clone(files.get(endgame).ok_or(EgtError::Internal(
                "dependency registry entry missing after insert",
            ))?)
        };
        Ok(file)
    }

    pub(crate) fn probe(&self, position: &Chess) -> EgtResult<PackedOutcome> {
        self.probe_indexed(position).map(|(_, outcome)| outcome)
    }

    pub(crate) fn probe_outcome(&self, position: &Chess) -> EgtResult<DtcOutcome> {
        let (index, outcome) = self.probe_indexed(position)?;
        outcome.to_outcome_at(index)
    }

    /// Probes a primary table without wrapping a missing file as a dependency error.
    pub(crate) fn probe_direct_outcome(&self, position: &Chess) -> EgtResult<DtcOutcome> {
        let (name, reflected) = position_table_name(position)?;
        let (index, outcome) = self.probe_routed(position, name.as_str(), reflected)?;
        outcome.to_outcome_at(index)
    }

    /// Groups direct probes by canonical table, preserving the input order.
    pub(crate) fn probe_direct_outcomes(&self, positions: &[Chess]) -> EgtResult<Vec<DtcOutcome>> {
        let mut groups = BTreeMap::<String, Vec<(usize, bool)>>::new();
        for (slot, position) in positions.iter().enumerate() {
            let (name, reflected) = position_table_name(position)?;
            groups
                .entry(name.to_string())
                .or_default()
                .push((slot, reflected));
        }
        let mut outcomes = vec![DtcOutcome::Draw; positions.len()];
        for (name, requests) in groups {
            let file = self.file(&name)?;
            let reader = file
                .read()
                .map_err(|_| EgtError::Internal("dependency file lock poisoned"))?;
            let indices = requests
                .iter()
                .map(|&(slot, reflected)| {
                    reader.map_routed_position_to_index(&positions[slot], reflected)
                })
                .collect::<EgtResult<Vec<_>>>()?;
            let mut frames: Vec<_> = indices
                .iter()
                .map(|&index| reader.locate_frame(index).0)
                .collect();
            frames.sort_unstable();
            frames.dedup();
            let values = if let Some(values) = reader.read_indices_cached(&indices)? {
                for &frame in &frames {
                    self.record_access(&file, frame, reader.frame_bytes(frame))?;
                }
                drop(reader);
                Ok(values)
            } else {
                drop(reader);
                let mut writer = file
                    .write()
                    .map_err(|_| EgtError::Internal("dependency file lock poisoned"))?;
                // Recheck residency under the write guard: another probe may have warmed it.
                let result = writer.ensure_uncompressed_frames(&frames).and_then(|()| {
                    writer
                        .read_indices_cached(&indices)?
                        .ok_or(EgtError::Internal(
                            "expected resident frames after batch read",
                        ))
                });
                // A later decode can fail after earlier frames were installed.
                for &frame in &frames {
                    if writer.frames[frame].is_some() {
                        self.record_access(&file, frame, writer.frame_bytes(frame))?;
                    }
                }
                drop(writer);
                result
            };
            // Copy outcomes before eviction, including when the cache budget is zero.
            self.enforce_limit()?;
            for ((&(slot, _), index), value) in requests.iter().zip(indices).zip(values?) {
                outcomes[slot] = value.to_outcome_at(index)?;
            }
        }
        Ok(outcomes)
    }

    fn probe_indexed(&self, position: &Chess) -> EgtResult<(usize, PackedOutcome)> {
        let (name, reflected) = position_table_name(position)?;
        self.probe_routed(position, name.as_str(), reflected)
            .map_err(|source| match source {
                EgtError::FileNotFound(_) => EgtError::DependencyUnavailable {
                    dependency: name.to_string(),
                    source: Box::new(source),
                },
                other => other,
            })
    }

    fn probe_routed(
        &self,
        position: &Chess,
        endgame: &str,
        reflected: bool,
    ) -> EgtResult<(usize, PackedOutcome)> {
        let file = self.file(endgame)?;
        let reader = file
            .read()
            .map_err(|_| EgtError::Internal("dependency file lock poisoned"))?;
        let index = reader.map_routed_position_to_index(position, reflected)?;
        let frame = reader.locate_frame(index).0;
        let bytes = reader.frame_bytes(frame);
        let (outcome, over_limit) = if let Some(outcome) = reader.read_from_index_cached(index)? {
            let over_limit = self.record_access(&file, frame, bytes)?;
            drop(reader);
            (outcome, over_limit)
        } else {
            drop(reader);
            // Metadata is immutable: retain the index while upgrading to a write
            // guard. read_from_index rechecks whether another worker warmed it.
            let mut writer = file
                .write()
                .map_err(|_| EgtError::Internal("dependency file lock poisoned"))?;
            let outcome = writer.read_from_index(index)?;
            (outcome, self.record_access(&file, frame, bytes)?)
        };
        if over_limit {
            self.enforce_limit()?;
        }
        Ok((index, outcome))
    }

    /// Copies a scan block so no file lock is held while probing successors.
    /// The range must lie within one frame.
    pub(crate) fn read_chunk(
        &self,
        endgame: &str,
        start: usize,
        end: usize,
    ) -> EgtResult<Vec<PackedOutcome>> {
        let file = self.file(endgame)?;
        let reader = file
            .read()
            .map_err(|_| EgtError::Internal("dependency file lock poisoned"))?;
        if start > end || end > reader.egt.index_range() {
            return Err(EgtError::IndexOutOfRange {
                index: end,
                range: reader.egt.index_range(),
            });
        }
        if start == end {
            return Ok(Vec::new());
        }
        let frame = reader.locate_frame(start).0;
        let bytes = reader.frame_bytes(frame);
        let (chunk, over_limit) = if let Some(chunk) = reader.frame_chunk_cached(start, end)? {
            let chunk = chunk.to_vec();
            let over_limit = self.record_access(&file, frame, bytes)?;
            drop(reader);
            (chunk, over_limit)
        } else {
            drop(reader);
            let mut writer = file
                .write()
                .map_err(|_| EgtError::Internal("dependency file lock poisoned"))?;
            let chunk = writer.frame_chunk(start, end)?.to_vec();
            (chunk, self.record_access(&file, frame, bytes)?)
        };
        if over_limit {
            self.enforce_limit()?;
        }
        Ok(chunk)
    }

    fn record_access(
        &self,
        file: &Arc<RwLock<EgtFileReader>>,
        frame: usize,
        bytes: usize,
    ) -> EgtResult<bool> {
        // Arc addresses are stable while registered. insert/remove/clear purge
        // their entries, so no table-name allocation is needed on a warm probe.
        let key = FrameKey {
            file: Arc::as_ptr(file) as usize,
            frame,
        };
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| EgtError::Internal("cache memory lock poisoned"))?;
        cache.clock = cache.clock.wrapping_add(1);
        let last_used = cache.clock;
        let new_resident = cache
            .resident
            .checked_add(bytes)
            .ok_or(EgtError::Internal("cache memory accounting overflow"))?;
        if let Some(frame) = cache.frames.get_mut(&key) {
            frame.last_used = last_used;
        } else {
            cache.resident = new_resident;
            cache.frames.insert(
                key,
                CachedFrame {
                    file: Arc::downgrade(file),
                    bytes,
                    last_used,
                },
            );
        }
        Ok(cache.assigned.is_some_and(|limit| cache.resident > limit))
    }

    fn enforce_limit(&self) -> EgtResult<()> {
        loop {
            let victim = {
                let cache = self
                    .cache
                    .lock()
                    .map_err(|_| EgtError::Internal("cache memory lock poisoned"))?;
                if cache.assigned.is_none_or(|limit| cache.resident <= limit) {
                    return Ok(());
                }
                cache
                    .frames
                    .iter()
                    .min_by_key(|(_, frame)| frame.last_used)
                    .map(|(key, frame)| (*key, frame.last_used, frame.file.clone()))
            };
            let Some((key, last_used, weak_file)) = victim else {
                return Err(EgtError::Internal(
                    "cache accounting has no evictable frame",
                ));
            };
            let Some(file) = weak_file.upgrade() else {
                let mut cache = self
                    .cache
                    .lock()
                    .map_err(|_| EgtError::Internal("cache memory lock poisoned"))?;
                if cache.frames.get(&key).map(|frame| frame.last_used) == Some(last_used) {
                    let frame = cache.frames.remove(&key).ok_or(EgtError::Internal(
                        "cache frame missing during stale-entry removal",
                    ))?;
                    cache.resident -= frame.bytes;
                }
                continue;
            };

            // Never wait for a file while holding the memory lock. Once the
            // write guard is held, revalidate that no newer access touched it.
            let mut file = file
                .write()
                .map_err(|_| EgtError::Internal("dependency file lock poisoned"))?;
            let mut cache = self
                .cache
                .lock()
                .map_err(|_| EgtError::Internal("cache memory lock poisoned"))?;
            if cache.frames.get(&key).map(|frame| frame.last_used) != Some(last_used) {
                continue;
            }
            if file.frames[key.frame].is_none() {
                return Err(EgtError::Internal("cached frame is not resident"));
            }
            file.frames[key.frame] = None;
            let frame = cache
                .frames
                .remove(&key)
                .ok_or(EgtError::Internal("cache frame missing during eviction"))?;
            cache.resident -= frame.bytes;
        }
    }

    fn forget_file(&self, file: &Arc<RwLock<EgtFileReader>>) -> EgtResult<()> {
        let file_id = Arc::as_ptr(file) as usize;
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| EgtError::Internal("cache memory lock poisoned"))?;
        let mut released = 0;
        cache.frames.retain(|key, frame| {
            if key.file == file_id {
                released += frame.bytes;
                false
            } else {
                true
            }
        });
        cache.resident -= released;
        Ok(())
    }

    /// Pins freshly published, cold output ahead of configured input lookup.
    pub(crate) fn insert(&self, file: EgtFileReader) -> EgtResult<()> {
        let old = self
            .files
            .write()
            .map_err(|_| EgtError::Internal("dependency registry lock poisoned"))?
            .insert(file.egt.tablename().to_owned(), Arc::new(RwLock::new(file)));
        if let Some(old) = old {
            self.forget_file(&old)?;
        }
        Ok(())
    }

    /// Releases completed outputs after verification without discarding warmed dependencies.
    pub(crate) fn remove(&self, names: &[String]) -> EgtResult<()> {
        let mut files = self
            .files
            .write()
            .map_err(|_| EgtError::Internal("dependency registry lock poisoned"))?;
        for name in names {
            if let Some(file) = files.remove(name) {
                self.forget_file(&file)?;
            }
        }
        Ok(())
    }

    /// Bounds cache lifetime between material families; call after workers join.
    pub(crate) fn clear(&self) -> EgtResult<()> {
        self.files
            .write()
            .map_err(|_| EgtError::Internal("dependency registry lock poisoned"))?
            .clear();
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| EgtError::Internal("cache memory lock poisoned"))?;
        cache.frames.clear();
        cache.resident = 0;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn contains_key(&self, name: &str) -> bool {
        self.files.read().unwrap().contains_key(name)
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.files.read().unwrap().is_empty()
    }
}

impl EgtFileReader {
    /// The immutable descriptor of this canonical table.
    pub fn egt(&self) -> &Egt {
        &self.egt
    }

    /// Opens an existing table without modifying storage. Data is read on demand.
    /// Objects must not be replaced while this reader is in use: reads are not
    /// pinned to an object version.
    pub fn new_from_file(base_path: &Path, tablename: &str) -> EgtResult<Self> {
        Self::open_in(&Location::parse(base_path)?, tablename)
    }

    pub(crate) fn open_in(location: &Location, tablename: &str) -> EgtResult<Self> {
        let id = TableId::parse(tablename)?;
        let egt = Egt::from_elements(id.parse_elements())?;
        let index_range = egt.index_range();
        let frame_layout: Vec<_> = (0..index_range)
            .step_by(FRAME_SIZE)
            .map(|start| FrameDescriptor {
                start,
                len: FRAME_SIZE.min(index_range - start),
            })
            .collect();
        let key = location.key(&id.relative_path("ggegt"));
        location.validate_format()?;
        let seek_table = Self::read_seek_table(location, &key, &frame_layout)?;
        let frames = vec![None; frame_layout.len()];
        Ok(Self {
            key,
            egt,
            location: location.clone(),
            seek_table,
            frames,
            frame_layout,
        })
    }

    fn read_seek_table(
        location: &Location,
        key: &ObjectPath,
        frame_layout: &[FrameDescriptor],
    ) -> EgtResult<zeekstd::SeekTable> {
        // Our checksum-free seek tables have 8-byte entries and 17 bytes of framing.
        let table_size = 17 + 8 * frame_layout.len() as u64;
        let options = GetOptions::new().with_range(Some(GetRange::Suffix(table_size)));
        let result = block_on(location.store.get_opts(key, options))
            .map_err(|error| location.error(key, error))?;
        let table_start = result.range.start;
        let bytes = block_on(result.bytes()).map_err(|error| location.error(key, error))?;
        if bytes.len() as u64 != table_size {
            return Err(EgtError::Internal("truncated tablebase seek table"));
        }
        // zeekstd allocates from the footer count before checking the source length.
        let count = u32::from_le_bytes(bytes[bytes.len() - 9..bytes.len() - 5].try_into().unwrap());
        if count as usize != frame_layout.len() {
            return Err(EgtError::Internal("invalid tablebase frame count"));
        }
        let mut source = zeekstd::BytesWrapper::new(&bytes);
        let table = zeekstd::SeekTable::from_seekable(&mut source)?;
        if table.size_comp() != table_start {
            return Err(EgtError::Internal("invalid tablebase compressed length"));
        }
        for (index, frame) in frame_layout.iter().enumerate() {
            if table.frame_size_decomp(index as u32)? != (frame.len * 2) as u64
                || table.frame_size_comp(index as u32)? == 0
            {
                return Err(EgtError::Internal("invalid tablebase frame length"));
            }
        }
        Ok(table)
    }

    pub fn num_frames(&self) -> usize {
        self.frames.len()
    }

    /// Probes the outcome of a specific position.
    pub fn probe(&mut self, position: &Chess) -> EgtResult<PackedOutcome> {
        let index = self.map_position_to_index(position)?;
        self.read_from_index(index)
    }

    /// Probes without allocating, decompressing, or reading from disk.
    /// `None` means the frame is cold, not that the outcome is invalid/unknown.
    #[cfg(test)]
    pub(crate) fn probe_cached(&self, position: &Chess) -> EgtResult<Option<PackedOutcome>> {
        self.read_from_index_cached(self.map_position_to_index(position)?)
    }

    fn read_from_index_cached(&self, index: usize) -> EgtResult<Option<PackedOutcome>> {
        if index >= self.egt.index_range() {
            return Err(EgtError::IndexOutOfRange {
                index,
                range: self.egt.index_range(),
            });
        }
        let (frame_idx, offset) = self.locate_frame(index);
        Ok(self.frames[frame_idx]
            .as_ref()
            .map(|uncompressed| uncompressed[offset]))
    }

    /// Get uncompressed data for the frame at `frame_index`.
    fn get_frame_data(&mut self, frame_idx: usize) -> EgtResult<&[PackedOutcome]> {
        self.ensure_uncompressed_frames(&[frame_idx])?;

        self.frames[frame_idx]
            .as_deref()
            .ok_or(EgtError::Internal("expected resident frame after loading"))
    }

    /// Returns the in-memory outcomes for the table-local indices `[start, end)`,
    /// loading and decompressing the frame if needed. The range must lie
    /// within a single frame. This gives table scans direct slice access,
    /// avoiding the per-element overhead of `read_from_index`.
    pub fn frame_chunk(&mut self, start: usize, end: usize) -> EgtResult<&[PackedOutcome]> {
        if start > end || end > self.egt.index_range() {
            return Err(EgtError::IndexOutOfRange {
                index: end,
                range: self.egt.index_range(),
            });
        }
        if start == end {
            return Ok(&[]);
        }
        let (frame_idx, offset) = self.locate_frame(start);
        if end - start > self.frame_layout[frame_idx].len - offset {
            return Err(EgtError::Internal(
                "frame_chunk range crosses a frame boundary",
            ));
        }
        let data = self.get_frame_data(frame_idx)?;
        Ok(&data[offset..offset + (end - start)])
    }

    fn frame_chunk_cached(
        &self,
        start: usize,
        end: usize,
    ) -> EgtResult<Option<&[PackedOutcome]>> {
        if start > end || end > self.egt.index_range() {
            return Err(EgtError::IndexOutOfRange {
                index: end,
                range: self.egt.index_range(),
            });
        }
        if start == end {
            return Ok(Some(&[]));
        }
        let (frame_idx, offset) = self.locate_frame(start);
        if end - start > self.frame_layout[frame_idx].len - offset {
            return Err(EgtError::Internal(
                "frame_chunk range crosses a frame boundary",
            ));
        }
        Ok(self.frames[frame_idx]
            .as_ref()
            .map(|uncompressed| &uncompressed[offset..offset + end - start]))
    }

    /// Maps a position to the corresponding index. This is used when probing
    /// for a specific position.
    pub fn map_position_to_index(&self, position: &Chess) -> EgtResult<usize> {
        let (name, reflected) = position_table_name(position)?;
        if name.as_str() != self.egt.tablename() {
            return Err(EgtError::PositionNotInTable {
                table: self.egt.tablename().to_owned(),
            });
        }
        self.map_routed_position_to_index(position, reflected)
    }

    /// Indexes a position already routed to this table. The caller must have
    /// selected this identity and its reflection flag via position_table_name.
    pub(crate) fn map_routed_position_to_index(
        &self,
        position: &Chess,
        reflected: bool,
    ) -> EgtResult<usize> {
        let index = if reflected {
            self.egt.reflected_position_to_index(position)
        } else {
            self.egt.position_to_index(position)
        };
        if index >= self.egt.index_range() {
            return Err(EgtError::IndexOutOfRange {
                index,
                range: self.egt.index_range(),
            });
        }
        Ok(index)
    }

    /// Converts a table-local index to a position.
    pub fn index_to_position(&self, index: usize, side_to_move: Color) -> Option<Chess> {
        self.egt.position_from_index(index, side_to_move)
    }

    fn locate_frame(&self, index: usize) -> (usize, usize) {
        let frame_idx = self
            .frame_layout
            .partition_point(|frame| frame.start <= index)
            - 1;
        (frame_idx, index - self.frame_layout[frame_idx].start)
    }

    fn frame_bytes(&self, frame_idx: usize) -> usize {
        self.frame_layout[frame_idx].len * std::mem::size_of::<PackedOutcome>()
    }

    /// Reads an outcome directly by its table-local index.
    pub fn read_from_index(&mut self, index: usize) -> EgtResult<PackedOutcome> {
        if index >= self.egt.index_range() {
            return Err(EgtError::IndexOutOfRange {
                index,
                range: self.egt.index_range(),
            });
        }

        let (frame_idx, offset) = self.locate_frame(index);

        let data = self.get_frame_data(frame_idx)?;
        Ok(data[offset])
    }

    fn read_indices_cached(&self, indices: &[usize]) -> EgtResult<Option<Vec<PackedOutcome>>> {
        indices
            .iter()
            .map(|&index| self.read_from_index_cached(index))
            .collect()
    }

    /// Loads distinct requested frames with one multi-range call, skipping warm frames.
    fn ensure_uncompressed_frames(&mut self, frames: &[usize]) -> EgtResult<()> {
        let missing: Vec<_> = frames
            .iter()
            .copied()
            .filter(|&frame| self.frames[frame].is_none())
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        let table = &self.seek_table;
        let ranges = missing
            .iter()
            .map(|&frame| {
                Ok(table.frame_start_comp(frame as u32)?..table.frame_end_comp(frame as u32)?)
            })
            .collect::<EgtResult<Vec<_>>>()?;
        // Local get_ranges reports partial EOF differently from get_range.
        let compressed = if let [range] = ranges.as_slice() {
            block_on(self.location.store.get_range(&self.key, range.clone()))
                .map(|bytes| vec![bytes])
        } else {
            block_on(self.location.store.get_ranges(&self.key, &ranges))
        }
        .map_err(|error| self.location.error(&self.key, error))?;
        if compressed.len() != missing.len() {
            return Err(EgtError::Internal("invalid tablebase range count"));
        }
        for ((frame, range), bytes) in missing.into_iter().zip(ranges).zip(compressed) {
            if bytes.len() as u64 != range.end - range.start {
                return Err(EgtError::Internal("truncated tablebase range"));
            }
            self.frames[frame] = Some(decode_frame(&bytes, self.frame_layout[frame].len)?);
        }
        Ok(())
    }
}

fn decode_frame(compressed: &[u8], len: usize) -> EgtResult<Vec<PackedOutcome>> {
    let mut transposed = vec![0; len * 2];
    // One-shot decoding is bounded by both slices and rejects truncated input.
    // zeekstd's streaming decoder can otherwise loop at premature EOF.
    let decoded = zstd_safe::decompress(&mut transposed[..], compressed).map_err(|code| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "invalid compressed tablebase frame: {}",
                zstd_safe::get_error_name(code)
            ),
        )
    })?;
    if decoded != transposed.len() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "decompressed tablebase frame has wrong length",
        )
        .into());
    }
    Ok(detranspose_frame(&transposed, len))
}

/// Transposes (bit-slices) a frame of `PackedOutcome` values using an 8-8
/// pattern: the low bytes (bits 0-7) are written first, then the high bytes
/// (bits 8-15).
pub fn transpose_frame(uncompressed: &[PackedOutcome]) -> Vec<u8> {
    let n = uncompressed.len();
    let mut output = vec![0u8; n * 2];

    // Slice 0 (bits 0-7) -> n bytes
    // Slice 1 (bits 8-15) -> n bytes
    let mut j = n;
    for i in 0..n {
        let val = uncompressed[i].to_u16();
        if val == 0b000 || val == 0b001 {
            output[i] = val as u8;
        } else {
            output[i] = (val & 0xFF) as u8;
            output[j] = (val >> 8) as u8;
            j += 1;
        }
    }

    output
}

/// Reverses the transposition (un-bit-slices) of a frame produced by
/// `transpose_frame` using the 8-8 pattern.
pub fn detranspose_frame(transposed: &[u8], n: usize) -> Vec<PackedOutcome> {
    let mut output = vec![PackedOutcome::INVALID; n];

    let mut j = n;
    for i in 0..n {
        if transposed[i] == 0b000 || transposed[i] == 0b001 {
            let val = transposed[i] as u16;
            output[i] = PackedOutcome::from_u16(val);
        } else {
            let val = u16::from_le_bytes([transposed[i], transposed[j]]);
            output[i] = PackedOutcome::from_u16(val);
            j += 1;
        }
    }
    output
}

/// Mirrors a chess position horizontally for independent indexing checks.
#[cfg(test)]
pub fn mirror_horizontally(position: &Chess) -> EgtResult<Chess> {
    let mut setup = position.to_setup(shakmaty::EnPassantMode::Legal);
    setup.board.flip_horizontal();
    setup.ep_square = setup.ep_square.map(|sq| sq.flip_horizontal());
    setup
        .position(CastlingMode::Standard)
        .map_err(|e| EgtError::InvalidPosition(format!("mirrored position is invalid: {:?}", e)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egt_writer::write_fixture;
    use shakmaty::Color;
    use shakmaty::fen::Fen;

    fn table_path(base: &Path, name: &str) -> std::path::PathBuf {
        let path = base.join(TableId::parse(name).unwrap().relative_path("ggegt"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        path
    }

    #[test]
    fn pawn_orders_route_to_distinct_tables_and_their_twins() {
        for name in ["KPa_KPa", "KPao_KPao", "KPaPa_KPb", "KPa_KPbPb"] {
            let id = TableId::parse(name).unwrap();
            let egt = Egt::from_elements(id.parse_elements()).unwrap();
            let (twin, _) = id.twin().unwrap();
            let twin_egt = Egt::from_elements(twin.parse_elements()).unwrap();
            let stride = (egt.index_range() / 64).max(1);
            for turn in [Color::White, Color::Black] {
                for index in (0..egt.index_range()).step_by(stride) {
                    let Some(position) = egt.position_from_index(index, turn) else {
                        continue;
                    };
                    assert_eq!(TableId::from_position(&position).unwrap().0, id);
                    assert_eq!(egt.position_to_index(&position), index);
                    for mv in position.legal_moves() {
                        if mv.is_capture() || mv.is_promotion() {
                            continue;
                        }
                        let successor = position.clone().play(mv).unwrap();
                        let (routed, reflected) = TableId::from_position(&successor).unwrap();
                        assert_eq!(routed, twin);
                        let local = if reflected {
                            twin_egt.reflected_position_to_index(&successor)
                        } else {
                            twin_egt.position_to_index(&successor)
                        };
                        assert_eq!(
                            twin_egt
                                .position_from_index(local, successor.turn())
                                .unwrap(),
                            if reflected {
                                mirror_horizontally(&successor).unwrap()
                            } else {
                                successor
                            }
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn requires_format_and_uses_exact_hierarchical_namespace() {
        let dir = crate::TestDir::new("exact_namespace");
        let legacy = dir.0.join("KP_K.ggegt");
        std::fs::write(&legacy, b"legacy indexing").unwrap();
        assert!(EgtFileReader::new_from_file(&dir.0, "KPa_K").is_err());
        assert!(!dir.0.join("format.json").exists());
        let file = write_fixture(&dir.0, "KPa_K", |_| PackedOutcome::INVALID);
        assert!(table_path(&dir.0, file.egt().tablename()).exists());
        assert_eq!(std::fs::read(legacy).unwrap(), b"legacy indexing");
        assert!(EgtFileReader::new_from_file(&dir.0, "KPa_K").is_ok());
        assert!(EgtFileReader::new_from_file(&dir.0, "KP_K").is_err());
    }

    #[test]
    fn cached_probe_does_not_warm_frames() {
        let dir = crate::TestDir::new("cached_probe_does_not_warm_frames");
        let position: Chess = "8/8/8/8/8/8/P7/K6k w - - 0 1"
            .parse::<Fen>()
            .unwrap()
            .into_position(CastlingMode::Standard)
            .unwrap();
        for outcome in [
            PackedOutcome::INVALID,
            PackedOutcome::new_unknown(3),
            PackedOutcome::DRAW,
        ] {
            let mut file = write_fixture(&dir.0, "KPa_K", |_| outcome);
            assert_eq!(file.probe_cached(&position).unwrap(), None);
            assert!(file.frames.iter().all(Option::is_none));
            assert_eq!(file.probe(&position).unwrap(), outcome);
            assert_eq!(file.probe_cached(&position).unwrap(), Some(outcome));
        }
    }

    #[test]
    fn shared_dependency_cold_and_warm_probes() {
        let base = crate::TestDir::new("shared_dependency_base");
        let input = crate::TestDir::new("shared_dependency_input");
        let position: Chess = "8/8/8/8/8/8/P7/K6k w - - 0 1"
            .parse::<Fen>()
            .unwrap()
            .into_position(CastlingMode::Standard)
            .unwrap();
        let outcome = PackedOutcome::new_win(ConversionType::Promotion, 7);
        // Distinct values verify that lazy loading honors input-path precedence.
        for (path, value) in [(&base.0, PackedOutcome::DRAW), (&input.0, outcome)] {
            write_fixture(path, "KPa_K", |_| value);
        }
        let reader = SharedFileCache::new(
            &Location::parse(&base.0).unwrap(),
            Some(&Location::parse(&input.0).unwrap()),
            None,
        );
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let reader = &reader;
                let position = &position;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    for _ in 0..20 {
                        assert_eq!(reader.probe(position).unwrap(), outcome);
                    }
                });
            }
        });
        // A warm probe needs only shared guards for both registry and file.
        let registry_guard = reader.files.read().unwrap();
        let file = Arc::clone(registry_guard.get("KPa_K").unwrap());
        let guard = file.read().unwrap();
        assert_eq!(reader.probe(&position).unwrap(), outcome);
        assert_eq!(guard.probe_cached(&position).unwrap(), Some(outcome));
        assert_eq!(
            guard.frames.iter().filter(|frame| frame.is_some()).count(),
            1
        );
    }

    #[test]
    fn shared_cache_evicts_lru_frames_and_accounts_for_short_frames() {
        let dir = crate::TestDir::new("shared_cache_lru");
        let positions = [
            ("KQ_K", "8/8/8/8/8/8/3Q4/K6k w - - 0 1"),
            ("KR_K", "8/8/8/8/8/8/3R4/K6k w - - 0 1"),
            ("KB_K", "8/8/8/8/8/8/3B4/K6k w - - 0 1"),
        ];
        let mut frame_bytes = None;
        for (name, _) in positions {
            let file = write_fixture(&dir.0, name, |_| PackedOutcome::DRAW);
            assert_eq!(file.num_frames(), 1);
            let bytes = file.frame_bytes(0);
            assert_eq!(bytes, file.egt.index_range() * 2);
            assert_eq!(frame_bytes.get_or_insert(bytes), &bytes);
        }
        let frame_bytes = frame_bytes.unwrap();
        let cache = SharedFileCache::new(
            &Location::parse(&dir.0).unwrap(),
            None,
            Some(2 * frame_bytes),
        );
        let positions = positions.map(|(name, fen)| {
            (
                name,
                fen.parse::<Fen>()
                    .unwrap()
                    .into_position(CastlingMode::Standard)
                    .unwrap(),
            )
        });
        let resident = |name| {
            let file = cache.file(name).unwrap();
            let file = file.read().unwrap();
            file.frames[0].is_some()
        };

        cache.probe(&positions[0].1).unwrap();
        cache.probe(&positions[1].1).unwrap();
        cache.probe(&positions[0].1).unwrap(); // Warm hit makes KR_K oldest.
        cache.probe(&positions[2].1).unwrap();
        assert!(resident("KQ_K"));
        assert!(!resident("KR_K"));
        assert!(resident("KB_K"));
        assert_eq!(cache.cache.lock().unwrap().resident, 2 * frame_bytes);

        cache.probe(&positions[1].1).unwrap();
        assert!(!resident("KQ_K"));
        assert!(resident("KR_K"));
        assert!(resident("KB_K"));

        std::thread::scope(|scope| {
            for _ in 0..4 {
                let cache = &cache;
                let positions = &positions;
                scope.spawn(move || {
                    for _ in 0..20 {
                        for (_, position) in positions {
                            assert_eq!(cache.probe(position).unwrap(), PackedOutcome::DRAW);
                        }
                    }
                });
            }
        });
        assert!(cache.cache.lock().unwrap().resident <= 2 * frame_bytes);

        cache.set_cache_size(0).unwrap();
        assert_eq!(cache.cache.lock().unwrap().resident, 0);
        assert!(positions.iter().all(|(name, _)| !resident(name)));
        assert_eq!(cache.probe(&positions[0].1).unwrap(), PackedOutcome::DRAW);
        assert!(!resident("KQ_K"));
        assert_eq!(cache.cache.lock().unwrap().resident, 0);
    }

    #[test]
    fn shared_dependency_insert_remove_and_clear_preserve_cache_precedence() {
        let base = crate::TestDir::new("shared_dependency_lifecycle_base");
        let input = crate::TestDir::new("shared_dependency_lifecycle_input");
        let pawn: Chess = "8/8/8/8/8/8/P7/K6k w - - 0 1"
            .parse::<Fen>()
            .unwrap()
            .into_position(CastlingMode::Standard)
            .unwrap();
        let queen: Chess = "8/8/8/8/8/8/Q7/K6k w - - 0 1"
            .parse::<Fen>()
            .unwrap()
            .into_position(CastlingMode::Standard)
            .unwrap();
        let dependency_value = PackedOutcome::new_win(ConversionType::Promotion, 7);
        let output_value = PackedOutcome::new_win(ConversionType::Checkmate, 3);
        for (path, name, value) in [
            (&input.0, "KPa_K", dependency_value),
            (&input.0, "KQ_K", PackedOutcome::DRAW),
            (&base.0, "KQ_K", output_value),
        ] {
            write_fixture(path, name, |_| value);
        }
        let reader = SharedFileCache::new(
            &Location::parse(&base.0).unwrap(),
            Some(&Location::parse(&input.0).unwrap()),
            None,
        );
        assert_eq!(reader.probe(&pawn).unwrap(), dependency_value);
        assert_eq!(reader.probe(&queen).unwrap(), PackedOutcome::DRAW);
        let dependency = Arc::downgrade(&reader.files.read().unwrap()["KPa_K"]);
        let old_output = Arc::downgrade(&reader.files.read().unwrap()["KQ_K"]);

        reader
            .insert(EgtFileReader::new_from_file(&base.0, "KQ_K").unwrap())
            .unwrap();
        assert!(old_output.upgrade().is_none());
        assert_eq!(reader.probe(&queen).unwrap(), output_value);
        let pinned_output = Arc::downgrade(&reader.files.read().unwrap()["KQ_K"]);
        reader.remove(&["KQ_K".to_owned()]).unwrap();
        assert!(pinned_output.upgrade().is_none());
        assert!(dependency.upgrade().is_some());
        assert_eq!(reader.probe(&pawn).unwrap(), dependency_value);
        assert_eq!(reader.probe(&queen).unwrap(), PackedOutcome::DRAW);
        assert_eq!(reader.probe_outcome(&queen).unwrap(), DtcOutcome::Draw);

        reader.clear().unwrap();
        assert!(dependency.upgrade().is_none());
        assert!(reader.files.read().unwrap().is_empty());
        assert_eq!(reader.probe(&pawn).unwrap(), dependency_value);
        assert_eq!(reader.probe(&queen).unwrap(), PackedOutcome::DRAW);
    }

    #[test]
    fn shared_dependency_missing_returns_error_without_generating() {
        let dir = crate::TestDir::new("shared_dependency_missing");
        let position: Chess = "8/8/8/8/8/8/P7/K6k w - - 0 1"
            .parse::<Fen>()
            .unwrap()
            .into_position(CastlingMode::Standard)
            .unwrap();
        let reader = SharedFileCache::new(&Location::parse(&dir.0).unwrap(), None, None);
        assert!(
            matches!(reader.probe(&position), Err(EgtError::DependencyUnavailable {
            dependency, source,
        }) if dependency == "KPa_K" && matches!(*source, EgtError::FileNotFound(_)))
        );
        assert!(
            !dir.0
                .join(TableId::parse("KPa_K").unwrap().relative_path("ggegt"))
                .exists()
        );
        assert!(reader.is_empty());
    }

    #[test]
    fn test_egt_file_creation() {
        let dir = crate::TestDir::new("single_table_creation");
        let file = write_fixture(&dir.0, "KPa_K", |_| PackedOutcome::INVALID);
        assert_eq!(file.egt().tablename(), "KPa_K");
        let other: Chess = "8/8/8/8/8/8/1P6/K6k w - - 0 1"
            .parse::<Fen>()
            .unwrap()
            .into_position(CastlingMode::Standard)
            .unwrap();
        assert!(matches!(
            file.map_position_to_index(&other),
            Err(EgtError::PositionNotInTable { .. })
        ));
        let other_material: Chess = "8/8/8/8/8/8/Q7/K6k w - - 0 1"
            .parse::<Fen>()
            .unwrap()
            .into_position(CastlingMode::Standard)
            .unwrap();
        assert!(matches!(
            file.map_position_to_index(&other_material),
            Err(EgtError::PositionNotInTable { .. })
        ));
        assert!(
            file.index_to_position(file.egt.index_range(), Color::White)
                .is_none()
        );
    }

    fn run_round_trip_test(tablename: &str, stride: usize) {
        let egt = Egt::from_tablename(tablename).unwrap();
        for turn in [Color::White, Color::Black] {
            for index in (0..egt.index_range()).step_by(stride) {
                if let Some(position) = egt.position_from_index(index, turn) {
                    assert_eq!(egt.position_to_index(&position), index);
                }
            }
        }
    }

    #[test]
    fn egt_file_decode_encode_kpp_kpp() {
        run_round_trip_test("KPaPb_KPcPd", 100_000);
    }

    #[test]
    fn egt_file_decode_encode_kq_kr() {
        run_round_trip_test("KQ_KR", 100);
    }

    #[test]
    fn egt_file_decode_encode_kppp_k() {
        run_round_trip_test("KPaPbPc_K", 100);
    }

    #[test]
    fn egt_file_decode_encode_kpp_kp() {
        run_round_trip_test("KPaPb_KPc", 500);
    }

    #[test]
    fn test_transpose_detranspose_roundtrip() {
        // Use a frame size that is a multiple of 8 (as in production).
        let n = 16;
        let mut inputs = vec![PackedOutcome::INVALID; n];
        inputs[4] = PackedOutcome::new_win(ConversionType::Checkmate, 67);
        inputs[6] = PackedOutcome::new_loss(ConversionType::Capture, 12);
        inputs[7] = PackedOutcome::new_win(ConversionType::Promotion, 4567);
        inputs[8] = PackedOutcome::DRAW;
        inputs[10] = PackedOutcome::new_loss(ConversionType::Checkmate, 30);
        inputs[11] = PackedOutcome::new_win(ConversionType::Capture, 1);
        inputs[12] = PackedOutcome::DRAW;
        inputs[13] = PackedOutcome::new_loss(ConversionType::Promotion, 1200);
        let transposed = transpose_frame(&inputs);
        let recovered = detranspose_frame(&transposed, n);
        for i in 0..n {
            assert_eq!(
                inputs[i].to_u16(),
                recovered[i].to_u16(),
                "mismatch at i={}",
                i
            );
        }
    }

    #[test]
    fn published_file_probes_canonical_and_mirrored_positions() {
        let dir = crate::TestDir::new("published_file_probe");
        let position = "8/8/8/8/8/8/P7/K6k w - - 0 1"
            .parse::<Fen>()
            .unwrap()
            .into_position(CastlingMode::Standard)
            .unwrap();
        let egt = Egt::from_tablename("KPa_K").unwrap();
        let index = egt.position_to_index(&position);
        let outcome = PackedOutcome::new_win(ConversionType::Checkmate, 987);
        let mut reader = write_fixture(&dir.0, "KPa_K", |i| {
            if i == index {
                outcome
            } else {
                PackedOutcome::INVALID
            }
        });
        assert!(reader.frames.iter().all(Option::is_none));
        for position in [&position, &mirror_horizontally(&position).unwrap()] {
            assert_eq!(reader.map_position_to_index(position).unwrap(), index);
            assert_eq!(reader.probe(position).unwrap(), outcome);
        }
        let mut reopened = EgtFileReader::new_from_file(&dir.0, "KPa_K").unwrap();
        assert_eq!(reopened.probe(&position).unwrap(), outcome);
    }

    #[test]
    fn published_file_scan_and_reload() {
        let dir = crate::TestDir::new("published_file_scan");
        let outcome = |idx| {
            if idx % 3 == 0 {
                PackedOutcome::new_win(ConversionType::Promotion, 4567)
            } else {
                PackedOutcome::DRAW
            }
        };
        let mut reader = write_fixture(&dir.0, "KPa_K", outcome);
        assert_eq!(
            reader.num_frames(),
            reader.egt.index_range().div_ceil(FRAME_SIZE)
        );
        for _ in 0..2 {
            for idx in 0..reader.egt.index_range() {
                assert_eq!(reader.read_from_index(idx).unwrap(), outcome(idx));
            }
            reader.frames.fill(None);
        }
        assert_eq!(
            reader
                .frame_chunk(reader.egt.index_range(), reader.egt.index_range())
                .unwrap(),
            &[]
        );
    }

    #[test]
    fn full_frames_short_tail_and_table_boundaries() {
        let dir = crate::TestDir::new("frame_boundaries");
        let length = Egt::from_tablename("KQ_KR").unwrap().index_range();
        let indexes = [0, FRAME_SIZE - 1, FRAME_SIZE, FRAME_SIZE + 1, length - 1];
        let mut reader = write_fixture(&dir.0, "KQ_KR", |idx| {
            indexes
                .iter()
                .position(|&i| i == idx)
                .map_or(PackedOutcome::INVALID, |n| {
                    PackedOutcome::new_loss(ConversionType::Capture, n as u16 + 1)
                })
        });
        assert!(reader.num_frames() > 1);
        assert_eq!(reader.frame_layout.last().unwrap().len, 174420);
        assert!(reader.frame_chunk(FRAME_SIZE - 1, FRAME_SIZE + 1).is_err());
        for (n, &idx) in indexes.iter().enumerate() {
            assert_eq!(
                reader.read_from_index(idx).unwrap(),
                PackedOutcome::new_loss(ConversionType::Capture, n as u16 + 1)
            );
        }
        let pawnful = write_fixture(&dir.0, "KPa_K", |_| PackedOutcome::INVALID);
        assert_eq!(pawnful.num_frames(), 1);
        assert_eq!(pawnful.frame_layout[0].start, 0);
        assert_eq!(pawnful.frame_layout[0].len, pawnful.egt.index_range());
        assert!(reader.read_from_index(reader.egt.index_range()).is_err());
        assert!(reader.frame_chunk(1, 0).is_err());
        assert!(reader.frame_chunk(0, reader.egt.index_range() + 1).is_err());
    }

    #[test]
    fn rejects_invalid_frame_layout() {
        let dir = crate::TestDir::new("invalid_frame_layout");
        let file = write_fixture(&dir.0, "K_K", |_| PackedOutcome::INVALID);
        for bytes in [0, 3, 922, 926, FRAME_SIZE * 2, FRAME_SIZE * 2 + 2] {
            let output = std::fs::File::create(table_path(&dir.0, "K_K")).unwrap();
            let mut encoder = zeekstd::EncodeOptions::new().into_encoder(output).unwrap();
            encoder.compress(&vec![0; bytes]).unwrap();
            encoder.finish().unwrap();
            assert!(EgtFileReader::new_from_file(&dir.0, "K_K").is_err());
        }
        // A correct payload with an extra empty frame has the wrong frame count.
        let output = std::fs::File::create(table_path(&dir.0, "K_K")).unwrap();
        let mut encoder = zeekstd::EncodeOptions::new().into_encoder(output).unwrap();
        encoder.compress(&vec![0; file.egt.index_range() * 2]).unwrap();
        encoder.end_frame().unwrap();
        encoder.finish().unwrap();
        assert!(EgtFileReader::new_from_file(&dir.0, "K_K").is_err());
    }

    #[test]
    fn rejects_malformed_seek_table_suffixes() {
        let dir = crate::TestDir::new("malformed_seek_table_suffixes");
        let path = table_path(&dir.0, "K_K");
        write_fixture(&dir.0, "K_K", |_| PackedOutcome::INVALID);
        let valid = std::fs::read(&path).unwrap();
        let table_start = valid.len() - 25;

        for length in [0, 8, 9, 24] {
            std::fs::write(&path, &valid[valid.len() - length..]).unwrap();
            assert!(EgtFileReader::new_from_file(&dir.0, "K_K").is_err());
        }
        // Reject the declared count before zeekstd can allocate from it.
        let mut inflated = valid.clone();
        let count_start = inflated.len() - 9;
        inflated[count_start..count_start + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        std::fs::write(&path, inflated).unwrap();
        assert!(matches!(
            EgtFileReader::new_from_file(&dir.0, "K_K"),
            Err(EgtError::Internal("invalid tablebase frame count"))
        ));

        // zeekstd, rather than our loader, validates magic and reserved bits.
        for offset in [valid.len() - 1, valid.len() - 5, table_start] {
            let mut corrupt = valid.clone();
            corrupt[offset] ^= 0x04;
            std::fs::write(&path, corrupt).unwrap();
            assert!(EgtFileReader::new_from_file(&dir.0, "K_K").is_err());
        }
        let mut prefixed = vec![0];
        prefixed.extend_from_slice(&valid);
        std::fs::write(&path, prefixed).unwrap();
        assert!(matches!(
            EgtFileReader::new_from_file(&dir.0, "K_K"),
            Err(EgtError::Internal("invalid tablebase compressed length"))
        ));

        // The reader deliberately accepts only our checksum-free seek-table format.
        let mut checksummed = valid.clone();
        checksummed.splice(table_start + 16..table_start + 16, [0; 4]);
        checksummed[table_start + 4..table_start + 8].copy_from_slice(&21u32.to_le_bytes());
        let descriptor = checksummed.len() - 5;
        checksummed[descriptor] |= 0x80;
        std::fs::write(&path, checksummed).unwrap();
        assert!(EgtFileReader::new_from_file(&dir.0, "K_K").is_err());

        std::fs::write(&path, valid).unwrap();
        EgtFileReader::new_from_file(&dir.0, "K_K").unwrap();
    }

    #[test]
    fn rejects_short_frame_range() {
        let dir = crate::TestDir::new("short_frame_range");
        let path = table_path(&dir.0, "K_K");
        let mut reader = write_fixture(&dir.0, "K_K", |_| PackedOutcome::INVALID);
        let frame_end = reader.seek_table.frame_end_comp(0).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_len(frame_end - 1)
            .unwrap();
        assert!(matches!(
            reader.read_from_index(0),
            Err(EgtError::Internal("truncated tablebase range"))
        ));
    }
}
