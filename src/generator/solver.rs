//! Retrograde analysis: decremental move counters driven by per-ply table
//! scans.
//!
//! Every unknown position stores the number of its moves not yet known to
//! lose. Every resolved position stores its distance, so
//! the positions to propagate at ply `p` are exactly the values at ply `p - 1`,
//! found by scanning the tables. A per-block summary (`PlySummary`) lets the
//! scans skip blocks by ply and source kind: all losses, or wins of each
//! conversion type. Per ply `p`, the loop runs:
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
//!
//! # Move counters and diagonal symmetry
//!
//! The move counter is generally initialized as the number of legal moves in a
//! position. Symmetry reduction in pawnless tables requires adjustments because
//! different legal moves can reach positions that map to the same index, and
//! different reverse moves can likewise map to the same predecessor index. For
//! a discussion of symmetry and move counters, see
//! [section 4.6 of this thesis](https://issuu.com/jespertk/docs/master_thesis).
//!
//! Let a position `p` have `#p=8` if it represents 8 equivalent positions and
//! `#p=4` if it represents 4 equivalent positions. With this indexer's choice
//! of canonicalization, `#p=4` positions have both kings on the a1–h8 diagonal.
//!
//! When initializing the counter for a canonical position `p`, if there is a
//! legal move `p -> p'` with `#p=8` and `#p'=4`, there is also a diagonally
//! reflected move from a non-canonical position (the reflection of `p`) to a
//! canonical position (the reflection of `p'`). Backward propagation explores
//! this reflected move too. To account for this, moves `p -> p'` with `#p=8`
//! and `#p'=4` contribute 2 to the counter during initialization. This is
//! implemented by [`symmetry_adjusted_move_counter`].
//!
//! Similarly, when backward propagation from a canonical position `p'` finds
//! a move `p -> p'` with `#p=4` and `#p'=8`, it must decrement both the counter
//! for `p` and the counter for its diagonal reflection. The reflected move
//! contributed to the reflected predecessor's counter but led to a
//! non-canonical successor. [`quiet_unmoves`] emits both predecessor indices
//! so propagation updates both counters.

use shakmaty::{attacks, Bitboard, Color, Chess, EnPassantMode, Move, Position, Role};

use crate::ConversionType;
use crate::PackedOutcome;
use crate::stats::EgtStats;
use crate::selection::TablePair;
use crate::error::{EgtError, EgtResult};
use crate::egt::Egt;
use crate::position::InvalidCounts;


use std::sync::atomic::{AtomicU16, Ordering};
use rayon::prelude::*;
use crate::egt_file::SharedFileCache;
use crate::storage::Location;

/// Mutable values for only the current pair; a self-pair has one shared array.
struct WorkingPair {
    values: Vec<Vec<AtomicU16>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WinUpdate {
    Unchanged,
    NewlyResolved,
    Upgraded,
}

impl WorkingPair {
    fn new<'a>(tables: impl IntoIterator<Item = (&'a str, usize)>) -> EgtResult<Self> {
        let mut values = Vec::new();
        for (table, len) in tables {
            let context = format!("table {table} working values");
            let bytes = len.checked_mul(std::mem::size_of::<AtomicU16>()).ok_or_else(|| {
                EgtError::WorkingMemoryAllocation {
                    context: context.clone(),
                    bytes: len as u128 * std::mem::size_of::<AtomicU16>() as u128,
                    reason: "allocation size overflow".to_string(),
                }
            })?;
            let mut table_values = Vec::new();
            table_values.try_reserve_exact(len).map_err(|err| EgtError::WorkingMemoryAllocation {
                context,
                bytes: bytes as u128,
                reason: err.to_string(),
            })?;
            table_values.resize_with(len, || AtomicU16::new(PackedOutcome::INVALID.to_u16()));
            values.push(table_values);
        }
        Ok(Self { values })
    }

    fn load(&self, slot: usize, idx: usize) -> PackedOutcome {
        PackedOutcome::from_u16(self.values[slot][idx].load(Ordering::Relaxed))
    }

    /// CAS preserves the best same-ply conversion even when workers race.
    fn mark_win(&self, slot: usize, idx: usize, ct: ConversionType, plies: u16) -> WinUpdate {
        let cell = &self.values[slot][idx];
        let mut raw = cell.load(Ordering::Relaxed);
        loop {
            let v = PackedOutcome::from_u16(raw);
            let newly_resolved = v.is_unknown();
            if !newly_resolved && !(v.is_win() && v.plies() == plies && ct > v.conversion_type()) {
                return WinUpdate::Unchanged;
            }
            match cell.compare_exchange_weak(raw, PackedOutcome::new_win(ct, plies).to_u16(), Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return if newly_resolved { WinUpdate::NewlyResolved } else { WinUpdate::Upgraded },
                Err(current) => raw = current,
            }
        }
    }

    /// Conversion-type barriers ensure the last decrement has the loss preference.
    fn decrement(&self, slot: usize, idx: usize, ct: ConversionType, plies: u16) -> bool {
        let cell = &self.values[slot][idx];
        let mut raw = cell.load(Ordering::Relaxed);
        loop {
            let v = PackedOutcome::from_u16(raw);
            if !v.is_unknown() {
                return false;
            }
            let counter = v.get_unknown_counter();
            debug_assert!(counter > 0);
            let next = if counter == 1 {
                PackedOutcome::new_loss(ct, plies)
            } else {
                PackedOutcome::new_unknown(counter - 1)
            };
            match cell.compare_exchange_weak(raw, next.to_u16(), Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return counter == 1,
                Err(current) => raw = current,
            }
        }
    }
}


/// Number of positions per summary block.
const SUMMARY_BLOCK_SIZE: usize = 4096;

#[derive(Clone, Copy)]
enum SummaryKind {
    Loss,
    Win(ConversionType),
}

impl SummaryKind {
    fn index(self) -> usize {
        match self {
            Self::Loss => 0,
            Self::Win(ConversionType::Checkmate) => 1,
            Self::Win(ConversionType::Capture) => 2,
            Self::Win(ConversionType::Promotion) => 3,
        }
    }
}

#[derive(Default)]
struct BlockSummary {
    // Zero means empty; otherwise the bound is the maximum recorded ply + 1.
    ply_bounds: [AtomicU16; 4],
}

/// Conservative per-block ply bounds, separated by source kind.
struct PlySummary {
    size: usize,
    blocks: Vec<BlockSummary>,
}

impl PlySummary {
    fn new(size: usize) -> Self {
        Self { size, blocks: (0..size.div_ceil(SUMMARY_BLOCK_SIZE)).map(|_| BlockSummary::default()).collect() }
    }

    fn record(&self, local_index: usize, kind: SummaryKind, plies: u16) {
        let last = &self.blocks[local_index / SUMMARY_BLOCK_SIZE].ply_bounds[kind.index()];
        let ply_bound = plies + 1;
        if last.load(Ordering::Relaxed) < ply_bound {
            last.fetch_max(ply_bound, Ordering::Relaxed);
        }
    }

    fn blocks_with(&self, kind: SummaryKind, plies: u16) -> Vec<(usize, usize)> {
        let kind = kind.index();
        let ply_bound = plies + 1;
        self.blocks.iter().enumerate()
            .filter(|(_, block)| block.ply_bounds[kind].load(Ordering::Relaxed) >= ply_bound)
            .map(|(b, _)| {
                let start = b * SUMMARY_BLOCK_SIZE;
                (start, (start + SUMMARY_BLOCK_SIZE).min(self.size))
            })
            .collect()
    }

    fn num_blocks(&self) -> usize {
        self.blocks.len()
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
    S: Fn(PackedOutcome) -> bool + Sync,
    F: Fn(&WorkingPair, usize, PackedOutcome, &mut Found) + Sync,
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

            // Quiet relocation preserves material and kings; no pawns or EP
            // need validation. Impossible checks remain accepted, as in indexing.
            if !crate::position::opposite_check(&setup.board, setup.turn) {
                f(Chess::from_setup_unchecked(setup, position.castles().mode()));
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
                    // The valid source supplies the board and turn; the pawn,
                    // empty target, and empty origin establish structural EP.
                    let successor = Chess::from_setup_unchecked(successor, position.castles().mode());
                    if successor.legal_ep_square().is_none() {
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
    // Quiet relocation preserves kings and material (at most eight pieces).
    // Validate the board once for all EP states, still accepting impossible checks.
    if (setup.board.pawns() & Bitboard::BACKRANKS).any()
        || crate::position::opposite_check(&setup.board, setup.turn)
    {
        return;
    }
    f(Chess::from_setup_unchecked(setup.clone(), CastlingMode::Standard));

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
        // pushed_pawns and the empty target/origin establish structural EP;
        // a legal capture is still required for this indexed EP state.
        let predecessor = Chess::from_setup_unchecked(setup_ep, CastlingMode::Standard);
        if predecessor.legal_ep_square() == Some(ep) {
            f(predecessor);
        }
    }
}

/// Emits predecessors of an index already validated for this table and White.
/// Propagation only passes resolved entries, which initialization validated.
pub fn quiet_unmoves<F>(
    table: &Egt,
    twin: &Egt,
    local_index: usize,
    mirrored: bool,
    mut f: F,
) where
    F: FnMut(usize),
{
    let position = table.position_from_index_unchecked(local_index, Color::White);

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

        // Emit the diagonal predecessor too for `#p=4` -> `#p'=8` moves.
        // See "Move counters and diagonal symmetry" in the module documentation.
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

    // Count both reflected moves for `#p=8` -> `#p'=4` transitions.
    // See "Move counters and diagonal symmetry" in the module documentation.
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
    invalid: InvalidCounts,
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
    summary: &PlySummary,
    reader: &SharedFileCache,
) -> EgtResult<InitCounts> {
    let (size, pawnless) = (egt.index_range(), egt.is_pawnless());
    debug_assert_eq!(values.len(), size);
    debug_assert_eq!(summary.size, size);
    values.par_chunks_mut(SUMMARY_BLOCK_SIZE).enumerate().map(|(block, chunk)| {
        let mut counts = InitCounts::default();
        let mut ply_bounds = [0u16; 4];
        for (offset, cell) in chunk.iter_mut().enumerate() {
            let idx = block * SUMMARY_BLOCK_SIZE + offset;
            let position = match egt.position_from_index_with_reason(idx, Color::White)? {
                Ok(position) => position,
                Err(reason) => {
                    *cell.get_mut() = PackedOutcome::INVALID.to_u16();
                    // Finalization counts redundant indices before invalidity.
                    if !egt.diagonal_symmetric(idx).is_some_and(|other| idx > other) {
                        counts.invalid.record(reason);
                    }
                    continue;
                }
            };
            let legals = position.legal_moves();

            if legals.is_empty() {
                if position.is_check() {
                    *cell.get_mut() = PackedOutcome::new_loss(ConversionType::Checkmate, 0).to_u16();
                    counts.checkmates += 1;
                    ply_bounds[SummaryKind::Loss.index()] = ply_bounds[SummaryKind::Loss.index()].max(1);
                } else {
                    *cell.get_mut() = PackedOutcome::DRAW.to_u16();
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
                        loss_conv = Some(loss_conv.map_or(ct, |previous| previous.min(ct)));
                    }
                }
            }

            let outcome = if let Some(ct) = win_conv {
                // A checkmate in 1 found by phase W(1) may still upgrade the
                // conversion type.
                counts.found.wins += 1;
                ply_bounds[SummaryKind::Win(ct).index()] = 2;
                PackedOutcome::new_win(ct, 1)
            } else if counter == 0 {
                // All moves are conversions to won positions.
                counts.found.losses += 1;
                ply_bounds[SummaryKind::Loss.index()] = 2;
                PackedOutcome::new_loss(loss_conv.expect("a loss by conversion has a losing conversion"), 1)
            } else {
                PackedOutcome::new_unknown(counter)
            };
            *cell.get_mut() = outcome.to_u16();
        }

        // Each initialization chunk owns one block; propagation starts after joining.
        for (bound, value) in summary.blocks[block].ply_bounds.iter().zip(ply_bounds) {
            bound.store(value, Ordering::Relaxed);
        }
        Ok(counts)
    }).try_reduce(InitCounts::default, |a, b| Ok(InitCounts {
        invalid: a.invalid.merge(b.invalid),
        checkmates: a.checkmates + b.checkmates,
        stalemates: a.stalemates + b.stalemates,
        found: a.found.merge(b.found),
    }))
}

struct TableState {
    egt: Egt,
    summary: PlySummary,
    invalid: InvalidCounts,
    previous: Found,
    found: Found,
    /// Maximum win and loss DTC, respectively.
    maxima: [u16; 2],
}

impl TableState {
    fn new(egt: Egt) -> Self {
        Self {
            summary: PlySummary::new(egt.index_range()),
            egt,
            invalid: InvalidCounts::default(),
            previous: Found::default(),
            found: Found::default(),
            maxima: [0; 2],
        }
    }
}

struct PairResult {
    working: WorkingPair,
    init_time: std::time::Duration,
    propagation_time: std::time::Duration,
}

/// Solves one table or two twins, leaving the draws as unknown. Reversing
/// the slot order selects the twin, including slot 0 for a self-twin.
fn solve_pair(
    tables: &mut [TableState],
    reflected: [bool; 2],
    reader: &SharedFileCache,
) -> EgtResult<PairResult> {
    debug_assert!((1..=2).contains(&tables.len()));
    let init_start = std::time::Instant::now();
    let mut working = WorkingPair::new(tables.iter().map(|table| {
        (table.egt.tablename(), table.egt.index_range())
    }))?;

    for (table, values) in tables.iter_mut().zip(&mut working.values) {
        let counts = initialize_table(&table.egt, values, &table.summary, reader)?;
        println!("{}: Initialized with {} checkmated positions, {} stalemated positions.", table.egt.tablename(), counts.checkmates, counts.stalemates);
        // Checkmates count as losses at ply 0, so that phase W(1) scans them.
        table.previous.losses = counts.checkmates;
        table.found = counts.found;
        table.invalid = counts.invalid;
    }

    let init_time = init_start.elapsed();
    let propagation_start = std::time::Instant::now();

    // Blocks visited by the scans, and blocks the same scans would visit
    // without the summaries.
    let mut blocks_scanned = 0usize;
    let mut blocks_total = 0usize;

    let mut plies: u16 = 1;
    loop {
        // Phase W: losses at `plies - 1` make their predecessors wins.
        for slot in 0..tables.len() {
            let table = &tables[slot];
            let twin_slot = tables.len() - 1 - slot;
            let twin = &tables[twin_slot];
            if table.previous.losses == 0 {
                continue;
            }
            // Blocks written during the scan only receive values at `plies`,
            // which the scan does not look for, so the snapshot is complete.
            let blocks = table.summary.blocks_with(SummaryKind::Loss, plies - 1);
            blocks_scanned += blocks.len();
            blocks_total += table.summary.num_blocks();
            if blocks.is_empty() {
                continue;
            }
            let newly = scan_table(&working, slot, &blocks, |v| v.is_loss() && v.plies() == plies - 1, |working, idx, v, local| {
                let ct = v.conversion_type();
                quiet_unmoves(&table.egt, &twin.egt, idx, reflected[slot], |pred_idx| {
                    match working.mark_win(twin_slot, pred_idx, ct, plies) {
                        WinUpdate::NewlyResolved => {
                            local.wins += 1;
                            twin.summary.record(pred_idx, SummaryKind::Win(ct), plies);
                        }
                        WinUpdate::Upgraded => {
                            // Keep the old kind's bound: stale bounds only add scans.
                            twin.summary.record(pred_idx, SummaryKind::Win(ct), plies);
                        }
                        WinUpdate::Unchanged => {}
                    }
                });
            });
            tables[twin_slot].found = tables[twin_slot].found.merge(newly);
        }

        // Phase L: wins at `plies - 1` decrement the counters of their
        // predecessors, one conversion type after the other.
        for ct in [ConversionType::Checkmate, ConversionType::Capture, ConversionType::Promotion] {
            let target = PackedOutcome::new_win(ct, plies - 1);
            for slot in 0..tables.len() {
                let table = &tables[slot];
                let twin_slot = tables.len() - 1 - slot;
                let twin = &tables[twin_slot];
                if table.previous.wins == 0 {
                    continue;
                }
                let blocks = table.summary.blocks_with(SummaryKind::Win(ct), plies - 1);
                blocks_scanned += blocks.len();
                blocks_total += table.summary.num_blocks();
                if blocks.is_empty() {
                    continue;
                }
                let newly = scan_table(&working, slot, &blocks, |v| v == target, |working, idx, _, local| {
                    quiet_unmoves(&table.egt, &twin.egt, idx, reflected[slot], |pred_idx| {
                        if working.decrement(twin_slot, pred_idx, ct, plies) {
                            local.losses += 1;
                            twin.summary.record(pred_idx, SummaryKind::Loss, plies);
                        }
                    });
                });
                tables[twin_slot].found = tables[twin_slot].found.merge(newly);
            }
        }

        for table in tables.iter_mut() {
            if table.found.wins > 0 {
                println!("{}: Found {} winning positions at depth {}", table.egt.tablename(), table.found.wins, plies);
                table.maxima[0] = plies;
            }
            if table.found.losses > 0 {
                println!("{}: Found {} losing positions at depth {}", table.egt.tablename(), table.found.losses, plies);
                table.maxima[1] = plies;
            }
            table.previous = std::mem::take(&mut table.found);
        }

        if tables.iter().all(|table| table.previous.is_empty()) {
            break;
        }
        plies += 1;
    }

    if blocks_total > 0 {
        println!(
            "{}: scans visited {} of {} blocks ({:.1}%)",
            tables[0].egt.tablename(),
            blocks_scanned,
            blocks_total,
            100.0 * blocks_scanned as f64 / blocks_total as f64,
        );
    }

    Ok(PairResult {
        working,
        init_time,
        propagation_time: propagation_start.elapsed(),
    })
}

/// Generates one exact table and its twin in the caller's Rayon pool.
///
/// The shared reader lazily opens existing conversion dependencies, including
/// outputs of earlier jobs. Missing dependencies are errors, not generation
/// requests. A self-twin is solved and published once. Statistics are returned
/// in requested-table/twin order without opening the published outputs.
pub(super) fn generate_pair(
    base: &Location,
    pair: &TablePair,
    reader: &SharedFileCache,
) -> EgtResult<Vec<EgtStats>> {
    base.ensure_writable()?;
    let ids = || std::iter::once(&pair.table).chain(pair.twin.iter());
    let mut tables = ids().map(|id| Egt::from_elements(id.parse_elements()).map(TableState::new))
        .collect::<EgtResult<Vec<_>>>()?;
    let result = solve_pair(&mut tables, pair.reflected, reader)?;

    let finalize_start = std::time::Instant::now();
    let mut stats = Vec::with_capacity(tables.len());
    for ((id, table), values) in ids().zip(tables).zip(result.working.values) {
        drop(table.summary);
        println!("{}: finalizing outcomes, statistics and compression...", id.tablename());
        stats.push(super::finalization::finalize_table(base, id, &table.egt, values, table.maxima, table.invalid)?);
    }
    println!(
        "{}: phase timings: init {:.3}s, propagation {:.3}s, finalize/publish {:.3}s",
        pair.table.tablename(),
        result.init_time.as_secs_f64(),
        result.propagation_time.as_secs_f64(),
        finalize_start.elapsed().as_secs_f64(),
    );
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use crate::{EgtError, EgtGenerator, TableId};
    use crate::selection::{GenerationPlan, TableSelector};

    fn tables(material: &str) -> Vec<Egt> {
        TableSelector::parse(material).unwrap().resolve().unwrap().into_iter()
            .map(|id| Egt::from_elements(id.parse_elements()).unwrap())
            .collect()
    }

    fn table(name: &str) -> Egt {
        Egt::from_elements(TableId::parse(name).unwrap().parse_elements()).unwrap()
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
            .into_position(shakmaty::CastlingMode::Standard)
            .or_else(shakmaty::PositionError::ignore_impossible_check).unwrap()
    }

    fn color_mirrored(position: &Chess) -> Chess {
        position.to_setup(EnPassantMode::Always).into_mirrored()
            .position(shakmaty::CastlingMode::Standard)
            .or_else(shakmaty::PositionError::ignore_impossible_check).unwrap()
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
                    if let Ok(predecessor) = setup.clone().position::<Chess>(CastlingMode::Standard)
                        .or_else(shakmaty::PositionError::ignore_impossible_check)
                    {
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
        for endgame in ["KP_K", "KQ_KP", "KP_KP"] {
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
    fn symmetric_pawn_quiet_edge_multiplicities() {
        let mut ep_predecessors = 0;
        let mut ep_successors = 0;
        for name in ["KPaPh_K", "K_KPaPh", "KPcPf_KPdPe", "KPd_KPe", "KPaPa_KPh"] {
            let id = TableId::parse(name).unwrap();
            let (twin_id, mirrored) = id.twin();
            let egt = Egt::from_elements(id.parse_elements()).unwrap();
            let twin = Egt::from_elements(twin_id.parse_elements()).unwrap();
            let reverse_mirrored = twin_id.twin().1;
            let stride = (egt.index_range() / 128).max(1);
            let mut samples: Vec<_> = (0..egt.index_range()).step_by(stride).collect();
            if name == "KPcPf_KPdPe" {
                // Exercise both reflected EP files, EP expiry, and double pushes creating EP.
                for fen in [
                    "7k/8/8/2PppP2/8/8/8/K7 w - d6 0 1",
                    "7k/8/8/2PppP2/8/8/8/K7 w - e6 0 1",
                    "7k/8/8/8/3pp3/8/2P2P2/K7 w - - 0 1",
                ] {
                    samples.push(egt.position_to_index(&test_position(fen)));
                }
            }
            let mut checked = 0;
            for turn in [Color::White, Color::Black] {
                for &pred_idx in &samples {
                    let Some(position) = egt.position_from_index(pred_idx, turn) else { continue };
                    assert_eq!(egt.position_to_index(&position), pred_idx);
                    ep_predecessors += usize::from(position.legal_ep_square().is_some());
                    let mut successors = HashMap::new();
                    for m in position.legal_moves() {
                        if m.is_capture() || m.is_promotion() { continue; }
                        let successor = position.clone().play(m).unwrap();
                        ep_successors += usize::from(successor.legal_ep_square().is_some());
                        let succ_idx = if mirrored {
                            twin.reflected_position_to_index(&successor)
                        } else {
                            twin.position_to_index(&successor)
                        };
                        *successors.entry(succ_idx).or_insert(0usize) += 1;
                    }
                    for (succ_idx, expected) in successors {
                        assert!(twin.position_from_index(succ_idx, Color::White).is_some());
                        let mut actual = 0;
                        quiet_unmoves(&twin, &egt, succ_idx, reverse_mirrored, |idx| {
                            if idx == pred_idx { actual += 1; }
                        });
                        assert_eq!(actual, expected,
                            "{name}, {turn:?}, predecessor {pred_idx}, successor {succ_idx}");
                    }
                    checked += 1;
                }
            }
            assert!(checked > 0, "no valid samples for {name}");
        }
        assert!(ep_predecessors > 0, "samples must include EP expiry");
        assert!(ep_successors > 0, "samples must include double pushes creating EP");
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
    fn pawnful_predecessors_reject_invalid_boards_before_restoring_ep() {
        for fen in [
            "7k/8/8/3Pp3/8/8/K7/P7 w - - 0 1", // Backrank pawn.
            "7k/8/8/3Pp3/8/8/K7/7R w - - 0 1", // Opposite check.
        ] {
            let original = fen.parse::<shakmaty::fen::Fen>().unwrap().into_setup();
            for mirrored in [false, true] {
                let mut setup = original.clone();
                if mirrored { setup.mirror(); }
                // Both no-EP and structurally valid restored EP states must fail.
                for ep in [None, Some(if mirrored { shakmaty::Square::E3 } else { shakmaty::Square::E6 })] {
                    let mut candidate = setup.clone();
                    candidate.ep_square = ep;
                    assert!(candidate.position::<Chess>(shakmaty::CastlingMode::Standard)
                        .or_else(shakmaty::PositionError::ignore_impossible_check).is_err());
                }
                let mut emitted = 0;
                emit_pawnful_predecessors(&setup, &mut |_| emitted += 1);
                assert_eq!(emitted, 0, "invalid predecessor board accepted: {fen}, {mirrored}");
            }
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

    fn atomic_values(values: Vec<PackedOutcome>) -> Vec<AtomicU16> {
        values.into_iter().map(|v| AtomicU16::new(v.to_u16())).collect()
    }

    fn plain_values(values: &[AtomicU16]) -> Vec<PackedOutcome> {
        values.iter().map(|v| PackedOutcome::from_u16(v.load(Ordering::Relaxed))).collect()
    }

    #[test]
    fn working_pair_reports_allocation_capacity_overflow() {
        let len = usize::MAX;
        let err = WorkingPair::new([("oversized", len)]).err().unwrap();
        match err {
            EgtError::WorkingMemoryAllocation { context, bytes, .. } => {
                assert_eq!(context, "table oversized working values");
                assert_eq!(bytes, len as u128 * std::mem::size_of::<AtomicU16>() as u128);
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn pair_local_mark_win_preference_and_newly_resolved() {
        for order in [
            [ConversionType::Promotion, ConversionType::Capture, ConversionType::Checkmate],
            [ConversionType::Checkmate, ConversionType::Capture, ConversionType::Promotion],
        ] {
            let working = WorkingPair {
                values: vec![atomic_values(vec![PackedOutcome::new_unknown(3)])],
            };
            let mut preferred = ConversionType::Promotion;
            for (i, ct) in order.into_iter().enumerate() {
                let expected = if i == 0 {
                    WinUpdate::NewlyResolved
                } else if ct > preferred {
                    WinUpdate::Upgraded
                } else {
                    WinUpdate::Unchanged
                };
                assert_eq!(working.mark_win(0, 0, ct, 7), expected);
                preferred = preferred.max(ct);
                assert_eq!(working.load(0, 0), PackedOutcome::new_win(preferred, 7));
            }
            assert_eq!(working.load(0, 0), PackedOutcome::new_win(ConversionType::Checkmate, 7));
            assert_eq!(working.mark_win(0, 0, ConversionType::Checkmate, 7), WinUpdate::Unchanged);
        }

        let existing = vec![
            PackedOutcome::new_win(ConversionType::Promotion, 2),
            PackedOutcome::new_loss(ConversionType::Capture, 4),
            PackedOutcome::DRAW,
            PackedOutcome::INVALID,
        ];
        let working = WorkingPair { values: vec![atomic_values(existing.clone())] };
        for idx in 0..existing.len() {
            assert_eq!(working.mark_win(0, idx, ConversionType::Checkmate, 3), WinUpdate::Unchanged);
        }
        assert_eq!(plain_values(&working.values[0]), existing);
    }

    #[test]
    fn pair_local_decrement_resolves_only_once_and_preserves_other_slot() {
        let untouched = vec![PackedOutcome::new_unknown(5)];
        let mut working = WorkingPair {
            values: vec![atomic_values(vec![PackedOutcome::new_unknown(3)]), atomic_values(untouched.clone())],
        };
        assert!(!working.decrement(0, 0, ConversionType::Checkmate, 8));
        assert_eq!(working.load(0, 0), PackedOutcome::new_unknown(2));
        assert!(!working.decrement(0, 0, ConversionType::Capture, 8));
        assert_eq!(working.load(0, 0), PackedOutcome::new_unknown(1));
        assert!(working.decrement(0, 0, ConversionType::Promotion, 8));
        assert_eq!(working.load(0, 0), PackedOutcome::new_loss(ConversionType::Promotion, 8));
        assert!(!working.decrement(0, 0, ConversionType::Checkmate, 9));
        assert_eq!(working.load(0, 0), PackedOutcome::new_loss(ConversionType::Promotion, 8));
        assert_eq!(plain_values(&working.values[1]), untouched);

        let assigned = vec![
            PackedOutcome::new_win(ConversionType::Capture, 2),
            PackedOutcome::DRAW,
            PackedOutcome::INVALID,
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
        let loss = SummaryKind::Loss;
        let capture = SummaryKind::Win(ConversionType::Capture);
        assert_eq!(summary.num_blocks(), 3);
        assert!(summary.blocks_with(loss, 0).is_empty());
        summary.record(0, loss, 0);
        assert_eq!(summary.blocks_with(loss, 0), vec![(0, block)]);
        assert!(summary.blocks_with(loss, 1).is_empty());
        summary.record(block - 1, capture, 4);
        summary.record(block, capture, 6);
        summary.record(2 * block + 2, capture, 9);
        summary.record(2 * block, capture, 2);
        assert_eq!(summary.blocks_with(capture, 5), vec![(block, 2 * block), (2 * block, 2 * block + 3)]);
        assert_eq!(summary.blocks_with(capture, 9), vec![(2 * block, 2 * block + 3)]);
        assert!(summary.blocks_with(capture, 10).is_empty());
        assert_eq!(summary.blocks_with(loss, 0), vec![(0, block)]);
        assert!(summary.blocks_with(SummaryKind::Win(ConversionType::Checkmate), 0).is_empty());
        assert!(summary.blocks_with(SummaryKind::Win(ConversionType::Promotion), 0).is_empty());
        let empty = PlySummary::new(0);
        assert_eq!(empty.num_blocks(), 0);
        assert!(empty.blocks_with(loss, 0).is_empty());
    }

    #[test]
    fn initialization_populates_only_matching_summary_kinds() {
        let dir = crate::TestDir::new("initial_summary");
        EgtGenerator::new(&dir.0).unwrap().generate("K_K").unwrap();
        let location = Location::parse(&dir.0).unwrap();
        let reader = SharedFileCache::new(&location, None, None);
        for name in ["K_KQ", "KQ_K"] {
            let egt = table(name);
            let mut values = atomic_values(vec![PackedOutcome::INVALID; egt.index_range()]);
            let summary = PlySummary::new(values.len());
            let counts = initialize_table(&egt, &mut values, &summary, &reader).unwrap();
            assert_eq!(counts.checkmates > 0, name == "K_KQ");
            let invalid = values.iter().enumerate()
                .filter(|(idx, cell)| {
                    PackedOutcome::from_u16(cell.load(Ordering::Relaxed)).is_invalid()
                        && !egt.diagonal_symmetric(*idx).is_some_and(|other| *idx > other)
                }).count();
            assert_eq!(invalid > 0, name == "KQ_K");
            assert_eq!(counts.invalid, InvalidCounts {
                opposite_check: invalid,
                ..InvalidCounts::default()
            });
            let expected: Vec<_> = values.chunks(SUMMARY_BLOCK_SIZE).enumerate()
                .filter(|(_, chunk)| chunk.iter().any(|cell| PackedOutcome::from_u16(cell.load(Ordering::Relaxed)).is_loss()))
                .map(|(block, chunk)| {
                    let start = block * SUMMARY_BLOCK_SIZE;
                    (start, start + chunk.len())
                }).collect();
            assert_eq!(summary.blocks_with(SummaryKind::Loss, 0), expected);
            assert!(summary.blocks_with(SummaryKind::Loss, 1).is_empty());
            for ct in [ConversionType::Checkmate, ConversionType::Capture, ConversionType::Promotion] {
                assert!(summary.blocks_with(SummaryKind::Win(ct), 0).is_empty());
            }
        }
    }

    #[test]
    fn pair_local_self_twin_uses_one_working_array() {
        let dir = crate::TestDir::new("pair_local_self_twin");
        let mut tables = [TableState::new(table("K_K"))];
        let location = Location::parse(&dir.0).unwrap();
        let reader = SharedFileCache::new(&location, None, None);
        let pair = solve_pair(&mut tables, [false; 2], &reader).unwrap();
        assert_eq!(tables[0].invalid, InvalidCounts::default());
        assert_eq!(pair.working.values.len(), 1);
        assert_eq!(pair.working.values[0].len(), tables[0].egt.index_range());
        assert!(plain_values(&pair.working.values[0]).iter().all(|v| v.is_unknown() || v.is_draw()));
        assert_eq!(tables[0].maxima, [0, 0]);
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
            let (twin, actual_reflection) = id.twin();
            assert_eq!(twin.tablename(), twin_name);
            assert_eq!(actual_reflection, reflected);
            for selected in [id.clone(), twin.clone()] {
                let plan = GenerationPlan::new([selected]);
                let pair = &plan.pairs[0];
                let ids: Vec<_> = std::iter::once(&pair.table).chain(pair.twin.iter()).collect();
                assert_eq!(ids.len(), if id == twin { 1 } else { 2 });
                for (slot, source) in ids.iter().enumerate() {
                    let (target, reflection) = source.twin();
                    assert_eq!(target, *ids[ids.len() - 1 - slot]);
                    assert_eq!(pair.reflected[slot], reflection);
                }
            }
        }
    }

    #[test]
    fn pair_local_self_twin_scan_snapshots_matches_before_updates() {
        let win = PackedOutcome::new_win(ConversionType::Capture, 1);
        let working = WorkingPair { values: vec![atomic_values(vec![win, win, PackedOutcome::new_unknown(1)])] };
        let visited = std::sync::Mutex::new(Vec::new());
        scan_table(&working, 0, &[(0, 3)], |v| v == win, |working, idx, v, _| {
            visited.lock().unwrap().push((idx, v));
            working.values[0][1].store(PackedOutcome::DRAW.to_u16(), Ordering::Relaxed);
            working.mark_win(0, 2, ConversionType::Capture, 2);
        });
        assert_eq!(*visited.lock().unwrap(), vec![(0, win), (1, win)]);
        assert_eq!(working.values.len(), 1);
        assert_eq!(working.load(0, 2), PackedOutcome::new_win(ConversionType::Capture, 2));
    }

    #[test]
    fn concurrent_cas_updates_resolve_once() {
        let pool = rayon::ThreadPoolBuilder::new().num_threads(4).build().unwrap();
        pool.install(|| {
            for _ in 0..32 {
                let working = WorkingPair { values: vec![atomic_values(vec![PackedOutcome::new_unknown(96)])] };
                let wins: usize = (0..96).into_par_iter().map(|i| {
                    let ct = [ConversionType::Promotion, ConversionType::Capture, ConversionType::Checkmate][i % 3];
                    usize::from(working.mark_win(0, 0, ct, 7) == WinUpdate::NewlyResolved)
                }).sum();
                assert_eq!(wins, 1);
                assert_eq!(working.load(0, 0), PackedOutcome::new_win(ConversionType::Checkmate, 7));
                let working = WorkingPair { values: vec![atomic_values(vec![PackedOutcome::new_unknown(96)])] };
                let losses: usize = (0..192).into_par_iter().map(|_| {
                    usize::from(working.decrement(0, 0, ConversionType::Promotion, 8))
                }).sum();
                assert_eq!(losses, 1);
                assert_eq!(working.load(0, 0), PackedOutcome::new_loss(ConversionType::Promotion, 8));
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
                    let kinds = [SummaryKind::Loss, SummaryKind::Win(ConversionType::Checkmate),
                        SummaryKind::Win(ConversionType::Capture), SummaryKind::Win(ConversionType::Promotion)];
                    for plies in (2..=32u16).rev() {
                        // All workers contend on each kind in the first block.
                        for kind in kinds {
                            summary.record(worker, kind, plies);
                        }
                        summary.record(block + worker, kinds[worker], plies + 8);
                        summary.record(2 * block + worker % 3, kinds[worker], plies + 16);
                    }
                });
            }
        });
        assert_eq!(summary.num_blocks(), 3);
        for kind in [SummaryKind::Loss, SummaryKind::Win(ConversionType::Checkmate),
            SummaryKind::Win(ConversionType::Capture), SummaryKind::Win(ConversionType::Promotion)]
        {
            assert_eq!(summary.blocks.iter().map(|b| b.ply_bounds[kind.index()].load(Ordering::Relaxed)).collect::<Vec<_>>(), vec![33, 41, 49]);
            assert_eq!(summary.blocks_with(kind, 32), vec![(0, block), (block, 2 * block), (2 * block, 2 * block + 3)]);
            assert_eq!(summary.blocks_with(kind, 33), vec![(block, 2 * block), (2 * block, 2 * block + 3)]);
            assert_eq!(summary.blocks_with(kind, 41), vec![(2 * block, 2 * block + 3)]);
            assert!(summary.blocks_with(kind, 49).is_empty());
        }
    }

    #[test]
    fn step4_ordered_loss_conversion_phases() {
        let pool = rayon::ThreadPoolBuilder::new().num_threads(4).build().unwrap();
        pool.install(|| {
            let working = WorkingPair {
                values: vec![atomic_values(vec![
                    PackedOutcome::new_unknown(32),
                    PackedOutcome::new_unknown(64),
                    PackedOutcome::new_unknown(96),
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
                        PackedOutcome::new_loss(expected_ct, 8)
                    } else {
                        PackedOutcome::new_unknown(32 * (idx - phase) as u16)
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
            PackedOutcome::new_win(ConversionType::Promotion, 2),
            PackedOutcome::INVALID,
            PackedOutcome::DRAW,
            PackedOutcome::new_loss(ConversionType::Capture, 4),
        ];
        let working = WorkingPair { values: vec![atomic_values(existing.clone())] };
        let resolved: usize = pool.install(|| {
            (0..384).into_par_iter().map(|i| {
                let ct = [ConversionType::Checkmate, ConversionType::Capture, ConversionType::Promotion][i % 3];
                usize::from(working.mark_win(0, i % existing.len(), ct, 7) == WinUpdate::NewlyResolved)
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
                    PackedOutcome::new_loss(ConversionType::Capture, plies - 1)
                } else {
                    PackedOutcome::new_win(ConversionType::Capture, plies - 1)
                };
                let working = WorkingPair {
                    values: vec![atomic_values((0..size).map(|idx| {
                        if idx % 2 == 0 { source } else { PackedOutcome::new_unknown(1) }
                    }).collect())],
                };
                let visits: Vec<_> = (0..size).map(|_| AtomicU16::new(0)).collect();
                let summary = PlySummary::new(size);
                let kind = if source_is_loss { SummaryKind::Loss } else { SummaryKind::Win(ConversionType::Capture) };
                for idx in (0..size).step_by(SUMMARY_BLOCK_SIZE) {
                    summary.record(idx, kind, plies - 1);
                }

                let blocks = summary.blocks_with(kind, plies - 1);
                assert_eq!(blocks.len(), 4);
                assert_eq!(blocks.last(), Some(&(3 * SUMMARY_BLOCK_SIZE, size)));
                let found = scan_table(&working, 0, &blocks,
                    |v| v == source && v.plies() == plies - 1,
                    |working, idx, v, local| {
                        assert_eq!(v, source);
                        visits[idx].fetch_add(1, Ordering::Relaxed);
                        // A bijection from even sources to odd destinations,
                        // shifted across blocks within the same working array.
                        let target = (idx + 2 * SUMMARY_BLOCK_SIZE) % size + 1;
                        if source_is_loss {
                            if working.mark_win(0, target, ConversionType::Capture, plies) == WinUpdate::NewlyResolved {
                                local.wins += 1;
                            }
                        } else if working.decrement(0, target, ConversionType::Capture, plies) {
                            local.losses += 1;
                        }
                    });
                assert_eq!((found.wins, found.losses), if source_is_loss { (size / 2, 0) } else { (0, size / 2) });
                let resolved = if source_is_loss {
                    PackedOutcome::new_win(ConversionType::Capture, plies)
                } else {
                    PackedOutcome::new_loss(ConversionType::Capture, plies)
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
    fn parallel_missing_dependency_returns_error_without_generating() {
        let dir = crate::TestDir::new("parallel_missing_dependency");
        let mut generator = EgtGenerator::new(&dir.0).unwrap();
        generator.set_threads(4);
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
        generator.set_threads(0);
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
        generator.set_threads(2);
        generator.set_verify(false);
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
                let excluded = stats.invalid_opposite_check
                                    + stats.invalid_ep_squares_occupied + stats.invalid_illegal_ep_capture + stats.redundant;
                                let counts = [stats.index_range, stats.win, stats.draw, stats.loss, excluded];
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
