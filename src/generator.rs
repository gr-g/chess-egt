mod finalization;
mod solver;
mod unmoves;

use std::time::Instant;

use self::solver::{TableState, solve_pair};
use crate::egt_file::{BatchReader, Source};
use crate::report::{generation_summary, print_table_stats};
use crate::selection::{GenerationPlan, TablePair};
use crate::{AggregateStats, Egt, EgtResult, EgtStats, Tablebase, resolve_selectors};

#[derive(Default, Clone, Copy)]
struct WinLoss<T> {
    win: T,
    loss: T,
}

impl Tablebase {
    /// Generates twin-pair jobs in the base location, ordered by pieces then pawns.
    /// Capture/promotion dependencies must already exist or be explicitly selected.
    /// Working arrays and each pair's eagerly loaded dependencies must fit in memory.
    /// Clears the probe cache before any tables can be replaced.
    pub fn generate(&mut self, selectors: &[&str]) -> EgtResult<Vec<EgtStats>> {
        let start = Instant::now();
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
                results.extend(generate_pair(source, &pair)?);
            }
            Ok(())
        })?;
        let mut total = AggregateStats::default();
        for stats in &results {
            total.add(stats);
        }
        let lowest_compression = results
            .iter()
            .filter(|stats| stats.totals.unique_positions > 0)
            .map(|stats| {
                (
                    stats.tablename.as_str(),
                    stats.totals.bytes as f64 * 8.0 / stats.totals.unique_positions as f64,
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

/// Generates and publishes one table and its twin in the caller's Rayon pool.
/// A self-twin is published once. Statistics are returned in table/twin order.
fn generate_pair(source: &Source, pair: &TablePair) -> EgtResult<Vec<EgtStats>> {
    let dependency_start = Instant::now();
    let ids = || std::iter::once(&pair.table).chain(pair.twin.iter());
    let mut dependencies: Vec<_> = ids().flat_map(|id| id.dependencies()).collect();
    dependencies.sort();
    dependencies.dedup();
    let reader = BatchReader::open(source, &dependencies)?;
    println!(
        "{}: loaded {} bytes of dependencies.",
        pair.table,
        reader.resident_bytes(),
    );
    let dependency_time = dependency_start.elapsed();

    let base = source.base();
    base.ensure_writable()?;
    let setup_start = Instant::now();
    let mut tables = ids()
        .map(|id| TableState::new(Egt::new(id)?))
        .collect::<EgtResult<Vec<_>>>()?;
    let setup_time = setup_start.elapsed();
    let timings = solve_pair(&mut tables, pair.reflected, &reader)?;
    let init_time = setup_time + timings.init;

    let finalize_start = Instant::now();
    let mut stats = Vec::with_capacity(tables.len());
    for (id, table) in ids().zip(tables) {
        let (egt, values, maxima, invalid) = table.into_parts();
        println!("{id}: finalizing outcomes, statistics and compression...");
        stats.push(finalization::finalize_table(base, id, &egt, values, maxima, invalid)?);
    }
    let finalize_time = finalize_start.elapsed();
    let total_time = dependency_time + init_time + timings.propagation + finalize_time;
    let names = ids().map(|id| id.name()).collect::<Vec<_>>().join(", ");
    println!(
        "{}: phase timings: dependencies {:.3}s, init {:.3}s, propagation {:.3}s, finalize/publish {:.3}s, total {:.3}s.",
        names,
        dependency_time.as_secs_f64(),
        init_time.as_secs_f64(),
        timings.propagation.as_secs_f64(),
        finalize_time.as_secs_f64(),
        total_time.as_secs_f64(),
    );
    for table_stats in &stats {
        print_table_stats(table_stats);
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EgtError;
    use crate::selection::TableSelector;
    use crate::storage::Location;
    use crate::table_id::TableId;

    #[test]
    fn parallel_missing_dependency_returns_error_without_generating() {
        let dir = crate::TestDir::new("parallel_missing_dependency");
        let mut generator = Tablebase::new(&dir.0).unwrap();
        generator.set_threads(4);
        let result = generator.generate(&["KQ_K"]);
        assert!(matches!(result, Err(EgtError::DependencyUnavailable { dependency, source })
            if dependency == "K_K" && matches!(*source, EgtError::FileNotFound(_))));
        for name in ["K_K", "KQ_K", "K_KQ"] {
            let id = TableId::parse(name).unwrap();
            assert!(!dir.0.join(id.relative_path("ggegt")).exists());
        }
    }

    #[test]
    fn zero_threads_uses_rayon_default() {
        let dir = crate::TestDir::new("zero_threads");
        let mut generator = Tablebase::new(&dir.0).unwrap();
        generator.set_threads(0);
        generator.generate(&["K_K"]).unwrap();
        assert!(dir.0.join(TableId::parse("K_K").unwrap().relative_path("ggegt")).exists());
    }

    fn verify_table_generation(
        endgame: &str,
        expected_range_a: usize,
        expected_wins_a: usize,
        expected_draws_a: usize,
        expected_losses_a: usize,
        expected_invalid_a: usize,
        expected_range_b: Option<usize>,
        expected_wins_b: Option<usize>,
        expected_draws_b: Option<usize>,
        expected_losses_b: Option<usize>,
        expected_invalid_b: Option<usize>,
    ) {
        let test_dir = crate::TestDir::new(&format!("generation_{}", endgame));
        let location = Location::parse(&test_dir.0).unwrap();
        let mut generator = Tablebase::new(&test_dir.0).unwrap();
        generator.set_threads(2);
        if endgame != "K_K" {
            generator.generate(&["K_K"]).unwrap();
        }
        if endgame == "KP_K" {
            for dependency in ["KQ_K", "KR_K", "KB_K", "KN_K"] {
                generator.generate(&[dependency]).unwrap();
            }
        }
        generator.generate(&[endgame]).unwrap();

        // Check the established outcome totals by summing individual sidecars,
        // not by constructing a composite table or comparing legacy artifacts.
        let totals = |material: &str| {
            let mut totals = [0usize; 5];
            for id in TableSelector::parse(material).unwrap().resolve().unwrap() {
                let stats = crate::stats::read_stats(&location, &id).unwrap();
                let excluded = stats.totals.invalid_opposite_check
                                    + stats.totals.invalid_ep_squares_occupied + stats.totals.invalid_illegal_ep_capture + stats.totals.redundant;
                                let counts = [stats.index_range, stats.totals.win, stats.totals.draw, stats.totals.loss, excluded];
                for (total, count) in totals.iter_mut().zip(counts) { *total += count; }
            }
            totals
        };
        assert_eq!(totals(endgame), [expected_range_a, expected_wins_a, expected_draws_a, expected_losses_a, expected_invalid_a]);
        if let Some(expected_range_b) = expected_range_b {
            let (a, b) = endgame.split_once('_').unwrap();
            assert_eq!(totals(&format!("{b}_{a}")), [expected_range_b, expected_wins_b.unwrap(), expected_draws_b.unwrap(), expected_losses_b.unwrap(), expected_invalid_b.unwrap()]);
        } else {
            let ids = TableSelector::parse(endgame).unwrap().resolve().unwrap();
            assert!(ids.iter().all(|id| id.twin().0 == *id));
        }
    }

    #[test]
    fn test_k_k_table_generation() {
        verify_table_generation("K_K", 462, 0, 462, 0, 0, None, None, None, None, None);
    }

    #[test]
    fn test_kq_k_table_generation() {
        verify_table_generation(
            "KQ_K",
            28644, 18081, 0, 0, 10563,
            Some(28644), Some(0), Some(2896), Some(25160), Some(588)
        );
    }

    #[test]
    fn test_kr_k_table_generation() {
        verify_table_generation(
            "KR_K",
            28644, 21959, 0, 0, 6685,
            Some(28644), Some(0), Some(2796), Some(25260), Some(588)
        );
    }

    #[test]
    fn test_kb_k_table_generation() {
        verify_table_generation(
            "KB_K",
            28644, 0, 24178, 0, 4466,
            Some(28644), Some(0), Some(28056), Some(0), Some(588)
        );
    }

    #[test]
    fn test_kn_k_table_generation() {
        verify_table_generation(
            "KN_K",
            28644, 0, 25750, 0, 2894,
            Some(28644), Some(0), Some(28056), Some(0), Some(588)
        );
    }

    #[test]
    fn test_kp_k_table_generation() {
        verify_table_generation(
            "KP_K",
            93744, 62480, 19184, 0, 12080,
            Some(93744), Some(0), Some(35210), Some(48802), Some(9732)
        );
    }
}
