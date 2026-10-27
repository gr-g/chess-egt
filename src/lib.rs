//! Chess endgame generation and probing through a synchronous API.
//!
//! Generator/prober constructors and `with_input_path` resolve storage locations
//! and are fallible: callers must handle their `EgtResult` (for example with `?`).
//! Locations may be local directories or supported object-store URLs. Each
//! canonical table is published independently after its twin pair is solved.
//!
//! Storage operations block the calling thread, including remote reads and writes.
//! Async applications should call this API from `tokio::task::spawn_blocking`
//! rather than directly from an async task, including construction/configuration
//! and calls to generate, probe, or verify tables.

mod egt;
mod egt_file;
mod egt_writer;
pub mod error;
mod finalization;
mod finalization_stats;
pub mod piece_set;
mod retrograde;
mod selection;
mod stats;
mod storage;
mod table_id;

use crate::egt_file::SharedDependencyReader;
use crate::storage::Location;
pub use egt::Egt;
pub use egt_file::EgtFileReader;
pub use error::{EgtError, EgtResult};
use rayon::prelude::*;
pub use selection::{GenerationPlan, TableSelector, resolve_selectors};
use shakmaty::{Chess, Color, Move, MoveList, Position, Role};
pub use stats::{AggregateStats, EgtStats, LongestDtcPosition};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
pub use table_id::TableId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConversionType {
    Promotion,
    Capture,
    Checkmate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DtcOutcome {
    Win(ConversionType, u16),
    Draw,
    Loss(ConversionType, u16),
}

impl Ord for DtcOutcome {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (DtcOutcome::Win(ct1, n1), DtcOutcome::Win(ct2, n2)) => n2.cmp(n1).then(ct1.cmp(ct2)),
            (DtcOutcome::Draw, DtcOutcome::Draw) => Ordering::Equal,
            (DtcOutcome::Loss(ct1, n1), DtcOutcome::Loss(ct2, n2)) => n1.cmp(n2).then(ct2.cmp(ct1)),
            (DtcOutcome::Win(_, _), DtcOutcome::Draw) => Ordering::Greater,
            (DtcOutcome::Draw, DtcOutcome::Win(_, _)) => Ordering::Less,
            (DtcOutcome::Win(_, _), DtcOutcome::Loss(_, _)) => Ordering::Greater,
            (DtcOutcome::Loss(_, _), DtcOutcome::Win(_, _)) => Ordering::Less,
            (DtcOutcome::Draw, DtcOutcome::Loss(_, _)) => Ordering::Greater,
            (DtcOutcome::Loss(_, _), DtcOutcome::Draw) => Ordering::Less,
        }
    }
}

impl PartialOrd for DtcOutcome {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

pub struct EgtGenerator {
    base_path: Location,
    assigned_memory: Option<usize>,
    input_path: Option<Location>,
    verify: bool,
    threads: usize,
}

impl EgtGenerator {
    /// Resolves a local output directory or object-store URL once.
    ///
    /// Returns an error for invalid locations or backend initialization failures.
    /// Read-only outputs are rejected when generation starts. This is a
    /// synchronous API; async applications should use `spawn_blocking`.
    pub fn new(path: impl AsRef<Path>) -> EgtResult<Self> {
        Ok(Self::from_location(Location::parse(path)?))
    }

    pub(crate) fn from_location(location: Location) -> Self {
        Self {
            base_path: location,
            assigned_memory: None,
            input_path: None,
            verify: true,
            threads: 0,
        }
    }

    /// Sets generation and verification workers. Zero (the default) uses Rayon's default.
    pub fn with_threads(&mut self, threads: usize) {
        self.threads = threads;
    }

    pub fn with_assigned_memory(&mut self, n: usize) {
        self.assigned_memory = Some(n);
    }

    /// Resolves an additional local directory or object-store URL for dependencies.
    /// Input is searched first; output is tried only if the input file is missing.
    /// If not set, only the output location is consulted.
    ///
    /// Returns an error if the location cannot be resolved, leaving the previous
    /// setting unchanged. This synchronous API should run in `spawn_blocking`
    /// when called from an async application.
    pub fn with_input_path(&mut self, path: impl AsRef<Path>) -> EgtResult<()> {
        self.input_path = Some(Location::parse(path)?);
        Ok(())
    }

    /// Enables or disables the internal consistency check performed after
    /// generation. Enabled by default.
    pub fn with_verify(&mut self, verify: bool) {
        self.verify = verify;
    }

    /// Generates a logical selector and its quiet-move twins.
    /// Capture/promotion dependencies must already exist or be explicitly selected.
    pub fn generate(&self, selector: &str) -> EgtResult<Vec<EgtStats>> {
        self.generate_selected(&[selector])
    }

    /// Deduplicates selectors into twin-pair jobs, ordered by pieces then pawns.
    /// Reuses one Rayon pool and shares dependency frames with verification.
    /// The cache is released between material families (there is no LRU yet).
    /// Completed pairs are published and optionally verified before the next job.
    pub fn generate_selected(&self, selectors: &[&str]) -> EgtResult<Vec<EgtStats>> {
        let plan = GenerationPlan::new(resolve_selectors(selectors)?)?;
        self.base_path.ensure_format()?;
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(self.threads)
            .build()
            .map_err(|_| EgtError::Internal("failed to build generation thread pool"))?;
        let reader =
            SharedDependencyReader::new(HashMap::new(), &self.base_path, self.input_path.as_ref());
        let mut results = Vec::new();
        let mut previous_material = None;
        println!(
            "Generating {} twin-pair jobs using {} threads.",
            plan.pairs.len(),
            pool.current_num_threads()
        );
        pool.install(|| -> EgtResult<()> {
            for (table, _) in plan.pairs {
                let material = material_family(&table)?;
                if previous_material.as_ref() != Some(&material) {
                    reader.clear()?;
                    previous_material = Some(material);
                }
                let start = std::time::Instant::now();
                let files = retrograde::generate_pair(&self.base_path, &table, &reader)?;
                let mut names = Vec::new();
                let mut verification = Vec::new();
                let mut pair_stats = Vec::new();
                for mut file in files {
                    let stats = file
                        .stats
                        .take()
                        .ok_or(EgtError::Internal("stats missing after generation"))?;
                    names.push(file.id.tablename().to_owned());
                    pair_stats.push(stats);
                    // Generation and verification share dependency frames. Pin
                    // both outputs so input artifacts cannot shadow either one.
                    if self.verify {
                        verification.push((file.egt.clone(), file.frame_size));
                        reader.insert(file)?;
                    }
                }
                if self.verify {
                    for (name, (egt, frame_size)) in names.iter().zip(verification) {
                        EgtProber::verify_egt(&reader, name, &egt, frame_size)?;
                    }
                    reader.remove(&names)?;
                }
                print_pair_stats(&pair_stats[0], pair_stats.get(1), start.elapsed());
                results.extend(pair_stats);
            }
            Ok(())
        })?;
        Ok(results)
    }
}

pub struct EgtProber {
    base_path: Location,
    /// Additional path used to look up dependency files. If `None`, only
    /// `base_path` is consulted.
    input_path: Option<Location>,
    assigned_memory: Option<usize>,
    cache: HashMap<String, EgtFileReader>,
    threads: usize,
}

impl EgtProber {
    /// Resolves a local tablebase directory or object-store URL once.
    ///
    /// Returns an error for invalid locations or backend initialization failures.
    /// This synchronous API should run in `spawn_blocking` when called from an
    /// async application, as should subsequent probing and verification.
    pub fn new(path: impl AsRef<Path>) -> EgtResult<Self> {
        Ok(Self::from_location(Location::parse(path)?))
    }

    pub(crate) fn from_location(location: Location) -> Self {
        Self {
            base_path: location,
            input_path: None,
            assigned_memory: None,
            cache: HashMap::new(),
            threads: 0,
        }
    }

    /// Sets verification workers. Zero (the default) uses Rayon's default.
    /// Does not change single-position probing.
    pub fn with_threads(&mut self, threads: usize) {
        self.threads = threads;
    }

    pub fn with_assigned_memory(&mut self, n: usize) {
        self.assigned_memory = Some(n);
    }

    /// Resolves an additional local directory or object-store URL for table lookup.
    /// Input is searched first; the base location is tried only for missing files.
    /// If not set, only the base location is consulted.
    ///
    /// Returns an error if the location cannot be resolved, leaving the previous
    /// setting unchanged. This synchronous API should run in `spawn_blocking`
    /// when called from an async application.
    pub fn with_input_path(&mut self, path: impl AsRef<Path>) -> EgtResult<()> {
        let input = Location::parse(path)?;
        self.cache.clear();
        self.input_path = Some(input);
        Ok(())
    }

    /// Attempts to load an existing file, looking it up first in `input_path`
    /// (if set) and then in `base_path`.
    fn load_existing(&self, endgame: &str) -> EgtResult<EgtFileReader> {
        load_existing_file(&self.base_path, self.input_path.as_ref(), endgame)
    }

    pub fn probe(&mut self, position: &Chess) -> EgtResult<DtcOutcome> {
        let (name, reflected) = table_id::position_table_name(position)?;
        let endgame = name.as_str();
        if !self.cache.contains_key(endgame) {
            let file = self.load_existing(endgame)?;
            self.cache.insert(endgame.to_owned(), file);
        }
        let file = self
            .cache
            .get_mut(endgame)
            .ok_or(EgtError::Internal("cache entry missing after insert"))?;
        let idx = file.map_routed_position_to_index(position, reflected)?;
        let outcome = file.read_from_index(idx)?;
        outcome.to_outcome_at(idx)
    }

    /// Verifies exactly the tables selected, without implicitly adding twins.
    /// Successor dependencies must be available. Frames are shared without eviction.
    pub fn verify_internal_consistency(&mut self, selector: &str) -> EgtResult<()> {
        self.verify_selected(&[selector])
    }

    pub fn verify_selected(&mut self, selectors: &[&str]) -> EgtResult<()> {
        let names: Vec<_> = resolve_selectors(selectors)?
            .into_iter()
            .map(|id| id.tablename().to_owned())
            .collect();
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(self.threads)
            .build()
            .map_err(|_| EgtError::Internal("failed to build verification thread pool"))?;
        pool.install(|| self.verify_tables_in_pool(&names))
    }

    fn verify_tables_in_pool(&mut self, names: &[String]) -> EgtResult<()> {
        // Load only the current selected table at a time. The shared reader
        // lazily opens successors; taking/recovering the cache keeps frames shared.
        for name in names {
            if !self.cache.contains_key(name) {
                let file = self.load_existing(name)?;
                self.cache.insert(name.clone(), file);
            }
            let file = self
                .cache
                .get(name)
                .ok_or(EgtError::Internal("cache entry missing after insert"))?;
            let egt = file.egt.clone();
            let frame_size = file.frame_size;
            let reader = SharedDependencyReader::new(
                std::mem::take(&mut self.cache),
                &self.base_path,
                self.input_path.as_ref(),
            );
            let result = Self::verify_egt(&reader, name, &egt, frame_size);
            self.cache = reader.into_cache()?;
            result?;
            println!(
                "Successfully verified internal consistency of table {}.",
                name
            );
        }
        Ok(())
    }

    /// Reads only versioned JSON sidecars. Missing data is an error.
    /// Overlapping selectors are deduplicated before retrieval and aggregation.
    pub fn stats(
        &self,
        selectors: &[&str],
    ) -> EgtResult<AggregateStats> {
        let tables = resolve_selectors(selectors)?;
        let mut total = AggregateStats {
            selected_tables: tables.len(),
            ..Default::default()
        };
        // Small bounded batches overlap remote requests without retaining all
        // sidecars or creating a worker for every selected table.
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(8)
            .build()
            .map_err(|_| EgtError::Internal("failed to build statistics thread pool"))?;
        for batch in tables.chunks(32) {
            let results: Vec<_> = pool.install(|| {
                batch
                    .par_iter()
                    .map(|id| {
                        if let Some(input) = &self.input_path {
                            match stats::read_stats(input, id) {
                                Ok(stats) => return Ok(stats),
                                Err(EgtError::FileNotFound(_)) => {}
                                Err(error) => return Err(error),
                            }
                        }
                        stats::read_stats(&self.base_path, id)
                    })
                    .collect()
            });
            for result in results {
                match result {
                    Ok(stats) => total.add(&stats),
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(total)
    }

    /// Verifies independent blocks of one table using table-local indices.
    fn verify_egt(
        reader: &SharedDependencyReader,
        endgame: &str,
        egt: &egt::Egt,
        frame_size: usize,
    ) -> EgtResult<()> {
        let tablename = egt.tablename();
        let local_range = egt.index_range();

        println!("{}: verifying {} indexes...", tablename, local_range);

        let completed = AtomicUsize::new(0);
        // Blocks divide frames so copying a block never crosses a frame boundary.
        let block_size = 4096.min(frame_size);
        let blocks: Vec<_> = (0..local_range)
            .step_by(frame_size)
            .flat_map(|frame_start| {
                let frame_end = (frame_start + frame_size).min(local_range);
                (frame_start..frame_end)
                    .step_by(block_size)
                    .map(move |start| (start, (start + block_size).min(frame_end)))
            })
            .collect();
        blocks.into_par_iter().try_for_each(|(start, end)| {
            Self::verify_block(reader, endgame, egt, start, end)?;
            let previous = completed.fetch_add(end - start, AtomicOrdering::Relaxed);
            let done = previous + end - start;
            if done / 10_000_000 != previous / 10_000_000 {
                println!(
                    "{}: verified {}/{} indexes...",
                    tablename, done, local_range
                );
            }
            Ok(())
        })
    }

    fn verify_block(
        reader: &SharedDependencyReader,
        endgame: &str,
        egt: &egt::Egt,
        start: usize,
        end: usize,
    ) -> EgtResult<()> {
        let tablename = egt.tablename();
        let outcomes = reader.read_chunk(endgame, start, end)?;
        // Keep the table identity and local index in verification diagnostics.
        let fail = |local_idx: usize, reason: &str| EgtError::ConsistencyCheckFailed {
            endgame: endgame.to_string(),
            index: local_idx,
            reason: format!("table {}, local index {}: {}", tablename, local_idx, reason),
        };

        for (relative_idx, outcome_maybe) in outcomes.into_iter().enumerate() {
            let local_idx = start + relative_idx;
            let index = local_idx;
            let position_opt = egt.position_from_index(local_idx, Color::White);

            if position_opt.is_none() {
                if !outcome_maybe.is_invalid() {
                    return Err(fail(
                        local_idx,
                        "index is invalid but outcome is not invalid",
                    ));
                }
                continue;
            }

            if outcome_maybe.is_invalid() {
                return Err(fail(local_idx, "index is valid but outcome is invalid"));
            }

            let position = position_opt.ok_or(EgtError::Internal(
                "position_opt should be Some after the None check",
            ))?;
            let outcome = outcome_maybe.to_outcome_at(index)?;
            let legals = position.legal_moves();

            if legals.is_empty() {
                if position.is_check() {
                    if outcome != DtcOutcome::Loss(ConversionType::Checkmate, 0) {
                        return Err(fail(
                            local_idx,
                            &format!("checkmate position has outcome {:?}", outcome),
                        ));
                    }
                } else if outcome != DtcOutcome::Draw {
                    return Err(fail(
                        local_idx,
                        &format!("stalemate position has outcome {:?}", outcome),
                    ));
                }
            } else {
                let best = Self::best_outcome(reader, &position, &legals)?;
                if outcome != best {
                    let details =
                        Self::describe_mismatch(reader, &position, &legals, outcome, best);
                    return Err(fail(local_idx, &details));
                }
            }
        }

        Ok(())
    }

    /// Computes the best outcome reachable from `position` by playing one of
    /// `legals`, probing each successor in the tablebases.
    fn best_outcome(
        reader: &SharedDependencyReader,
        position: &Chess,
        legals: &MoveList,
    ) -> EgtResult<DtcOutcome> {
        let mut best_value: Option<DtcOutcome> = None;

        for m in legals {
            let mut successor_position = position.clone();
            successor_position.play_unchecked(*m);

            let successor_outcome = reader.probe_outcome(&successor_position)?;
            let v_m = move_value(m, successor_outcome);

            if let Some(ref mut best) = best_value {
                if v_m > *best {
                    *best = v_m;
                }
            } else {
                best_value = Some(v_m);
            }
        }

        best_value.ok_or(EgtError::Internal(
            "best_value should be Some when legals is non-empty",
        ))
    }

    /// Builds a human-readable diagnostic for a mismatch between the outcome
    /// stored in the file and the best outcome computed from the legal moves.
    fn describe_mismatch(
        reader: &SharedDependencyReader,
        position: &Chess,
        legals: &MoveList,
        outcome: DtcOutcome,
        best: DtcOutcome,
    ) -> String {
        let mut reason = format!(
            "consistency check failed.\nPosition: {:?}\nOutcome in file: {:?}\nBest outcome from legal moves: {:?}",
            position, outcome, best
        );
        reason.push_str("\nLegal moves and their outcomes:");
        for m in legals {
            let mut successor_position = position.clone();
            successor_position.play_unchecked(*m);
            // Best-effort diagnostic: skip moves whose successor cannot be probed.
            if let Ok(successor_outcome) = reader.probe_outcome(&successor_position) {
                let v_m = move_value(m, successor_outcome);
                reason.push_str(&format!(
                    "\n  Move: {:?}, Successor Outcome: {:?}, Value: {:?}",
                    m, successor_outcome, v_m
                ));
            }
        }
        reason
    }
}

/// An unordered material family, independent of the oriented representative.
fn material_family(table: &TableId) -> EgtResult<String> {
    let material = table.material();
    let twin = table.twin()?.0.material();
    Ok(material.min(twin))
}

/// The value of a position for the side to move after playing `m`, given the
/// outcome of the successor position. Conversions (captures and promotions)
/// reset the distance to conversion to 1.
fn move_value(m: &Move, successor_outcome: DtcOutcome) -> DtcOutcome {
    if m.is_capture() || m.is_promotion() {
        let ct = if m.is_capture() {
            ConversionType::Capture
        } else {
            ConversionType::Promotion
        };
        match successor_outcome {
            DtcOutcome::Loss(_, _) => DtcOutcome::Win(ct, 1),
            DtcOutcome::Draw => DtcOutcome::Draw,
            DtcOutcome::Win(_, _) => DtcOutcome::Loss(ct, 1),
        }
    } else {
        match successor_outcome {
            DtcOutcome::Loss(ct, n) => DtcOutcome::Win(ct, n + 1),
            DtcOutcome::Draw => DtcOutcome::Draw,
            DtcOutcome::Win(ct, n) => DtcOutcome::Loss(ct, n + 1),
        }
    }
}

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

pub fn get_endgame(position: &Chess) -> String {
    let stm_color = position.turn();
    let sntm_color = !stm_color;

    let mut stm = String::new();
    let mut sntm = String::new();

    // We always have exactly 1 King for each side
    stm.push('K');
    sntm.push('K');

    // Count other pieces in order: Q, R, B, N, P
    let board = position.board();
    for &role in &[
        Role::Queen,
        Role::Rook,
        Role::Bishop,
        Role::Knight,
        Role::Pawn,
    ] {
        let stm_count = (board.by_role(role) & board.by_color(stm_color))
            .into_iter()
            .count();
        for _ in 0..stm_count {
            stm.push(role.upper_char());
        }

        let sntm_count = (board.by_role(role) & board.by_color(sntm_color))
            .into_iter()
            .count();
        for _ in 0..sntm_count {
            sntm.push(role.upper_char());
        }
    }

    format!("{}_{}", stm, sntm)
}

/// Prints table-specific statistics (wins, draws, losses, compression).
pub fn print_table_stats(stats: &EgtStats) {
    let unique_positions = stats.unique_positions;
    let compressed_size_mb = stats.bytes as f64 / (1024.0 * 1024.0);
    let bits_per_pos = if unique_positions > 0 {
        (stats.bytes as f64 * 8.0) / unique_positions as f64
    } else {
        0.0
    };

    println!(
        "Generated endgame {} with {} unique positions: {} wins, {} draws, {} losses. Compressed size: {:.2}MiB ({:.2} bits/pos).",
        stats.tablename,
        unique_positions,
        stats.win,
        stats.draw,
        stats.loss,
        compressed_size_mb,
        bits_per_pos
    );
}

/// Prints detailed statistics about a pair of generated files (or a single file if symmetric).
pub fn print_pair_stats(
    stats_a: &EgtStats,
    stats_b: Option<&EgtStats>,
    duration: std::time::Duration,
) {
    let mut unique_positions = stats_a.unique_positions;
    print_table_stats(stats_a);

    if let Some(sb) = stats_b {
        unique_positions += sb.unique_positions;
        print_table_stats(sb);
    }

    let us_per_pos = if unique_positions > 0 {
        duration.as_micros() as f64 / unique_positions as f64
    } else {
        0.0
    };

    let total_secs = duration.as_secs();
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    let format_dur = format!("{:02}h:{:02}m:{:02}s", hours, minutes, seconds);

    println!("Time used {} ({:.2} μs/pos).", format_dur, us_per_pos,);
}

/// A fresh, test-specific directory under the system temp dir. Tests running in
/// parallel must not share output directories: they would overwrite (or read
/// half-written) tables generated by another test. The directory is removed on
/// drop, unless the test panicked (to keep the files for debugging).
#[cfg(test)]
pub(crate) struct TestDir(pub PathBuf);

#[cfg(test)]
impl TestDir {
    pub(crate) fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join("chess-egt-tests").join(format!(
            "{}-{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

#[cfg(test)]
impl Drop for TestDir {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn unverified_generator(path: impl AsRef<Path>) -> EgtGenerator {
        let mut generator = EgtGenerator::new(path).unwrap();
        generator.with_threads(2);
        generator.with_verify(false);
        generator
    }

    fn table_path(root: &Path, name: &str, extension: &str) -> PathBuf {
        root.join(TableId::parse(name).unwrap().relative_path(extension))
    }

    fn write_artifact(root: &Path, name: &str, extension: &str, bytes: &[u8]) {
        let path = table_path(root, name, extension);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    fn aggregate(tables: &[EgtStats]) -> AggregateStats {
        let mut total = AggregateStats {
            selected_tables: tables.len(),
            ..Default::default()
        };
        for stats in tables {
            total.add(stats);
        }
        total
    }

    fn verify_internal_consistency(selector: &str) {
        let dir = TestDir::new(&format!("verify_internal_consistency_{selector}"));
        let generator = unverified_generator(&dir.0);
        generator.generate_selected(&["K_K", selector]).unwrap();
        let mut prober = EgtProber::new(&dir.0).unwrap();
        prober.with_threads(2);
        let mut tables = resolve_selectors(&[selector]).unwrap();
        tables.extend(tables.clone().into_iter().map(|id| id.twin().unwrap().0));
        let names: Vec<_> = tables.iter().map(TableId::tablename).collect();
        prober.verify_selected(&names).unwrap();
    }

    #[test]
    fn test_verify_internal_consistency_k_k() {
        verify_internal_consistency("K_K");
    }

    #[test]
    fn test_verify_internal_consistency_kr_k() {
        verify_internal_consistency("KR_K");
    }

    #[test]
    fn test_verify_internal_consistency_kp_k() {
        let dir = TestDir::new("verify_internal_consistency_KP_K");
        let generator = unverified_generator(&dir.0);
        let tables = generator
            .generate_selected(&["K_K", "KQ_K", "KR_K", "KB_K", "KN_K", "KP_K"])
            .unwrap();
        let pawn_tables: BTreeSet<_> = tables
            .iter()
            .filter(|stats| stats.tablename.contains('P'))
            .map(|stats| stats.tablename.as_str())
            .collect();
        let expected = resolve_selectors(&["KP_K", "K_KP"]).unwrap();
        assert_eq!(
            pawn_tables,
            expected.iter().map(TableId::tablename).collect()
        );
        assert_eq!(pawn_tables.len(), 8);
        for id in &expected {
            let file = EgtFileReader::new_from_file(&dir.0, id.tablename()).unwrap();
            assert_eq!(&file.id, id);
            assert_eq!(file.egt.tablename(), id.tablename());
        }
        let mut prober = EgtProber::new(&dir.0).unwrap();
        prober.with_threads(2);
        prober.verify_selected(&["KP_K", "K_KP"]).unwrap();

        // Exact verification must not expand an identity to its material family.
        drop(prober);
        for id in expected
            .iter()
            .filter(|id| !["KPa_K", "K_KPa"].contains(&id.tablename()))
        {
            std::fs::remove_file(dir.0.join(id.relative_path("ggegt"))).unwrap();
        }
        let mut prober = EgtProber::new(&dir.0).unwrap();
        prober.with_threads(2);
        prober.verify_internal_consistency("KPa_K").unwrap();
        assert!(matches!(
            prober.verify_internal_consistency("KP_K"),
            Err(EgtError::FileNotFound(_))
        ));
    }

    #[test]
    fn verification_rejects_invalid_outcomes_and_recovers_cache() {
        use egt_file::MaybeDtcOutcome;

        let dir = TestDir::new("verify_invalid_outcome");
        unverified_generator(&dir.0).generate("K_K").unwrap();
        let mut prober = EgtProber::new(&dir.0).unwrap();
        let file = prober.load_existing("K_K").unwrap();
        let index = (0..file.index_range)
            .find(|&index| file.egt.position_from_index(index, Color::White).is_some())
            .unwrap();
        prober.cache.insert("K_K".to_string(), file);

        for threads in [0, 1, 4] {
            prober.with_threads(threads);
            prober
                .cache
                .get_mut("K_K")
                .unwrap()
                .write_to_index(index, MaybeDtcOutcome::INVALID)
                .unwrap();
            assert!(matches!(prober.verify_internal_consistency("K_K"),
                Err(EgtError::ConsistencyCheckFailed { index: failed, .. }) if failed == index));
            assert!(prober.cache.contains_key("K_K"));

            prober
                .cache
                .get_mut("K_K")
                .unwrap()
                .write_to_index(index, MaybeDtcOutcome::from_u16(8))
                .unwrap();
            assert!(matches!(prober.verify_internal_consistency("K_K"),
                Err(EgtError::CorruptedOutcome { index: failed, value: 8 }) if failed == index));
            prober
                .cache
                .get_mut("K_K")
                .unwrap()
                .write_to_index(index, MaybeDtcOutcome::DRAW)
                .unwrap();
            prober.verify_internal_consistency("K_K").unwrap();
        }
    }

    #[test]
    fn default_and_zero_threads_generate_and_verify() {
        for threads in [None, Some(0)] {
            let dir = TestDir::new(&format!("default_threads_{threads:?}"));
            let mut generator = EgtGenerator::new(&dir.0).unwrap();
            generator.with_verify(false);
            let mut prober = EgtProber::new(&dir.0).unwrap();
            assert_eq!(generator.threads, 0);
            assert_eq!(prober.threads, 0);
            if let Some(threads) = threads {
                generator.with_threads(threads);
                prober.with_threads(threads);
            }
            let tables = generator.generate("K_K").unwrap();
            assert_eq!(tables.len(), 1);
            prober.verify_internal_consistency("K_K").unwrap();
        }
    }

    #[test]
    fn generator_publishes_finalized_stats_and_verifies_both_outputs_not_input() {
        use sha2::{Digest, Sha256};

        let output = TestDir::new("generator_stats_output");
        let input = TestDir::new("generator_stats_input");
        Location::parse(&input.0).unwrap().ensure_format().unwrap();
        // Valid input metadata makes these malformed artifacts effective: only
        // pinning both output orientations can keep verification from opening them.
        for name in ["KQ_K", "K_KQ"] {
            write_artifact(&input.0, name, "ggegt", &[0xff; 64]);
        }
        let mut generator = EgtGenerator::new(&output.0).unwrap();
        generator.with_input_path(&input.0).unwrap();
        generator.with_threads(2);
        assert!(generator.verify);
        generator.generate("K_K").unwrap();
        let tables = generator.generate("KQ_K").unwrap();
        assert_eq!(tables.len(), 2);
        assert!(table_path(&output.0, "K_K", "ggegt").exists());
        for stats in &tables {
            let data = std::fs::read(table_path(&output.0, &stats.tablename, "ggegt")).unwrap();
            assert_eq!(stats.bytes, data.len() as u64);
            assert_eq!(stats.sha256, format!("{:x}", Sha256::digest(&data)));
            let json = std::fs::read(table_path(&output.0, &stats.tablename, "json")).unwrap();
            assert_eq!(serde_json::from_slice::<EgtStats>(&json).unwrap(), *stats);
            assert_eq!(stats.schema_version, stats::SCHEMA_VERSION);
            assert_eq!(stats.indexing_version, stats::INDEXING_VERSION);
        }
        let mut shadowed = EgtProber::new(&output.0).unwrap();
        shadowed.with_input_path(&input.0).unwrap();
        for name in ["KQ_K", "K_KQ"] {
            let error = shadowed.load_existing(name).unwrap_err();
            assert!(!matches!(error, EgtError::FileNotFound(_)));
        }
        let prober = EgtProber::new(&output.0).unwrap();
        assert_eq!(
            prober
                .stats(&["KQ_K", "K_KQ"])
                .unwrap(),
            aggregate(&tables)
        );
    }

    #[test]
    fn read_only_output_is_rejected_before_generation() {
        let mut generator = EgtGenerator::new("https://example.com/tables/").unwrap();
        for threads in [1, 4] {
            generator.with_threads(threads);
            assert!(matches!(generator.generate("K_K"),
                Err(EgtError::InvalidStorageLocation(message)) if message.contains("read-only")));
        }
    }

    #[test]
    fn in_memory_prefix_reuses_backend_for_dependencies_and_verification() {
        let generator = unverified_generator("memory:///nested/tables");
        generator.generate("K_K").unwrap();
        let tables = generator.generate("KQ_K").unwrap();
        assert_eq!(tables.len(), 2);

        let mut prober = EgtProber::from_location(generator.base_path.clone());
        prober.with_threads(2);
        prober.verify_selected(&["KQ_K", "K_KQ"]).unwrap();
        let file = prober.load_existing("K_K").unwrap();
        assert_eq!(
            file.key.as_ref(),
            format!("nested/tables/{}", file.id.relative_path("ggegt"))
        );
        assert_eq!(
            prober
                .stats(&["KQ_K", "K_KQ"])
                .unwrap(),
            aggregate(&tables)
        );
    }

    #[test]
    fn input_lookup_falls_back_only_for_missing_files() {
        let output = TestDir::new("input_fallback_output");
        let input = TestDir::new("input_fallback_input");
        unverified_generator(&output.0).generate("K_K").unwrap();
        Location::parse(&input.0).unwrap().ensure_format().unwrap();

        let mut prober = EgtProber::new(&output.0).unwrap();
        prober.with_input_path(&input.0).unwrap();
        assert!(prober.load_existing("K_K").is_ok());
        write_artifact(&input.0, "K_K", "ggegt", &[0xff; 64]);
        let error = prober.load_existing("K_K").unwrap_err();
        assert!(!matches!(error, EgtError::FileNotFound(_)));
        std::fs::remove_file(table_path(&input.0, "K_K", "ggegt")).unwrap();
        assert!(prober.load_existing("K_K").is_ok());
    }

    #[test]
    fn changing_input_path_invalidates_warmed_lookup_precedence() {
        let output = TestDir::new("changed_input_output");
        let input = TestDir::new("changed_input_input");
        unverified_generator(&output.0).generate("K_K").unwrap();
        Location::parse(&input.0).unwrap().ensure_format().unwrap();
        write_artifact(&input.0, "K_K", "ggegt", &[0xff; 64]);
        let egt = Egt::from_tablename("K_K").unwrap();
        let position = (0..egt.index_range())
            .find_map(|index| egt.position_from_index(index, Color::White))
            .unwrap();
        let mut prober = EgtProber::new(&output.0).unwrap();
        assert_eq!(prober.probe(&position).unwrap(), DtcOutcome::Draw);
        assert!(prober.cache.contains_key("K_K"));
        prober.with_input_path(&input.0).unwrap();
        assert!(prober.cache.is_empty());
        assert!(prober.probe(&position).is_err());
    }

    #[test]
    fn selector_union_is_sorted_deduplicated_and_preserves_orientation() {
        let tables = resolve_selectors(&["KP_K", "KPa_K", "3/2v1/op0/1pawns", "KP_K"]).unwrap();
        assert_eq!(tables, resolve_selectors(&["KP_K", "K_KP"]).unwrap());
        assert_eq!(tables.len(), 8);
        assert!(tables.windows(2).all(|pair| pair[0] < pair[1]));
        let exact = resolve_selectors(&["3/KQ_K", "KQ_K", "KR_K"]).unwrap();
        assert_eq!(
            exact.iter().map(TableId::tablename).collect::<Vec<_>>(),
            ["KQ_K", "KR_K"]
        );
        assert!(!exact.contains(&TableId::parse("K_KQ").unwrap()));
    }
}
