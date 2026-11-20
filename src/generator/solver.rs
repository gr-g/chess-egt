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
//!   `mark_win`).
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
//! fetch-max operations are sufficient within each phase. Initialization probes
//! conversion outcomes through a shared reader.

use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant};

use rayon::prelude::*;
use shakmaty::{Color, Position};

use super::WinLoss;
use super::unmoves::{quiet_unmoves, symmetry_adjusted_move_counter};
use crate::ConversionType;
use crate::egt::Egt;
use crate::egt_file::BatchReader;
use crate::error::{EgtError, EgtResult};
use crate::outcome::PackedOutcome;
use crate::position::InvalidCounts;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WinUpdate {
    Unchanged,
    NewlyResolved,
    Upgraded,
}

fn load(cell: &AtomicU16) -> PackedOutcome {
    PackedOutcome::from_u16(cell.load(Ordering::Relaxed))
}

/// CAS preserves the best same-ply conversion even when workers race.
fn mark_win(cell: &AtomicU16, ct: ConversionType, plies: u16) -> WinUpdate {
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
fn decrement(cell: &AtomicU16, ct: ConversionType, plies: u16) -> bool {
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
    block_size: usize,
    blocks: Vec<BlockSummary>,
}

impl PlySummary {
    fn new(size: usize, block_size: usize) -> Self {
        Self { size, block_size, blocks: (0..size.div_ceil(block_size)).map(|_| BlockSummary::default()).collect() }
    }

    fn record(&self, local_index: usize, kind: SummaryKind, plies: u16) {
        let last = &self.blocks[local_index / self.block_size].ply_bounds[kind.index()];
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
                let start = b * self.block_size;
                (start, (start + self.block_size).min(self.size))
            })
            .collect()
    }

    fn num_blocks(&self) -> usize {
        self.blocks.len()
    }
}

/// Collect each block's matches before updates, including for a self-pair.
fn scan_table<S, F>(
    source: &TableState,
    target: &TableState,
    blocks: &[(usize, usize)],
    select: S,
    process: F,
) -> Found
where
    S: Fn(PackedOutcome) -> bool + Sync,
    F: Fn(&TableState, usize, PackedOutcome, &mut Found) + Sync,
{
    blocks.par_iter().map(|&(start, end)| {
        let matches: Vec<_> = (start..end).filter_map(|idx| {
            let v = load(&source.values[idx]);
            select(v).then_some((idx, v))
        }).collect();
        let mut found = Found::default();
        for (idx, v) in matches {
            process(target, idx, v, &mut found);
        }
        found
    }).reduce(Found::default, Found::merge)
}

/// Number of positions of a table resolved at a given ply.
type Found = WinLoss<usize>;

impl WinLoss<usize> {
    fn merge(self, other: Self) -> Self {
        Self { win: self.win + other.win, loss: self.loss + other.loss }
    }

    fn is_empty(self) -> bool {
        self.win == 0 && self.loss == 0
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
    reader: &BatchReader,
) -> EgtResult<InitCounts> {
    let (size, pawnless) = (egt.index_range(), egt.is_pawnless());
    debug_assert_eq!(values.len(), size);
    debug_assert_eq!(summary.size, size);
    values.par_chunks_mut(summary.block_size).enumerate().map(|(block, chunk)| {
        let mut counts = InitCounts::default();
        let mut ply_bounds = [0u16; 4];
        for (offset, cell) in chunk.iter_mut().enumerate() {
            let idx = block * summary.block_size + offset;
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
                counts.found.win += 1;
                ply_bounds[SummaryKind::Win(ct).index()] = 2;
                PackedOutcome::new_win(ct, 1)
            } else if counter == 0 {
                // All moves are conversions to won positions.
                counts.found.loss += 1;
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

pub(super) struct TableState {
    egt: Egt,
    values: Vec<AtomicU16>,
    summary: PlySummary,
    invalid: InvalidCounts,
    previous: Found,
    found: Found,
    maxima: WinLoss<u16>,
}

impl TableState {
    pub(super) fn new(egt: Egt) -> EgtResult<Self> {
        let values = Self::allocate_values(egt.id(), egt.index_range())?;
        Ok(Self {
            summary: PlySummary::new(egt.index_range(), egt.summary_block_size()),
            egt,
            values,
            invalid: InvalidCounts::default(),
            previous: Found::default(),
            found: Found::default(),
            maxima: WinLoss::default(),
        })
    }

    fn allocate_values(table: impl std::fmt::Display, len: usize) -> EgtResult<Vec<AtomicU16>> {
        let context = format!("table {table} working values");
        let bytes = len.checked_mul(std::mem::size_of::<AtomicU16>()).ok_or_else(|| {
            EgtError::WorkingMemoryAllocation {
                context: context.clone(),
                bytes: len as u128 * std::mem::size_of::<AtomicU16>() as u128,
                reason: "allocation size overflow".to_string(),
            }
        })?;
        let mut values = Vec::new();
        values.try_reserve_exact(len).map_err(|err| EgtError::WorkingMemoryAllocation {
            context,
            bytes: bytes as u128,
            reason: err.to_string(),
        })?;
        values.resize_with(len, || AtomicU16::new(PackedOutcome::INVALID.to_u16()));
        Ok(values)
    }

    pub(super) fn into_parts(self) -> (Egt, Vec<AtomicU16>, WinLoss<u16>, InvalidCounts) {
        (self.egt, self.values, self.maxima, self.invalid)
    }
}

pub(super) struct PairTimings {
    pub(super) init: Duration,
    pub(super) propagation: Duration,
}

/// Solves one table or two twins, leaving the draws as unknown. Reversing
/// the slot order selects the twin, including slot 0 for a self-twin.
pub(super) fn solve_pair(
    tables: &mut [TableState],
    reflected: [bool; 2],
    reader: &BatchReader,
) -> EgtResult<PairTimings> {
    debug_assert!((1..=2).contains(&tables.len()));
    let init_start = Instant::now();
    for table in tables.iter_mut() {
        let counts = initialize_table(&table.egt, &mut table.values, &table.summary, reader)?;
        println!("{}: Initialized with {} checkmated positions, {} stalemated positions.", table.egt.id(), counts.checkmates, counts.stalemates);
        // Checkmates count as losses at ply 0, so that phase W(1) scans them.
        table.previous.loss = counts.checkmates;
        table.found = counts.found;
        table.invalid = counts.invalid;
    }

    let init_time = init_start.elapsed();
    let propagation_start = Instant::now();

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
            if table.previous.loss == 0 {
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
            let newly = scan_table(table, twin, &blocks, |v| v.is_loss() && v.plies() == plies - 1, |twin, idx, v, local| {
                let ct = v.conversion_type();
                quiet_unmoves(&table.egt, &twin.egt, idx, reflected[slot], |pred_idx| {
                    match mark_win(&twin.values[pred_idx], ct, plies) {
                        WinUpdate::NewlyResolved => {
                            local.win += 1;
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
                if table.previous.win == 0 {
                    continue;
                }
                let blocks = table.summary.blocks_with(SummaryKind::Win(ct), plies - 1);
                blocks_scanned += blocks.len();
                blocks_total += table.summary.num_blocks();
                if blocks.is_empty() {
                    continue;
                }
                let newly = scan_table(table, twin, &blocks, |v| v == target, |twin, idx, _, local| {
                    quiet_unmoves(&table.egt, &twin.egt, idx, reflected[slot], |pred_idx| {
                        if decrement(&twin.values[pred_idx], ct, plies) {
                            local.loss += 1;
                            twin.summary.record(pred_idx, SummaryKind::Loss, plies);
                        }
                    });
                });
                tables[twin_slot].found = tables[twin_slot].found.merge(newly);
            }
        }

        for table in tables.iter_mut() {
            if table.found.win > 0 {
                println!("{}: Found {} winning positions at depth {}", table.egt.id(), table.found.win, plies);
                table.maxima.win = plies;
            }
            if table.found.loss > 0 {
                println!("{}: Found {} losing positions at depth {}", table.egt.id(), table.found.loss, plies);
                table.maxima.loss = plies;
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
            tables[0].egt.id(),
            blocks_scanned,
            blocks_total,
            100.0 * blocks_scanned as f64 / blocks_total as f64,
        );
    }

    Ok(PairTimings {
        init: init_time,
        propagation: propagation_start.elapsed(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Tablebase;
    use crate::selection::plan_pairs;
    use crate::storage::Location;
    use crate::table_id::TableId;

    fn table(name: &str) -> Egt {
        Egt::new(&TableId::parse(name).unwrap()).unwrap()
    }

    fn atomic_values(values: Vec<PackedOutcome>) -> Vec<AtomicU16> {
        values.into_iter().map(|v| AtomicU16::new(v.to_u16())).collect()
    }

    fn plain_values(values: &[AtomicU16]) -> Vec<PackedOutcome> {
        values.iter().map(|v| PackedOutcome::from_u16(v.load(Ordering::Relaxed))).collect()
    }

    #[test]
    fn table_values_report_allocation_capacity_overflow() {
        let len = usize::MAX;
        let err = TableState::allocate_values("oversized", len).err().unwrap();
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
            let values = atomic_values(vec![PackedOutcome::new_unknown(3)]);
            let mut preferred = ConversionType::Promotion;
            for (i, ct) in order.into_iter().enumerate() {
                let expected = if i == 0 {
                    WinUpdate::NewlyResolved
                } else if ct > preferred {
                    WinUpdate::Upgraded
                } else {
                    WinUpdate::Unchanged
                };
                assert_eq!(mark_win(&values[0], ct, 7), expected);
                preferred = preferred.max(ct);
                assert_eq!(load(&values[0]), PackedOutcome::new_win(preferred, 7));
            }
            assert_eq!(load(&values[0]), PackedOutcome::new_win(ConversionType::Checkmate, 7));
            assert_eq!(mark_win(&values[0], ConversionType::Checkmate, 7), WinUpdate::Unchanged);
        }

        let existing = vec![
            PackedOutcome::new_win(ConversionType::Promotion, 2),
            PackedOutcome::new_loss(ConversionType::Capture, 4),
            PackedOutcome::DRAW,
            PackedOutcome::INVALID,
        ];
        let values = atomic_values(existing.clone());
        for cell in &values {
            assert_eq!(mark_win(cell, ConversionType::Checkmate, 3), WinUpdate::Unchanged);
        }
        assert_eq!(plain_values(&values), existing);
    }

    #[test]
    fn pair_local_decrement_resolves_only_once_and_preserves_other_slot() {
        let untouched = vec![PackedOutcome::new_unknown(5)];
        let mut tables = [
            table_with_values(vec![PackedOutcome::new_unknown(3)]),
            table_with_values(untouched.clone()),
        ];
        let values = &tables[0].values;
        assert!(!decrement(&values[0], ConversionType::Checkmate, 8));
        assert_eq!(load(&values[0]), PackedOutcome::new_unknown(2));
        assert!(!decrement(&values[0], ConversionType::Capture, 8));
        assert_eq!(load(&values[0]), PackedOutcome::new_unknown(1));
        assert!(decrement(&values[0], ConversionType::Promotion, 8));
        assert_eq!(load(&values[0]), PackedOutcome::new_loss(ConversionType::Promotion, 8));
        assert!(!decrement(&values[0], ConversionType::Checkmate, 9));
        assert_eq!(load(&values[0]), PackedOutcome::new_loss(ConversionType::Promotion, 8));
        assert_eq!(plain_values(&tables[1].values), untouched);

        let assigned = vec![
            PackedOutcome::new_win(ConversionType::Capture, 2),
            PackedOutcome::DRAW,
            PackedOutcome::INVALID,
        ];
        tables[0].values = atomic_values(assigned.clone());
        let values = &tables[0].values;
        for cell in values {
            assert!(!decrement(cell, ConversionType::Promotion, 3));
        }
        assert_eq!(plain_values(values), assigned);
    }

    #[test]
    fn pair_local_ply_summary_clips_tails_and_keeps_maximum() {
        let block = 10;
        let summary = PlySummary::new(2 * block + 3, block);
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
        let empty = PlySummary::new(0, block);
        assert_eq!(empty.num_blocks(), 0);
        assert!(empty.blocks_with(loss, 0).is_empty());
    }

    #[test]
    fn initialization_populates_only_matching_summary_kinds() {
        let dir = crate::TestDir::new("initial_summary");
        Tablebase::new(&dir.0).unwrap().generate(&["K_K"]).unwrap();
        let location = Location::parse(&dir.0).unwrap();
        let source = crate::egt_file::Source::new(location);
        let reader = BatchReader::open(&source, &[TableId::parse("K_K").unwrap()]).unwrap();
        for name in ["K_KQ", "KQ_K"] {
            let egt = table(name);
            let mut values = atomic_values(vec![PackedOutcome::INVALID; egt.index_range()]);
            let summary = PlySummary::new(values.len(), egt.summary_block_size());
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
            let expected: Vec<_> = values.chunks(egt.summary_block_size()).enumerate()
                .filter(|(_, chunk)| chunk.iter().any(|cell| PackedOutcome::from_u16(cell.load(Ordering::Relaxed)).is_loss()))
                .map(|(block, chunk)| {
                    let start = block * egt.summary_block_size();
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
        let pair = &plan_pairs([TableId::parse("K_K").unwrap()])[0];
        let mut tables = std::iter::once(&pair.table).chain(pair.twin.iter())
            .map(|id| TableState::new(Egt::new(id).unwrap()).unwrap())
            .collect::<Vec<_>>();
        let reader = BatchReader::default();
        solve_pair(&mut tables, pair.reflected, &reader).unwrap();
        assert_eq!(tables.len(), 1);
        let table = &tables[0];
        assert_eq!(table.invalid, InvalidCounts::default());
        assert_eq!(table.values.len(), table.egt.index_range());
        assert!(plain_values(&table.values).iter().all(|v| v.is_unknown() || v.is_draw()));
        assert_eq!((table.maxima.win, table.maxima.loss), (0, 0));
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
            assert_eq!(twin.name().as_str(), twin_name);
            assert_eq!(actual_reflection, reflected);
            for selected in [id, twin] {
                let pairs = plan_pairs([selected]);
                let pair = &pairs[0];
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

    fn table_with_values(values: Vec<PackedOutcome>) -> TableState {
        let mut table = TableState::new(table("K_K")).unwrap();
        table.summary = PlySummary::new(values.len(), table.summary.block_size);
        table.values = atomic_values(values);
        table
    }

    #[test]
    fn pair_local_self_twin_scan_snapshots_matches_before_updates() {
        let win = PackedOutcome::new_win(ConversionType::Capture, 1);
        let table = table_with_values(vec![win, win, PackedOutcome::new_unknown(1)]);
        let values = &table.values;
        let visited = std::sync::Mutex::new(Vec::new());
        scan_table(&table, &table, &[(0, 3)], |v| v == win, |target, idx, v, _| {
            visited.lock().unwrap().push((idx, v));
            target.values[1].store(PackedOutcome::DRAW.to_u16(), Ordering::Relaxed);
            mark_win(&target.values[2], ConversionType::Capture, 2);
        });
        assert_eq!(*visited.lock().unwrap(), vec![(0, win), (1, win)]);
        assert_eq!(values.len(), 3);
        assert_eq!(load(&values[2]), PackedOutcome::new_win(ConversionType::Capture, 2));
    }

    #[test]
    fn concurrent_cas_updates_resolve_once() {
        let pool = rayon::ThreadPoolBuilder::new().num_threads(4).build().unwrap();
        pool.install(|| {
            for _ in 0..32 {
                let values = atomic_values(vec![PackedOutcome::new_unknown(96)]);
                let wins: usize = (0..96).into_par_iter().map(|i| {
                    let ct = [ConversionType::Promotion, ConversionType::Capture, ConversionType::Checkmate][i % 3];
                    usize::from(mark_win(&values[0], ct, 7) == WinUpdate::NewlyResolved)
                }).sum();
                assert_eq!(wins, 1);
                assert_eq!(load(&values[0]), PackedOutcome::new_win(ConversionType::Checkmate, 7));
                let values = atomic_values(vec![PackedOutcome::new_unknown(96)]);
                let losses: usize = (0..192).into_par_iter().map(|_| {
                    usize::from(decrement(&values[0], ConversionType::Promotion, 8))
                }).sum();
                assert_eq!(losses, 1);
                assert_eq!(load(&values[0]), PackedOutcome::new_loss(ConversionType::Promotion, 8));
            }
        });
    }

    #[test]
    fn step4_concurrent_summary_max_and_clipped_tail() {
        let block = 10;
        let summary = PlySummary::new(2 * block + 3, block);
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
            let values = atomic_values(vec![
                PackedOutcome::new_unknown(32),
                PackedOutcome::new_unknown(64),
                PackedOutcome::new_unknown(96),
            ]);
            let conversions = [ConversionType::Checkmate, ConversionType::Capture, ConversionType::Promotion];
            for (phase, ct) in conversions.into_iter().enumerate() {
                // Each reduction joins before the next conversion type starts.
                let resolved: usize = (0..96).into_par_iter().map(|i| {
                    usize::from(decrement(&values[i % 3], ct, 8))
                }).sum();
                assert_eq!(resolved, 1);
                for (idx, expected_ct) in conversions.into_iter().enumerate() {
                    let expected = if idx <= phase {
                        PackedOutcome::new_loss(expected_ct, 8)
                    } else {
                        PackedOutcome::new_unknown(32 * (idx - phase) as u16)
                    };
                    assert_eq!(load(&values[idx]), expected);
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
        let values = atomic_values(existing.clone());
        let resolved: usize = pool.install(|| {
            (0..384).into_par_iter().map(|i| {
                let ct = [ConversionType::Checkmate, ConversionType::Capture, ConversionType::Promotion][i % 3];
                usize::from(mark_win(&values[i % existing.len()], ct, 7) == WinUpdate::NewlyResolved)
            }).sum()
        });
        assert_eq!(resolved, 0);
        assert_eq!(plain_values(&values), existing);
    }

    #[test]
    fn step4_multiblock_self_twin_scan_visits_sources_once() {
        let pool = rayon::ThreadPoolBuilder::new().num_threads(4).build().unwrap();
        pool.install(|| {
            let block = 10;
            let size = 3 * block + 6;
            let plies = 7;
            for source_is_loss in [true, false] {
                let source = if source_is_loss {
                    PackedOutcome::new_loss(ConversionType::Capture, plies - 1)
                } else {
                    PackedOutcome::new_win(ConversionType::Capture, plies - 1)
                };
                let table = table_with_values((0..size).map(|idx| {
                    if idx % 2 == 0 { source } else { PackedOutcome::new_unknown(1) }
                }).collect());
                let values = &table.values;
                let visits: Vec<_> = (0..size).map(|_| AtomicU16::new(0)).collect();
                let summary = PlySummary::new(size, block);
                let kind = if source_is_loss { SummaryKind::Loss } else { SummaryKind::Win(ConversionType::Capture) };
                for idx in (0..size).step_by(block) {
                    summary.record(idx, kind, plies - 1);
                }

                let blocks = summary.blocks_with(kind, plies - 1);
                assert_eq!(blocks.len(), 4);
                assert_eq!(blocks.last(), Some(&(3 * block, size)));
                let found = scan_table(&table, &table, &blocks,
                    |v| v == source && v.plies() == plies - 1,
                    |twin, idx, v, local| {
                        assert_eq!(v, source);
                        visits[idx].fetch_add(1, Ordering::Relaxed);
                        // A bijection from even sources to odd destinations,
                        // shifted across blocks within the same working array.
                        let target = (idx + 2 * block) % size + 1;
                        if source_is_loss {
                            if mark_win(&twin.values[target], ConversionType::Capture, plies) == WinUpdate::NewlyResolved {
                                local.win += 1;
                            }
                        } else if decrement(&twin.values[target], ConversionType::Capture, plies) {
                            local.loss += 1;
                        }
                    });
                assert_eq!((found.win, found.loss), if source_is_loss { (size / 2, 0) } else { (0, size / 2) });
                let resolved = if source_is_loss {
                    PackedOutcome::new_win(ConversionType::Capture, plies)
                } else {
                    PackedOutcome::new_loss(ConversionType::Capture, plies)
                };
                for (idx, visits) in visits.iter().enumerate() {
                    assert_eq!(visits.load(Ordering::Relaxed), if idx % 2 == 0 { 1 } else { 0 });
                    assert_eq!(load(&values[idx]), if idx % 2 == 0 { source } else { resolved });
                }
                assert_eq!(values.len(), size);
            }
        });
    }
}
