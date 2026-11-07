//! Shared table readers and byte-bounded LRU frame caching.
//!
//! Registry replacement, removal, and clearing happen between joined worker
//! phases. Accounting owns reader references until those entries are purged.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, RwLock};

use shakmaty::Chess;

use super::EgtFileReader;
use crate::storage::Location;
use crate::table_id::position_table_name;
use crate::{DtcOutcome, EgtError, EgtResult, PackedOutcome};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FrameKey {
    file: usize,
    frame: usize,
}

#[derive(Debug)]
struct CachedFrame {
    file: Arc<RwLock<EgtFileReader>>,
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
                    file: Arc::clone(file),
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
            let Some((key, last_used, file)) = victim else {
                return Err(EgtError::Internal(
                    "cache accounting has no evictable frame",
                ));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ConversionType, TableId};
    use crate::egt_file::writer::write_fixture;
    use shakmaty::{CastlingMode, fen::Fen};

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
        assert_eq!(reader.cache.lock().unwrap().resident, 0);
        assert!(dependency.upgrade().is_none());
        assert!(reader.files.read().unwrap().is_empty());
        assert_eq!(reader.probe(&pawn).unwrap(), dependency_value);
        assert_eq!(reader.probe(&queen).unwrap(), PackedOutcome::DRAW);
        let file = Arc::downgrade(&reader.files.read().unwrap()["KPa_K"]);
        drop(reader);
        assert!(file.upgrade().is_none());
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
}
