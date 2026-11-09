mod finalization;
mod solver;

use std::path::Path;

use crate::AggregateStats;
use crate::egt_file::BatchReader;
use crate::stats::format_timing;
use crate::storage::Location;
use crate::{
    EgtError, EgtFileReader, EgtResult, EgtStats, GenerationPlan, resolve_selectors, verification,
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
        let start = std::time::Instant::now();
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
                    pair.table.tablename(),
                    reader.resident_bytes(),
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
        let mut total = AggregateStats::default();
        for stats in &results {
            total.add(stats);
        }
        let lowest_compression = results
            .iter()
            .filter(|stats| stats.unique_positions > 0)
            .map(|stats| {
                (
                    stats.tablename.as_str(),
                    stats.bytes as f64 * 8.0 / stats.unique_positions as f64,
                )
            })
            .max_by(|a, b| a.1.total_cmp(&b.1));
        println!(
            "{}",
            generation_summary(
                &total,
                start.elapsed(),
                pool.current_num_threads(),
                lowest_compression
            )
        );
        Ok(results)
    }
}

fn generation_summary(
    total: &AggregateStats,
    elapsed: std::time::Duration,
    threads: usize,
    lowest_compression: Option<(&str, f64)>,
) -> String {
    let unique_positions = total
        .unique_positions
        .expect("generation statistics are complete");
    let bytes = total.bytes.expect("generation statistics are complete");
    let average_compression = if unique_positions > 0 {
        bytes as f64 * 8.0 / unique_positions as f64
    } else {
        0.0
    };
    let mut summary = format!(
        "Generated {} tables: {} unique positions, {:.2} MiB in {}.\nAverage compression: {:.2} bits/pos.",
        total.available_tables,
        unique_positions,
        bytes as f64 / (1024.0 * 1024.0),
        format_timing(elapsed, total.index_range, threads),
        average_compression,
    );
    if let Some((tablename, bits_per_pos)) = lowest_compression {
        summary.push_str(&format!(
            " Lowest compression ({tablename}): {bits_per_pos:.2} bits/pos."
        ));
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn summaries_use_allocated_core_time_per_index() {
        let total = AggregateStats {
            selected_tables: 2,
            available_tables: 2,
            bytes: Some(1024 * 1024),
            index_range: 5_000_000,
            unique_positions: Some(4 * 1024 * 1024),
            ..Default::default()
        };
        assert_eq!(
            generation_summary(&total, Duration::from_secs(65), 2, Some(("KQ_K", 3.0))),
            "Generated 2 tables: 4194304 unique positions, 1.00 MiB in 00h:01m:05s (26.00 μs/index on 2 threads).\nAverage compression: 2.00 bits/pos. Lowest compression (KQ_K): 3.00 bits/pos."
        );
        assert_eq!(
            format_timing(Duration::from_millis(1250), 1_000_000, 1),
            "00h:00m:01s (1.25 μs/index on 1 thread)"
        );
        assert_eq!(
            format_timing(Duration::from_secs(3661), 0, 4),
            "01h:01m:01s (0.00 μs/index on 4 threads)"
        );
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
    let mut index_range = stats_a.index_range;
    print_table_stats(stats_a);

    if let Some(sb) = stats_b {
        index_range += sb.index_range;
        print_table_stats(sb);
    }

    println!(
        "Time used {}.",
        format_timing(duration, index_range, rayon::current_num_threads())
    );
}
