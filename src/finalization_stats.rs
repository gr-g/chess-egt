use std::collections::BTreeMap;

use shakmaty::{Color, EnPassantMode, fen::Epd};

use crate::egt::Egt;
use crate::egt_file::{EgtFile, EgtFileStats, LongestDtcPosition, MaybeDtcOutcome};

/// Statistics for a contiguous batch; candidate indices are local to its table.
#[derive(Default)]
pub(crate) struct TableStats {
    win: usize,
    draw: usize,
    loss: usize,
    invalid_or_redundant: usize,
    histograms: [BTreeMap<u16, usize>; 2],
    candidates: [Vec<usize>; 2],
}

impl TableStats {
    /// Use the same table maxima for every record in a batch and for its merge.
    pub(crate) fn record_outcome(
        &mut self,
        outcome: MaybeDtcOutcome,
        local_idx: usize,
        egt: &Egt,
        max_win: u16,
        max_loss: u16,
    ) {
        // Statistics exclude the redundant half even though compression retains it.
        if let Some(other_idx) = egt.diagonal_symmetric(local_idx)
            && local_idx > other_idx
        {
            self.invalid_or_redundant += 1;
            return;
        }
        let side = if outcome.is_win() {
            self.win += 1;
            0
        } else if outcome.is_loss() {
            self.loss += 1;
            1
        } else {
            if outcome.is_draw() {
                self.draw += 1;
            } else if outcome.is_invalid() {
                self.invalid_or_redundant += 1;
            }
            return;
        };
        let ply = outcome.to_u16() >> 3;
        *self.histograms[side].entry(ply).or_default() += 1;
        if (side == 0 && ply == max_win && max_win > 0) || (side == 1 && ply == max_loss) {
            self.candidates[side].push(local_idx);
        }
    }
}

#[derive(Default)]
pub(crate) struct EgtFileStatsBuilder {
    totals: TableStats,
    maxima: [u16; 2],
    candidates: [Vec<(usize, usize)>; 2],
}

impl EgtFileStatsBuilder {
    /// Merge batches and tables in visitation order, never sorted by table index.
    pub(crate) fn merge_table(
        &mut self,
        egt_idx: usize,
        max_win: u16,
        max_loss: u16,
        parts: impl IntoIterator<Item = TableStats>,
    ) {
        let maxima = [max_win, max_loss];
        for (side, &maximum) in maxima.iter().enumerate() {
            if maximum > self.maxima[side] {
                self.maxima[side] = maximum;
                self.candidates[side].clear();
            }
        }
        for part in parts {
            self.totals.win += part.win;
            self.totals.draw += part.draw;
            self.totals.loss += part.loss;
            self.totals.invalid_or_redundant += part.invalid_or_redundant;
            for (side, (histogram, candidates)) in
                part.histograms.into_iter().zip(part.candidates).enumerate()
            {
                for (ply, count) in histogram {
                    *self.totals.histograms[side].entry(ply).or_default() += count;
                }
                if maxima[side] == self.maxima[side] {
                    self.candidates[side]
                        .extend(candidates.into_iter().map(|local_idx| (egt_idx, local_idx)));
                }
            }
        }
    }

    pub(crate) fn build(self, file: &EgtFile, bytes: u64, sha256: String) -> EgtFileStats {
        let [win_candidates, loss_candidates] = self.candidates;
        let materialize = |candidates: Vec<(usize, usize)>, ply| {
            candidates
                .into_iter()
                .filter_map(|(egt_idx, local_idx)| {
                    file.egts[egt_idx]
                        .position_from_index(local_idx, Color::White)
                        .map(|pos| LongestDtcPosition {
                            epd: Epd::from_position(&pos, EnPassantMode::Legal).to_string(),
                            ply,
                        })
                })
                .collect()
        };
        let [histogram_win, histogram_loss] = self.totals.histograms;
        EgtFileStats {
            endgame: file.endgame.clone(),
            bytes,
            sha256,
            index_range: file.index_range,
            frame_size: file.frame_size,
            num_frames: file.num_frames(),
            unique_positions: self.totals.win + self.totals.draw + self.totals.loss,
            win: self.totals.win,
            draw: self.totals.draw,
            loss: self.totals.loss,
            invalid_or_redundant: self.totals.invalid_or_redundant,
            histogram_win,
            histogram_loss,
            longest_dtc_win: materialize(win_candidates, self.maxima[0]),
            longest_dtc_loss: materialize(loss_candidates, self.maxima[1]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ConversionType;
    use std::path::Path;

    #[test]
    fn merge_filters_maxima_and_preserves_visitation_order() {
        let file = EgtFile::new(Path::new("."), "KQ_K").unwrap();
        let egt = &file.egts[0];
        let idx: Vec<_> = (0..egt.index_range())
            .filter(|&idx| {
                !egt.diagonal_symmetric(idx).is_some_and(|other| idx > other)
                    && egt.position_from_index(idx, Color::White).is_some()
            })
            .take(6)
            .collect();
        assert_eq!(idx.len(), 6);
        let part = |w, l, max_win, max_loss| {
            let mut stats = TableStats::default();
            for (local_idx, outcome) in [
                (
                    w,
                    MaybeDtcOutcome::new_win(ConversionType::Capture, max_win),
                ),
                (
                    l,
                    MaybeDtcOutcome::new_loss(ConversionType::Checkmate, max_loss),
                ),
                (w, MaybeDtcOutcome::DRAW),
                (l, MaybeDtcOutcome::INVALID),
            ] {
                stats.record_outcome(outcome, local_idx, egt, max_win, max_loss);
            }
            stats
        };
        let mut builder = EgtFileStatsBuilder::default();
        builder.merge_table(7, 0, 0, [part(idx[0], idx[1], 0, 0)]);
        assert!(builder.candidates[0].is_empty());
        assert_eq!(builder.candidates[1], [(7, idx[1])]);
        builder.merge_table(
            7,
            5,
            3,
            [part(idx[4], idx[3], 5, 3), part(idx[2], idx[1], 5, 3)],
        );
        builder.merge_table(9, 4, 2, [part(idx[0], idx[1], 4, 2)]);
        builder.merge_table(3, 5, 3, [part(idx[0], idx[5], 5, 3)]);
        assert_eq!(builder.maxima, [5, 3]);
        assert_eq!(
            builder.candidates[0],
            [(7, idx[4]), (7, idx[2]), (3, idx[0])]
        );
        assert_eq!(
            builder.candidates[1],
            [(7, idx[3]), (7, idx[1]), (3, idx[5])]
        );
        let totals = builder.totals;
        assert_eq!(
            (
                totals.win,
                totals.draw,
                totals.loss,
                totals.invalid_or_redundant
            ),
            (5, 5, 5, 5)
        );
        assert_eq!(
            totals.histograms[0],
            BTreeMap::from([(0, 1), (4, 1), (5, 3)])
        );
        assert_eq!(
            totals.histograms[1],
            BTreeMap::from([(0, 1), (2, 1), (3, 3)])
        );
    }
}
