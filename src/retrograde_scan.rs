//! Decremental move counters driven by per-ply table scans: the counter loop
//! of `retrograde.rs`, without its BFS queues (see
//! `scaling_and_parallelization.md`, Section 4.2).
//!
//! Every resolved position stores its distance, so the positions to propagate
//! at ply `p` are exactly the values at ply `p - 1`: the queues only re-derive
//! information that is already in the tables. Per ply `p`, the loop runs:
//! 1. Phase W(p): unmoves from losses at ply `p - 1` mark their unknown
//!    predecessors as wins at ply `p`.
//! 2. Phase L(p), in three sub-phases for Checkmate, Capture and Promotion (in
//!    this order): unmoves from wins of that conversion type at ply `p - 1`
//!    decrement the counters of their unknown predecessors. A counter reaching
//!    zero gives a loss at ply `p`, with the conversion type of the sub-phase.
//!
//! This makes exactly the same updates as the counter loop, grouped
//! differently, so the output is byte-identical:
//! - A phase only scans values at ply `p - 1` and only writes values at ply
//!   `p` (or counters), so it never sees its own writes.
//! - A win keeps the preferred conversion type among the losses at `p - 1`
//!   reaching it (Checkmate > Capture > Promotion, see
//!   `RetrogradeSolver::mark_win`). The counter loop gets the same result by
//!   processing the Checkmate queue first.
//! - A loss gets the conversion type of the decrement that brings its counter
//!   to zero. The counter loop processes its queues in Checkmate, Capture,
//!   Promotion order, and the sub-phases reproduce this order.
//!
//! Conversions are resolved during initialization, where a position only
//! updates itself: a winning conversion gives a win at ply 1, and losing
//! conversions are subtracted from the position's own counter directly (a
//! counter reaching zero gives a loss at ply 1).

use shakmaty::{Color, Position};
use crate::ConversionType;
use crate::egt_file::MaybeDtcOutcome;
use crate::error::EgtResult;
use crate::retrograde::{
    DependencyCache, EgtHandle, PairResult, RetrogradeSolver, better_loss_ct, is_mirrored, outcome_ct, ply,
    quiet_unmoves, scan_table, symmetry_adjusted_move_counter, table_name,
};

/// Number of positions of a table resolved at a given ply.
#[derive(Clone, Copy, Default)]
struct Found {
    wins: usize,
    losses: usize,
}

#[derive(Default)]
struct ScanCounters {
    scans: usize,
    propagated: usize,
}

struct InitCounts {
    checkmates: usize,
    stalemates: usize,
    /// Wins and losses at ply 1, resolved by conversions.
    found: Found,
}

fn initialize_table(
    solver: &mut RetrogradeSolver,
    table: EgtHandle,
    dep_cache: &mut DependencyCache,
) -> EgtResult<InitCounts> {
    let (size, pawnless) = {
        let egt = &solver.files[table.file_idx].egts[table.egt_idx];
        (egt.index_range(), egt.is_pawnless())
    };
    let mut counts = InitCounts { checkmates: 0, stalemates: 0, found: Found::default() };

    for idx in 0..size {
        let position_opt = solver.files[table.file_idx].egts[table.egt_idx].position_from_index(idx, Color::White);

        if (idx+1) % 10000000 == 0 {
            println!("Scanned {}/{} indexes...", idx+1, size);
        }

        let Some(position) = position_opt else {
            solver.write_outcome(table, idx, MaybeDtcOutcome::INVALID);
            continue;
        };
        let legals = position.legal_moves();

        if legals.is_empty() {
            if position.is_check() {
                solver.write_outcome(table, idx, MaybeDtcOutcome::new_loss(ConversionType::Checkmate, 0));
                counts.checkmates += 1;
            } else {
                solver.write_outcome(table, idx, MaybeDtcOutcome::DRAW);
                counts.stalemates += 1;
            }
            continue;
        }

        let mut counter = if pawnless {
            symmetry_adjusted_move_counter(&position)
        } else {
            legals.len() as u16
        };
        let mut win_conv: Option<ConversionType> = None;
        let mut loss_conv: Option<ConversionType> = None;

        for m in legals {
            if m.is_capture() || m.is_promotion() {
                let mut successor_position = position.clone();
                successor_position.play_unchecked(m);
                let dep_endgame = crate::get_endgame(&successor_position);
                let dep_outcome = dep_cache.get_or_load(&dep_endgame)?.probe(&successor_position)?;

                let ct = if m.is_capture() { ConversionType::Capture } else { ConversionType::Promotion };
                if dep_outcome.is_loss() {
                    win_conv = Some(win_conv.map_or(ct, |w| w.max(ct)));
                } else if dep_outcome.is_win() {
                    // Conversion moves always count 1 in the counter (the
                    // symmetry adjustment only applies to quiet king moves).
                    counter -= 1;
                    loss_conv = Some(better_loss_ct(loss_conv, ct));
                }
            }
        }

        let outcome = if let Some(ct) = win_conv {
            // A later checkmate at ply 1 may still upgrade the conversion type.
            counts.found.wins += 1;
            MaybeDtcOutcome::new_win(ct, 1)
        } else if counter == 0 {
            // All moves are conversions to won positions.
            counts.found.losses += 1;
            MaybeDtcOutcome::new_loss(loss_conv.expect("a loss by conversion has a losing conversion"), 1)
        } else {
            MaybeDtcOutcome::new_unknown(counter)
        };
        solver.write_outcome(table, idx, outcome);
    }

    Ok(counts)
}

pub(crate) fn solve_pair(
    solver: &mut RetrogradeSolver,
    table_a: EgtHandle,
    table_b: EgtHandle,
    dep_cache: &mut DependencyCache,
) -> EgtResult<PairResult> {
    let init_start = std::time::Instant::now();

    // (table, twin, mirrored, slot of the table, slot of the twin), where the
    // slot indexes the per-table counts (0 for A, 1 for B).
    let tables: Vec<(EgtHandle, EgtHandle, bool, usize, usize)> = if table_a == table_b {
        vec![(table_a, table_a, is_mirrored(solver, table_a, table_a), 0, 0)]
    } else {
        vec![
            (table_a, table_b, is_mirrored(solver, table_a, table_b), 0, 1),
            (table_b, table_a, is_mirrored(solver, table_b, table_a), 1, 0),
        ]
    };

    // Positions resolved at the current ply. Checkmates count as losses at
    // ply 0, so that the first phase W(1) scans them.
    let mut found = [Found::default(); 2];
    let mut found_ply_1 = [Found::default(); 2];
    for &(table, _, _, slot, _) in &tables {
        let counts = initialize_table(solver, table, dep_cache)?;
        println!("{}: Initialized with {} checkmated positions, {} stalemated positions.", table_name(solver, table), counts.checkmates, counts.stalemates);
        found[slot].losses = counts.checkmates;
        found_ply_1[slot] = counts.found;
    }

    let init_time = init_start.elapsed();
    let propagation_start = std::time::Instant::now();

    let mut buf = Vec::new();
    let mut counters = ScanCounters::default();
    let mut max_win = [0u16; 2];
    let mut max_loss = [0u16; 2];

    let mut plies: u16 = 1;
    loop {
        // Positions found at `plies - 1` drive this ply: scans are skipped
        // when a table has no values to propagate.
        let previous = found;
        found = if plies == 1 { found_ply_1 } else { [Found::default(); 2] };

        // Phase W: losses at `plies - 1` make their predecessors wins.
        for &(table, twin, mirrored, slot, twin_slot) in &tables {
            if previous[slot].losses == 0 {
                continue;
            }
            counters.scans += 1;
            scan_table(
                solver,
                table,
                &mut buf,
                |v| v.is_loss() && ply(v) == plies - 1,
                |solver, idx, v| {
                    counters.propagated += 1;
                    let ct = outcome_ct(v);
                    quiet_unmoves(solver, table, twin, idx, mirrored, |solver, pred_idx| {
                        if solver.mark_win(twin, pred_idx, ct, plies) {
                            found[twin_slot].wins += 1;
                        }
                    });
                    Ok(())
                },
            )?;
        }

        // Phase L: wins at `plies - 1` decrement the counters of their
        // predecessors, one conversion type after the other.
        for ct in [ConversionType::Checkmate, ConversionType::Capture, ConversionType::Promotion] {
            let target = MaybeDtcOutcome::new_win(ct, plies - 1);
            for &(table, twin, mirrored, slot, twin_slot) in &tables {
                if previous[slot].wins == 0 {
                    continue;
                }
                counters.scans += 1;
                scan_table(
                    solver,
                    table,
                    &mut buf,
                    |v| v == target,
                    |solver, idx, _| {
                        counters.propagated += 1;
                        quiet_unmoves(solver, table, twin, idx, mirrored, |solver, pred_idx| {
                            if solver.decrement(twin, pred_idx, ct, plies) {
                                found[twin_slot].losses += 1;
                            }
                        });
                        Ok(())
                    },
                )?;
            }
        }

        for &(table, _, _, slot, _) in &tables {
            if found[slot].wins > 0 {
                println!("{}: Found {} winning positions at depth {}", table_name(solver, table), found[slot].wins, plies);
                max_win[slot] = plies;
            }
            if found[slot].losses > 0 {
                println!("{}: Found {} losing positions at depth {}", table_name(solver, table), found[slot].losses, plies);
                max_loss[slot] = plies;
            }
        }

        if found.iter().all(|f| f.wins == 0 && f.losses == 0) {
            break;
        }
        plies += 1;
    }

    println!(
        "{}: scan stats: {} scans, {} positions propagated",
        table_name(solver, table_a),
        counters.scans,
        counters.propagated,
    );

    Ok(PairResult {
        max_win_dtc_a: max_win[0],
        max_loss_dtc_a: max_loss[0],
        max_win_dtc_b: max_win[1],
        max_loss_dtc_b: max_loss[1],
        init_time,
        propagation_time: propagation_start.elapsed(),
    })
}
