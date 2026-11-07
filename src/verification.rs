use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

use rayon::prelude::*;
use shakmaty::{Chess, Color, MoveList, Position};

use crate::egt_file::SharedFileCache;
use crate::storage::{FRAME_SIZE, Location};
use crate::{ConversionType, DtcOutcome, Egt, EgtError, EgtResult, resolve_selectors};

/// Checks table outcomes against legal successors using a shared frame cache.
///
/// Verification is synchronous and prints fixed progress lines. Async callers
/// should construct, configure and use the verifier in `tokio::task::spawn_blocking`.
/// Input overlays have the same missing-only fallback as [`crate::EgtProber`].
pub struct EgtVerifier {
    cache: SharedFileCache,
    threads: usize,
}

impl EgtVerifier {
    /// Resolves a local tablebase directory or supported object-store URL once.
    pub fn new(path: impl AsRef<Path>) -> EgtResult<Self> {
        Ok(Self::from_location(Location::parse(path)?))
    }

    pub(crate) fn from_location(location: Location) -> Self {
        Self {
            cache: SharedFileCache::new(&location, None, None),
            threads: 0,
        }
    }

    /// Sets verification workers. Zero (the default) uses Rayon's default.
    pub fn with_threads(&mut self, threads: usize) {
        self.threads = threads;
    }

    /// Sets the retained-frame cache budget in bytes and evicts frames if necessary.
    pub fn with_cache_size(&mut self, n: usize) -> EgtResult<()> {
        self.cache.set_cache_size(n)
    }

    /// Resolves an input location, searched first with missing-only fallback to
    /// the base location. A resolution failure leaves the previous setting intact.
    pub fn with_input_path(&mut self, path: impl AsRef<Path>) -> EgtResult<()> {
        self.cache.set_input_path(Some(Location::parse(path)?))
    }

    /// Verifies exactly the tables selected, without implicitly adding twins.
    /// Successor dependencies must be available. Frames are shared and subject
    /// to the configured LRU cache budget.
    pub fn verify_internal_consistency(&self, selector: &str) -> EgtResult<()> {
        self.verify_selected(&[selector])
    }

    /// Verifies the sorted, deduplicated union of selectors without adding twins.
    pub fn verify_selected(&self, selectors: &[&str]) -> EgtResult<()> {
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

    fn verify_tables_in_pool(&self, names: &[String]) -> EgtResult<()> {
        for name in names {
            let file = self.cache.file(name)?;
            let egt = {
                let file = file
                    .read()
                    .map_err(|_| EgtError::Internal("dependency file lock poisoned"))?;
                file.egt().clone()
            };
            verify_egt(&self.cache, name, &egt)?;
            println!(
                "Successfully verified internal consistency of table {}.",
                name
            );
        }
        Ok(())
    }
}

/// Verifies independent blocks of one table using table-local indices.
pub(crate) fn verify_egt(reader: &SharedFileCache, endgame: &str, egt: &Egt) -> EgtResult<()> {
    let tablename = egt.tablename();
    let local_range = egt.index_range();

    println!("{}: verifying {} indices...", tablename, local_range);

    let completed = AtomicUsize::new(0);
    // Blocks divide frames so copying a block never crosses a frame boundary.
    let block_size = 4096.min(FRAME_SIZE);
    let blocks: Vec<_> = (0..local_range)
        .step_by(FRAME_SIZE)
        .flat_map(|frame_start| {
            let frame_end = (frame_start + FRAME_SIZE).min(local_range);
            (frame_start..frame_end)
                .step_by(block_size)
                .map(move |start| (start, (start + block_size).min(frame_end)))
        })
        .collect();
    blocks.into_par_iter().try_for_each(|(start, end)| {
        verify_block(reader, endgame, egt, start, end)?;
        let previous = completed.fetch_add(end - start, AtomicOrdering::Relaxed);
        let done = previous + end - start;
        if done / 10_000_000 != previous / 10_000_000 {
            println!(
                "{}: verified {}/{} indices...",
                tablename, done, local_range
            );
        }
        Ok(())
    })
}

fn verify_block(
    reader: &SharedFileCache,
    endgame: &str,
    egt: &Egt,
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
            let best = best_outcome(reader, &position, &legals)?;
            if outcome != best {
                let details = describe_mismatch(reader, &position, &legals, outcome, best);
                return Err(fail(local_idx, &details));
            }
        }
    }

    Ok(())
}

/// Computes the best outcome reachable from `position` by playing one of
/// `legals`, probing each successor in the tablebases.
fn best_outcome(
    reader: &SharedFileCache,
    position: &Chess,
    legals: &MoveList,
) -> EgtResult<DtcOutcome> {
    let mut best_value: Option<DtcOutcome> = None;

    for m in legals {
        let mut successor_position = position.clone();
        successor_position.play_unchecked(*m);

        let successor_outcome = reader.probe_outcome(&successor_position)?;
        let v_m = successor_outcome.before_move(m);

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
    reader: &SharedFileCache,
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
            let v_m = successor_outcome.before_move(m);
            reason.push_str(&format!(
                "\n  Move: {:?}, Successor Outcome: {:?}, Value: {:?}",
                m, successor_outcome, v_m
            ));
        }
    }
    reason
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::TestDir;

    #[test]
    fn verification_rejects_invalid_outcomes_and_recovers_cache() {
        use crate::PackedOutcome;

        let dir = TestDir::new("verify_invalid_outcome");
        let egt = Egt::from_tablename("K_K").unwrap();
        let index = (0..egt.index_range())
            .find(|&index| egt.position_from_index(index, Color::White).is_some())
            .unwrap();
        let mut verifier = EgtVerifier::new(&dir.0).unwrap();
        for threads in [0, 1, 4] {
            verifier.with_threads(threads);
            for outcome in [
                PackedOutcome::INVALID,
                PackedOutcome::from_u16(8),
                PackedOutcome::DRAW,
            ] {
                // Drop cached readers before replacing their backing artifact.
                verifier.cache.clear().unwrap();
                let file = crate::egt_writer::write_fixture(&dir.0, "K_K", |i| {
                    if i == index {
                        outcome
                    } else {
                        PackedOutcome::DRAW
                    }
                });
                verifier.cache.insert(file).unwrap();
                let result = verifier.verify_internal_consistency("K_K");
                if outcome.is_invalid() {
                    assert!(matches!(result,
                        Err(EgtError::ConsistencyCheckFailed { index: failed, .. }) if failed == index));
                } else if outcome.is_unknown() {
                    assert!(matches!(result,
                        Err(EgtError::CorruptedOutcome { index: failed, value: 8 }) if failed == index));
                } else {
                    result.unwrap();
                }
                assert!(verifier.cache.contains_key("K_K"));
            }
        }
    }
}
