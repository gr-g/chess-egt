//! Deterministic per-table statistics, merged in contiguous batch order.

use shakmaty::{Color, EnPassantMode, fen::Epd};

use super::WinLoss;
use crate::egt::Egt;
use crate::outcome::PackedOutcome;
use crate::position::InvalidCounts;
use crate::stats::{EgtStats, LongestDtcPosition, Totals};

/// Statistics for a contiguous batch; candidate indices are local to its table.
#[derive(Default)]
pub(super) struct StatsAccumulator {
    totals: Totals,
    candidates: WinLoss<Vec<usize>>,
}

impl StatsAccumulator {
    /// Use the same table maxima for every record in a batch and for its merge.
    pub(super) fn record_outcome(
        &mut self,
        outcome: PackedOutcome,
        local_idx: usize,
        egt: &Egt,
        maxima: WinLoss<u16>,
    ) {
        // Statistics exclude the redundant half even though compression retains it.
        if let Some(other_idx) = egt.diagonal_symmetric(local_idx)
            && local_idx > other_idx
        {
            self.totals.redundant += 1;
            return;
        }
        let (histogram, candidates, max) = if outcome.is_win() {
            self.totals.win += 1;
            (
                &mut self.totals.histogram_win,
                &mut self.candidates.win,
                (maxima.win > 0).then_some(maxima.win),
            )
        } else if outcome.is_loss() {
            self.totals.loss += 1;
            (
                &mut self.totals.histogram_loss,
                &mut self.candidates.loss,
                Some(maxima.loss),
            )
        } else {
            if outcome.is_draw() {
                self.totals.draw += 1;
            }
            return;
        };
        let ply = outcome.plies();
        *histogram.entry(ply).or_default() += 1;
        if max == Some(ply) {
            candidates.push(local_idx);
        }
    }

    /// Merge contiguous batches in visitation order to preserve example order.
    /// All batches belong to this one table and use its fixed maxima.
    pub(super) fn merge(&mut self, parts: impl IntoIterator<Item = StatsAccumulator>) {
        for part in parts {
            self.totals.add(&part.totals);
            self.candidates.win.extend(part.candidates.win);
            self.candidates.loss.extend(part.candidates.loss);
        }
    }

    pub(super) fn build(
        self,
        egt: &Egt,
        maxima: WinLoss<u16>,
        invalid: InvalidCounts,
        bytes: u64,
        sha256: String,
    ) -> EgtStats {
        let materialize = |candidates: Vec<usize>, ply| {
            candidates
                .into_iter()
                .filter_map(|local_idx| {
                    egt.position_from_index(local_idx, Color::White)
                        .map(|pos| LongestDtcPosition {
                            epd: Epd::from_position(&pos, EnPassantMode::Legal).to_string(),
                            ply,
                        })
                })
                .collect()
        };
        let mut totals = self.totals;
        totals.bytes = bytes;
        totals.num_frames = egt.index_range().div_ceil(egt.frame_size());
        totals.unique_positions = totals.win + totals.draw + totals.loss;
        totals.invalid_opposite_check = invalid.opposite_check;
        totals.invalid_ep_squares_occupied = invalid.ep_squares_occupied;
        totals.invalid_illegal_ep_capture = invalid.illegal_ep_capture;
        totals.longest_dtc_win = materialize(self.candidates.win, maxima.win);
        totals.longest_dtc_loss = materialize(self.candidates.loss, maxima.loss);
        EgtStats {
            tablename: egt.id().to_string(),
            sha256,
            index_range: egt.index_range(),
            frame_size: egt.frame_size(),
            totals,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::ConversionType;
    use crate::table_id::TableId;

    fn table(name: &str) -> Egt {
        Egt::new(&TableId::parse(name).unwrap()).unwrap()
    }

    #[test]
    fn merge_uses_table_maxima_and_preserves_batch_visitation_order() {
        let egt = table("KQ_K");
        let idx: Vec<_> = (0..egt.index_range())
            .filter(|&idx| {
                !egt.diagonal_symmetric(idx).is_some_and(|other| idx > other)
                    && egt.position_from_index(idx, Color::White).is_some()
            })
            .take(6)
            .collect();
        assert_eq!(idx.len(), 6);
        let part = |w, l, win, loss| {
            let mut stats = StatsAccumulator::default();
            for (local_idx, outcome) in [
                (w, PackedOutcome::new_win(ConversionType::Capture, win)),
                (l, PackedOutcome::new_loss(ConversionType::Checkmate, loss)),
                (w, PackedOutcome::DRAW),
                // A synthetic invalid outcome at a legal index must not be decoded.
                (w, PackedOutcome::INVALID),
            ] {
                stats.record_outcome(outcome, local_idx, &egt, WinLoss { win: 5, loss: 3 });
            }
            stats
        };
        let mut totals = StatsAccumulator::default();
        totals.merge([
            part(idx[0], idx[1], 0, 0),
            part(idx[4], idx[3], 5, 3),
            part(idx[2], idx[1], 5, 3),
        ]);
        totals.merge([part(idx[0], idx[1], 4, 2), part(idx[0], idx[5], 5, 3)]);
        assert_eq!(totals.candidates.win, [idx[4], idx[2], idx[0]]);
        assert_eq!(totals.candidates.loss, [idx[3], idx[1], idx[5]]);
        assert_eq!(
            (totals.totals.win, totals.totals.draw, totals.totals.loss),
            (5, 5, 5)
        );
        assert_eq!(
            totals.totals.histogram_win,
            BTreeMap::from([(0, 1), (4, 1), (5, 3)])
        );
        assert_eq!(
            totals.totals.histogram_loss,
            BTreeMap::from([(0, 1), (2, 1), (3, 3)])
        );
    }

    #[test]
    fn supplied_invalid_counts_survive_merges_and_build() {
        let egt = table("K_K");
        let invalid = InvalidCounts {
            opposite_check: 7,
            ep_squares_occupied: 11,
            illegal_ep_capture: 13,
        };
        let mut totals = StatsAccumulator::default();
        for _ in 0..2 {
            let mut part = StatsAccumulator::default();
            part.record_outcome(PackedOutcome::INVALID, 0, &egt, WinLoss::default());
            totals.merge([part, StatsAccumulator::default()]);
        }
        assert_eq!(totals.totals.redundant, 0);
        assert_eq!(
            totals.totals.win + totals.totals.draw + totals.totals.loss,
            0
        );
        assert!(totals.totals.histogram_win.is_empty());
        assert!(totals.totals.histogram_loss.is_empty());
        assert!(totals.candidates.win.is_empty());
        assert!(totals.candidates.loss.is_empty());
        let stats = totals.build(&egt, WinLoss::default(), invalid, 0, String::new());
        assert_eq!(stats.frame_size, egt.frame_size());
        assert_eq!(
            stats.totals.num_frames,
            egt.index_range().div_ceil(egt.frame_size())
        );
        assert_eq!(stats.totals.invalid_opposite_check, invalid.opposite_check);
        assert_eq!(
            stats.totals.invalid_ep_squares_occupied,
            invalid.ep_squares_occupied
        );
        assert_eq!(
            stats.totals.invalid_illegal_ep_capture,
            invalid.illegal_ep_capture
        );
        assert_eq!(stats.totals.unique_positions, 0);
    }

    #[test]
    fn checkmate_examples_include_zero_but_zero_distance_wins_do_not() {
        let egt = table("KQ_K");
        let idx = (0..egt.index_range())
            .find(|&idx| {
                !egt.diagonal_symmetric(idx).is_some_and(|other| idx > other)
                    && egt.position_from_index(idx, Color::White).is_some()
            })
            .unwrap();
        let mut stats = StatsAccumulator::default();
        stats.record_outcome(
            PackedOutcome::new_win(ConversionType::Capture, 0),
            idx,
            &egt,
            WinLoss::default(),
        );
        stats.record_outcome(
            PackedOutcome::new_loss(ConversionType::Checkmate, 0),
            idx,
            &egt,
            WinLoss::default(),
        );
        assert!(stats.candidates.win.is_empty());
        assert_eq!(stats.candidates.loss, [idx]);
        assert_eq!(stats.totals.histogram_win, BTreeMap::from([(0, 1)]));
        assert_eq!(stats.totals.histogram_loss, BTreeMap::from([(0, 1)]));
    }

    #[test]
    fn diagonal_redundancy_is_excluded_from_histograms_and_candidates() {
        let egt = table("KQ_K");
        let idx = (0..egt.index_range())
            .find(|&idx| {
                egt.diagonal_symmetric(idx).is_some_and(|other| idx > other)
                    && egt.position_from_index(idx, Color::White).is_none()
            })
            .unwrap();
        let mut stats = StatsAccumulator::default();
        let maxima = WinLoss { win: 5, loss: 0 };
        stats.record_outcome(
            PackedOutcome::new_win(ConversionType::Checkmate, 5),
            idx,
            &egt,
            maxima,
        );
        stats.record_outcome(PackedOutcome::INVALID, idx, &egt, maxima);
        assert_eq!(stats.totals.redundant, 2);
        assert_eq!(stats.totals.win, 0);
        assert!(stats.totals.histogram_win.is_empty());
        assert!(stats.candidates.win.is_empty());
    }
}
