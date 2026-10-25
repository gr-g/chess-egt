use std::collections::HashMap;

use crate::egt::Egt;
use crate::error::{EgtError, EgtResult};
use crate::stats::EgtStats;
use crate::storage::{Location, block_on};
use crate::table_id::TableId;
use crate::{ConversionType, DtcOutcome};
use object_store::{GetOptions, GetRange, ObjectStoreExt, path::Path as ObjectPath};
#[cfg(test)]
use shakmaty::CastlingMode;
#[cfg(test)]
use shakmaty::Position;
use shakmaty::{Chess, Color};
use std::path::Path;
use std::sync::{Arc, RwLock};

use crate::storage::DEFAULT_FRAME_SIZE;

// Compression level used when storing data on disk.
pub(crate) const DEFAULT_COMPRESSION_LEVEL: i32 = 19;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MaybeDtcOutcome(pub u16);

impl MaybeDtcOutcome {
    pub const INVALID: Self = Self(0b000);
    pub const DRAW: Self = Self(0b001);

    pub fn from_u16(value: u16) -> Self {
        Self(value)
    }

    pub fn to_u16(self) -> u16 {
        self.0
    }

    pub fn is_assigned(self) -> bool {
        (self.0 & 0b111) != 0b000
    }

    pub fn is_invalid(self) -> bool {
        self.0 == 0b000
    }

    pub fn is_unknown(self) -> bool {
        (self.0 & 0b111) == 0b000 && (self.0 >> 3) != 0
    }

    pub fn get_unknown_counter(self) -> u16 {
        self.0 >> 3
    }

    pub fn is_draw(self) -> bool {
        self.0 == 0b001
    }

    pub fn is_win(self) -> bool {
        matches!(self.0 & 0b111, 0b010 | 0b100 | 0b110)
    }

    pub fn is_loss(self) -> bool {
        matches!(self.0 & 0b111, 0b011 | 0b101 | 0b111)
    }

    pub fn new_win(ct: ConversionType, distance: u16) -> Self {
        let bits = match ct {
            ConversionType::Checkmate => 0b010,
            ConversionType::Capture => 0b100,
            ConversionType::Promotion => 0b110,
        };
        Self(bits | (distance << 3))
    }

    pub fn new_loss(ct: ConversionType, distance: u16) -> Self {
        let bits = match ct {
            ConversionType::Checkmate => 0b011,
            ConversionType::Capture => 0b101,
            ConversionType::Promotion => 0b111,
        };
        Self(bits | (distance << 3))
    }

    pub fn new_unknown(counter: u16) -> Self {
        Self(counter << 3)
    }

    pub fn to_outcome_at(self, index: usize) -> EgtResult<DtcOutcome> {
        let n = self.0 >> 3;
        match self.0 & 0b111 {
            0b001 => Ok(DtcOutcome::Draw),
            0b010 => Ok(DtcOutcome::Win(ConversionType::Checkmate, n)),
            0b100 => Ok(DtcOutcome::Win(ConversionType::Capture, n)),
            0b110 => Ok(DtcOutcome::Win(ConversionType::Promotion, n)),
            0b011 => Ok(DtcOutcome::Loss(ConversionType::Checkmate, n)),
            0b101 => Ok(DtcOutcome::Loss(ConversionType::Capture, n)),
            0b111 => Ok(DtcOutcome::Loss(ConversionType::Promotion, n)),
            _ => Err(EgtError::CorruptedOutcome {
                value: self.0,
                index,
            }),
        }
    }
}

/// Represents the state of a single frame in an EgtFileReader.
///
/// The `Compressed` variant (and `ensure_compressed`) are not reached yet: they
/// exist for the planned LRU eviction of in-memory frames, which is what will
/// compress a dirty frame without going through a full `save_to_file()`.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum FrameState {
    /// The frame is empty.
    Empty,
    /// The frame is compressed on file.
    CompressedOnFile,
    /// The frame is compressed in memory.
    Compressed(Vec<u8>),
    /// The frame is fully uncompressed in memory.
    Uncompressed {
        /// Cached compressed bytes to avoid re-compression if not dirty.
        compressed: Vec<u8>,
        /// Uncompressed values.
        uncompressed: Vec<MaybeDtcOutcome>,
        /// True if the compressed bytes are not updated.
        dirty: bool,
    },
}

/// Logical extent of an unpadded, table-local transposed Zstd frame.
#[derive(Debug, Clone, Copy)]
struct FrameDescriptor {
    start: usize,
    len: usize,
}

/// Storage and frame cache for one exact canonical table.
#[derive(Debug)]
pub struct EgtFileReader {
    pub egt: Egt,
    pub id: TableId,
    pub tablename: String,
    pub(crate) location: Location,
    pub(crate) key: ObjectPath,
    seek_table: Option<zeekstd::SeekTable>,
    frames: Vec<FrameState>,
    frame_layout: Vec<FrameDescriptor>,
    /// Maximum number of indexed locations per frame; the tail may be shorter.
    pub frame_size: usize,
    pub index_range: usize,
    pub stats: Option<EgtStats>,
}

/// Shared files for parallel initialization and verification.
///
/// Existing dependencies are loaded lazily; missing tables are reported as errors.
/// Frames are shared, not copied per worker; no eviction is performed.
#[derive(Debug)]
pub(crate) struct SharedDependencyReader {
    files: RwLock<HashMap<String, Arc<RwLock<EgtFileReader>>>>,
    base_path: Location,
    input_path: Option<Location>,
}

impl SharedDependencyReader {
    pub(crate) fn new(
        files: HashMap<String, EgtFileReader>,
        base_path: &Location,
        input_path: Option<&Location>,
    ) -> Self {
        Self {
            files: RwLock::new(
                files
                    .into_iter()
                    .map(|(name, file)| (name, Arc::new(RwLock::new(file))))
                    .collect(),
            ),
            base_path: base_path.clone(),
            input_path: input_path.cloned(),
        }
    }

    fn file(&self, endgame: &str) -> EgtResult<Arc<RwLock<EgtFileReader>>> {
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
                let file =
                    crate::load_existing_file(&self.base_path, self.input_path.as_ref(), endgame)?;
                files.insert(endgame.to_string(), Arc::new(RwLock::new(file)));
            }
            Arc::clone(files.get(endgame).ok_or(EgtError::Internal(
                "dependency registry entry missing after insert",
            ))?)
        };
        Ok(file)
    }

    pub(crate) fn probe(&self, position: &Chess) -> EgtResult<MaybeDtcOutcome> {
        self.probe_indexed(position).map(|(_, outcome)| outcome)
    }

    pub(crate) fn probe_outcome(&self, position: &Chess) -> EgtResult<DtcOutcome> {
        let (index, outcome) = self.probe_indexed(position)?;
        outcome.to_outcome_at(index)
    }

    fn probe_indexed(&self, position: &Chess) -> EgtResult<(usize, MaybeDtcOutcome)> {
        let (id, reflected) = TableId::from_position(position)?;
        let result = (|| {
            let file = self.file(id.tablename())?;
            let index = {
                let reader = file
                    .read()
                    .map_err(|_| EgtError::Internal("dependency file lock poisoned"))?;
                let index = reader.map_routed_position_to_index(position, reflected)?;
                if let Some(outcome) = reader.read_from_index_cached(index)? {
                    return Ok((index, outcome));
                }
                index
            };
            // Metadata is immutable: retain the index while upgrading to a write
            // guard. read_from_index rechecks whether another worker warmed it.
            let mut writer = file
                .write()
                .map_err(|_| EgtError::Internal("dependency file lock poisoned"))?;
            Ok((index, writer.read_from_index(index)?))
        })();
        result.map_err(|source| match source {
            EgtError::FileNotFound(_) => EgtError::DependencyUnavailable {
                dependency: id.tablename().to_owned(),
                source: Box::new(source),
            },
            other => other,
        })
    }

    /// Copies a scan block so no file lock is held while probing successors.
    /// The range must lie within one frame.
    pub(crate) fn read_chunk(
        &self,
        endgame: &str,
        start: usize,
        end: usize,
    ) -> EgtResult<Vec<MaybeDtcOutcome>> {
        let file = self.file(endgame)?;
        {
            let reader = file
                .read()
                .map_err(|_| EgtError::Internal("dependency file lock poisoned"))?;
            if let Some(chunk) = reader.frame_chunk_cached(start, end)? {
                return Ok(chunk.to_vec());
            }
        }
        let mut writer = file
            .write()
            .map_err(|_| EgtError::Internal("dependency file lock poisoned"))?;
        Ok(writer.frame_chunk(start, end)?.to_vec())
    }

    /// Pins freshly published output ahead of configured input lookup.
    pub(crate) fn insert(&self, file: EgtFileReader) -> EgtResult<()> {
        self.files
            .write()
            .map_err(|_| EgtError::Internal("dependency registry lock poisoned"))?
            .insert(file.tablename.clone(), Arc::new(RwLock::new(file)));
        Ok(())
    }

    /// Releases completed outputs after verification without discarding warmed dependencies.
    pub(crate) fn remove(&self, names: &[String]) -> EgtResult<()> {
        let mut files = self
            .files
            .write()
            .map_err(|_| EgtError::Internal("dependency registry lock poisoned"))?;
        for name in names {
            files.remove(name);
        }
        Ok(())
    }

    /// Bounds cache lifetime between material families; call after workers join.
    pub(crate) fn clear(&self) -> EgtResult<()> {
        self.files
            .write()
            .map_err(|_| EgtError::Internal("dependency registry lock poisoned"))?
            .clear();
        Ok(())
    }

    /// Recovers owned files, including frames warmed by workers, after they join.
    pub(crate) fn into_cache(self) -> EgtResult<HashMap<String, EgtFileReader>> {
        let files = self
            .files
            .into_inner()
            .map_err(|_| EgtError::Internal("dependency registry lock poisoned"))?;
        files
            .into_iter()
            .map(|(name, file)| {
                let file = Arc::try_unwrap(file)
                    .map_err(|_| EgtError::Internal("dependency file still shared"))?
                    .into_inner()
                    .map_err(|_| EgtError::Internal("dependency file lock poisoned"))?;
                Ok((name, file))
            })
            .collect()
    }
}

impl EgtFileReader {
    /// Creates an empty single-table cache and ensures the writable root format.
    pub fn new(base_path: &Path, tablename: &str) -> EgtResult<Self> {
        Self::new_in(&Location::parse(base_path)?, tablename)
    }

    pub(crate) fn new_in(location: &Location, tablename: &str) -> EgtResult<Self> {
        let file = Self::construct(location, tablename)?;
        location.ensure_format()?;
        Ok(file)
    }

    // Construction itself performs no storage I/O, so opening never writes.
    fn construct(location: &Location, tablename: &str) -> EgtResult<Self> {
        let id = TableId::parse(tablename)?;
        let egt = Egt::from_elements(id.elements().to_vec())?;
        let index_range = egt.index_range();
        let frame_size = DEFAULT_FRAME_SIZE;
        let frame_layout: Vec<_> = (0..index_range)
            .step_by(frame_size)
            .map(|start| FrameDescriptor {
                start,
                len: frame_size.min(index_range - start),
            })
            .collect();
        let frames = vec![FrameState::Empty; frame_layout.len()];
        Ok(Self {
            key: location.key(&id.relative_path("ggegt")),
            tablename: id.tablename().to_owned(),
            id,
            egt,
            location: location.clone(),
            seek_table: None,
            frames,
            frame_layout,
            frame_size,
            index_range,
            stats: None,
        })
    }

    /// Opens an existing table without modifying storage. Data is read on demand.
    /// Objects must not be replaced while this reader is in use: reads are not
    /// pinned to an object version.
    pub fn new_from_file(base_path: &Path, tablename: &str) -> EgtResult<Self> {
        Self::open_in(&Location::parse(base_path)?, tablename)
    }

    pub(crate) fn open_in(location: &Location, tablename: &str) -> EgtResult<Self> {
        let mut file = Self::construct(location, tablename)?;
        location.validate_format()?;
        file.load_seek_table()?;
        file.frames.fill(FrameState::CompressedOnFile);
        Ok(file)
    }

    fn load_seek_table(&mut self) -> EgtResult<()> {
        // Our checksum-free seek tables have 8-byte entries and 17 bytes of framing.
        let table_size = 17 + 8 * self.num_frames() as u64;
        let options = GetOptions::new().with_range(Some(GetRange::Suffix(table_size)));
        let result = block_on(self.location.store.get_opts(&self.key, options))
            .map_err(|error| self.location.error(&self.key, error))?;
        let table_start = result.range.start;
        let bytes =
            block_on(result.bytes()).map_err(|error| self.location.error(&self.key, error))?;
        if bytes.len() as u64 != table_size {
            return Err(EgtError::Internal("truncated tablebase seek table"));
        }
        // zeekstd allocates from the footer count before checking the source length.
        let count = u32::from_le_bytes(bytes[bytes.len() - 9..bytes.len() - 5].try_into().unwrap());
        if count as usize != self.num_frames() {
            return Err(EgtError::Internal("invalid tablebase frame count"));
        }
        let mut source = zeekstd::BytesWrapper::new(&bytes);
        let table = zeekstd::SeekTable::from_seekable(&mut source)?;
        if table.size_comp() != table_start {
            return Err(EgtError::Internal("invalid tablebase compressed length"));
        }
        for (index, frame) in self.frame_layout.iter().enumerate() {
            if table.frame_size_decomp(index as u32)? != (frame.len * 2) as u64
                || table.frame_size_comp(index as u32)? == 0
            {
                return Err(EgtError::Internal("invalid tablebase frame length"));
            }
        }
        self.seek_table = Some(table);
        Ok(())
    }

    pub(crate) fn save_stats(&self, stats: &EgtStats) -> EgtResult<()> {
        self.location.ensure_format()?;
        let key = self.location.key(&self.id.relative_path("json"));
        let json = serde_json::to_vec_pretty(stats)?;
        block_on(self.location.store.put(&key, json.into()))
            .map_err(|error| self.location.error(&key, error))?;
        Ok(())
    }

    pub fn num_frames(&self) -> usize {
        self.frames.len()
    }

    /// Probes the outcome of a specific position.
    pub fn probe(&mut self, position: &Chess) -> EgtResult<MaybeDtcOutcome> {
        let index = self.map_position_to_index(position)?;
        self.read_from_index(index)
    }

    /// Probes without allocating, decompressing, or reading from disk.
    /// `None` means the frame is cold, not that the outcome is invalid/unknown.
    #[cfg(test)]
    pub(crate) fn probe_cached(&self, position: &Chess) -> EgtResult<Option<MaybeDtcOutcome>> {
        self.read_from_index_cached(self.map_position_to_index(position)?)
    }

    fn read_from_index_cached(&self, index: usize) -> EgtResult<Option<MaybeDtcOutcome>> {
        if index >= self.index_range {
            return Err(EgtError::IndexOutOfRange {
                index,
                range: self.index_range,
            });
        }
        let (frame_idx, offset) = self.locate_frame(index);
        match &self.frames[frame_idx] {
            FrameState::Uncompressed { uncompressed, .. } => Ok(Some(uncompressed[offset])),
            _ => Ok(None),
        }
    }

    /// Save the entire table using seekable Zstd compression.
    ///
    /// Afterwards all frames are left in the `CompressedOnFile` state: the
    /// in-memory data is released and re-read from the object on demand.
    pub fn save_to_file(&mut self) -> EgtResult<u64> {
        use crate::egt_writer::{encode_frame, publish_buffers};
        use std::io::{Read, Write};
        use zeekstd::EncodeOptions;
        self.location.ensure_format()?;
        let mut compressed = Vec::new();
        let mut encoder = EncodeOptions::new()
            .compression_level(DEFAULT_COMPRESSION_LEVEL)
            .into_encoder(&mut compressed)?;

        // Complete encoding before publication so disk-backed input stays readable.
        for frame_idx in 0..self.frames.len() {
            self.ensure_uncompressed(frame_idx)?;

            // Transpose the frame
            if let FrameState::Uncompressed { uncompressed, .. } = &self.frames[frame_idx] {
                encode_frame(&mut encoder, uncompressed)?;
            } else {
                return Err(EgtError::Internal(
                    "expected Uncompressed frame state after ensure_uncompressed",
                ));
            }
        }

        encoder.flush()?;
        let table = encoder.into_seek_table();
        table.into_serializer().read_to_end(&mut compressed)?;
        let (bytes, _) = publish_buffers(
            &self.location,
            &self.key,
            std::iter::once(compressed.into()),
        )?;
        self.load_seek_table()?;
        self.frames.fill(FrameState::CompressedOnFile);
        Ok(bytes)
    }

    /// Get uncompressed data for the frame at `frame_index`.
    fn get_frame_data(&mut self, frame_idx: usize) -> EgtResult<&mut [MaybeDtcOutcome]> {
        self.ensure_uncompressed(frame_idx)?;

        if let FrameState::Uncompressed { uncompressed, .. } = &mut self.frames[frame_idx] {
            Ok(uncompressed)
        } else {
            Err(EgtError::Internal(
                "expected Uncompressed frame state after ensure_uncompressed",
            ))
        }
    }

    /// Returns the in-memory outcomes for the table-local indexes `[start, end)`,
    /// allocating or decompressing the frame if needed. The range must lie
    /// within a single frame. This gives table scans direct slice access,
    /// avoiding the per-element overhead of `read_from_index`.
    pub fn frame_chunk(&mut self, start: usize, end: usize) -> EgtResult<&[MaybeDtcOutcome]> {
        if start > end || end > self.index_range {
            return Err(EgtError::IndexOutOfRange {
                index: end,
                range: self.index_range,
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
    ) -> EgtResult<Option<&[MaybeDtcOutcome]>> {
        if start > end || end > self.index_range {
            return Err(EgtError::IndexOutOfRange {
                index: end,
                range: self.index_range,
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
        match &self.frames[frame_idx] {
            FrameState::Uncompressed { uncompressed, .. } => {
                Ok(Some(&uncompressed[offset..offset + end - start]))
            }
            _ => Ok(None),
        }
    }

    /// Maps a position to the corresponding index. This is used when probing
    /// for a specific position.
    pub fn map_position_to_index(&self, position: &Chess) -> EgtResult<usize> {
        let (id, reflected) = TableId::from_position(position)?;
        if id != self.id {
            return Err(EgtError::PositionNotInTable {
                table: self.tablename.clone(),
            });
        }
        self.map_routed_position_to_index(position, reflected)
    }

    /// Indexes a position already routed to this table. The caller must have
    /// selected this identity and its reflection flag via TableId::from_position.
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
        if index >= self.index_range {
            return Err(EgtError::IndexOutOfRange {
                index,
                range: self.index_range,
            });
        }
        Ok(index)
    }

    /// Converts a table-local index to a position.
    pub fn index_to_position(&self, index: usize, side_to_move: Color) -> Option<Chess> {
        if index >= self.index_range {
            return None;
        }
        self.egt.position_from_index(index, side_to_move)
    }

    fn locate_frame(&self, index: usize) -> (usize, usize) {
        let frame_idx = self
            .frame_layout
            .partition_point(|frame| frame.start <= index)
            - 1;
        (frame_idx, index - self.frame_layout[frame_idx].start)
    }

    /// Reads an outcome directly by its table-local index.
    pub fn read_from_index(&mut self, index: usize) -> EgtResult<MaybeDtcOutcome> {
        if index >= self.index_range {
            return Err(EgtError::IndexOutOfRange {
                index,
                range: self.index_range,
            });
        }

        let (frame_idx, offset) = self.locate_frame(index);

        let data = self.get_frame_data(frame_idx)?;
        Ok(data[offset])
    }

    /// Writes an outcome directly by its table-local index.
    pub fn write_to_index(&mut self, index: usize, outcome: MaybeDtcOutcome) -> EgtResult<()> {
        if index >= self.index_range {
            return Err(EgtError::IndexOutOfRange {
                index,
                range: self.index_range,
            });
        }

        let (frame_idx, offset) = self.locate_frame(index);

        self.ensure_uncompressed(frame_idx)?;

        if let FrameState::Uncompressed {
            uncompressed,
            dirty,
            ..
        } = &mut self.frames[frame_idx]
        {
            uncompressed[offset] = outcome;
            *dirty = true;
            Ok(())
        } else {
            Err(EgtError::Internal(
                "expected Uncompressed frame state after ensure_uncompressed",
            ))
        }
    }

    /// Ensures that the frame at `frame_idx` is in the `Uncompressed` state.
    /// Allocates memory and decompresses if necessary.
    fn ensure_uncompressed(&mut self, frame_idx: usize) -> EgtResult<()> {
        let frame = self.frame_layout[frame_idx];
        match &self.frames[frame_idx] {
            FrameState::Empty => {
                // TODO: Allocate memory from arena
                let uncompressed = vec![MaybeDtcOutcome::INVALID; frame.len];

                self.frames[frame_idx] = FrameState::Uncompressed {
                    compressed: vec![],
                    uncompressed,
                    dirty: true,
                };
                Ok(())
            }
            FrameState::CompressedOnFile => {
                let table = self
                    .seek_table
                    .as_ref()
                    .ok_or(EgtError::Internal("missing tablebase seek table"))?;
                let range = table.frame_start_comp(frame_idx as u32)?
                    ..table.frame_end_comp(frame_idx as u32)?;
                let length = range.end - range.start;
                let compressed = block_on(self.location.store.get_range(&self.key, range))
                    .map_err(|error| self.location.error(&self.key, error))?;
                if compressed.len() as u64 != length {
                    return Err(EgtError::Internal("truncated tablebase range"));
                }
                let uncompressed = decode_frame(&compressed, frame.len)?;
                self.frames[frame_idx] = FrameState::Uncompressed {
                    compressed: vec![],
                    uncompressed,
                    dirty: false,
                };
                Ok(())
            }
            FrameState::Compressed(compressed_bytes) => {
                let uncompressed = decode_frame(compressed_bytes, frame.len)?;
                self.frames[frame_idx] = FrameState::Uncompressed {
                    compressed: compressed_bytes.clone(),
                    uncompressed,
                    dirty: false,
                };
                Ok(())
            }
            FrameState::Uncompressed { .. } => {
                // Already uncompressed
                // TODO: update LRU status
                Ok(())
            }
        }
    }

    /// Ensures that the frame at `frame_idx` is in the `Compressed` state.
    ///
    /// Currently only exercised by tests; see the note on `FrameState`.
    #[allow(dead_code)]
    fn ensure_compressed(&mut self, frame_idx: usize) -> EgtResult<()> {
        match &self.frames[frame_idx] {
            FrameState::Empty | FrameState::CompressedOnFile => {
                self.ensure_uncompressed(frame_idx)?;
                self.ensure_compressed(frame_idx)
            }
            FrameState::Compressed(_) => {
                // Already compressed
                Ok(())
            }
            FrameState::Uncompressed {
                uncompressed,
                dirty,
                ..
            } => {
                if !dirty {
                    let mut temp = FrameState::Empty;
                    std::mem::swap(&mut self.frames[frame_idx], &mut temp);
                    if let FrameState::Uncompressed { compressed, .. } = temp {
                        self.frames[frame_idx] = if compressed.is_empty() {
                            FrameState::CompressedOnFile
                        } else {
                            FrameState::Compressed(compressed)
                        };
                    }
                } else {
                    // Regenerate the compressed bytes
                    // TODO: Use the exact compressed frame bytes that would be used for the
                    // full compressed file.
                    use zeekstd::EncodeOptions;

                    let mut compressed_bytes = vec![];
                    let mut encoder = EncodeOptions::new()
                        .compression_level(DEFAULT_COMPRESSION_LEVEL)
                        .into_encoder(&mut compressed_bytes)?;

                    // Transpose (bit-slice) the frame
                    let transposed = transpose_frame(uncompressed);

                    // Compress the transposed frame
                    encoder.compress(&transposed)?;
                    encoder.end_frame()?;
                    encoder.finish()?;

                    self.frames[frame_idx] = FrameState::Compressed(compressed_bytes);
                }
                Ok(())
            }
        }
    }
}

fn decode_frame(compressed: &[u8], len: usize) -> EgtResult<Vec<MaybeDtcOutcome>> {
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

/// Transposes (bit-slices) a frame of `MaybeDtcOutcome` values using an 8-8
/// pattern: the low bytes (bits 0-7) are written first, then the high bytes
/// (bits 8-15).
pub fn transpose_frame(uncompressed: &[MaybeDtcOutcome]) -> Vec<u8> {
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
pub fn detranspose_frame(transposed: &[u8], n: usize) -> Vec<MaybeDtcOutcome> {
    let mut output = vec![MaybeDtcOutcome::INVALID; n];

    let mut j = n;
    for i in 0..n {
        if transposed[i] == 0b000 || transposed[i] == 0b001 {
            let val = transposed[i] as u16;
            output[i] = MaybeDtcOutcome::from_u16(val);
        } else {
            let val = u16::from_le_bytes([transposed[i], transposed[j]]);
            output[i] = MaybeDtcOutcome::from_u16(val);
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
    use shakmaty::Color;
    use shakmaty::fen::Fen;

    fn table_path(base: &Path, name: &str) -> std::path::PathBuf {
        let path = base.join(TableId::parse(name).unwrap().relative_path("ggegt"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        path
    }

    #[test]
    fn pawn_orders_route_to_distinct_tables_and_their_twins() {
        let dir = crate::TestDir::new("pawn_order_routing");
        for name in ["KPa_KPa", "KPao_KPao", "KPaPa_KPb", "KPa_KPbPb"] {
            let file = EgtFileReader::new(&dir.0, name).unwrap();
            let (twin, _) = file.id.twin().unwrap();
            let twin_file = EgtFileReader::new(&dir.0, twin.tablename()).unwrap();
            let stride = (file.index_range / 64).max(1);
            for turn in [Color::White, Color::Black] {
                for index in (0..file.index_range).step_by(stride) {
                    let Some(position) = file.index_to_position(index, turn) else {
                        continue;
                    };
                    assert_eq!(file.map_position_to_index(&position).unwrap(), index);
                    for mv in position.legal_moves() {
                        if mv.is_capture() || mv.is_promotion() {
                            continue;
                        }
                        let successor = position.clone().play(mv).unwrap();
                        let local = twin_file.map_position_to_index(&successor).unwrap();
                        let (_, reflected) = TableId::from_position(&successor).unwrap();
                        assert_eq!(
                            twin_file
                                .index_to_position(local, successor.turn())
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
        let mut file = EgtFileReader::new(&dir.0, "KPa_K").unwrap();
        file.save_to_file().unwrap();
        assert!(dir.0.join(file.id.relative_path("ggegt")).exists());
        assert_eq!(std::fs::read(legacy).unwrap(), b"legacy indexing");
        assert!(EgtFileReader::new_from_file(&dir.0, "KPa_K").is_ok());
        assert!(EgtFileReader::new(&dir.0, "KP_K").is_err());
    }

    #[test]
    fn cached_probe_does_not_warm_frames() {
        let dir = crate::TestDir::new("cached_probe_does_not_warm_frames");
        let mut file = EgtFileReader::new(&dir.0, "KPa_K").unwrap();
        let position: Chess = "8/8/8/8/8/8/P7/K6k w - - 0 1"
            .parse::<Fen>()
            .unwrap()
            .into_position(CastlingMode::Standard)
            .unwrap();
        assert_eq!(file.probe_cached(&position).unwrap(), None);
        assert!(
            file.frames
                .iter()
                .all(|frame| matches!(frame, FrameState::Empty))
        );
        let index = file.map_position_to_index(&position).unwrap();
        for outcome in [
            MaybeDtcOutcome::INVALID,
            MaybeDtcOutcome::new_unknown(3),
            MaybeDtcOutcome::DRAW,
        ] {
            file.write_to_index(index, outcome).unwrap();
            assert_eq!(file.probe_cached(&position).unwrap(), Some(outcome));
        }
        file.ensure_compressed(file.locate_frame(index).0).unwrap();
        assert_eq!(file.probe_cached(&position).unwrap(), None);
        assert_eq!(file.probe(&position).unwrap(), MaybeDtcOutcome::DRAW);
        file.save_to_file().unwrap();
        assert_eq!(file.probe_cached(&position).unwrap(), None);
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
        let outcome = MaybeDtcOutcome::new_win(ConversionType::Promotion, 7);
        // Distinct values verify that lazy loading honors input-path precedence.
        for (path, value) in [(&base.0, MaybeDtcOutcome::DRAW), (&input.0, outcome)] {
            let mut file = EgtFileReader::new(path, "KPa_K").unwrap();
            file.write_to_index(file.map_position_to_index(&position).unwrap(), value)
                .unwrap();
            file.save_to_file().unwrap();
        }
        for preloaded in [false, true] {
            let mut files = HashMap::new();
            if preloaded {
                files.insert(
                    "KPa_K".to_string(),
                    EgtFileReader::new_from_file(&input.0, "KPa_K").unwrap(),
                );
            }
            let reader = SharedDependencyReader::new(
                files,
                &Location::parse(&base.0).unwrap(),
                Some(&Location::parse(&input.0).unwrap()),
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
            drop(guard);
            drop(file);
            drop(registry_guard);
            let cache = reader.into_cache().unwrap();
            assert_eq!(cache.len(), 1);
            let file = &cache["KPa_K"];
            assert_eq!(file.probe_cached(&position).unwrap(), Some(outcome));
            assert_eq!(
                file.frames
                    .iter()
                    .filter(|frame| matches!(frame, FrameState::Uncompressed { .. }))
                    .count(),
                1
            );
        }
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
        let dependency_value = MaybeDtcOutcome::new_win(ConversionType::Promotion, 7);
        let output_value = MaybeDtcOutcome::new_win(ConversionType::Checkmate, 3);
        for (path, name, position, value) in [
            (&input.0, "KPa_K", &pawn, dependency_value),
            (&input.0, "KQ_K", &queen, MaybeDtcOutcome::DRAW),
            (&base.0, "KQ_K", &queen, output_value),
        ] {
            let mut file = EgtFileReader::new(path, name).unwrap();
            file.write_to_index(file.map_position_to_index(position).unwrap(), value)
                .unwrap();
            file.save_to_file().unwrap();
        }
        let reader = SharedDependencyReader::new(
            HashMap::new(),
            &Location::parse(&base.0).unwrap(),
            Some(&Location::parse(&input.0).unwrap()),
        );
        assert_eq!(reader.probe(&pawn).unwrap(), dependency_value);
        assert_eq!(reader.probe(&queen).unwrap(), MaybeDtcOutcome::DRAW);
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
        assert_eq!(reader.probe(&queen).unwrap(), MaybeDtcOutcome::DRAW);
        assert_eq!(reader.probe_outcome(&queen).unwrap(), DtcOutcome::Draw);

        reader.clear().unwrap();
        assert!(dependency.upgrade().is_none());
        assert!(reader.files.read().unwrap().is_empty());
        assert_eq!(reader.probe(&pawn).unwrap(), dependency_value);
        assert_eq!(reader.probe(&queen).unwrap(), MaybeDtcOutcome::DRAW);
    }

    #[test]
    fn shared_dependency_missing_returns_error_without_generating() {
        let dir = crate::TestDir::new("shared_dependency_missing");
        let position: Chess = "8/8/8/8/8/8/P7/K6k w - - 0 1"
            .parse::<Fen>()
            .unwrap()
            .into_position(CastlingMode::Standard)
            .unwrap();
        let reader =
            SharedDependencyReader::new(HashMap::new(), &Location::parse(&dir.0).unwrap(), None);
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
        assert!(reader.into_cache().unwrap().is_empty());
    }

    #[test]
    fn test_egt_file_creation() {
        let dir = crate::TestDir::new("single_table_creation");
        let file = EgtFileReader::new(&dir.0, "KPa_K").unwrap();
        assert_eq!(file.tablename, "KPa_K");
        assert_eq!(file.id.tablename(), file.egt.tablename());
        assert_eq!(file.index_range, file.egt.index_range());
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
            file.index_to_position(file.index_range, Color::White)
                .is_none()
        );
    }

    #[test]
    fn egt_file_probing_canonical() {
        let dir = crate::TestDir::new("single_table_probe");
        let path = dir.0.clone();
        let mut egt_file = EgtFileReader::new(&path, "KPa_K").unwrap();

        // A canonical position: White pawn on a2, White king on a1, Black king on h8
        // FEN: 8/8/8/8/8/8/P7/K6k w - - 0 1
        let fen: Fen = "8/8/8/8/8/8/P7/K6k w - - 0 1".parse().unwrap();
        let position = fen.into_position(CastlingMode::Standard).unwrap();

        // Probing should succeed (returns Invalid because the frame is initialized to 0/invalid)
        let outcome = egt_file.probe(&position).unwrap();
        assert_eq!(outcome, MaybeDtcOutcome::INVALID);

        // Write an outcome
        let expected_outcome = MaybeDtcOutcome::new_win(crate::ConversionType::Checkmate, 12);
        let idx = egt_file.map_position_to_index(&position).unwrap();
        egt_file.write_to_index(idx, expected_outcome).unwrap();

        // Probe again
        let outcome = egt_file.probe(&position).unwrap();
        assert_eq!(outcome, expected_outcome);
    }

    #[test]
    fn egt_file_probing_mirrored() {
        let dir = crate::TestDir::new("single_table_probe");
        let path = dir.0.clone();
        let mut egt_file = EgtFileReader::new(&path, "KPa_K").unwrap();

        // A mirrored (non-canonical) position: White pawn on h2, White king on h1, Black king on a1
        // This is the horizontal reflection of the canonical position above.
        // FEN: 8/8/8/8/8/8/7P/k6K w - - 0 1
        let fen_mirrored: Fen = "8/8/8/8/8/8/7P/k6K w - - 0 1".parse().unwrap();
        let position_mirrored = fen_mirrored.into_position(CastlingMode::Standard).unwrap();
        let fen_canonical: Fen = "8/8/8/8/8/8/P7/K6k w - - 0 1".parse().unwrap();
        let position_canonical = fen_canonical.into_position(CastlingMode::Standard).unwrap();

        // Write to the canonical position
        let expected_outcome = MaybeDtcOutcome::new_win(crate::ConversionType::Checkmate, 12);
        let idx_canonical = egt_file.map_position_to_index(&position_canonical).unwrap();
        egt_file
            .write_to_index(idx_canonical, expected_outcome)
            .unwrap();

        // Probing the mirrored position should return the same outcome because it gets canonicalized/mirrored
        let outcome = egt_file.probe(&position_mirrored).unwrap();
        assert_eq!(outcome, expected_outcome);
    }

    fn run_round_trip_test(tablename: &str, stride: usize) {
        let dir = crate::TestDir::new("single_table_roundtrip");
        let file = EgtFileReader::new(&dir.0, tablename).unwrap();
        for turn in [Color::White, Color::Black] {
            for index in (0..file.index_range).step_by(stride) {
                if let Some(position) = file.index_to_position(index, turn) {
                    assert_eq!(file.map_position_to_index(&position).unwrap(), index);
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
        let mut inputs = vec![MaybeDtcOutcome::INVALID; n];
        inputs[4] = MaybeDtcOutcome::new_win(ConversionType::Checkmate, 67);
        inputs[6] = MaybeDtcOutcome::new_loss(ConversionType::Capture, 12);
        inputs[7] = MaybeDtcOutcome::new_win(ConversionType::Promotion, 4567);
        inputs[8] = MaybeDtcOutcome::DRAW;
        inputs[10] = MaybeDtcOutcome::new_loss(ConversionType::Checkmate, 30);
        inputs[11] = MaybeDtcOutcome::new_win(ConversionType::Capture, 1);
        inputs[12] = MaybeDtcOutcome::DRAW;
        inputs[13] = MaybeDtcOutcome::new_loss(ConversionType::Promotion, 1200);
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
    fn egt_file_compression_decompression() {
        let test_dir = crate::TestDir::new("egt_file_compression_decompression");
        let base_path = test_dir.0.clone();
        let mut egt_file = EgtFileReader::new(&base_path, "KPa_K").unwrap();

        // Sample a position
        let fen: Fen = "8/8/8/8/8/8/P7/K6k w - - 0 1".parse().unwrap();
        let position = fen.into_position(CastlingMode::Standard).unwrap();
        let outcome = MaybeDtcOutcome::new_win(ConversionType::Checkmate, 987);
        let idx = egt_file.map_position_to_index(&position).unwrap();
        egt_file.write_to_index(idx, outcome).unwrap();
        assert_eq!(egt_file.probe(&position).unwrap(), outcome);

        // Save to file
        egt_file.save_to_file().unwrap();
        for f in 0..egt_file.frames.len() {
            match egt_file.frames[f] {
                FrameState::CompressedOnFile => {}
                _ => panic!("frame {} should be compressed on file", f),
            }
        }

        // Probing the saved file re-reads the frame from disk.
        assert_eq!(egt_file.probe(&position).unwrap(), outcome);

        // Probe again (triggers on-demand decompression from file)
        let mut another_egt_file = EgtFileReader::new_from_file(&base_path, "KPa_K").unwrap();
        let loaded_outcome = another_egt_file.probe(&position).unwrap();
        assert_eq!(loaded_outcome, outcome);
    }

    #[test]
    fn resaving_disk_backed_file() {
        let dir = crate::TestDir::new("resaving_disk_backed_file");
        let mut file = EgtFileReader::new(&dir.0, "KPa_K").unwrap();
        let mut values = vec![MaybeDtcOutcome::INVALID; file.index_range];
        for (idx, value) in values.iter_mut().enumerate() {
            *value = if idx % 3 == 0 {
                MaybeDtcOutcome::new_win(ConversionType::Promotion, 4567)
            } else {
                MaybeDtcOutcome::DRAW
            };
        }
        for (idx, &value) in values.iter().enumerate() {
            file.write_to_index(idx, value).unwrap();
        }
        file.save_to_file().unwrap();
        let mut reader = EgtFileReader::new_from_file(&dir.0, "KPa_K").unwrap();
        assert_eq!(
            reader.num_frames(),
            file.index_range.div_ceil(file.frame_size)
        );
        for idx in 0..reader.index_range {
            assert_eq!(reader.read_from_index(idx).unwrap(), values[idx]);
        }
        // Saving must not truncate disk-backed source data, including uncached frames.
        reader.frames.fill(FrameState::CompressedOnFile);
        reader.save_to_file().unwrap();
        let mut reloaded = EgtFileReader::new_from_file(&dir.0, "KPa_K").unwrap();
        for idx in 0..reloaded.index_range {
            assert_eq!(reloaded.read_from_index(idx).unwrap(), values[idx]);
        }
        assert_eq!(
            reloaded
                .frame_chunk(reloaded.index_range, reloaded.index_range)
                .unwrap(),
            &[]
        );
    }

    #[test]
    fn full_frames_short_tail_and_table_boundaries() {
        let dir = crate::TestDir::new("frame_boundaries");
        let mut file = EgtFileReader::new(&dir.0, "KQ_KR").unwrap();
        assert!(file.num_frames() > 1);
        assert_eq!(file.frame_layout.last().unwrap().len, 174420);
        let indexes = [
            0,
            DEFAULT_FRAME_SIZE - 1,
            DEFAULT_FRAME_SIZE,
            DEFAULT_FRAME_SIZE + 1,
            file.index_range - 1,
        ];
        for (n, &idx) in indexes.iter().enumerate() {
            file.write_to_index(
                idx,
                MaybeDtcOutcome::new_loss(ConversionType::Capture, n as u16 + 1),
            )
            .unwrap();
        }
        assert!(
            file.frame_chunk(DEFAULT_FRAME_SIZE - 1, DEFAULT_FRAME_SIZE + 1)
                .is_err()
        );
        file.save_to_file().unwrap();
        let mut reader = EgtFileReader::new_from_file(&dir.0, "KQ_KR").unwrap();
        for (n, &idx) in indexes.iter().enumerate() {
            assert_eq!(
                reader.read_from_index(idx).unwrap(),
                MaybeDtcOutcome::new_loss(ConversionType::Capture, n as u16 + 1)
            );
        }
        let pawnful = EgtFileReader::new(&dir.0, "KPa_K").unwrap();
        assert_eq!(pawnful.num_frames(), 1);
        assert_eq!(pawnful.frame_layout[0].start, 0);
        assert_eq!(pawnful.frame_layout[0].len, pawnful.index_range);
        assert!(file.read_from_index(file.index_range).is_err());
        assert!(
            file.write_to_index(file.index_range, MaybeDtcOutcome::DRAW)
                .is_err()
        );
        assert!(file.frame_chunk(1, 0).is_err());
        assert!(file.frame_chunk(0, file.index_range + 1).is_err());
    }

    #[test]
    fn rejects_invalid_frame_layout() {
        let dir = crate::TestDir::new("invalid_frame_layout");
        let file = EgtFileReader::new(&dir.0, "K_K").unwrap();
        for bytes in [
            0,
            3,
            922,
            926,
            DEFAULT_FRAME_SIZE * 2,
            DEFAULT_FRAME_SIZE * 2 + 2,
        ] {
            let output = std::fs::File::create(table_path(&dir.0, "K_K")).unwrap();
            let mut encoder = zeekstd::EncodeOptions::new().into_encoder(output).unwrap();
            encoder.compress(&vec![0; bytes]).unwrap();
            encoder.finish().unwrap();
            assert!(EgtFileReader::new_from_file(&dir.0, "K_K").is_err());
        }
        // A correct payload with an extra empty frame has the wrong frame count.
        let output = std::fs::File::create(table_path(&dir.0, "K_K")).unwrap();
        let mut encoder = zeekstd::EncodeOptions::new().into_encoder(output).unwrap();
        encoder.compress(&vec![0; file.index_range * 2]).unwrap();
        encoder.end_frame().unwrap();
        encoder.finish().unwrap();
        assert!(EgtFileReader::new_from_file(&dir.0, "K_K").is_err());
    }

    #[test]
    fn rejects_malformed_seek_table_suffixes() {
        let dir = crate::TestDir::new("malformed_seek_table_suffixes");
        let path = table_path(&dir.0, "K_K");
        let mut file = EgtFileReader::new(&dir.0, "K_K").unwrap();
        file.save_to_file().unwrap();
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
        let mut file = EgtFileReader::new(&dir.0, "K_K").unwrap();
        file.save_to_file().unwrap();
        let mut reader = EgtFileReader::new_from_file(&dir.0, "K_K").unwrap();
        let frame_end = reader
            .seek_table
            .as_ref()
            .unwrap()
            .frame_end_comp(0)
            .unwrap();
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

    #[test]
    fn egt_memory_compression_decompression() {
        let test_dir = crate::TestDir::new("egt_memory_compression_decompression");
        let path = test_dir.0.clone();
        let mut egt_file = EgtFileReader::new(&path, "KPa_K").unwrap();

        // Sample a position
        let fen: Fen = "8/8/8/8/8/8/P7/K6k w - - 0 1".parse().unwrap();
        let position = fen.into_position(CastlingMode::Standard).unwrap();
        let outcome = MaybeDtcOutcome::new_win(ConversionType::Checkmate, 987);
        let idx = egt_file.map_position_to_index(&position).unwrap();
        egt_file.write_to_index(idx, outcome).unwrap();
        assert_eq!(egt_file.probe(&position).unwrap(), outcome);

        for f in 0..egt_file.frames.len() {
            egt_file.ensure_compressed(f).unwrap();
            match egt_file.frames[f] {
                FrameState::Compressed(_) => {}
                _ => panic!(),
            }
        }

        // Probe again (triggers decompression from memory)
        let loaded_outcome = egt_file.probe(&position).unwrap();
        assert_eq!(loaded_outcome, outcome);
    }
}
