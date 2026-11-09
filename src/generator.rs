mod finalization;
mod solver;

use std::path::Path;

use crate::egt_file::BatchReader;
use crate::storage::Location;
use crate::{
    EgtError, EgtFileReader, EgtResult, EgtStats, GenerationPlan, resolve_selectors,
    verification,
};

/// Generates one pair at a time with eagerly loaded conversion dependencies.
/// Working arrays and all decompressed dependencies must fit in memory together;
/// dependencies are retained through optional verification, then released.
pub struct EgtGenerator {
    base_path: Location,
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
            input_path: None,
            verify: true,
            threads: 0,
        }
    }

    /// Sets generation and verification workers. Zero (the default) uses Rayon's default.
    pub fn set_threads(&mut self, threads: usize) {
        self.threads = threads;
    }

    /// Resolves an additional local directory or object-store URL for dependencies.
    /// Input is searched first; output is tried only if the input file is missing.
    /// If not set, only the output location is consulted.
    ///
    /// Returns an error if the location cannot be resolved, leaving the previous
    /// setting unchanged. This synchronous API should run in `spawn_blocking`
    /// when called from an async application.
    pub fn set_input_path(&mut self, path: impl AsRef<Path>) -> EgtResult<()> {
        self.input_path = Some(Location::parse(path)?);
        Ok(())
    }

    /// Enables or disables the internal consistency check performed after
    /// generation. Enabled by default.
    pub fn set_verify(&mut self, verify: bool) {
        self.verify = verify;
    }

    /// Generates a logical selector and its quiet-move twins.
    /// Capture/promotion dependencies must already exist or be explicitly selected.
    pub fn generate(&self, selector: &str) -> EgtResult<Vec<EgtStats>> {
        self.generate_selected(&[selector])
    }

    /// Deduplicates selectors into twin-pair jobs, ordered by pieces then pawns.
    /// Reuses one Rayon pool and loaded dependencies with verification.
    /// All enumerated dependencies must exist before initialization starts.
    /// Completed pairs are published and optionally verified before releasing
    /// their loaded tables and starting the next job.
    pub fn generate_selected(&self, selectors: &[&str]) -> EgtResult<Vec<EgtStats>> {
        let plan = GenerationPlan::new(resolve_selectors(selectors)?);
        self.base_path.ensure_format()?;
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(self.threads)
            .build()
            .map_err(|_| EgtError::Internal("failed to build generation thread pool"))?;
        let mut results = Vec::new();
        println!(
            "Generating {} twin-pair jobs using {} threads.",
            plan.pairs.len(),
            pool.current_num_threads()
        );
        pool.install(|| -> EgtResult<()> {
            for pair in plan.pairs {
                let start = std::time::Instant::now();
                let mut dependencies: Vec<_> = std::iter::once(&pair.table)
                    .chain(pair.twin.iter())
                    .flat_map(|id| id.dependencies())
                    .collect();
                dependencies.sort();
                dependencies.dedup();
                let mut reader =
                    BatchReader::open(&self.base_path, self.input_path.as_ref(), &dependencies)?;
                println!(
                    "{}: loading {} bytes of dependencies...",
                    pair.table.tablename(), reader.resident_bytes(),
                );
                reader.load()?;
                let dependency_time = start.elapsed();
                let pair_stats =
                    solver::generate_pair(&self.base_path, &pair, &reader, dependency_time)?;
                if self.verify {
                    let names: Vec<_> = pair_stats
                        .iter()
                        .map(|stats| stats.tablename.clone())
                        .collect();
                    let mut verification = Vec::new();
                    // Load explicit outputs so input artifacts cannot shadow either one.
                    for name in &names {
                        let file = EgtFileReader::open_in(&self.base_path, name)?;
                        verification.push(file.egt().clone());
                        reader.insert(file);
                    }
                    reader.load()?;
                    for (name, egt) in names.iter().zip(verification) {
                        verification::verify_egt(&reader, name, &egt)?;
                    }
                }
                print_pair_stats(&pair_stats[0], pair_stats.get(1), start.elapsed());
                results.extend(pair_stats);
            }
            Ok(())
        })?;
        Ok(results)
    }
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
