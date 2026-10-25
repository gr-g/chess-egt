//! Retrograde analysis: decremental move counters driven by per-ply table
//! scans (see `scaling_and_parallelization.md`, Section 4.2).
//!
//! Every unknown position stores the number of its moves not yet known to
//! lose (see `README.md`, 6.2). Every resolved position stores its distance, so
//! the positions to propagate at ply `p` are exactly the values at ply `p - 1`,
//! found by scanning the tables. A per-block summary (`PlySummary`) lets the
//! scans skip the blocks that cannot contain values of the scanned ply. Per
//! ply `p`, the loop runs:
//! 1. Phase W(p): unmoves from losses at ply `p - 1` mark their unknown
//!    predecessors as wins at ply `p`.
//! 2. Phase L(p), in three sub-phases for Checkmate, Capture and Promotion (in
//!    this order): unmoves from wins of that conversion type at ply `p - 1`
//!    decrement the counters of their unknown predecessors. A counter reaching
//!    zero gives a loss at ply `p`, with the conversion type of the sub-phase.
//!
//! The output does not depend on the order in which a phase visits positions:
//! - A phase only scans values at ply `p - 1` and only writes values at ply
//!   `p` (or counters), so it never sees its own writes.
//! - A win keeps the preferred conversion type among the losses at `p - 1`
//!   reaching it (Checkmate > Capture > Promotion, see
//!   `WorkingPair::mark_win`).
//! - A loss gets the conversion type of the last sub-phase that decrements its
//!   counter, i.e. the preference Promotion > Capture > Checkmate among the
//!   wins at `p - 1` reaching it.
//!
//! Conversions are resolved during initialization, where a position only
//! updates itself: a winning conversion gives a win at ply 1, and losing
//! conversions are subtracted from the position's own counter directly (a
//! counter reaching zero gives a loss at ply 1).
//!
//! Rayon parallelizes disjoint initialization chunks and propagation blocks.
//! Joined scans provide phase barriers; relaxed atomic CAS updates and summary
//! fetch-max operations are sufficient within each phase. Initialization lazily
//! loads existing dependency tables through a shared reader.
//!
//! The solver holds only the logical table pair and its working arrays. After
//! propagation joins, each exact table is finalized in bounded parallel frame
//! batches and published separately. Generation uses the caller's Rayon pool
//! and shared dependency reader; it does not generate missing prerequisites.

use shakmaty::{attacks, Bitboard, Color, Chess, EnPassantMode, Move, Position, Role};

use crate::ConversionType;
use crate::egt_file::{MaybeDtcOutcome, EgtFileReader};
use crate::table_id::{PawnKey, TableId};
use crate::error::EgtResult;
use crate::egt::Egt;


use std::sync::atomic::{AtomicU16, Ordering};
use rayon::prelude::*;
use crate::egt_file::SharedDependencyReader;
use crate::storage::Location;

type EgtHandle = usize;

fn table_name(solver: &RetrogradeSolver, handle: EgtHandle) -> &str {
    solver.tables[handle].tablename()
}

struct RetrogradeSolver {
    tables: Vec<Egt>,
}

impl RetrogradeSolver {
    fn new(table_a: Egt, table_b: Option<Egt>) -> Self {
        let mut tables = vec![table_a];
        tables.extend(table_b);
        Self { tables }
    }
}

/// Mutable values for only the current pair; a self-pair has one shared array.
struct WorkingPair {
    values: Vec<Vec<AtomicU16>>,
}

impl WorkingPair {
    fn load(&self, slot: usize, idx: usize) -> MaybeDtcOutcome {
        MaybeDtcOutcome::from_u16(self.values[slot][idx].load(Ordering::Relaxed))
    }

    /// CAS preserves the best same-ply conversion even when workers race.
    fn mark_win(&self, slot: usize, idx: usize, ct: ConversionType, plies: u16) -> bool {
        let cell = &self.values[slot][idx];
        let mut raw = cell.load(Ordering::Relaxed);
        loop {
            let v = MaybeDtcOutcome::from_u16(raw);
            let newly_resolved = v.is_unknown();
            if !newly_resolved && !(v.is_win() && ply(v) == plies && ct > outcome_ct(v)) {
                return false;
            }
            match cell.compare_exchange_weak(raw, MaybeDtcOutcome::new_win(ct, plies).to_u16(), Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return newly_resolved,
                Err(current) => raw = current,
            }
        }
    }

    /// Conversion-type barriers ensure the last decrement has the loss preference.
    fn decrement(&self, slot: usize, idx: usize, ct: ConversionType, plies: u16) -> bool {
        let cell = &self.values[slot][idx];
        let mut raw = cell.load(Ordering::Relaxed);
        loop {
            let v = MaybeDtcOutcome::from_u16(raw);
            if !v.is_unknown() {
                return false;
            }
            let counter = v.get_unknown_counter();
            debug_assert!(counter > 0);
            let next = if counter == 1 {
                MaybeDtcOutcome::new_loss(ct, plies)
            } else {
                MaybeDtcOutcome::new_unknown(counter - 1)
            };
            match cell.compare_exchange_weak(raw, next.to_u16(), Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return counter == 1,
                Err(current) => raw = current,
            }
        }
    }
}

/// Distance (ply) of a win or loss value.
fn ply(v: MaybeDtcOutcome) -> u16 {
    v.to_u16() >> 3
}

/// Conversion type of a win or loss value.
fn outcome_ct(v: MaybeDtcOutcome) -> ConversionType {
    match v.to_u16() & 0b110 {
        0b010 => ConversionType::Checkmate,
        0b100 => ConversionType::Capture,
        _ => ConversionType::Promotion,
    }
}

// Preference among conversion types for a loss (Promotion > Capture > Checkmate),
// the reverse of the win preference given by `ConversionType`'s `Ord`.
fn loss_rank(ct: ConversionType) -> u8 {
    match ct {
        ConversionType::Checkmate => 0,
        ConversionType::Capture => 1,
        ConversionType::Promotion => 2,
    }
}

/// The preferred conversion type for a loss among `a` (if any) and `b`.
fn better_loss_ct(a: Option<ConversionType>, b: ConversionType) -> ConversionType {
    match a {
        Some(a) if loss_rank(a) >= loss_rank(b) => a,
        _ => b,
    }
}

/// Number of positions per pair-local summary block.
const SUMMARY_BLOCK_SIZE: usize = 4096;

/// Per local block, an upper bound on the plies of its resolved values.
struct PlySummary {
    size: usize,
    last_ply: Vec<AtomicU16>,
}

impl PlySummary {
    /// Initialization resolves values only at plies 0 and 1.
    fn new(size: usize) -> Self {
        Self { size, last_ply: (0..size.div_ceil(SUMMARY_BLOCK_SIZE)).map(|_| AtomicU16::new(1)).collect() }
    }

    fn record(&self, local_index: usize, plies: u16) {
        let last = &self.last_ply[local_index / SUMMARY_BLOCK_SIZE];
        if last.load(Ordering::Relaxed) < plies {
            last.fetch_max(plies, Ordering::Relaxed);
        }
    }

    fn blocks_with(&self, plies: u16) -> Vec<(usize, usize)> {
        self.last_ply.iter().enumerate()
            .filter(|(_, last)| last.load(Ordering::Relaxed) >= plies)
            .map(|(b, _)| {
                let start = b * SUMMARY_BLOCK_SIZE;
                (start, (start + SUMMARY_BLOCK_SIZE).min(self.size))
            })
            .collect()
    }

    fn num_blocks(&self) -> usize {
        self.last_ply.len()
    }
}

/// Collect each block's matches before updates, including for a self-pair.
fn scan_table<S, F>(
    working: &WorkingPair,
    slot: usize,
    blocks: &[(usize, usize)],
    select: S,
    process: F,
) -> Found
where
    S: Fn(MaybeDtcOutcome) -> bool + Sync,
    F: Fn(&WorkingPair, usize, MaybeDtcOutcome, &mut Found) + Sync,
{
    blocks.par_iter().map(|&(start, end)| {
        let matches: Vec<_> = (start..end).filter_map(|idx| {
            let v = working.load(slot, idx);
            select(v).then_some((idx, v))
        }).collect();
        let mut found = Found::default();
        for (idx, v) in matches {
            process(working, idx, v, &mut found);
        }
        found
    }).reduce(Found::default, Found::merge)
}


// Whether both kings stand on one of the two long diagonals. Kings on the h1-a8
// diagonal end up on the a1-h8 diagonal after canonicalization, so both
// diagonals have to be checked.
fn kings_on_long_diagonal(kings: Bitboard) -> bool {
    kings.into_iter().all(|sq| sq.rank().to_usize() == sq.file().to_usize())
        || kings.into_iter().all(|sq| sq.rank().to_usize() == 7 - sq.file().to_usize())
}

fn both_kings_on_diagonal(position: &Chess) -> bool {
    kings_on_long_diagonal(position.board().by_role(Role::King))
}

// Derive the reflection from the twin transformation, not from material counts.
// This is computed once per table pair rather than per unmove.
fn is_mirrored(table: &Egt, twin: &Egt) -> bool {
    let (key, reflected) = PawnKey::from_elements(&table.elements).twin().canonicalized();
    debug_assert_eq!(key, PawnKey::from_elements(&twin.elements));
    reflected
}

/// Quiet predecessors of a pawnless tablebase position, without castling rights.
fn pawnless_quiet_unmoves(position: &Chess, mut f: impl FnMut(Chess)) {
    debug_assert!(position.board().pawns().is_empty());
    debug_assert!(position.castles().castling_rights().is_empty());

    let occupied = position.board().occupied();
    let mut base_setup = position.to_setup(EnPassantMode::Always);
    base_setup.turn = !position.turn();

    for to in position.board().by_color(base_setup.turn) {
        let piece = position.board().piece_at(to).expect("occupied square must have a piece");
        let origins = attacks::attacks(to, piece, occupied) & !occupied;
        for from in origins {
            let mut setup = base_setup.clone();
            setup.board.discard_piece_at(to);
            setup.board.set_piece_at(from, piece);

            if let Ok(predecessor) = setup.position::<Chess>(position.castles().mode()) {
                f(predecessor);
            }
        }
    }
}

/// Quiet predecessors of a pawnful tablebase position, without castling rights.
fn pawnful_quiet_unmoves(position: &Chess, mut f: impl FnMut(Chess)) {
    use shakmaty::{Rank, Square};

    debug_assert!(position.board().pawns().any());
    debug_assert!(position.castles().castling_rights().is_empty());

    let occupied = position.board().occupied();
    let us = !position.turn();
    let backward = if us == Color::White { -8 } else { 8 };
    let mut base_setup = position.to_setup(EnPassantMode::Always);
    base_setup.turn = us;
    base_setup.ep_square = None;
    let mut emit = |from: Square, to: Square| {
        let mut setup = base_setup.clone();
        let piece = setup.board.piece_at(to).expect("occupied square must have a piece");
        setup.board.discard_piece_at(to);
        setup.board.set_piece_at(from, piece);
        emit_pawnful_predecessors(&setup, &mut f);
    };

    // If there is a legal en-passant move, the previous move can only be a
    // double push.
    if let Some(ep) = position.legal_ep_square() {
        let to = ep.offset(-backward).expect("EP pawn must be on the board");
        let from = ep.offset(backward).expect("EP origin must be on the board");
        if position.board().piece_at(to) == Some(Role::Pawn.of(us))
            && !occupied.contains(from) && !occupied.contains(ep)
        {
            emit(from, to);
        }
        return;
    }

    for to in position.board().by_color(us) {
        let piece = position.board().piece_at(to).expect("occupied square must have a piece");
        if piece.role != Role::Pawn {
            for from in attacks::attacks(to, piece, occupied) & !occupied {
                emit(from, to);
            }
        } else if let Some(from) = to.offset(backward) {
            if occupied.contains(from) {
                continue;
            }
            emit(from, to);
            if to.rank() == us.relative_rank(Rank::Fourth) {
                // The previous move can be a double push, as long as it does
                // not create a legal en-passant.
                let start = from.offset(backward).expect("double-push origin must be on the board");
                if !occupied.contains(start) {
                    let mut successor = base_setup.clone();
                    successor.turn = position.turn();
                    successor.ep_square = Some(from);
                    if successor.position::<Chess>(position.castles().mode())
                        .is_ok_and(|successor| successor.legal_ep_square().is_none())
                    {
                        emit(start, to);
                    }
                }
            }
        }
    }
}

fn emit_pawnful_predecessors(setup: &shakmaty::Setup, f: &mut impl FnMut(Chess)) {
    use shakmaty::{CastlingMode, Rank, Square};

    debug_assert!(setup.ep_square.is_none());
    // Reverse geometry and the valid successor replace forward replay. Each
    // predecessor still needs validation, independently for every EP state.
    if let Ok(predecessor) = setup.clone().position::<Chess>(CastlingMode::Standard) {
        f(predecessor);
    }

    let inactive = !setup.turn;
    let occupied = setup.board.occupied();
    let capturers = setup.board.by_piece(Role::Pawn.of(setup.turn));
    let pushed_pawns = setup.board.by_piece(Role::Pawn.of(inactive))
        & Bitboard::from_rank(inactive.relative_rank(Rank::Fourth));
    for pawn in pushed_pawns {
        let ep = Square::from_coords(pawn.file(), inactive.relative_rank(Rank::Third));
        let start = Square::from_coords(pawn.file(), inactive.relative_rank(Rank::Second));
        if occupied.contains(ep) || occupied.contains(start)
            || (attacks::pawn_attacks(inactive, ep) & capturers).is_empty()
        {
            continue;
        }
        let mut setup_ep = setup.clone();
        setup_ep.ep_square = Some(ep);
        if let Ok(predecessor) = setup_ep.position::<Chess>(CastlingMode::Standard) {
            if predecessor.legal_ep_square() == Some(ep) {
                f(predecessor);
            }
        }
    }
}

pub fn quiet_unmoves<F>(
    table: &Egt,
    twin: &Egt,
    local_index: usize,
    mirrored: bool,
    mut f: F,
) where
    F: FnMut(usize),
{
    let position = table.position_from_index(local_index, Color::White);
    let position = match position {
        Some(s) => s,
        _ => return,
    };

    debug_assert_eq!(mirrored, is_mirrored(table, twin));

    // The diagonal symmetry adjustment below only applies to pawnless tables,
    // and never to a source position that is already on the diagonal (`#p=4`).
    let use_diagonal_symmetry = twin.is_pawnless()
        && !both_kings_on_diagonal(&position);

    let emit = |pred_position: Chess| {
        let pred_idx = if mirrored {
            twin.reflected_position_to_index(&pred_position)
        } else {
            twin.position_to_index(&pred_position)
        };
        f(pred_idx);

        // Let's say a canonical position `p` has `#p=8` if it represents 8 equivalent
        // positions and `#p=4` if it represents 4 equivalent positions (with our choice
        // of canonicalization, `#p=4` positions are positions with both kings on the
        // a1-h8 diagonal). When retrograde propagation from `p'` finds move `p -> p'`
        // with `#p=4` and `#p'=8`, in addition to decrementing the counter for p, the
        // counter for the reflection of `p` along the diagonal should also be decremented
        // (since the symmetric move contributed to the counter for the reflection of `p`
        // but led to a non-canonical position).
        if use_diagonal_symmetry {
            let maybe_reflected_idx = twin.diagonal_symmetric(pred_idx);
            if let Some(reflected_idx) = maybe_reflected_idx {
                // The current index represents 8 positions, while the predecessor index
                // represents 4 positions. Push the diagonal reflection of the predecessor.
                f(reflected_idx);
            }
        }
    };

    if table.is_pawnless() {
        pawnless_quiet_unmoves(&position, emit);
    } else {
        pawnful_quiet_unmoves(&position, emit);
    }
}

fn symmetry_adjusted_move_counter(position: &Chess, legals: &[Move]) -> u16 {
    let kings = position.board().by_role(Role::King);

    // Let's say a canonical position `p` has `#p=8` if it represents 8
    // equivalent positions and `#p=4` if it represents 4 equivalent positions
    // (with our choice of canonicalization, `#p=4` positions are positions
    // with both kings on the a1-h8 diagonal). When initializing the counters,
    // if there is a legal move `p -> p'` with `#p=8` and `#p'=4`, then there
    // is a move (the symmetric along the diagonal) which goes from a non canonical
    // position (the reflection of `p` along the diagonal) to a canonical position
    // (the reflection of `p'` along the diagonal), which will be explored during
    // backward propagation. To account for this, moves `p -> p'` with `#p=8` and
    // `#p'=4` should increment the counter by 2 during initialization.
    if kings_on_long_diagonal(kings) {
        // Already `#p=4`: no successor can require an adjustment.
        return legals.len() as u16;
    }

    let mut counter = 0;
    for m in legals {
        // Whether `#p'=4` depends only on the two king squares, so only a quiet
        // king move can require the adjustment. Deriving the successor's king
        // bitboard directly avoids cloning and replaying the position for every
        // legal move. Captures and promotions lead to simpler tables, for which
        // we don't generate unmoves (they are visited only forward, once during
        // initialization), so they never need an adjustment either.
        let adjust = m.role() == Role::King
            && !m.is_capture()
            && !m.is_promotion()
            && match m.from() {
                Some(from) => kings_on_long_diagonal(
                    (kings ^ Bitboard::from_square(from)) | Bitboard::from_square(m.to()),
                ),
                None => false,
            };
        counter += if adjust { 2 } else { 1 };
    }
    counter
}

/// Number of positions of a table resolved at a given ply.
#[derive(Clone, Copy, Default)]
struct Found {
    wins: usize,
    losses: usize,
}

impl Found {
    fn merge(self, other: Self) -> Self {
        Self { wins: self.wins + other.wins, losses: self.losses + other.losses }
    }

    fn is_empty(self) -> bool {
        self.wins == 0 && self.losses == 0
    }
}

#[derive(Default)]
struct InitCounts {
    checkmates: usize,
    stalemates: usize,
    /// Wins and losses at ply 1, resolved by conversions.
    found: Found,
}

/// Writes the initial value of every position of `table`: invalid, checkmate
/// (loss at ply 0), stalemate, win or loss at ply 1 by conversion, or unknown
/// with its move counter.
fn initialize_table(
    egt: &Egt,
    values: &mut [AtomicU16],
    reader: &SharedDependencyReader,
) -> EgtResult<InitCounts> {
    let (size, pawnless) = (egt.index_range(), egt.is_pawnless());
    debug_assert_eq!(values.len(), size);
    values.par_chunks_mut(SUMMARY_BLOCK_SIZE).enumerate().map(|(block, chunk)| {
        let mut counts = InitCounts::default();
        for (offset, cell) in chunk.iter_mut().enumerate() {
            let idx = block * SUMMARY_BLOCK_SIZE + offset;
            let position_opt = egt.position_from_index(idx, Color::White);


            let Some(position) = position_opt else {
                *cell.get_mut() = MaybeDtcOutcome::INVALID.to_u16();
                continue;
            };
            let legals = position.legal_moves();

            if legals.is_empty() {
                if position.is_check() {
                    *cell.get_mut() = MaybeDtcOutcome::new_loss(ConversionType::Checkmate, 0).to_u16();
                    counts.checkmates += 1;
                } else {
                    *cell.get_mut() = MaybeDtcOutcome::DRAW.to_u16();
                    counts.stalemates += 1;
                }
                continue;
            }

            let mut counter = if pawnless {
                symmetry_adjusted_move_counter(&position, &legals)
            } else {
                legals.len() as u16
            };
            let mut win_conv: Option<ConversionType> = None;
            let mut loss_conv: Option<ConversionType> = None;

            for m in legals {
                if m.is_capture() || m.is_promotion() {
                    let mut successor_position = position.clone();
                    successor_position.play_unchecked(m);
                    let dep_outcome = reader.probe(&successor_position)?;

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
                // A checkmate in 1 found by phase W(1) may still upgrade the
                // conversion type.
                counts.found.wins += 1;
                MaybeDtcOutcome::new_win(ct, 1)
            } else if counter == 0 {
                // All moves are conversions to won positions.
                counts.found.losses += 1;
                MaybeDtcOutcome::new_loss(loss_conv.expect("a loss by conversion has a losing conversion"), 1)
            } else {
                MaybeDtcOutcome::new_unknown(counter)
            };
            *cell.get_mut() = outcome.to_u16();
        }

        Ok(counts)
    }).try_reduce(InitCounts::default, |a, b| Ok(InitCounts {
        checkmates: a.checkmates + b.checkmates,
        stalemates: a.stalemates + b.stalemates,
        found: a.found.merge(b.found),
    }))
}

/// Per-pair output of `solve_pair`.
struct PairResult {
    working: WorkingPair,
    max_win_dtc_a: u16,
    max_loss_dtc_a: u16,
    max_win_dtc_b: u16,
    max_loss_dtc_b: u16,
    init_time: std::time::Duration,
    propagation_time: std::time::Duration,
}

/// Solves the logical tables `table_a` and `table_b` (twins under changing
/// the side to move), leaving the draws as unknown.
fn solve_pair(
    solver: &RetrogradeSolver,
    table_a: EgtHandle,
    table_b: EgtHandle,
    reader: &SharedDependencyReader,
) -> EgtResult<PairResult> {
    let init_start = std::time::Instant::now();
    let egt = |table: EgtHandle| &solver.tables[table];

    // (table, twin, mirrored, slot of the table, slot of the twin), where the
    // slot indexes the per-table counts (0 for A, 1 for B).
    let tables: Vec<(EgtHandle, EgtHandle, bool, usize, usize)> = if table_a == table_b {
        vec![(table_a, table_a, is_mirrored(egt(table_a), egt(table_a)), 0, 0)]
    } else {
        vec![
            (table_a, table_b, is_mirrored(egt(table_a), egt(table_b)), 0, 1),
            (table_b, table_a, is_mirrored(egt(table_b), egt(table_a)), 1, 0),
        ]
    };

    let mut working = WorkingPair {
        values: tables.iter().map(|&(table, ..)| {
            (0..egt(table).index_range()).map(|_| AtomicU16::new(MaybeDtcOutcome::INVALID.to_u16())).collect()
        }).collect(),
    };

    // Checkmates count as losses at ply 0, so that phase W(1) scans them.
    let mut previous = [Found::default(); 2];
    let mut found_by_conversion = [Found::default(); 2];
    for &(table, _, _, slot, _) in &tables {
        let counts = initialize_table(egt(table), &mut working.values[slot], reader)?;
        println!("{}: Initialized with {} checkmated positions, {} stalemated positions.", table_name(solver, table), counts.checkmates, counts.stalemates);
        previous[slot].losses = counts.checkmates;
        found_by_conversion[slot] = counts.found;
    }

    let init_time = init_start.elapsed();
    let propagation_start = std::time::Instant::now();

    let mut max_win = [0u16; 2];
    let mut max_loss = [0u16; 2];

    // Indexed by slot. A symmetric pair has a single table and summary.
    let summaries: Vec<PlySummary> = tables.iter().map(|&(table, ..)| PlySummary::new(egt(table).index_range())).collect();
    // Blocks visited by the scans, and blocks the same scans would visit
    // without the summaries.
    let mut blocks_scanned = 0usize;
    let mut blocks_total = 0usize;

    let mut plies: u16 = 1;
    loop {
        // Positions resolved at `plies`. The scans of a table are skipped
        // when it resolved nothing at `plies - 1`.
        let mut found = if plies == 1 { found_by_conversion } else { [Found::default(); 2] };

        // Phase W: losses at `plies - 1` make their predecessors wins.
        for &(table, twin, mirrored, slot, twin_slot) in &tables {
            if previous[slot].losses == 0 {
                continue;
            }
            // Blocks written during the scan only receive values at `plies`,
            // which the scan does not look for, so the snapshot is complete.
            let blocks = summaries[slot].blocks_with(plies - 1);
            blocks_scanned += blocks.len();
            blocks_total += summaries[slot].num_blocks();
            let newly = scan_table(&working, slot, &blocks, |v| v.is_loss() && ply(v) == plies - 1, |working, idx, v, local| {
                let ct = outcome_ct(v);
                quiet_unmoves(egt(table), egt(twin), idx, mirrored, |pred_idx| {
                    if working.mark_win(twin_slot, pred_idx, ct, plies) {
                        local.wins += 1;
                        summaries[twin_slot].record(pred_idx, plies);
                    }
                });
            });
            found[twin_slot] = found[twin_slot].merge(newly);
        }

        // Phase L: wins at `plies - 1` decrement the counters of their
        // predecessors, one conversion type after the other.
        for ct in [ConversionType::Checkmate, ConversionType::Capture, ConversionType::Promotion] {
            let target = MaybeDtcOutcome::new_win(ct, plies - 1);
            for &(table, twin, mirrored, slot, twin_slot) in &tables {
                if previous[slot].wins == 0 {
                    continue;
                }
                let blocks = summaries[slot].blocks_with(plies - 1);
                blocks_scanned += blocks.len();
                blocks_total += summaries[slot].num_blocks();
                let newly = scan_table(&working, slot, &blocks, |v| v == target, |working, idx, _, local| {
                    quiet_unmoves(egt(table), egt(twin), idx, mirrored, |pred_idx| {
                        if working.decrement(twin_slot, pred_idx, ct, plies) {
                            local.losses += 1;
                            summaries[twin_slot].record(pred_idx, plies);
                        }
                    });
                });
                found[twin_slot] = found[twin_slot].merge(newly);
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

        if found.iter().all(|f| f.is_empty()) {
            break;
        }
        previous = found;
        plies += 1;
    }

    if blocks_total > 0 {
        println!(
            "{}: scans visited {} of {} blocks ({:.1}%)",
            table_name(solver, table_a),
            blocks_scanned,
            blocks_total,
            100.0 * blocks_scanned as f64 / blocks_total as f64,
        );
    }

    Ok(PairResult {
        working,
        max_win_dtc_a: max_win[0],
        max_loss_dtc_a: max_loss[0],
        max_win_dtc_b: max_win[1],
        max_loss_dtc_b: max_loss[1],
        init_time,
        propagation_time: propagation_start.elapsed(),
    })
}

/// Generates one exact table and its twin in the caller's Rayon pool.
///
/// The shared reader lazily opens existing conversion dependencies, including
/// outputs of earlier jobs. Missing dependencies are errors, not generation
/// requests. A self-twin is solved and published once. Readers are returned in
/// requested-table/twin order with their per-table statistics attached.
pub(crate) fn generate_pair(
    base: &Location,
    table: &TableId,
    reader: &SharedDependencyReader,
) -> EgtResult<Vec<EgtFileReader>> {
    base.ensure_writable()?;
    let (twin, _) = table.twin()?;
    let mut ids = vec![table.clone()];
    let table_a = Egt::from_elements(table.elements().to_vec())?;
    let table_b = if twin == *table {
        None
    } else {
        let egt = Egt::from_elements(twin.elements().to_vec())?;
        ids.push(twin);
        Some(egt)
    };
    let solver = RetrogradeSolver::new(table_a, table_b);
    let pair = solve_pair(&solver, 0, ids.len() - 1, reader)?;
    // Readers construct their own descriptors. Release the solver's indexers
    // before finalizing so only this pair's descriptors and arrays are retained.
    drop(solver);

    let finalize_start = std::time::Instant::now();
    let maxima = [
        [pair.max_win_dtc_a, pair.max_loss_dtc_a],
        [pair.max_win_dtc_b, pair.max_loss_dtc_b],
    ];
    let mut files = Vec::with_capacity(ids.len());
    for ((id, values), maxima) in ids.iter().zip(pair.working.values).zip(maxima) {
        println!("{}: finalizing outcomes, statistics and compression...", id.tablename());
        let file = EgtFileReader::new_in(base, id.tablename())?;
        let stats = crate::finalization::finalize_table(&file, values, maxima)?;
        drop(file);
        let mut published = EgtFileReader::open_in(base, id.tablename())?;
        published.stats = Some(stats);
        files.push(published);
    }
    println!(
        "{}: phase timings: init {:.3}s, propagation {:.3}s, finalize/publish {:.3}s",
        table.tablename(),
        pair.init_time.as_secs_f64(),
        pair.propagation_time.as_secs_f64(),
        finalize_start.elapsed().as_secs_f64(),
    );
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use crate::{EgtError, EgtGenerator};
    use crate::selection::{GenerationPlan, TableSelector};

    fn tables(material: &str) -> Vec<Egt> {
        TableSelector::parse(material).unwrap().resolve().unwrap().into_iter()
            .map(|id| Egt::from_elements(id.elements().to_vec()).unwrap())
            .collect()
    }

    fn table(name: &str) -> Egt {
        Egt::from_elements(TableId::parse(name).unwrap().elements().to_vec()).unwrap()
    }

    #[test]
    fn pawnless_unmoves_match_forward_play() {
        for endgame in ["K_K", "KQ_K", "KR_K", "KB_K", "KN_K", "KQ_KR", "KNN_K", "KQR_KB"] {
            for egt in tables(endgame) {
                for turn in [Color::White, Color::Black] {
                    let mut checked = 0;
                    let stride = (egt.index_range() / 512).max(1);
                    for idx in (0..egt.index_range()).step_by(stride) {
                        if let Some(position) = egt.position_from_index(idx, turn) {
                            assert_quiet_unmoves_match_forward_play(&position);
                            checked += 1;
                        }
                    }
                    assert!(checked > 0, "no valid samples for {endgame}, {turn:?}");
                }
            }
        }
    }

    #[test]
    fn pawnless_unmoves_match_forward_play_in_checks_and_terminal_positions() {
        for fen in [
            "7k/6Q1/5K2/8/8/8/8/8 b - - 0 1", // Checkmate.
            "7k/5K2/6Q1/8/8/8/8/8 b - - 0 1", // Stalemate.
            "4k3/8/8/1B6/8/8/8/K3R3 b - - 0 1", // Discovered double check.
            "4k3/4r3/8/8/8/8/8/K3R3 w - - 0 1", // Pinned previous mover.
            "4r2k/8/8/8/8/8/4R3/4K3 b - - 0 1", // Last move can block check.
        ] {
            let position = test_position(fen);
            assert_quiet_unmoves_match_forward_play(&position);
        }
    }

    fn test_position(fen: &str) -> Chess {
        fen.parse::<shakmaty::fen::Fen>().unwrap()
            .into_position(shakmaty::CastlingMode::Standard).unwrap()
    }

    fn color_mirrored(position: &Chess) -> Chess {
        position.to_setup(EnPassantMode::Always).into_mirrored()
            .position(shakmaty::CastlingMode::Standard).unwrap()
    }

    // Exhaust all empty origins and canonical EP states. Checked forward play
    // provides an oracle independent of the optimized reverse-move geometry.
    fn quiet_predecessors_by_forward_play(position: &Chess) -> HashMap<Chess, usize> {
        use shakmaty::{CastlingMode, File, Rank, Square};

        let mut expected = HashMap::new();
        let mut base = position.to_setup(EnPassantMode::Always);
        base.turn = !position.turn();
        for to in position.board().by_color(base.turn) {
            let piece = position.board().piece_at(to).unwrap();
            for from in !position.board().occupied() {
                let mut setup = base.clone();
                setup.board.discard_piece_at(to);
                setup.board.set_piece_at(from, piece);
                let m = Move::Normal {
                    role: piece.role, from, to, capture: None, promotion: None,
                };
                let ep_rank = setup.turn.relative_rank(Rank::Sixth);
                for ep in std::iter::once(None).chain(File::ALL.into_iter()
                    .map(|file| Some(Square::from_coords(file, ep_rank))))
                {
                    setup.ep_square = ep;
                    if let Ok(predecessor) = setup.clone().position::<Chess>(CastlingMode::Standard) {
                        if predecessor.legal_ep_square() == ep
                            && predecessor.clone().play(m).is_ok_and(|next| next == *position)
                        {
                            *expected.entry(predecessor).or_insert(0usize) += 1;
                        }
                    }
                }
            }
        }
        expected
    }

    fn assert_quiet_unmoves_match_forward_play(position: &Chess) -> HashMap<Chess, usize> {
        let expected = quiet_predecessors_by_forward_play(position);
        let mut actual = HashMap::new();
        let emit = |predecessor: Chess| {
            assert_eq!(predecessor.maybe_ep_square(), predecessor.legal_ep_square());
            *actual.entry(predecessor).or_insert(0usize) += 1;
        };
        if position.board().pawns().is_empty() {
            pawnless_quiet_unmoves(position, emit);
        } else {
            pawnful_quiet_unmoves(position, emit);
        }
        assert_eq!(actual, expected, "predecessor multisets differ for {position:?}");
        actual
    }

    #[test]
    fn pawnful_unmoves_match_forward_play() {
        let mut ep_checked = 0;
        for endgame in ["KP_K", "KQ_KP", "KP_KP", "KPP_K", "KPP_KP", "KRP_KP"] {
            for egt in tables(endgame) {
                for turn in [Color::White, Color::Black] {
                    let mut checked = 0;
                    let stride = (egt.index_range() / 64).max(1);
                    for idx in (0..egt.index_range()).step_by(stride) {
                        if let Some(position) = egt.position_from_index(idx, turn) {
                            assert_quiet_unmoves_match_forward_play(&position);
                            ep_checked += usize::from(position.legal_ep_square().is_some());
                            checked += 1;
                        }
                    }
                    assert!(checked > 0, "no valid samples for {}, {turn:?}", egt.tablename());
                }
            }
        }
        assert!(ep_checked > 0, "samples must include EP successors");
    }

    #[test]
    fn pawnful_unmoves_match_forward_play_in_ep_and_blocked_positions() {
        for fen in [
            "7k/8/8/8/8/4P3/8/K7 b - - 0 1", // Single pawn push.
            "7k/8/8/8/4P3/8/8/K7 b - - 0 1", // Double pawn push without EP.
            "7k/8/8/8/4P3/4B3/8/K7 b - - 0 1", // Blocked double-push intermediate.
            "7k/8/8/8/4P3/8/4B3/K7 b - - 0 1", // Blocked double-push origin.
            "7k/8/8/8/8/8/4P3/K7 b - - 0 1", // Reverse push to backrank is invalid.
            "4k3/8/8/KPp4r/8/8/8/8 b - - 0 1", // EP exposes a horizontal rook check.
            "4k3/8/8/KPp4r/8/8/8/8 w - - 0 1", // Double push with illegal horizontal EP.
            "7k/8/8/3pP3/8/8/8/K7 w - - 0 1", // A legal EP right would distinguish a double push.
            "k3r3/8/8/2PpP3/8/8/8/4K3 w - - 0 1", // One legal EP capturer is sufficient.
            "k3r3/8/8/2PpP3/8/8/7R/4K3 b - - 0 1", // Only one of two EP capturers is legal.
            "7k/8/2N5/3Pp3/8/8/8/K7 b - - 0 1", // Unmoving to e7 prevents EP restoration.
            "7k/8/4N3/3Pp3/8/8/8/K7 b - - 0 1", // Unmoving from e6 can restore EP.
            "4r2k/8/8/8/8/3P4/8/4K3 w - - 0 1", // Checked successor.
        ] {
            let position = test_position(fen);
            assert_quiet_unmoves_match_forward_play(&position);
            assert_quiet_unmoves_match_forward_play(&color_mirrored(&position));
        }
    }

    #[test]
    fn pawnful_unmoves_force_double_push_for_ep_successor() {
        use shakmaty::Square;

        for (fen, states) in [
            ("7k/8/8/3pP3/8/8/8/K7 w - d6 0 1", 1),
            ("7k/8/8/3pP3/4K3/8/8/8 w - d6 0 1", 1), // EP captures the checking pawn.
            // The forced double push can itself expire a previous legal EP right.
            ("7k/8/8/3pP3/4Pp2/8/8/K7 w - d6 0 1", 2),
        ] {
            let position = test_position(fen);
            for mirrored in [false, true] {
                let successor = if mirrored { color_mirrored(&position) } else { position.clone() };
                let actual = assert_quiet_unmoves_match_forward_play(&successor);
                assert_eq!(actual.values().sum::<usize>(), states);
                let (from, to) = if mirrored {
                    (Square::D7.flip_vertical(), Square::D5.flip_vertical())
                } else {
                    (Square::D7, Square::D5)
                };
                for predecessor in actual.keys() {
                    assert_eq!(predecessor.board().piece_at(from), Some(Role::Pawn.of(predecessor.turn())));
                    assert!(predecessor.board().piece_at(to).is_none());
                }
            }
        }
    }

    #[test]
    fn pawnful_unmoves_restore_distinct_ep_states() {
        use shakmaty::Square;

        for (successor_fen, predecessor_fen, ep_squares) in [
            ("4k3/8/8/3Pp3/8/8/R3K3/8 b - - 0 1",
             "4k3/8/8/3Pp3/8/8/4K3/R7 w - - 0 1", vec![Square::E6]),
            ("4k3/8/8/3Pp3/8/P7/4K3/8 b - - 0 1",
             "4k3/8/8/3Pp3/8/8/P3K3/8 w - - 0 1", vec![Square::E6]),
            ("7k/8/8/3Pp1Pp/8/7R/8/K7 b - - 0 1",
             "7k/8/8/3Pp1Pp/8/8/7R/K7 w - - 0 1", vec![Square::E6, Square::H6]),
        ] {
            let position = test_position(successor_fen);
            let predecessor = test_position(predecessor_fen);
            for mirrored in [false, true] {
                let successor = if mirrored { color_mirrored(&position) } else { position.clone() };
                let actual = assert_quiet_unmoves_match_forward_play(&successor);
                for ep in std::iter::once(None).chain(ep_squares.iter().copied().map(Some)) {
                    let mut setup = predecessor.to_setup(EnPassantMode::Always);
                    setup.ep_square = ep;
                    if mirrored { setup.mirror(); }
                    let expected: Chess = setup.position(shakmaty::CastlingMode::Standard).unwrap();
                    assert_eq!(expected.legal_ep_square(), ep.map(|sq| if mirrored { sq.flip_vertical() } else { sq }));
                    assert_eq!(actual.get(&expected), Some(&1), "missing restored EP state {expected:?}");
                }
            }
        }
    }

    #[test]
    fn pawnful_unmoves_include_double_push_with_illegal_ep() {
        use shakmaty::Square;

        let position = test_position("k3r3/8/8/3pP3/8/8/8/4K3 w - - 0 1");
        let predecessor = test_position("k3r3/3p4/8/4P3/8/8/8/4K3 b - - 0 1");
        for mirrored in [false, true] {
            let successor = if mirrored { color_mirrored(&position) } else { position.clone() };
            let predecessor = if mirrored { color_mirrored(&predecessor) } else { predecessor.clone() };
            let m = Move::Normal {
                role: Role::Pawn,
                from: if mirrored { Square::D7.flip_vertical() } else { Square::D7 },
                to: if mirrored { Square::D5.flip_vertical() } else { Square::D5 },
                capture: None, promotion: None,
            };
            let replay = predecessor.clone().play(m).unwrap();
            assert_eq!(replay, successor);
            assert!(replay.maybe_ep_square().is_some());
            assert!(replay.legal_ep_square().is_none());
            let actual = assert_quiet_unmoves_match_forward_play(&successor);
            assert_eq!(actual.get(&predecessor), Some(&1));
            // Raw EP after forward play represents the same indexed state.
            assert_eq!(assert_quiet_unmoves_match_forward_play(&replay), actual);
        }
    }

    #[test]
    fn pawnful_unmoves_include_double_push_giving_check() {
        use shakmaty::Square;

        let predecessor = test_position("8/8/8/8/2p5/8/RPk5/7K w - - 0 1");
        let push = Move::Normal {
            role: Role::Pawn, from: Square::B2, to: Square::B4,
            capture: None, promotion: None,
        };
        let successor = predecessor.clone().play(push).unwrap();
        assert!(successor.is_check());
        assert_eq!(successor.maybe_ep_square(), Some(Square::B3));
        assert!(successor.legal_ep_square().is_none(), "EP cannot evade the rook check");
        for (before, after) in [
            (predecessor.clone(), successor.clone()),
            (color_mirrored(&predecessor), color_mirrored(&successor)),
        ] {
            let actual = assert_quiet_unmoves_match_forward_play(&after);
            assert_eq!(actual.get(&before), Some(&1));
        }

        // Exercise the indexed propagation path without generating the
        // five-piece table; the edge must retain its multiplicity.
        let before = table("KRPb_KPc");
        let after = table("KPc_KRPb");
        let predecessor_idx = before.position_to_index(&predecessor);
        assert_eq!(predecessor_idx, 479947);
        let successor_idx = after.position_to_index(&successor);
        let mut hits = 0;
        quiet_unmoves(&after, &before, successor_idx, false, |idx| {
            if idx == predecessor_idx { hits += 1; }
        });
        assert_eq!(hits, 1, "missing the b2-b4 predecessor edge");
    }

    fn atomic_values(values: Vec<MaybeDtcOutcome>) -> Vec<AtomicU16> {
        values.into_iter().map(|v| AtomicU16::new(v.to_u16())).collect()
    }

    fn plain_values(values: &[AtomicU16]) -> Vec<MaybeDtcOutcome> {
        values.iter().map(|v| MaybeDtcOutcome::from_u16(v.load(Ordering::Relaxed))).collect()
    }

    #[test]
    fn pair_local_mark_win_preference_and_newly_resolved() {
        for order in [
            [ConversionType::Promotion, ConversionType::Capture, ConversionType::Checkmate],
            [ConversionType::Checkmate, ConversionType::Capture, ConversionType::Promotion],
        ] {
            let working = WorkingPair {
                values: vec![atomic_values(vec![MaybeDtcOutcome::new_unknown(3)])],
            };
            let mut preferred = ConversionType::Promotion;
            for (i, ct) in order.into_iter().enumerate() {
                assert_eq!(working.mark_win(0, 0, ct, 7), i == 0);
                preferred = preferred.max(ct);
                assert_eq!(working.load(0, 0), MaybeDtcOutcome::new_win(preferred, 7));
            }
            assert_eq!(working.load(0, 0), MaybeDtcOutcome::new_win(ConversionType::Checkmate, 7));
            assert!(!working.mark_win(0, 0, ConversionType::Checkmate, 7));
        }

        let existing = vec![
            MaybeDtcOutcome::new_win(ConversionType::Promotion, 2),
            MaybeDtcOutcome::new_loss(ConversionType::Capture, 4),
            MaybeDtcOutcome::DRAW,
            MaybeDtcOutcome::INVALID,
        ];
        let working = WorkingPair { values: vec![atomic_values(existing.clone())] };
        for idx in 0..existing.len() {
            assert!(!working.mark_win(0, idx, ConversionType::Checkmate, 3));
        }
        assert_eq!(plain_values(&working.values[0]), existing);
    }

    #[test]
    fn pair_local_decrement_resolves_only_once_and_preserves_other_slot() {
        let untouched = vec![MaybeDtcOutcome::new_unknown(5)];
        let mut working = WorkingPair {
            values: vec![atomic_values(vec![MaybeDtcOutcome::new_unknown(3)]), atomic_values(untouched.clone())],
        };
        assert!(!working.decrement(0, 0, ConversionType::Checkmate, 8));
        assert_eq!(working.load(0, 0), MaybeDtcOutcome::new_unknown(2));
        assert!(!working.decrement(0, 0, ConversionType::Capture, 8));
        assert_eq!(working.load(0, 0), MaybeDtcOutcome::new_unknown(1));
        assert!(working.decrement(0, 0, ConversionType::Promotion, 8));
        assert_eq!(working.load(0, 0), MaybeDtcOutcome::new_loss(ConversionType::Promotion, 8));
        assert!(!working.decrement(0, 0, ConversionType::Checkmate, 9));
        assert_eq!(working.load(0, 0), MaybeDtcOutcome::new_loss(ConversionType::Promotion, 8));
        assert_eq!(plain_values(&working.values[1]), untouched);

        let assigned = vec![
            MaybeDtcOutcome::new_win(ConversionType::Capture, 2),
            MaybeDtcOutcome::DRAW,
            MaybeDtcOutcome::INVALID,
        ];
        working.values[0] = atomic_values(assigned.clone());
        for idx in 0..assigned.len() {
            assert!(!working.decrement(0, idx, ConversionType::Promotion, 3));
        }
        assert_eq!(plain_values(&working.values[0]), assigned);
    }

    #[test]
    fn pair_local_ply_summary_clips_tails_and_keeps_maximum() {
        let block = SUMMARY_BLOCK_SIZE;
        let summary = PlySummary::new(2 * block + 3);
        assert_eq!(summary.num_blocks(), 3);
        assert_eq!(summary.blocks_with(0), vec![(0, block), (block, 2 * block), (2 * block, 2 * block + 3)]);
        assert_eq!(summary.blocks_with(1), summary.blocks_with(0));
        assert!(summary.blocks_with(2).is_empty());
        summary.record(block - 1, 4);
        summary.record(block, 6);
        summary.record(2 * block + 2, 9);
        summary.record(2 * block, 2);
        assert_eq!(summary.blocks_with(5), vec![(block, 2 * block), (2 * block, 2 * block + 3)]);
        assert_eq!(summary.blocks_with(9), vec![(2 * block, 2 * block + 3)]);
        assert!(summary.blocks_with(10).is_empty());
        assert_eq!(PlySummary::new(block).blocks_with(1), vec![(0, block)]);
        assert_eq!(PlySummary::new(3).blocks_with(1), vec![(0, 3)]);
        let empty = PlySummary::new(0);
        assert_eq!(empty.num_blocks(), 0);
        assert!(empty.blocks_with(0).is_empty());
    }

    #[test]
    fn pair_local_self_twin_uses_one_working_array() {
        let dir = crate::TestDir::new("pair_local_self_twin");
        let solver = RetrogradeSolver::new(table("K_K"), None);
        let table = 0;
        let location = Location::parse(&dir.0).unwrap();
        let reader = SharedDependencyReader::new(HashMap::new(), &location, None);
        let pair = solve_pair(&solver, table, table, &reader).unwrap();
        assert_eq!(pair.working.values.len(), 1);
        assert_eq!(pair.working.values[0].len(), solver.tables[0].index_range());
        assert!(plain_values(&pair.working.values[0]).iter().all(|v| v.is_unknown() || v.is_draw()));
        assert_eq!((pair.max_win_dtc_a, pair.max_loss_dtc_a, pair.max_win_dtc_b, pair.max_loss_dtc_b), (0, 0, 0, 0));
    }

    #[test]
    fn logical_pairs_preserve_exact_twins_and_reflection() {
        for (name, twin_name, reflected) in [
            ("KQ_K", "K_KQ", false),
            ("KPd_KPe", "KPd_KPe", true),
            ("KPa_KPa", "KPa_KPa", false),
            ("KPao_KPao", "KPao_KPao", false),
            ("KPaPa_KPh", "KPa_KPhPh", true),
        ] {
            let id = TableId::parse(name).unwrap();
            let (twin, actual_reflection) = id.twin().unwrap();
            assert_eq!(twin.tablename(), twin_name);
            assert_eq!(actual_reflection, reflected);
            let solver = RetrogradeSolver::new(table(name), (id != twin).then(|| table(twin_name)));
            let twin_slot = solver.tables.len() - 1;
            assert_eq!(is_mirrored(&solver.tables[0], &solver.tables[twin_slot]), reflected);
            assert_eq!(table_name(&solver, twin_slot), twin_name);
            assert_eq!(solver.tables.len(), if id == twin { 1 } else { 2 });
        }
    }

    #[test]
    fn pair_local_self_twin_scan_snapshots_matches_before_updates() {
        let win = MaybeDtcOutcome::new_win(ConversionType::Capture, 1);
        let working = WorkingPair { values: vec![atomic_values(vec![win, win, MaybeDtcOutcome::new_unknown(1)])] };
        let visited = std::sync::Mutex::new(Vec::new());
        scan_table(&working, 0, &[(0, 3)], |v| v == win, |working, idx, v, _| {
            visited.lock().unwrap().push((idx, v));
            working.values[0][1].store(MaybeDtcOutcome::DRAW.to_u16(), Ordering::Relaxed);
            working.mark_win(0, 2, ConversionType::Capture, 2);
        });
        assert_eq!(*visited.lock().unwrap(), vec![(0, win), (1, win)]);
        assert_eq!(working.values.len(), 1);
        assert_eq!(working.load(0, 2), MaybeDtcOutcome::new_win(ConversionType::Capture, 2));
    }

    #[test]
    fn concurrent_cas_updates_resolve_once() {
        let pool = rayon::ThreadPoolBuilder::new().num_threads(4).build().unwrap();
        pool.install(|| {
            for _ in 0..32 {
                let working = WorkingPair { values: vec![atomic_values(vec![MaybeDtcOutcome::new_unknown(96)])] };
                let wins: usize = (0..96).into_par_iter().map(|i| {
                    let ct = [ConversionType::Promotion, ConversionType::Capture, ConversionType::Checkmate][i % 3];
                    usize::from(working.mark_win(0, 0, ct, 7))
                }).sum();
                assert_eq!(wins, 1);
                assert_eq!(working.load(0, 0), MaybeDtcOutcome::new_win(ConversionType::Checkmate, 7));
                let working = WorkingPair { values: vec![atomic_values(vec![MaybeDtcOutcome::new_unknown(96)])] };
                let losses: usize = (0..192).into_par_iter().map(|_| {
                    usize::from(working.decrement(0, 0, ConversionType::Promotion, 8))
                }).sum();
                assert_eq!(losses, 1);
                assert_eq!(working.load(0, 0), MaybeDtcOutcome::new_loss(ConversionType::Promotion, 8));
            }
        });
    }

    #[test]
    fn step4_concurrent_summary_max_and_clipped_tail() {
        let block = SUMMARY_BLOCK_SIZE;
        let summary = PlySummary::new(2 * block + 3);
        let barrier = std::sync::Barrier::new(4);
        std::thread::scope(|scope| {
            for worker in 0..4 {
                let summary = &summary;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    for stamp in (2..=32u16).rev() {
                        // All workers contend on the first block, then stamp
                        // distinct positions in both remaining blocks.
                        summary.record(worker, stamp);
                        summary.record(block + worker, stamp + 8);
                        summary.record(2 * block + worker % 3, stamp + 16);
                    }
                });
            }
        });
        assert_eq!(summary.num_blocks(), 3);
        assert_eq!(summary.last_ply.iter().map(|v| v.load(Ordering::Relaxed)).collect::<Vec<_>>(), vec![32, 40, 48]);
        assert_eq!(summary.blocks_with(32), vec![(0, block), (block, 2 * block), (2 * block, 2 * block + 3)]);
        assert_eq!(summary.blocks_with(33), vec![(block, 2 * block), (2 * block, 2 * block + 3)]);
        assert_eq!(summary.blocks_with(41), vec![(2 * block, 2 * block + 3)]);
        assert!(summary.blocks_with(49).is_empty());
    }

    #[test]
    fn step4_ordered_loss_conversion_phases() {
        let pool = rayon::ThreadPoolBuilder::new().num_threads(4).build().unwrap();
        pool.install(|| {
            let working = WorkingPair {
                values: vec![atomic_values(vec![
                    MaybeDtcOutcome::new_unknown(32),
                    MaybeDtcOutcome::new_unknown(64),
                    MaybeDtcOutcome::new_unknown(96),
                ])],
            };
            let conversions = [ConversionType::Checkmate, ConversionType::Capture, ConversionType::Promotion];
            for (phase, ct) in conversions.into_iter().enumerate() {
                // Each reduction joins before the next conversion type starts.
                let resolved: usize = (0..96).into_par_iter().map(|i| {
                    usize::from(working.decrement(0, i % 3, ct, 8))
                }).sum();
                assert_eq!(resolved, 1);
                for (idx, expected_ct) in conversions.into_iter().enumerate() {
                    let expected = if idx <= phase {
                        MaybeDtcOutcome::new_loss(expected_ct, 8)
                    } else {
                        MaybeDtcOutcome::new_unknown(32 * (idx - phase) as u16)
                    };
                    assert_eq!(working.load(0, idx), expected);
                }
            }
        });
    }

    #[test]
    fn step4_concurrent_win_updates_preserve_resolved_values() {
        let pool = rayon::ThreadPoolBuilder::new().num_threads(4).build().unwrap();
        let existing = vec![
            MaybeDtcOutcome::new_win(ConversionType::Promotion, 2),
            MaybeDtcOutcome::INVALID,
            MaybeDtcOutcome::DRAW,
            MaybeDtcOutcome::new_loss(ConversionType::Capture, 4),
        ];
        let working = WorkingPair { values: vec![atomic_values(existing.clone())] };
        let resolved: usize = pool.install(|| {
            (0..384).into_par_iter().map(|i| {
                let ct = [ConversionType::Checkmate, ConversionType::Capture, ConversionType::Promotion][i % 3];
                usize::from(working.mark_win(0, i % existing.len(), ct, 7))
            }).sum()
        });
        assert_eq!(resolved, 0);
        assert_eq!(plain_values(&working.values[0]), existing);
    }

    #[test]
    fn step4_multiblock_self_twin_scan_visits_sources_once() {
        let pool = rayon::ThreadPoolBuilder::new().num_threads(4).build().unwrap();
        pool.install(|| {
            let size = 3 * SUMMARY_BLOCK_SIZE + 6;
            let plies = 7;
            for source_is_loss in [true, false] {
                let source = if source_is_loss {
                    MaybeDtcOutcome::new_loss(ConversionType::Capture, plies - 1)
                } else {
                    MaybeDtcOutcome::new_win(ConversionType::Capture, plies - 1)
                };
                let working = WorkingPair {
                    values: vec![atomic_values((0..size).map(|idx| {
                        if idx % 2 == 0 { source } else { MaybeDtcOutcome::new_unknown(1) }
                    }).collect())],
                };
                let visits: Vec<_> = (0..size).map(|_| AtomicU16::new(0)).collect();
                let summary = PlySummary::new(size);

                let blocks = summary.blocks_with(1);
                assert_eq!(blocks.len(), 4);
                assert_eq!(blocks.last(), Some(&(3 * SUMMARY_BLOCK_SIZE, size)));
                let found = scan_table(&working, 0, &blocks,
                    |v| v == source && ply(v) == plies - 1,
                    |working, idx, v, local| {
                        assert_eq!(v, source);
                        visits[idx].fetch_add(1, Ordering::Relaxed);
                        // A bijection from even sources to odd destinations,
                        // shifted across blocks within the same working array.
                        let target = (idx + 2 * SUMMARY_BLOCK_SIZE) % size + 1;
                        if source_is_loss {
                            if working.mark_win(0, target, ConversionType::Capture, plies) {
                                local.wins += 1;
                            }
                        } else if working.decrement(0, target, ConversionType::Capture, plies) {
                            local.losses += 1;
                        }
                    });
                assert_eq!((found.wins, found.losses), if source_is_loss { (size / 2, 0) } else { (0, size / 2) });
                let resolved = if source_is_loss {
                    MaybeDtcOutcome::new_win(ConversionType::Capture, plies)
                } else {
                    MaybeDtcOutcome::new_loss(ConversionType::Capture, plies)
                };
                for (idx, visits) in visits.iter().enumerate() {
                    assert_eq!(visits.load(Ordering::Relaxed), if idx % 2 == 0 { 1 } else { 0 });
                    assert_eq!(working.load(0, idx), if idx % 2 == 0 { source } else { resolved });
                }
                assert_eq!(working.values.len(), 1);
            }
        });
    }

    #[test]
    fn parallel_generation_is_byte_identical() {
        let serial = crate::TestDir::new("serial_generation");
        let parallel = crate::TestDir::new("parallel_generation");
        let mut serial_generator = EgtGenerator::new(&serial.0).unwrap();
        serial_generator.with_threads(1);
        serial_generator.with_verify(false);
        let mut parallel_generator = EgtGenerator::new(&parallel.0).unwrap();
        parallel_generator.with_threads(4);
        parallel_generator.with_verify(false);
        for material in ["K_K", "KQ_K", "KR_K", "KB_K", "KN_K", "KP_K"] {
            serial_generator.generate(material).unwrap();
            parallel_generator.generate(material).unwrap();
            let plan = GenerationPlan::new(TableSelector::parse(material).unwrap().resolve().unwrap()).unwrap();
            for id in plan.pairs.into_iter().flat_map(|(a, b)| std::iter::once(a).chain(b)) {
                for extension in ["ggegt", "json"] {
                    let path = id.relative_path(extension);
                    assert_eq!(std::fs::read(serial.0.join(&path)).unwrap(),
                        std::fs::read(parallel.0.join(&path)).unwrap(), "{path}");
                }
                let location = Location::parse(&serial.0).unwrap();
                let stats = crate::stats::read_stats(&location, &id).unwrap();
                use sha2::{Digest, Sha256};
                let bytes = std::fs::read(serial.0.join(id.relative_path("ggegt"))).unwrap();
                assert_eq!(stats.sha256, format!("{:x}", Sha256::digest(bytes)));
            }
        }
    }

    #[test]
    fn parallel_missing_dependency_returns_error_without_generating() {
        let dir = crate::TestDir::new("parallel_missing_dependency");
        let mut generator = EgtGenerator::new(&dir.0).unwrap();
        generator.with_threads(4);
        let result = generator.generate("KQ_K");
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
        let mut generator = EgtGenerator::new(&dir.0).unwrap();
        generator.with_threads(0);
        generator.generate("K_K").unwrap();
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
        let mut generator = EgtGenerator::new(&test_dir.0).unwrap();
        generator.with_threads(2);
        generator.with_verify(false);
        if endgame != "K_K" {
            generator.generate("K_K").unwrap();
        }
        if endgame == "KP_K" {
            for dependency in ["KQ_K", "KR_K", "KB_K", "KN_K"] {
                generator.generate(dependency).unwrap();
            }
        }
        generator.generate(endgame).unwrap();

        // Check the established outcome totals by summing individual sidecars,
        // not by constructing a composite table or comparing legacy artifacts.
        let totals = |material: &str| {
            let mut totals = [0usize; 5];
            for id in TableSelector::parse(material).unwrap().resolve().unwrap() {
                let stats = crate::stats::read_stats(&location, &id).unwrap();
                let counts = [stats.index_range, stats.win, stats.draw, stats.loss, stats.invalid_or_redundant];
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
            assert!(ids.iter().all(|id| id.twin().unwrap().0 == *id));
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
