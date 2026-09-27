//! Experimental alternative to the decremental-counter loop in `retrograde.rs`:
//! Syzygy-style `CHANGED` candidate flags, forward loss verification and full
//! table sweeps at every ply (no BFS queues).
//!
//! Everything else (indexing, `quiet_unmoves`, dependency probing, statistics,
//! file output) is shared with the counter implementation, so that the two can
//! be compared on equal terms. The output is required to be byte-identical.
//!
//! Per ply `p`, the loop runs:
//! 1. Verification sweep: every position flagged `CHANGED` (because one of its
//!    successors became a win at ply `p-1`) is decoded and its quiet moves are
//!    played forward. If all successors are wins at ply `<= p-1` (conversion
//!    moves were already checked to be losing during initialization), it is a
//!    loss at ply `p`. Otherwise the flag is cleared.
//! 2. Propagation sweep: unmoves from wins at ply `p` flag their unknown
//!    predecessors as `CHANGED`, unmoves from losses at ply `p` mark their
//!    unknown predecessors as wins at ply `p+1`.
//!
//! Conversion types are chosen to match the counter implementation (which is
//! also what `EgtProber::verify_internal_consistency` checks): among the moves
//! realizing the distance, a win prefers Checkmate > Capture > Promotion, a
//! loss prefers Promotion > Capture > Checkmate. Since this rule does not depend
//! on processing order, the sweeps can visit indexes in any order.

use shakmaty::{Color, Position};
use crate::ConversionType;
use crate::egt_file::{MaybeDtcOutcome, mirror_horizontally};
use crate::error::{EgtError, EgtResult};
use crate::retrograde::{
    DependencyCache, EgtHandle, PairResult, RetrogradeSolver, better_loss_ct, is_mirrored, outcome_ct, ply,
    quiet_unmoves, scan_table as sweep, table_name,
};

// Layout of the 13 upper bits of an 'unknown' value (the low 3 bits are 0b000).
// `UNKNOWN` is always set, so that `MaybeDtcOutcome::is_unknown()` holds.
const UNKNOWN: u16 = 1 << 3;
// Candidate loss, to be verified in the next verification sweep.
const CHANGED: u16 = 1 << 4;
// Some conversion move does not lose: the position can never be a loss.
const NO_LOSS: u16 = 1 << 5;
// Best conversion type among the conversion moves reaching a lost dependency
// position (i.e. the position is a win at ply 1).
const WIN_CONV_SHIFT: u16 = 6;
// Conversion type to use for a loss at ply 1 (all moves are losing conversions).
const LOSS_CONV_SHIFT: u16 = 8;
const CT_MASK: u16 = 0b11;

fn encode_ct(ct: Option<ConversionType>) -> u16 {
    match ct {
        None => 0,
        Some(ConversionType::Checkmate) => 1,
        Some(ConversionType::Capture) => 2,
        Some(ConversionType::Promotion) => 3,
    }
}

fn decode_ct(bits: u16) -> Option<ConversionType> {
    match bits & CT_MASK {
        1 => Some(ConversionType::Checkmate),
        2 => Some(ConversionType::Capture),
        3 => Some(ConversionType::Promotion),
        _ => None,
    }
}


#[derive(Default)]
struct SweepCounters {
    sweeps: usize,
    candidates: usize,
    confirmed: usize,
    moves_checked: usize,
}


fn initialize_table(
    solver: &mut RetrogradeSolver,
    table: EgtHandle,
    dep_cache: &mut DependencyCache,
) -> EgtResult<(usize, usize)> {
    let size = solver.files[table.file_idx].egts[table.egt_idx].index_range();

    let mut checkmate_count = 0;
    let mut stalemate_count = 0;

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
                checkmate_count += 1;
            } else {
                solver.write_outcome(table, idx, MaybeDtcOutcome::DRAW);
                stalemate_count += 1;
            }
            continue;
        }

        let mut quiet_moves = 0;
        let mut win_conv: Option<ConversionType> = None;
        let mut loss_conv: Option<ConversionType> = None;
        let mut no_loss = false;

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
                    loss_conv = Some(better_loss_ct(loss_conv, ct));
                } else {
                    no_loss = true;
                }
            } else {
                quiet_moves += 1;
            }
        }

        let mut bits = UNKNOWN
            | (encode_ct(win_conv) << WIN_CONV_SHIFT)
            | (encode_ct(loss_conv) << LOSS_CONV_SHIFT);
        if no_loss || win_conv.is_some() {
            bits |= NO_LOSS;
        } else if quiet_moves == 0 {
            // All moves are losing conversions: candidate loss at ply 1.
            bits |= CHANGED;
        }
        solver.write_outcome(table, idx, MaybeDtcOutcome::from_u16(bits));
    }

    Ok((checkmate_count, stalemate_count))
}

// Forward verification of all `CHANGED` candidates in `table` at ply `plies`.
// Returns the number of confirmed losses.
fn verify_candidates(
    solver: &mut RetrogradeSolver,
    table: EgtHandle,
    twin: EgtHandle,
    mirrored: bool,
    plies: u16,
    buf: &mut Vec<(usize, MaybeDtcOutcome)>,
    counters: &mut SweepCounters,
) -> EgtResult<usize> {
    let mut losses = 0;
    counters.sweeps += 1;
    sweep(
        solver,
        table,
        buf,
        |v| v.is_unknown() && v.to_u16() & CHANGED != 0,
        |solver, idx, v| {
            counters.candidates += 1;
            let position = solver.files[table.file_idx].egts[table.egt_idx]
                .position_from_index(idx, Color::White)
                .ok_or(EgtError::Internal("candidate loss has an invalid index"))?;

            // Conversion moves are known to be losing (otherwise `NO_LOSS`
            // would be set). They only determine the conversion type at ply 1.
            let mut best = if plies == 1 { decode_ct(v.to_u16() >> LOSS_CONV_SHIFT) } else { None };
            let mut refuted = false;
            for m in position.legal_moves() {
                if m.is_capture() || m.is_promotion() {
                    continue;
                }
                counters.moves_checked += 1;
                let mut successor = position.clone();
                successor.play_unchecked(m);
                if mirrored {
                    successor = mirror_horizontally(&successor)
                        .expect("mirrored successor position must be legal");
                }
                let succ_idx = solver.files[twin.file_idx].egts[twin.egt_idx].position_to_index(&successor);
                let sv = solver.read_outcome(twin, succ_idx);
                if sv.is_win() && ply(sv) < plies {
                    if ply(sv) == plies - 1 {
                        best = Some(better_loss_ct(best, outcome_ct(sv)));
                    }
                } else {
                    refuted = true;
                    break;
                }
            }

            if refuted {
                solver.write_outcome(table, idx, MaybeDtcOutcome::from_u16(v.to_u16() & !CHANGED));
            } else {
                let ct = best.ok_or(EgtError::Internal("confirmed loss without a successor at the previous ply"))?;
                solver.write_outcome(table, idx, MaybeDtcOutcome::new_loss(ct, plies));
                counters.confirmed += 1;
                losses += 1;
            }
            Ok(())
        },
    )?;
    Ok(losses)
}

// Propagation sweep over `table` at ply `plies`: wins at `plies` flag their
// predecessors in `twin` as candidate losses, losses at `plies` mark their
// predecessors in `twin` as wins at `plies + 1`. At ply 0, also resolves the
// wins by conversion found during initialization. Returns the number of new
// wins in `table` and in `twin`.
fn propagate(
    solver: &mut RetrogradeSolver,
    table: EgtHandle,
    twin: EgtHandle,
    mirrored: bool,
    plies: u16,
    buf: &mut Vec<(usize, MaybeDtcOutcome)>,
    counters: &mut SweepCounters,
) -> EgtResult<(usize, usize)> {
    let mut wins_table = 0;
    let mut wins_twin = 0;
    counters.sweeps += 1;
    sweep(
        solver,
        table,
        buf,
        |v| {
            ((v.is_win() || v.is_loss()) && ply(v) == plies)
                || (plies == 0 && v.is_unknown() && (v.to_u16() >> WIN_CONV_SHIFT) & CT_MASK != 0)
        },
        |solver, idx, v| {
            if v.is_win() {
                quiet_unmoves(solver, table, twin, idx, mirrored, |solver, pred_idx| {
                    let pv = solver.read_outcome(twin, pred_idx);
                    if pv.is_unknown() && pv.to_u16() & (NO_LOSS | CHANGED) == 0 {
                        solver.write_outcome(twin, pred_idx, MaybeDtcOutcome::from_u16(pv.to_u16() | CHANGED));
                    }
                });
            } else if v.is_loss() {
                let ct = outcome_ct(v);
                quiet_unmoves(solver, table, twin, idx, mirrored, |solver, pred_idx| {
                    if solver.mark_win(twin, pred_idx, ct, plies + 1) {
                        wins_twin += 1;
                    }
                });
            } else {
                let ct = decode_ct(v.to_u16() >> WIN_CONV_SHIFT)
                    .ok_or(EgtError::Internal("missing conversion type for win by conversion"))?;
                if solver.mark_win(table, idx, ct, 1) {
                    wins_table += 1;
                }
            }
            Ok(())
        },
    )?;
    Ok((wins_table, wins_twin))
}

pub(crate) fn solve_pair(
    solver: &mut RetrogradeSolver,
    table_a: EgtHandle,
    table_b: EgtHandle,
    dep_cache: &mut DependencyCache,
) -> EgtResult<PairResult> {
    let init_start = std::time::Instant::now();

    let (checkmates, stalemates) = initialize_table(solver, table_a, dep_cache)?;
    println!("{}: Initialized with {} checkmated positions, {} stalemated positions.", table_name(solver, table_a), checkmates, stalemates);
    if table_a != table_b {
        let (checkmates, stalemates) = initialize_table(solver, table_b, dep_cache)?;
        println!("{}: Initialized with {} checkmated positions and {} stalemated positions.", table_name(solver, table_b), checkmates, stalemates);
    }

    let init_time = init_start.elapsed();
    let propagation_start = std::time::Instant::now();

    // (table, twin, mirrored, slot of the table, slot of the twin), where the
    // slot indexes the per-table counters (0 for A, 1 for B).
    let tables: Vec<(EgtHandle, EgtHandle, bool, usize, usize)> = if table_a == table_b {
        vec![(table_a, table_a, is_mirrored(solver, table_a, table_a), 0, 0)]
    } else {
        vec![
            (table_a, table_b, is_mirrored(solver, table_a, table_b), 0, 1),
            (table_b, table_a, is_mirrored(solver, table_b, table_a), 1, 0),
        ]
    };

    let mut buf = Vec::new();
    let mut counters = SweepCounters::default();
    let mut max_win = [0u16; 2];
    let mut max_loss = [0u16; 2];

    // Ply 1 wins: predecessors of checkmates, and winning conversions.
    let mut wins = [0usize; 2];
    for &(table, twin, mirrored, slot, twin_slot) in &tables {
        let (w_table, w_twin) = propagate(solver, table, twin, mirrored, 0, &mut buf, &mut counters)?;
        wins[slot] += w_table;
        wins[twin_slot] += w_twin;
    }

    let mut plies: u16 = 1;
    loop {
        let mut losses = [0usize; 2];
        for &(table, twin, mirrored, slot, _) in &tables {
            losses[slot] += verify_candidates(solver, table, twin, mirrored, plies, &mut buf, &mut counters)?;
        }

        for &(table, _, _, slot, _) in &tables {
            if wins[slot] > 0 {
                println!("{}: Found {} winning positions at depth {}", table_name(solver, table), wins[slot], plies);
                max_win[slot] = plies;
            }
            if losses[slot] > 0 {
                println!("{}: Found {} losing positions at depth {}", table_name(solver, table), losses[slot], plies);
                max_loss[slot] = plies;
            }
        }

        if wins.iter().sum::<usize>() == 0 && losses.iter().sum::<usize>() == 0 {
            break;
        }

        wins = [0; 2];
        for &(table, twin, mirrored, slot, twin_slot) in &tables {
            let (w_table, w_twin) = propagate(solver, table, twin, mirrored, plies, &mut buf, &mut counters)?;
            wins[slot] += w_table;
            wins[twin_slot] += w_twin;
        }
        plies += 1;
    }

    println!(
        "{}: sweep stats: {} sweeps, {} candidates, {} confirmed losses ({:.1}%), {:.2} quiet moves checked per candidate",
        table_name(solver, table_a),
        counters.sweeps,
        counters.candidates,
        counters.confirmed,
        100.0 * counters.confirmed as f64 / counters.candidates.max(1) as f64,
        counters.moves_checked as f64 / counters.candidates.max(1) as f64,
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
