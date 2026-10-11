//! Chess endgame generation and probing through a synchronous API.
//!
//! Generator/prober constructors and `with_input_path` resolve storage locations
//! and are fallible: callers must handle their `EgtResult` (for example with `?`).
//! Locations may be local directories or supported object-store URLs. Generated
//! compressed results are retained in RAM until publication.
//!
//! Storage operations block the calling thread, including remote reads and writes.
//! Async applications should call this API from `tokio::task::spawn_blocking`
//! rather than directly from an async task, including construction/configuration
//! and calls to generate, probe, or verify tables.

pub mod error;
pub mod piece_set;
mod egt;
mod egt_file;
mod egt_writer;
mod retrograde;
mod storage;

pub use error::{EgtError, EgtResult};
pub use egt_file::{EgtFile, EgtFileStats, LongestDtcPosition};
use shakmaty::{Role, Chess, Color, Move, MoveList, Position};
use std::cmp::Ordering;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use crate::storage::Location;
use std::collections::HashMap;

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
    generate_deps: bool,
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
            generate_deps: false,
            verify: true,
            threads: 1,
        }
    }

    /// Sets generation workers. Zero is rejected by `generate`.
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

    /// Enables automatic generation of missing dependencies on the fly.
    /// Disabled by default: when disabled, a missing dependency causes
    /// `generate()` to return a `DependencyUnavailable` error.
    pub fn with_generate_deps(&mut self, generate_deps: bool) {
        self.generate_deps = generate_deps;
    }

    /// Enables or disables the internal consistency check performed after
    /// generation. Enabled by default.
    pub fn with_verify(&mut self, verify: bool) {
        self.verify = verify;
    }

    pub fn list_n_pieces_endgames(n: usize) -> EgtResult<Vec<String>> {
        if n > 8 {
            return Err(EgtError::InvalidPieceConfig("list_n_pieces_endgames() called with n > 8"));
        }
        if n < 2 {
            return Ok(Vec::new());
        }
        let mut endgames = Vec::new();
        let num_non_kings = n - 2;

        // Helper to generate all combinations of non-king pieces of size k
        fn get_piece_combinations(k: usize) -> Vec<String> {
            let mut results = Vec::new();
            let mut current = String::new();
            let roles = ['Q', 'R', 'B', 'N', 'P'];
            fn recurse(k: usize, start_idx: usize, current: &mut String, results: &mut Vec<String>, roles: &[char]) {
                if current.len() == k {
                    results.push(current.clone());
                    return;
                }
                for idx in start_idx..roles.len() {
                    current.push(roles[idx]);
                    recurse(k, idx, current, results, roles);
                    current.pop();
                }
            }
            recurse(k, 0, &mut current, &mut results, &roles);
            results
        }

        // Distribute num_non_kings between side_a and side_b
        for a in 0..=num_non_kings {
            let b = num_non_kings - a;
            let combos_a = get_piece_combinations(a);
            let combos_b = get_piece_combinations(b);

            for pieces_a in &combos_a {
                for pieces_b in &combos_b {
                    let side_a = format!("K{}", pieces_a);
                    let side_b = format!("K{}", pieces_b);

                    // Keep only one-sided endgames: side_a >= side_b
                    if side_a >= side_b {
                        endgames.push(format!("{}_{}", side_a, side_b));
                    }
                }
            }
        }

        // Sort by:
        // 1. Number of pawns (ascending)
        // 2. Lexicographical order of the endgame string
        endgames.sort_by(|a, b| {
            let pawns_a = a.chars().filter(|&c| c == 'P').count();
            let pawns_b = b.chars().filter(|&c| c == 'P').count();
            pawns_a.cmp(&pawns_b).then_with(|| a.cmp(b))
        });

        Ok(endgames)
    }

    pub fn generate(&self, endgame: &str) -> EgtResult<(EgtFileStats, Option<EgtFileStats>)> {
        self.generate_impl(endgame, false)
    }

    /// Recursive dependencies reuse the explicitly installed solver pool.
    pub(crate) fn generate_in_pool(&self, endgame: &str) -> EgtResult<(EgtFileStats, Option<EgtFileStats>)> {
        self.generate_impl(endgame, true)
    }

    fn generate_impl(&self, endgame: &str, in_pool: bool) -> EgtResult<(EgtFileStats, Option<EgtFileStats>)> {
        let start_time = std::time::Instant::now();
        println!("Generating endgame {} at {}", endgame, self.base_path.display);

        // TODO: Preallocate memory
        //if let Some(n) = self.assigned_memory {
        //    ...
        //}

        // Run retrograde analysis
        let (mut file_a, mut file_b) = if in_pool {
            crate::retrograde::retrograde_analysis_in_pool(
                &self.base_path, endgame, self.input_path.as_ref(), self.generate_deps,
            )?
        } else if self.threads == 1 {
            crate::retrograde::retrograde_analysis(
                &self.base_path, endgame, self.input_path.as_ref(), self.generate_deps,
            )?
        } else {
            crate::retrograde::retrograde_analysis_with_threads(
                &self.base_path, endgame, self.input_path.as_ref(), self.generate_deps, self.threads,
            )?
        };

        // Publication supplies the size and hash of each output file.
        let stats_a = file_a.stats.take()
            .ok_or(EgtError::Internal("stats missing for file_a after generation"))?;
        file_a.save_stats(&stats_a)?;

        let mut stats_b = None;
        if let Some(ref mut fb) = file_b {
            let s_b = fb.stats.take()
                .ok_or(EgtError::Internal("stats missing for file_b after generation"))?;
            fb.save_stats(&s_b)?;
            stats_b = Some(s_b);
        }

        let duration = start_time.elapsed();
        print_pair_stats(&stats_a, stats_b.as_ref(), duration);

        // Check internal consistency
        if self.verify {
            let mut prober = EgtProber::from_location(self.base_path.clone());
            prober.input_path = self.input_path.clone();
            // Verify the freshly published outputs, not same-named input files.
            prober.cache.insert(file_a.endgame.clone(), file_a);
            if let Some(fb) = file_b {
                prober.cache.insert(fb.endgame.clone(), fb);
            }
            prober.verify_internal_consistency(&stats_a.endgame)?;
            if let Some(ref sb) = stats_b {
                prober.verify_internal_consistency(&sb.endgame)?;
            }
        }

        Ok((stats_a, stats_b))
    }
}

pub struct EgtProber {
    base_path: Location,
    /// Additional path used to look up dependency files. If `None`, only
    /// `base_path` is consulted.
    input_path: Option<Location>,
    assigned_memory: Option<usize>,
    cache: HashMap<String, EgtFile>,
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
        }
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
        self.input_path = Some(Location::parse(path)?);
        Ok(())
    }

    /// Attempts to load an existing file, looking it up first in `input_path`
    /// (if set) and then in `base_path`.
    fn load_existing(&self, endgame: &str) -> EgtResult<EgtFile> {
        load_existing_file(&self.base_path, self.input_path.as_ref(), endgame)
    }

    pub fn probe(&mut self, position: &Chess) -> EgtResult<DtcOutcome> {
        let endgame = get_endgame(position);
        if !self.cache.contains_key(&endgame) {
            let file = self.load_existing(&endgame)?;
            self.cache.insert(endgame.clone(), file);
        }
        let file = self.cache.get_mut(&endgame)
            .ok_or(EgtError::Internal("cache entry missing after insert"))?;
        let idx = file.map_position_to_index(position)?;
        let outcome = file.read_from_index(idx)?;
        outcome.to_outcome_at(idx)
    }

    /// Verifies the internal consistency of an endgame file, one constituent
    /// `Egt` sub-table at a time.
    pub fn verify_internal_consistency(&mut self, endgame: &str) -> EgtResult<()> {
        // Ensure the main file is in the cache
        if !self.cache.contains_key(endgame) {
            let file = self.load_existing(endgame)?;
            self.cache.insert(endgame.to_string(), file);
        }

        let (n_egts, index_range, sum_of_ranges) = {
            let file = self.cache.get(endgame)
                .ok_or(EgtError::Internal("cache entry missing after insert"))?;
            let sum: usize = file.egts.iter().map(|egt| egt.index_range()).sum();
            (file.egts.len(), file.index_range, sum)
        };

        if sum_of_ranges != index_range {
            return Err(EgtError::Internal(
                "sum of Egt index ranges does not match the EgtFile index range",
            ));
        }

        println!(
            "Verifying internal consistency of endgame {} ({} indexes, {} tables)...",
            endgame, index_range, n_egts
        );

        for egt_idx in 0..n_egts {
            self.verify_egt(endgame, egt_idx)?;
        }

        println!("Successfully verified internal consistency of endgame {}.", endgame);
        Ok(())
    }

    /// Verifies the internal consistency of a single `Egt` sub-table of a file.
    ///
    /// Indexes are enumerated locally within the sub-table and translated to
    /// global file indexes by adding the sub-table offset, which avoids the
    /// per-index sub-table lookup done by `EgtFile::index_to_position`.
    fn verify_egt(&mut self, endgame: &str, egt_idx: usize) -> EgtResult<()> {
        let (tablename, local_range, egt_offset) = {
            let file = self.cache.get(endgame)
                .ok_or(EgtError::Internal("cache entry missing after insert"))?;
            let egt = file.egts.get(egt_idx)
                .ok_or(EgtError::Internal("egt_idx out of range for this file"))?;
            (egt.tablename().to_string(), egt.index_range(), file.get_global_index(egt_idx, 0))
        };

        println!(
            "{}: verifying {} indexes (offset {})...",
            tablename, local_range, egt_offset
        );

        // Builds the error for a failure at `local_idx`, tagging the reason with
        // the sub-table name and local index. The reported index is the global
        // one, so that it can be fed back to `read_from_index` for debugging.
        let fail = |local_idx: usize, reason: &str| EgtError::ConsistencyCheckFailed {
            endgame: endgame.to_string(),
            index: egt_offset + local_idx,
            reason: format!("table {}, local index {}: {}", tablename, local_idx, reason),
        };

        for local_idx in 0..local_range {
            if (local_idx + 1) % 10000000 == 0 {
                println!("{}: verified {}/{} indexes...", tablename, local_idx + 1, local_range);
            }

            let global_idx = egt_offset + local_idx;

            // Retrieve the setup and the outcome for this index. The borrow of
            // the cached file must end before probing successors below.
            let (position_opt, outcome_maybe) = {
                let file = self.cache.get_mut(endgame)
                    .ok_or(EgtError::Internal("cache entry missing after insert"))?;
                let position_opt = file.egts[egt_idx].position_from_index(local_idx, Color::White);
                let outcome_maybe = file.read_from_index(global_idx)?;
                (position_opt, outcome_maybe)
            };

            if position_opt.is_none() {
                if !outcome_maybe.is_invalid() {
                    return Err(fail(local_idx, "index is invalid but outcome is not invalid"));
                }
                continue;
            }

            if outcome_maybe.is_invalid() {
                return Err(fail(local_idx, "index is valid but outcome is invalid"));
            }

            let position = position_opt
                .ok_or(EgtError::Internal("position_opt should be Some after the None check"))?;
            let outcome = outcome_maybe.to_outcome_at(global_idx)?;
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
                let best = self.best_outcome(&position, &legals)?;
                if outcome != best {
                    let details = self.describe_mismatch(&position, &legals, outcome, best);
                    return Err(fail(local_idx, &details));
                }
            }
        }

        Ok(())
    }

    /// Computes the best outcome reachable from `position` by playing one of
    /// `legals`, probing each successor in the tablebases.
    fn best_outcome(&mut self, position: &Chess, legals: &MoveList) -> EgtResult<DtcOutcome> {
        let mut best_value: Option<DtcOutcome> = None;

        for m in legals {
            let mut successor_position = position.clone();
            successor_position.play_unchecked(*m);

            let successor_outcome = self.probe(&successor_position)?;
            let v_m = move_value(m, successor_outcome);

            if let Some(ref mut best) = best_value {
                if v_m > *best {
                    *best = v_m;
                }
            } else {
                best_value = Some(v_m);
            }
        }

        best_value.ok_or(EgtError::Internal("best_value should be Some when legals is non-empty"))
    }

    /// Builds a human-readable diagnostic for a mismatch between the outcome
    /// stored in the file and the best outcome computed from the legal moves.
    fn describe_mismatch(
        &mut self,
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
            if let Ok(successor_outcome) = self.probe(&successor_position) {
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

/// The value of a position for the side to move after playing `m`, given the
/// outcome of the successor position. Conversions (captures and promotions)
/// reset the distance to conversion to 1.
fn move_value(m: &Move, successor_outcome: DtcOutcome) -> DtcOutcome {
    if m.is_capture() || m.is_promotion() {
        let ct = if m.is_capture() { ConversionType::Capture } else { ConversionType::Promotion };
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

/// Attempts to load an existing `EgtFile`, looking it up first in `input_path`
/// (if set) and then in `base_path`, falling back only when a file is missing.
fn load_existing_file(
    base_path: &Location,
    input_path: Option<&Location>,
    endgame: &str,
) -> EgtResult<EgtFile> {
    if let Some(input) = input_path {
        match EgtFile::open_in(input, endgame) {
            Ok(f) => return Ok(f),
            Err(EgtError::FileNotFound(_)) => {}
            Err(e) => return Err(e),
        }
    }
    EgtFile::open_in(base_path, endgame)
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
    for &role in &[Role::Queen, Role::Rook, Role::Bishop, Role::Knight, Role::Pawn] {
        let stm_count = (board.by_role(role) & board.by_color(stm_color)).into_iter().count();
        for _ in 0..stm_count {
            stm.push(role.upper_char());
        }

        let sntm_count = (board.by_role(role) & board.by_color(sntm_color)).into_iter().count();
        for _ in 0..sntm_count {
            sntm.push(role.upper_char());
        }
    }

    format!("{}_{}", stm, sntm)
}

/// Prints table-specific statistics (wins, draws, losses, compression).
pub fn print_table_stats(stats: &EgtFileStats) {
    let unique_positions = stats.unique_positions;
    let compressed_size_mb = stats.bytes as f64 / (1024.0 * 1024.0);
    let bits_per_pos = if unique_positions > 0 {
        (stats.bytes as f64 * 8.0) / unique_positions as f64
    } else {
        0.0
    };

    println!(
        "Generated endgame {} with {} unique positions: {} wins, {} draws, {} losses. Compressed size: {:.2}MiB ({:.2} bits/pos).",
        stats.endgame,
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
    stats_a: &EgtFileStats,
    stats_b: Option<&EgtFileStats>,
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

    println!(
        "Time used {} ({:.2} μs/pos).",
        format_dur,
        us_per_pos,
    );
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
        let dir = std::env::temp_dir()
            .join("chess-egt-tests")
            .join(format!("{}-{}", name, std::process::id()));
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

    // Generates and saves `endgame` (and its twin), then checks the internal
    // consistency of both tables.
    fn verify_internal_consistency(endgame: &str) {
        let test_dir = TestDir::new(&format!("verify_internal_consistency_{}", endgame));
        let location = Location::parse(&test_dir.0).unwrap();
        crate::retrograde::retrograde_analysis(&location, endgame, None, true).unwrap();

        let mut prober = EgtProber::new(&test_dir.0).unwrap();
        prober.verify_internal_consistency(endgame).unwrap();
        let (a, b) = endgame.split_once('_').unwrap();
        if a != b {
            prober.verify_internal_consistency(&format!("{}_{}", b, a)).unwrap();
        }
    }

    #[test]
    fn test_verify_internal_consistency_k_k() {
        verify_internal_consistency("K_K");
    }

    #[test]
    fn test_verify_internal_consistency_kr_k() {
        verify_internal_consistency("KR_K");
    }

    // Unlike the pawnless cases above, KP_K / K_KP are composed of several
    // `Egt` sub-tables (one per pawn file), which exercises the loop over
    // sub-tables in `verify_internal_consistency()`.
    #[test]
    fn test_verify_internal_consistency_kp_k() {
        let egt_file = EgtFile::new(&std::env::temp_dir(), "KP_K").unwrap();
        assert!(egt_file.egts.len() > 1, "expected KP_K to be composed of several Egts");
        verify_internal_consistency("KP_K");
    }

    #[test]
    fn generator_publishes_finalized_stats() {
        use sha2::{Digest, Sha256};

        let output = TestDir::new("generator_stats_output");
        let input = TestDir::new("generator_stats_input");
        // Same-named input files must not shadow outputs during verification.
        for endgame in ["KQ_K", "K_KQ"] {
            std::fs::write(input.0.join(format!("{endgame}.ggegt")), b"invalid table").unwrap();
        }
        let mut generator = EgtGenerator::new(&output.0).unwrap();
        generator.with_input_path(&input.0).unwrap();
        generator.with_generate_deps(true);
        // Default verification also exercises reopening published dependencies.
        let (stats_a, stats_b) = generator.generate("KQ_K").unwrap();
        assert!(output.0.join("K_K.ggegt").exists());
        for stats in std::iter::once(stats_a).chain(stats_b) {
            let data = std::fs::read(output.0.join(format!("{}.ggegt", stats.endgame))).unwrap();
            assert_eq!(stats.bytes, data.len() as u64);
            assert_eq!(stats.sha256, format!("{:x}", Sha256::digest(&data)));
            let json = std::fs::read(output.0.join(format!("{}.json", stats.endgame))).unwrap();
            assert_eq!(serde_json::from_slice::<serde_json::Value>(&json).unwrap(),
                serde_json::to_value(&stats).unwrap());
        }
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
        let mut generator = EgtGenerator::new("memory:///nested/tables").unwrap();
        generator.with_generate_deps(true);
        let (stats_a, stats_b) = generator.generate("KQ_K").unwrap();

        let mut prober = EgtProber::from_location(generator.base_path.clone());
        prober.verify_internal_consistency(&stats_a.endgame).unwrap();
        prober.verify_internal_consistency(&stats_b.unwrap().endgame).unwrap();
        assert!(prober.load_existing("K_K").is_ok());
    }

    #[test]
    fn input_lookup_falls_back_only_for_missing_files() {
        let output = TestDir::new("input_fallback_output");
        let input = TestDir::new("input_fallback_input");
        EgtGenerator::new(&output.0).unwrap().generate("K_K").unwrap();

        let mut prober = EgtProber::new(&output.0).unwrap();
        prober.with_input_path(&input.0).unwrap();
        assert!(prober.load_existing("K_K").is_ok());

        std::fs::write(input.0.join("K_K.ggegt"), b"invalid table").unwrap();
        let error = prober.load_existing("K_K").unwrap_err();
        assert!(!matches!(error, EgtError::FileNotFound(_)));
    }

    #[test]
    fn test_list_n_pieces_endgames() {
        let all_three = EgtGenerator::list_n_pieces_endgames(3).unwrap();
        assert_eq!(all_three, vec![
            "KB_K".to_string(),
            "KN_K".to_string(),
            "KQ_K".to_string(),
            "KR_K".to_string(),
            "KP_K".to_string(),
        ]);

        let all_two = EgtGenerator::list_n_pieces_endgames(2).unwrap();
        assert_eq!(all_two, vec!["K_K".to_string()]);

        let all_four = EgtGenerator::list_n_pieces_endgames(4).unwrap();
        // Ensure pawnless endgames are first, then 1 pawn, then 2 pawns
        let mut max_pawns = 0;
        for eg in all_four {
            let pawns = eg.chars().filter(|&c| c == 'P').count();
            assert!(pawns >= max_pawns, "Endgame {} with {} pawns came after an endgame with more pawns", eg, pawns);
            max_pawns = pawns;
        }
    }
}
