use std::time::Duration;

use crate::{AggregateStats, EgtStats};

/// Formats wall time and allocated core time per index slot.
pub(crate) fn format_timing(
    elapsed: Duration,
    index_range: usize,
    threads: usize,
) -> String {
    let seconds = elapsed.as_secs();
    let core_us_per_index = if index_range > 0 {
        elapsed.as_secs_f64() * threads as f64 * 1_000_000.0 / index_range as f64
    } else {
        0.0
    };
    let unit = if threads == 1 { "thread" } else { "threads" };
    format!(
        "{:02}h:{:02}m:{:02}s ({core_us_per_index:.2} μs/index on {threads} {unit})",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60,
    )
}

pub(crate) fn generation_summary(
    total: &AggregateStats,
    elapsed: Duration,
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

/// Prints table-specific statistics (wins, draws, losses, compression).
pub(crate) fn print_table_stats(stats: &EgtStats) {
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

#[cfg(test)]
mod tests {
    use super::*;

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
