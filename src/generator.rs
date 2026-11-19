mod finalization;
mod solver;

use crate::egt_file::BatchReader;
use crate::stats::format_timing;
use crate::{AggregateStats, EgtResult, EgtStats, GenerationPlan, Tablebase, resolve_selectors};

impl Tablebase {
    /// Generates twin-pair jobs in the base location, ordered by pieces then pawns.
    /// Capture/promotion dependencies must already exist or be explicitly selected.
    /// Working arrays and each pair's eagerly loaded dependencies must fit in memory.
    /// Clears the probe cache before any tables can be replaced.
    pub fn generate(&mut self, selectors: &[&str]) -> EgtResult<Vec<EgtStats>> {
        let start = std::time::Instant::now();
        let plan = GenerationPlan::new(resolve_selectors(selectors)?);
        self.source().base().ensure_format()?;
        let pool = self.pool("failed to build generation thread pool")?;
        self.clear_probe_cache();
        let source = self.source();
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
                let reader = BatchReader::open(source, &dependencies)?;
                println!(
                    "{}: loaded {} bytes of dependencies.",
                    pair.table,
                    reader.resident_bytes(),
                );
                let dependency_time = start.elapsed();
                let pair_stats =
                    solver::generate_pair(source.base(), &pair, &reader, dependency_time)?;
                for stats in &pair_stats {
                    print_table_stats(stats);
                }
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
        "Generated {} tables: {} unique positions in {}.\nCompressed size: {:.2} MiB ({:.2} bits/pos).",
        total.available_tables,
        unique_positions,
        format_timing(elapsed, total.index_range, threads),
        bytes as f64 / (1024.0 * 1024.0),
        average_compression,
    );
    if let Some((tablename, bits_per_pos)) = lowest_compression {
        summary.push_str(&format!(
            " Lowest compression: {tablename} ({bits_per_pos:.2} bits/pos)."
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
            "Generated 2 tables: 4194304 unique positions in 00h:01m:05s (26.00 μs/index on 2 threads).\nCompressed size: 1.00 MiB (2.00 bits/pos). Lowest compression: KQ_K (3.00 bits/pos)."
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
