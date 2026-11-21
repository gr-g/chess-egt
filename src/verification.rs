use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

use rayon::prelude::*;
use shakmaty::{Chess, Color, Move, Position};

use crate::egt_file::BatchReader;
use crate::report::format_timing;
use crate::table_id::TableId;
use crate::{ConversionType, DtcOutcome, Egt, EgtError, EgtResult, Tablebase, resolve_selectors};

impl Tablebase {
    /// Verifies the sorted, deduplicated selection against legal successors.
    /// Each selected table, its quiet twin, and its direct conversion dependencies
    /// must fit in memory together. Twins are loaded but not implicitly verified.
    pub fn verify(&self, selectors: &[&str]) -> EgtResult<()> {
        let start = std::time::Instant::now();
        let ids = resolve_selectors(selectors)?;
        let pool = self.pool("failed to build verification thread pool")?;
        let (unique_positions, index_range) = pool.install(|| self.verify_tables_in_pool(&ids))?;
        println!(
            "Verified {} tables: {} unique positions in {}.",
            ids.len(),
            unique_positions,
            format_timing(start.elapsed(), index_range, pool.current_num_threads())
        );
        Ok(())
    }

    fn verify_tables_in_pool(&self, ids: &[TableId]) -> EgtResult<(usize, usize)> {
        let mut unique_positions = 0;
        let mut index_range = 0;
        for id in ids {
            let file = self.source().open(id)?;
            let mut dependencies = id.dependencies();
            dependencies.push(id.twin().0);
            dependencies.retain(|dependency| dependency != id);
            dependencies.sort();
            dependencies.dedup();
            let mut reader = BatchReader::open(self.source(), &dependencies)?;
            let frames = file.read_all()?;
            reader.insert(file, frames);
            println!(
                "{}: loaded {} bytes of dependencies.",
                id,
                reader.resident_bytes()
            );
            let egt = reader.egt(id);
            unique_positions += verify_egt(&reader, egt)?;
            index_range += egt.index_range();
            println!(
                "Successfully verified internal consistency of table {}.",
                id
            );
        }
        Ok((unique_positions, index_range))
    }
}

/// Verifies independent blocks and returns the number of valid, unique positions.
fn verify_egt(reader: &BatchReader, egt: &Egt) -> EgtResult<usize> {
    let tablename = egt.id();
    let local_range = egt.index_range();

    println!("{}: verifying {} indices...", tablename, local_range);

    let completed = AtomicUsize::new(0);
    // Keep each expanded block within a single frame.
    let frame_size = egt.frame_size();
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
    blocks
        .into_par_iter()
        .map(|(start, end)| {
            let unique_positions = verify_block(reader, egt, start, end)?;
            let previous = completed.fetch_add(end - start, AtomicOrdering::Relaxed);
            let done = previous + end - start;
            if done / 10_000_000 != previous / 10_000_000 {
                println!(
                    "{}: verified {}/{} indices...",
                    tablename, done, local_range
                );
            }
            Ok(unique_positions)
        })
        .try_reduce(|| 0, |a, b| Ok(a + b))
}

fn verify_block(reader: &BatchReader, egt: &Egt, start: usize, end: usize) -> EgtResult<usize> {
    let tablename = egt.id();
    let outcomes = reader.read_chunk(&tablename, start, end)?;
    // Keep the table identity and local index in verification diagnostics.
    let fail = |local_idx: usize, reason: &str| EgtError::ConsistencyCheckFailed {
        endgame: tablename.to_string(),
        index: local_idx,
        reason: format!("table {}, local index {}: {}", tablename, local_idx, reason),
    };

    let mut unique_positions = 0;
    for (relative_idx, outcome_maybe) in outcomes.iter().copied().enumerate() {
        let local_idx = start + relative_idx;
        let position = match egt.position_from_index(local_idx, Color::White) {
            Ok(position) => position,
            Err(_) if outcome_maybe.is_invalid() => continue,
            Err(_) => return Err(fail(local_idx, "index is invalid but outcome is not invalid")),
        };

        if outcome_maybe.is_invalid() {
            return Err(fail(local_idx, "index is valid but outcome is invalid"));
        }

        // Verify redundant slots too, but count unique positions as generation does.
        if !egt
            .diagonal_symmetric(local_idx)
            .is_some_and(|other| local_idx > other)
        {
            unique_positions += 1;
        }
        let outcome = DtcOutcome::try_from(outcome_maybe).map_err(|value| {
            EgtError::CorruptedOutcome {
                value,
                index: local_idx,
            }
        })?;
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
            let moves: Vec<_> = legals
                .into_iter()
                .map(|m| {
                    let mut successor = position.clone();
                    successor.play_unchecked(m);
                    Ok((m, reader.probe_outcome(&successor)?))
                })
                .collect::<EgtResult<_>>()?;
            let best = moves
                .iter()
                .map(|(m, successor)| successor.before_move(m))
                .max()
                .expect("nonempty legal moves");
            if outcome != best {
                let details = describe_mismatch(&position, &moves, outcome, best);
                return Err(fail(local_idx, &details));
            }
        }
    }

    Ok(unique_positions)
}


/// Builds a human-readable diagnostic for a mismatch between the outcome
/// stored in the file and the best outcome computed from the legal moves.
fn describe_mismatch(
    position: &Chess,
    moves: &[(Move, DtcOutcome)],
    outcome: DtcOutcome,
    best: DtcOutcome,
) -> String {
    let mut reason = format!(
        "consistency check failed.\nPosition: {:?}\nOutcome in file: {:?}\nBest outcome from legal moves: {:?}",
        position, outcome, best
    );
    reason.push_str("\nLegal moves and their outcomes:");
    for (m, successor_outcome) in moves {
        reason.push_str(&format!(
            "\n  Move: {:?}, Successor Outcome: {:?}, Value: {:?}",
            m,
            successor_outcome,
            successor_outcome.before_move(m)
        ));
    }
    reason
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TestDir;

    #[test]
    fn verification_counts_only_source_positions() {
        let dir = TestDir::new("verify_position_counts");
        let mut generator = crate::Tablebase::new(&dir.0).unwrap();
        generator.set_threads(2);
        generator.generate(&["K_K"]).unwrap();
        let stats = generator.generate(&["KQ_K"]).unwrap();
        let source = stats
            .iter()
            .find(|stats| stats.tablename == "KQ_K")
            .unwrap();
        let counts = generator
            .verify_tables_in_pool(&resolve_selectors(&["KQ_K"]).unwrap())
            .unwrap();
        assert_eq!(counts, (source.totals.unique_positions, source.index_range));
    }

    #[test]
    fn verification_rejects_invalid_outcomes_and_recovers() {
        use crate::outcome::PackedOutcome;

        let dir = TestDir::new("verify_invalid_outcome");
        let egt = Egt::new(&crate::TableId::parse("K_K").unwrap()).unwrap();
        let index = (0..egt.index_range())
            .find(|&index| egt.position_from_index(index, Color::White).is_ok())
            .unwrap();
        let mut verifier = Tablebase::new(&dir.0).unwrap();
        for threads in [0, 1, 4] {
            verifier.set_threads(threads);
            for outcome in [
                PackedOutcome::INVALID,
                PackedOutcome::from_u16(8),
                PackedOutcome::DRAW,
            ] {
                crate::egt_file::writer::write_fixture(&dir.0, "K_K", |i| {
                    if i == index {
                        outcome
                    } else {
                        PackedOutcome::DRAW
                    }
                });

                let result = verifier.verify(&["K_K"]);
                if outcome.is_invalid() {
                    assert!(matches!(result,
                        Err(EgtError::ConsistencyCheckFailed { index: failed, .. }) if failed == index));
                } else if outcome.is_unknown() {
                    assert!(matches!(result,
                        Err(EgtError::CorruptedOutcome { index: failed, value: 8 }) if failed == index));
                } else {
                    result.unwrap();
                }
            }
        }
    }
}
