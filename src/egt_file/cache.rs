//! Shared table metadata and byte-bounded LRU frame caching.

use std::collections::{HashMap, hash_map::Entry};
use std::sync::{Arc, Mutex, RwLock};

use shakmaty::Chess;

use super::{Frame, TableFile, load_existing_file};
use crate::storage::Location;
use crate::table_id::TableId;
use crate::{DtcOutcome, EgtResult};

#[derive(Debug, Default)]
struct Lru {
    map: HashMap<(TableId, usize), (Arc<Frame>, u64)>,
    resident: usize,
    assigned: Option<usize>,
    clock: u64,
}

impl Lru {
    fn get(&mut self, key: (TableId, usize)) -> Option<Arc<Frame>> {
        let (frame, last_used) = self.map.get_mut(&key)?;
        self.clock = self.clock.wrapping_add(1);
        *last_used = self.clock;
        Some(Arc::clone(frame))
    }

    fn insert(&mut self, key: (TableId, usize), frame: Frame) -> Arc<Frame> {
        self.clock = self.clock.wrapping_add(1);
        match self.map.entry(key) {
            Entry::Occupied(mut entry) => {
                let (frame, last_used) = entry.get_mut();
                *last_used = self.clock;
                Arc::clone(frame)
            }
            Entry::Vacant(entry) => {
                self.resident += frame.bytes();
                let frame = Arc::new(frame);
                entry.insert((Arc::clone(&frame), self.clock));
                frame
            }
        }
    }

    fn enforce_limit(&mut self) {
        if self.assigned == Some(0) {
            self.map.clear();
            self.resident = 0;
            return;
        }
        while self.assigned.is_some_and(|limit| self.resident > limit) {
            let (&victim, _) = self.map.iter().min_by_key(|(_, (_, age))| age).unwrap();
            let (frame, _) = self.map.remove(&victim).unwrap();
            self.resident -= frame.bytes();
        }
    }
}

/// Shared files and their uncompressed frames for on-demand probing.
///
/// The assigned memory bounds retained frames. In-flight probes may hold extra
/// frames and decompression buffers; metadata is also excluded.
#[derive(Debug)]
pub(crate) struct SharedFileCache {
    files: RwLock<HashMap<TableId, Arc<TableFile>>>,
    base_path: Location,
    input_path: Option<Location>,
    frames: Mutex<Lru>,
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
            frames: Mutex::new(Lru {
                assigned: cache_size,
                ..Lru::default()
            }),
        }
    }

    pub(crate) fn set_cache_size(&self, assigned: usize) {
        let mut lru = self.frames.lock().unwrap();
        lru.assigned = Some(assigned);
        lru.enforce_limit();
    }

    pub(crate) fn set_input_path(&mut self, input: Option<Location>) {
        self.clear();
        self.input_path = input;
    }

    fn file(&self, id: &TableId) -> EgtResult<Arc<TableFile>> {
        if let Some(file) = self.files.read().unwrap().get(id) {
            return Ok(Arc::clone(file));
        }
        let file = Arc::new(load_existing_file(&self.base_path, self.input_path.as_ref(), id)?);
        Ok(Arc::clone(self.files.write().unwrap().entry(*id).or_insert(file)))
    }

    fn read_frames(
        &self,
        id: TableId,
        file: &TableFile,
        frames: &[usize],
    ) -> EgtResult<HashMap<usize, Arc<Frame>>> {
        let mut ready = HashMap::with_capacity(frames.len());
        let mut missing = Vec::new();
        {
            let mut lru = self.frames.lock().unwrap();
            for &frame in frames {
                if let Some(data) = lru.get((id, frame)) {
                    ready.insert(frame, data);
                } else {
                    missing.push(frame);
                }
            }
        }
        if !missing.is_empty() {
            let decoded = file.read_frames(&missing)?;
            let mut lru = self.frames.lock().unwrap();
            for (frame, data) in missing.into_iter().zip(decoded) {
                ready.insert(frame, lru.insert((id, frame), data));
            }
            lru.enforce_limit();
        }
        Ok(ready)
    }

    pub(crate) fn probe(&self, position: &Chess) -> EgtResult<DtcOutcome> {
        Ok(self.probe_outcomes(std::slice::from_ref(position))?[0])
    }

    /// Groups probes by canonical table, preserving the input order of outcomes.
    pub(crate) fn probe_outcomes(&self, positions: &[Chess]) -> EgtResult<Vec<DtcOutcome>> {
        let mut groups = HashMap::<TableId, Vec<(usize, bool)>>::new();
        for (slot, position) in positions.iter().enumerate() {
            let (id, reflected) = TableId::from_position(position)?;
            groups.entry(id).or_default().push((slot, reflected));
        }
        let mut outcomes = vec![DtcOutcome::Draw; positions.len()];
        let mut groups: Vec<_> = groups.into_iter().collect();
        groups.sort_by_cached_key(|(id, _)| id.name());
        for (id, requests) in groups {
            let file = self.file(&id)?;
            let indices = requests
                .iter()
                .map(|&(slot, reflected)| file.index(&positions[slot], reflected))
                .collect::<EgtResult<Vec<_>>>()?;
            let mut frames: Vec<_> = indices.iter().map(|&index| file.locate(index).0).collect();
            frames.sort_unstable();
            frames.dedup();
            let ready = self.read_frames(id, &file, &frames)?;
            for ((slot, _), index) in requests.into_iter().zip(indices) {
                let (frame, offset) = file.locate(index);
                outcomes[slot] = ready[&frame].get(offset).to_outcome_at(index)?;
            }
        }
        Ok(outcomes)
    }

    pub(crate) fn clear(&mut self) {
        self.files.get_mut().unwrap().clear();
        let lru = self.frames.get_mut().unwrap();
        *lru = Lru { assigned: lru.assigned, ..Lru::default() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ConversionType, EgtError, PackedOutcome};
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
                        assert_eq!(reader.probe(position).unwrap(), DtcOutcome::Win(ConversionType::Promotion, 7));
                    }
                });
            }
        });
        let id = TableId::parse("KPa_K").unwrap();
        std::fs::remove_file(input.0.join(id.relative_path("ggegt"))).unwrap();
        assert_eq!(reader.probe(&position).unwrap(), DtcOutcome::Win(ConversionType::Promotion, 7));
        assert_eq!(reader.frames.lock().unwrap().map.len(), 1);
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
            let bytes = file.read_frames(&[0]).unwrap()[0].bytes();
            assert_eq!(bytes, 4);
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
            let id = TableId::parse(name).unwrap();
            cache.frames.lock().unwrap().map.contains_key(&(id, 0))
        };

        cache.probe(&positions[0].1).unwrap();
        cache.probe(&positions[1].1).unwrap();
        cache.probe(&positions[0].1).unwrap(); // Warm hit makes KR_K oldest.
        cache.probe(&positions[2].1).unwrap();
        assert!(resident("KQ_K"));
        assert!(!resident("KR_K"));
        assert!(resident("KB_K"));
        assert_eq!(cache.frames.lock().unwrap().resident, 2 * frame_bytes);

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
                            assert_eq!(cache.probe(position).unwrap(), DtcOutcome::Draw);
                        }
                    }
                });
            }
        });
        assert!(cache.frames.lock().unwrap().resident <= 2 * frame_bytes);

        cache.set_cache_size(0);
        assert_eq!(cache.frames.lock().unwrap().resident, 0);
        assert!(positions.iter().all(|(name, _)| !resident(name)));
        assert_eq!(cache.probe(&positions[0].1).unwrap(), DtcOutcome::Draw);
        assert!(!resident("KQ_K"));
        assert_eq!(cache.frames.lock().unwrap().resident, 0);
    }

    #[test]
    fn shared_cache_clear_and_input_change_preserve_lookup_precedence() {
        let base = crate::TestDir::new("shared_dependency_lifecycle_base");
        let input = crate::TestDir::new("shared_dependency_lifecycle_input");
        let queen: Chess = "8/8/8/8/8/8/Q7/K6k w - - 0 1"
            .parse::<Fen>()
            .unwrap()
            .into_position(CastlingMode::Standard)
            .unwrap();
        let output = DtcOutcome::Win(ConversionType::Checkmate, 3);
        write_fixture(&base.0, "KQ_K", |_| PackedOutcome::new_win(ConversionType::Checkmate, 3));
        write_fixture(&input.0, "KQ_K", |_| PackedOutcome::DRAW);
        let base = Location::parse(&base.0).unwrap();
        let input = Location::parse(&input.0).unwrap();
        let mut reader = SharedFileCache::new(&base, Some(&input), None);
        assert_eq!(reader.probe(&queen).unwrap(), DtcOutcome::Draw);

        reader.clear();
        assert_eq!(reader.frames.lock().unwrap().resident, 0);
        assert!(reader.files.read().unwrap().is_empty());
        assert_eq!(reader.probe(&queen).unwrap(), DtcOutcome::Draw);

        reader.set_input_path(None);
        assert_eq!(reader.probe(&queen).unwrap(), output);
        reader.set_input_path(Some(input));
        assert_eq!(reader.probe(&queen).unwrap(), DtcOutcome::Draw);
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
        assert!(matches!(reader.probe(&position), Err(EgtError::FileNotFound(_))));
        assert!(
            !dir.0
                .join(TableId::parse("KPa_K").unwrap().relative_path("ggegt"))
                .exists()
        );
        assert!(reader.files.read().unwrap().is_empty());
    }
}
