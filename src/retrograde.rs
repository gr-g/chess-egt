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
//! fetch-max operations are sufficient within each phase. Dependency generation
//! remains on the coordinator after failed initialization tasks have joined.

use shakmaty::{Bitboard, Color, Chess, Position, Role};
use shakmaty::retrograde::{RetrogradeAnalysis, CastlingRetrogradeMode};
use crate::{ConversionType, EgtGenerator};
use crate::egt_file::{MaybeDtcOutcome, EgtFile, FileVec, PawnKey, reflect_files, is_canonical, mirror_horizontally, EgtFileStats, LongestDtcPosition};
use crate::error::{EgtError, EgtResult};
use crate::egt::Egt;
use crate::piece_set::{EgtRole, EgtSide};
use std::collections::{HashMap, BTreeMap, HashSet};
use std::sync::atomic::{AtomicU16, Ordering};
use rayon::prelude::*;
use crate::egt_file::SharedDependencyReader;

struct EgtFileStatsBuilder {
    endgame: String,
    index_range: usize,
    frame_size: usize,
    num_frames: usize,
    unique_positions: usize,
    win: usize,
    draw: usize,
    loss: usize,
    invalid_or_redundant: usize,
    histogram_win: BTreeMap<u16, usize>,
    histogram_loss: BTreeMap<u16, usize>,
    longest_dtc_win_candidates: Vec<LongestDtcPosition>,
    longest_dtc_loss_candidates: Vec<LongestDtcPosition>,
    max_win_dtc: u16,
    max_loss_dtc: u16,
}

impl EgtFileStatsBuilder {
    fn new(endgame: String, index_range: usize, frame_size: usize, num_frames: usize) -> Self {
        Self {
            endgame,
            index_range,
            frame_size,
            num_frames,
            unique_positions: 0,
            win: 0,
            draw: 0,
            loss: 0,
            invalid_or_redundant: 0,
            histogram_win: BTreeMap::new(),
            histogram_loss: BTreeMap::new(),
            longest_dtc_win_candidates: Vec::new(),
            longest_dtc_loss_candidates: Vec::new(),
            max_win_dtc: 0,
            max_loss_dtc: 0,
        }
    }

    fn record_outcome(
        &mut self,
        outcome: MaybeDtcOutcome,
        local_idx: usize,
        egt: &Egt,
        current_max_win_dtc: u16,
        current_max_loss_dtc: u16,
    ) {
        let sym = egt.diagonal_symmetric(local_idx);
        if let Some(other_idx) = sym && local_idx > other_idx {
            self.invalid_or_redundant += 1;
            return;
        }

        if outcome.is_win() {
            self.win += 1;
            self.unique_positions += 1;
            let ply = outcome.to_u16() >> 3;
            *self.histogram_win.entry(ply).or_insert(0) += 1;
            if ply == current_max_win_dtc && current_max_win_dtc > 0 {
                if let Some(pos) = egt.position_from_index(local_idx, Color::White) {
                    let epd = shakmaty::fen::Epd::from_position(&pos, shakmaty::EnPassantMode::Legal).to_string();
                    self.longest_dtc_win_candidates.push(LongestDtcPosition {
                        epd,
                        ply,
                    });
                }
            }
        } else if outcome.is_loss() {
            self.loss += 1;
            self.unique_positions += 1;
            let ply = outcome.to_u16() >> 3;
            *self.histogram_loss.entry(ply).or_insert(0) += 1;
            if ply == current_max_loss_dtc {
                if let Some(pos) = egt.position_from_index(local_idx, Color::White) {
                    let epd = shakmaty::fen::Epd::from_position(&pos, shakmaty::EnPassantMode::Legal).to_string();
                    self.longest_dtc_loss_candidates.push(LongestDtcPosition {
                        epd,
                        ply,
                    });
                }
            }
        } else if outcome.is_draw() {
            self.draw += 1;
            self.unique_positions += 1;
        } else if outcome.is_invalid() {
            self.invalid_or_redundant += 1;
        }
    }

    fn build(mut self, bytes: u64, sha256: String) -> EgtFileStats {
        let global_max_win = self.max_win_dtc;
        self.longest_dtc_win_candidates.retain(|p| p.ply == global_max_win);
        let global_max_loss = self.max_loss_dtc;
        self.longest_dtc_loss_candidates.retain(|p| p.ply == global_max_loss);
        EgtFileStats {
            endgame: self.endgame,
            bytes,
            sha256,
            index_range: self.index_range,
            frame_size: self.frame_size,
            num_frames: self.num_frames,
            unique_positions: self.unique_positions,
            win: self.win,
            draw: self.draw,
            loss: self.loss,
            invalid_or_redundant: self.invalid_or_redundant,
            histogram_win: self.histogram_win,
            histogram_loss: self.histogram_loss,
            longest_dtc_win: self.longest_dtc_win_candidates,
            longest_dtc_loss: self.longest_dtc_loss_candidates,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EgtHandle {
    pub file_idx: usize,
    pub egt_idx: usize,
}

// Returns the tablename of the sub-table referenced by `handle` within `solver`.
// Used for logging; the name is not stored on the handle itself.
fn table_name(solver: &RetrogradeSolver, handle: EgtHandle) -> &str {
    solver.files[handle.file_idx].egts[handle.egt_idx].tablename()
}

pub struct RetrogradeSolver {
    pub files: Vec<EgtFile>,
    pub is_symmetric: bool,
}

impl RetrogradeSolver {
    pub fn new(file_a: EgtFile, file_b: Option<EgtFile>) -> Self {
        let is_symmetric = file_b.is_none();
        let mut files = vec![file_a];
        if let Some(fb) = file_b {
            files.push(fb);
        }
        Self {
            files,
            is_symmetric,
        }
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

fn get_pawn_files(pieces: &[(EgtRole, EgtSide, usize)]) -> (FileVec, FileVec) {
    let mut stm_files = FileVec::new();
    let mut sntm_files = FileVec::new();
    for &(piece, side, multiplicity) in pieces {
        if let EgtRole::Pawn(file) = piece {
            for _ in 0..multiplicity {
                match side {
                    EgtSide::SideToMove => stm_files.push(file),
                    EgtSide::SideNotToMove => sntm_files.push(file),
                }
            }
        }
    }
    stm_files.sort_by_key(|f| f.to_usize());
    sntm_files.sort_by_key(|f| f.to_usize());
    (stm_files, sntm_files)
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

// Whether predecessors generated from `table` have to be mirrored horizontally
// before being indexed in `twin`. This depends only on the pawn files of the two
// sub-tables, so it is computed once per table pair rather than per unmove.
fn is_mirrored(table: &Egt, twin: &Egt) -> bool {
    let (stm_files_a, sntm_files_a) = get_pawn_files(table.pieces());
    let (stm_files_b, sntm_files_b) = get_pawn_files(twin.pieces());
    stm_files_b != sntm_files_a || sntm_files_b != stm_files_a
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

    RetrogradeAnalysis::new(&position)
        .with_castling_mode(CastlingRetrogradeMode::NoCastling)
        .quiet_unmoves(|mut pred_position, _m| {
            if mirrored {
                pred_position = mirror_horizontally(&pred_position)
                    .expect("mirrored predecessor position must be legal");
            }

            let pred_idx = twin.position_to_index(&pred_position);
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
        });
}

fn symmetry_adjusted_move_counter(position: &Chess) -> u16 {
    let legals = position.legal_moves();
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
    for m in &legals {
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

pub struct DependencyCache {
    pub cache: HashMap<String, EgtFile>,
    pub base_path: std::path::PathBuf,
    /// Additional path used to look up dependency files. If `None`, only
    /// `base_path` is consulted.
    pub input_path: Option<std::path::PathBuf>,
    /// Whether missing dependencies should be generated on the fly. When
    /// `false`, a missing dependency results in a `DependencyUnavailable`
    /// error instead of triggering a recursive generation.
    pub generate_deps: bool,
}

impl DependencyCache {
    pub fn with_options(
        base_path: &std::path::Path,
        input_path: Option<&std::path::Path>,
        generate_deps: bool,
    ) -> Self {
        Self {
            cache: HashMap::new(),
            base_path: base_path.to_path_buf(),
            input_path: input_path.map(|p| p.to_path_buf()),
            generate_deps,
        }
    }

    /// Attempts to load an existing dependency table, looking it up first in
    /// `input_path` (if set) and then in `base_path`.
    fn load_existing(&self, endgame: &str) -> EgtResult<EgtFile> {
        crate::load_existing_file(&self.base_path, self.input_path.as_deref(), endgame)
    }

    pub fn get_or_load(&mut self, endgame: &str) -> EgtResult<&mut EgtFile> {
        if !self.cache.contains_key(endgame) {
            let file = match self.load_existing(endgame) {
                Ok(f) => f,
                Err(EgtError::FileNotFound(_)) => {
                    if !self.generate_deps {
                        return Err(EgtError::DependencyUnavailable {
                            dependency: endgame.to_string(),
                            source: Box::new(EgtError::FileNotFound(
                                self.base_path.join(format!("{}.ggegt", endgame)),
                            )),
                        });
                    }
                    println!("Dependency endgame {} not found. Generating on the fly...", endgame);
                    let mut g = EgtGenerator::new(&self.base_path);
                    // Propagate lookup options so transitively-missing
                    // dependencies can also be resolved. Generated files are
                    // always written to `base_path` (never to `input_path`).
                    if let Some(ref input) = self.input_path {
                        g.with_input_path(input.clone());
                    }
                    g.with_generate_deps(self.generate_deps);
                    g.generate_in_pool(endgame).map_err(|source| EgtError::DependencyUnavailable {
                        dependency: endgame.to_string(),
                        source: Box::new(source),
                    })?;
                    // The freshly generated file lives under `base_path`.
                    EgtFile::new_from_file(&self.base_path, endgame)?
                }
                Err(e) => return Err(e),
            };
            self.cache.insert(endgame.to_string(), file);
        }
        self.cache.get_mut(endgame).ok_or(EgtError::Internal("cache entry missing after insert"))
    }
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

/// Solves the pair of sub-tables `table_a` and `table_b` (the same sub-table
/// with the other side to move), leaving the draws as unknown.
fn solve_pair(
    solver: &RetrogradeSolver,
    table_a: EgtHandle,
    table_b: EgtHandle,
    dep_cache: &mut DependencyCache,
) -> EgtResult<PairResult> {
    let init_start = std::time::Instant::now();
    let egt = |table: EgtHandle| &solver.files[table.file_idx].egts[table.egt_idx];

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
    let mut previous;
    let mut found_by_conversion;
    let mut attempted = HashSet::new();
    loop {
        // A failed attempt may leave partially initialized chunks. Reset all
        // slots and counts, including tables completed before the failure.
        previous = [Found::default(); 2];
        found_by_conversion = [Found::default(); 2];
        for values in &mut working.values {
            for cell in values {
                *cell.get_mut() = MaybeDtcOutcome::INVALID.to_u16();
            }
        }
        let reader = SharedDependencyReader::new(
            std::mem::take(&mut dep_cache.cache), &dep_cache.base_path,
            dep_cache.input_path.as_deref(),
        );
        let result = (|| -> EgtResult<()> {
            for &(table, _, _, slot, _) in &tables {
                let counts = initialize_table(egt(table), &mut working.values[slot], &reader)?;
                println!("{}: Initialized with {} checkmated positions, {} stalemated positions.", table_name(solver, table), counts.checkmates, counts.stalemates);
                previous[slot].losses = counts.checkmates;
                found_by_conversion[slot] = counts.found;
            }
            Ok(())
        })();
        // Rayon has joined every task before ownership of warmed files returns.
        dep_cache.cache = reader.into_cache()?;
        match result {
            Ok(()) => break,
            Err(EgtError::DependencyUnavailable { dependency, source }) if dep_cache.generate_deps => {
                if !attempted.insert(dependency.clone()) {
                    return Err(EgtError::DependencyUnavailable { dependency, source });
                }
                dep_cache.get_or_load(&dependency)?;
            }
            Err(error) => return Err(error),
        }
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

pub fn retrograde_analysis(
    base_path: &std::path::Path,
    endgame: &str,
    input_path: Option<&std::path::Path>,
    generate_deps: bool,
) -> EgtResult<(EgtFile, Option<EgtFile>)> {
    retrograde_analysis_with_threads(base_path, endgame, input_path, generate_deps, 1)
}

/// Generates tables in a dedicated Rayon pool. Zero threads is rejected.
pub fn retrograde_analysis_with_threads(
    base_path: &std::path::Path,
    endgame: &str,
    input_path: Option<&std::path::Path>,
    generate_deps: bool,
    threads: usize,
) -> EgtResult<(EgtFile, Option<EgtFile>)> {
    if threads == 0 {
        return Err(EgtError::Internal("thread count must be positive"));
    }
    let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build()
        .map_err(|_| EgtError::Internal("failed to build retrograde thread pool"))?;
    pool.install(|| retrograde_analysis_in_pool(base_path, endgame, input_path, generate_deps))
}

pub(crate) fn retrograde_analysis_in_pool(
    base_path: &std::path::Path,
    endgame: &str,
    input_path: Option<&std::path::Path>,
    generate_deps: bool,
) -> EgtResult<(EgtFile, Option<EgtFile>)> {
    let parts: Vec<&str> = endgame.split('_').collect();
    if parts.len() != 2 {
        return Err(EgtError::InvalidEndgameName {
            name: endgame.to_string(),
            reason: "expected exactly one '_' separator",
        });
    }
    let twin_endgame = format!("{}_{}", parts[1], parts[0]);

    let is_symmetric = endgame == twin_endgame;

    let file_a = EgtFile::new(base_path, endgame)?;
    let file_b = if is_symmetric {
        None
    } else {
        Some(EgtFile::new(base_path, &twin_endgame)?)
    };

    let solver = RetrogradeSolver::new(file_a, file_b);
    let mut writers = solver.files.iter()
        .map(crate::egt_writer::EgtFileWriter::new)
        .collect::<EgtResult<Vec<_>>>()?;

    // Match Egt sub-tables into pairs
    let mut table_pairs = Vec::new();

    for (egt_idx_a, egt_a) in solver.files[0].egts.iter().enumerate() {
        let (stm_files, sntm_files) = get_pawn_files(egt_a.pieces());
        let (twin_stm, twin_sntm) = if is_canonical(&sntm_files, &stm_files) {
            (sntm_files, stm_files)
        } else {
            (reflect_files(&sntm_files), reflect_files(&stm_files))
        };
        let twin_key = PawnKey::new(&twin_stm, &twin_sntm);

        let (file_idx_b, egt_idx_b) = if solver.is_symmetric {
            let egt_idx_b = *solver.files[0].egt_map.get(&twin_key)
                .expect("twin sub-table must exist in symmetric endgame");
            (0, egt_idx_b)
        } else {
            let egt_idx_b = *solver.files[1].egt_map.get(&twin_key)
                .expect("twin sub-table must exist in asymmetric endgame");
            (1, egt_idx_b)
        };

        if solver.is_symmetric && egt_idx_a > egt_idx_b {
            continue;
        }

        let table_a = EgtHandle {
            file_idx: 0,
            egt_idx: egt_idx_a,
        };

        let table_b = EgtHandle {
            file_idx: file_idx_b,
            egt_idx: egt_idx_b,
        };

        table_pairs.push((table_a, table_b));
    }

    // Initialization & Propagation Phase for each independent pair
    let mut dep_cache = DependencyCache::with_options(base_path, input_path, generate_deps);
    let mut stats_builder_a = EgtFileStatsBuilder::new(
        solver.files[0].endgame.clone(),
        solver.files[0].index_range,
        solver.files[0].frame_size,
        solver.files[0].num_frames(),
    );
    let mut stats_builder_b = if is_symmetric {
        None
    } else {
        Some(EgtFileStatsBuilder::new(
            solver.files[1].endgame.clone(),
            solver.files[1].index_range,
            solver.files[1].frame_size,
            solver.files[1].num_frames(),
        ))
    };

    let mut init_time = std::time::Duration::ZERO;
    let mut propagation_time = std::time::Duration::ZERO;
    let mut finalize_time = std::time::Duration::ZERO;

    for &(table_a, table_b) in &table_pairs {
        let mut pair = solve_pair(&solver, table_a, table_b, &mut dep_cache)?;
        init_time += pair.init_time;
        propagation_time += pair.propagation_time;
        let current_max_win_dtc_a = pair.max_win_dtc_a;
        let current_max_loss_dtc_a = pair.max_loss_dtc_a;
        let current_max_win_dtc_b = pair.max_win_dtc_b;
        let current_max_loss_dtc_b = pair.max_loss_dtc_b;

        stats_builder_a.max_win_dtc = stats_builder_a.max_win_dtc.max(current_max_win_dtc_a);
        stats_builder_a.max_loss_dtc = stats_builder_a.max_loss_dtc.max(current_max_loss_dtc_a);
        if let Some(ref mut builder_b) = stats_builder_b {
            builder_b.max_win_dtc = builder_b.max_win_dtc.max(current_max_win_dtc_b);
            builder_b.max_loss_dtc = builder_b.max_loss_dtc.max(current_max_loss_dtc_b);
        } else {
            stats_builder_a.max_win_dtc = stats_builder_a.max_win_dtc.max(current_max_win_dtc_b);
            stats_builder_a.max_loss_dtc = stats_builder_a.max_loss_dtc.max(current_max_loss_dtc_b);
        }

        let finalize_start = std::time::Instant::now();

        let mut values_a: Vec<_> = std::mem::take(&mut pair.working.values[0]).into_iter()
            .map(|cell| MaybeDtcOutcome::from_u16(cell.into_inner())).collect();
        // Mark all remaining 'unknown' positions as draws
        println!("{}: marking remaining positions as draws...", table_name(&solver, table_a));
        let size_a = solver.files[table_a.file_idx].egts[table_a.egt_idx].index_range();
        for idx in 0..size_a {
            let mut outcome = values_a[idx];
            if outcome.is_unknown() {
                outcome = MaybeDtcOutcome::DRAW;
                values_a[idx] = outcome;
            }

            stats_builder_a.record_outcome(
                outcome,
                idx,
                &solver.files[table_a.file_idx].egts[table_a.egt_idx],
                current_max_win_dtc_a,
                current_max_loss_dtc_a,
            );
        }

        writers[table_a.file_idx].write_table(table_a.egt_idx, &values_a)?;
        drop(values_a);
        if table_a != table_b {
            let mut values_b: Vec<_> = std::mem::take(&mut pair.working.values[1]).into_iter()
                .map(|cell| MaybeDtcOutcome::from_u16(cell.into_inner())).collect();
            println!("{}: marking remaining positions as draws...", table_name(&solver, table_b));
            let size_b = solver.files[table_b.file_idx].egts[table_b.egt_idx].index_range();
            for idx in 0..size_b {
                let mut outcome = values_b[idx];
                if outcome.is_unknown() {
                    outcome = MaybeDtcOutcome::DRAW;
                    values_b[idx] = outcome;
                }

                if let Some(ref mut builder_b) = stats_builder_b {
                    builder_b.record_outcome(
                        outcome,
                        idx,
                        &solver.files[table_b.file_idx].egts[table_b.egt_idx],
                        current_max_win_dtc_b,
                        current_max_loss_dtc_b,
                    );
                } else {
                    stats_builder_a.record_outcome(
                        outcome,
                        idx,
                        &solver.files[table_b.file_idx].egts[table_b.egt_idx],
                        current_max_win_dtc_b,
                        current_max_loss_dtc_b,
                    );
                }
            }
            writers[table_b.file_idx].write_table(table_b.egt_idx, &values_b)?;
        }
        // The pair arrays are dropped here, before allocating the next pair.
        finalize_time += finalize_start.elapsed();
    }

    println!(
        "{}: phase timings: init {:.3}s, propagation {:.3}s, finalize {:.3}s",
        endgame,
        init_time.as_secs_f64(),
        propagation_time.as_secs_f64(),
        finalize_time.as_secs_f64(),
    );

    let assembly_start = std::time::Instant::now();
    for writer in &mut writers {
        writer.finish()?;
    }
    // Each rename is atomic, but publishing the two destination files is not.
    for writer in &mut writers {
        writer.publish()?;
    }
    println!("{}: output assembly {:.3}s", endgame, assembly_start.elapsed().as_secs_f64());

    let mut files = solver.files.into_iter().map(|file| {
        EgtFile::new_from_file(base_path, &file.endgame)
    }).collect::<EgtResult<Vec<_>>>()?;
    let mut file_a = files.remove(0);
    let mut file_b = if is_symmetric { None } else { Some(files.remove(0)) };

    file_a.stats = Some(stats_builder_a.build(0, String::new()));
    if let Some(ref mut fb) = file_b && let Some(builder_b) = stats_builder_b {
        fb.stats = Some(builder_b.build(0, String::new()));
    }

    Ok((file_a, file_b))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let solver = RetrogradeSolver::new(EgtFile::new(&dir.0, "K_K").unwrap(), None);
        let table = EgtHandle { file_idx: 0, egt_idx: 0 };
        let mut deps = DependencyCache::with_options(&dir.0, None, false);
        let pair = solve_pair(&solver, table, table, &mut deps).unwrap();
        assert_eq!(pair.working.values.len(), 1);
        assert_eq!(pair.working.values[0].len(), solver.files[0].egts[0].index_range());
        assert!(plain_values(&pair.working.values[0]).iter().all(|v| v.is_unknown() || v.is_draw()));
        assert_eq!((pair.max_win_dtc_a, pair.max_loss_dtc_a, pair.max_win_dtc_b, pair.max_loss_dtc_b), (0, 0, 0, 0));
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
        for endgame in ["K_K", "KQ_K", "KP_K"] {
            let serial = crate::TestDir::new(&format!("serial_{endgame}"));
            let parallel = crate::TestDir::new(&format!("parallel_{endgame}"));
            let (a, b) = retrograde_analysis_with_threads(&serial.0, endgame, None, true, 1).unwrap();
            let (pa, pb) = retrograde_analysis_with_threads(&parallel.0, endgame, None, true, 4).unwrap();
            for (file, parallel_file) in std::iter::once((a, pa)).chain(b.into_iter().zip(pb)) {
                let name = format!("{}.ggegt", file.endgame);
                assert_eq!(std::fs::read(serial.0.join(&name)).unwrap(), std::fs::read(parallel.0.join(&name)).unwrap(), "{name}");
                let stats = file.stats.unwrap();
                let parallel_stats = parallel_file.stats.unwrap();
                assert_eq!(serde_json::to_value(stats).unwrap(), serde_json::to_value(parallel_stats).unwrap());
            }
        }
    }

    #[test]
    fn parallel_missing_dependency_does_not_generate_when_disabled() {
        let dir = crate::TestDir::new("parallel_missing_dependency");
        let result = retrograde_analysis_with_threads(&dir.0, "KQ_K", None, false, 4);
        assert!(matches!(result, Err(EgtError::DependencyUnavailable { dependency, .. }) if dependency == "K_K"));
        assert!(!dir.0.join("K_K.ggegt").exists());
    }

    #[test]
    fn rejects_zero_threads() {
        let dir = crate::TestDir::new("zero_threads");
        assert!(matches!(retrograde_analysis_with_threads(&dir.0, "K_K", None, false, 0), Err(EgtError::Internal(_))));
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
        let (file_a, file_b) = retrograde_analysis(&test_dir.0, endgame, None, true).unwrap();

        let stats_a = file_a.stats.as_ref().expect("file_a should have stats");
        println!("{} stats:", file_a.endgame);
        println!("  Draws: {}", stats_a.draw);
        println!("  Wins: {}", stats_a.win);
        println!("  Losses: {}", stats_a.loss);
        println!("  Invalids: {}", stats_a.invalid_or_redundant);

        assert_eq!(file_a.index_range, expected_range_a);
        assert_eq!(stats_a.win, expected_wins_a);
        assert_eq!(stats_a.draw, expected_draws_a);
        assert_eq!(stats_a.loss, expected_losses_a);
        assert_eq!(stats_a.invalid_or_redundant, expected_invalid_a);

        if let Some(expected_range_b) = expected_range_b {
            let file_b = file_b.expect("file_b should be present");
            let stats_b = file_b.stats.as_ref().expect("file_b should have stats");
            println!("{} stats:", file_b.endgame);
            println!("  Draws: {}", stats_b.draw);
            println!("  Wins: {}", stats_b.win);
            println!("  Losses: {}", stats_b.loss);
            println!("  Invalids: {}", stats_b.invalid_or_redundant);

            assert_eq!(file_b.index_range, expected_range_b);
            assert_eq!(stats_b.win, expected_wins_b.unwrap());
            assert_eq!(stats_b.draw, expected_draws_b.unwrap());
            assert_eq!(stats_b.loss, expected_losses_b.unwrap());
            assert_eq!(stats_b.invalid_or_redundant, expected_invalid_b.unwrap());
        } else {
            assert!(file_b.is_none());
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
