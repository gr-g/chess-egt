mod finalization;
mod solver;
mod unmoves;

use std::time::Instant;

use self::solver::{TableState, solve_pair};
use crate::egt_file::{BatchReader, Source};
use crate::report::{generation_summary, print_table_stats};
use crate::selection::{TablePair, plan_pairs};
use crate::{AggregateStats, Egt, EgtResult, EgtStats, resolve_selectors};

#[derive(Default, Clone, Copy)]
struct WinLoss<T> {
    win: T,
    loss: T,
}

/// Generates the selected tables and their twins in the base location.
pub(crate) fn generate(
    source: &Source,
    pool: &rayon::ThreadPool,
    selectors: &[&str],
) -> EgtResult<Vec<EgtStats>> {
    let start = Instant::now();
    let pairs = plan_pairs(resolve_selectors(selectors)?);
    source.base().ensure_format()?;
    let mut results = Vec::new();
    println!(
        "Generating {} twin-pair jobs using {} threads.",
        pairs.len(),
        pool.current_num_threads()
    );
    pool.install(|| -> EgtResult<()> {
        for pair in pairs {
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
        .map(|id| TableState::new(Egt::new(id)))
        .collect::<EgtResult<Vec<_>>>()?;
    let setup_time = setup_start.elapsed();
    let timings = solve_pair(&mut tables, pair.reflected, &reader)?;
    let init_time = setup_time + timings.init;

    let finalize_start = Instant::now();
    let mut stats = Vec::with_capacity(tables.len());
    for (id, table) in ids().zip(tables) {
        let (egt, values, maxima, invalid) = table.into_parts();
        println!("{id}: finalizing outcomes, statistics and compression...");
        stats.push(finalization::finalize_table(
            base, id, &egt, values, maxima, invalid,
        )?);
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
    use crate::selection::TableSelector;
    use crate::storage::Location;
    use crate::table_id::TableId;
    use crate::{EgtError, Tablebase};

    #[test]
    fn parallel_missing_dependency_returns_error_without_generating() {
        let dir = crate::TestDir::new("parallel_missing_dependency");
        let mut generator = Tablebase::new(&dir.0).unwrap();
        generator.set_threads(4);
        let result = generator.generate(&["KQ_K"]);
        assert!(
            matches!(result, Err(EgtError::DependencyUnavailable { dependency, source })
            if dependency == "K_K" && matches!(*source, EgtError::FileNotFound(_)))
        );
        for name in ["K_K", "KQ_K", "K_KQ"] {
            let id = TableId::parse(name).unwrap();
            assert!(!dir.0.join(id.relative_path("ggegt")).exists());
        }
    }

    #[test]
    fn three_piece_generation_matches_established_outcome_counts() {
        let dir = crate::TestDir::new("three_piece_generation");
        let location = Location::parse(&dir.0).unwrap();
        let mut generator = Tablebase::new(&dir.0).unwrap();
        generator.set_threads(2);
        generator.generate(&["2", "3"]).unwrap();

        // Index range, wins, draws, losses, excluded indices.
        for (material, expected) in [
            ("K_K", [462, 0, 462, 0, 0]),
            ("KQ_K", [28644, 18081, 0, 0, 10563]),
            ("K_KQ", [28644, 0, 2896, 25160, 588]),
            ("KR_K", [28644, 21959, 0, 0, 6685]),
            ("K_KR", [28644, 0, 2796, 25260, 588]),
            ("KB_K", [28644, 0, 24178, 0, 4466]),
            ("K_KB", [28644, 0, 28056, 0, 588]),
            ("KN_K", [28644, 0, 25750, 0, 2894]),
            ("K_KN", [28644, 0, 28056, 0, 588]),
            ("KP_K", [93744, 62480, 19184, 0, 12080]),
            ("K_KP", [93744, 0, 35210, 48802, 9732]),
        ] {
            let mut totals = [0; 5];
            for id in TableSelector::parse(material).unwrap().resolve().unwrap() {
                let stats = crate::stats::read_stats(&location, &id).unwrap();
                let t = stats.totals;
                let excluded = t.invalid_opposite_check
                    + t.invalid_ep_squares_occupied
                    + t.invalid_illegal_ep_capture
                    + t.redundant;
                for (total, count) in
                    totals
                        .iter_mut()
                        .zip([stats.index_range, t.win, t.draw, t.loss, excluded])
                {
                    *total += count;
                }
            }
            assert_eq!(totals, expected, "{material}");
        }
    }
}
