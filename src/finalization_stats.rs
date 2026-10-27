//! Deterministic per-table statistics, merged in contiguous batch order.

use std::collections::BTreeMap;

use shakmaty::{Color, EnPassantMode, fen::Epd};

use crate::egt::Egt;
use crate::egt_file::{EgtFileReader, MaybeDtcOutcome};
use crate::stats::{EgtStats, INDEXING_VERSION, LongestDtcPosition, SCHEMA_VERSION};

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

pub(crate) struct EgtStatsBuilder {
    totals: TableStats,
    maxima: [u16; 2],
}

impl EgtStatsBuilder {
    pub(crate) fn new(maxima: [u16; 2]) -> Self {
        Self {
            totals: TableStats::default(),
            maxima,
        }
    }

    /// Merge contiguous batches in visitation order to preserve example order.
    /// All batches belong to this one table and use its fixed maxima.
    pub(crate) fn merge(&mut self, parts: impl IntoIterator<Item = TableStats>) {
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
                self.totals.candidates[side].extend(candidates);
            }
        }
    }

    pub(crate) fn build(self, file: &EgtFileReader, bytes: u64, sha256: String) -> EgtStats {
        let [win_candidates, loss_candidates] = self.totals.candidates;
        let materialize = |candidates: Vec<usize>, ply| {
            candidates
                .into_iter()
                .filter_map(|local_idx| {
                    file.egt
                        .position_from_index(local_idx, Color::White)
                        .map(|pos| LongestDtcPosition {
                            epd: Epd::from_position(&pos, EnPassantMode::Legal).to_string(),
                            ply,
                        })
                })
                .collect()
        };
        let [histogram_win, histogram_loss] = self.totals.histograms;
        EgtStats {
            schema_version: SCHEMA_VERSION,
            indexing_version: INDEXING_VERSION,
            tablename: file.id.tablename().to_owned(),
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
    use crate::table_id::TableId;

    fn table(name: &str) -> Egt {
        Egt::from_elements(TableId::parse(name).unwrap().parse_elements()).unwrap()
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
            let mut stats = TableStats::default();
            for (local_idx, outcome) in [
                (w, MaybeDtcOutcome::new_win(ConversionType::Capture, win)),
                (
                    l,
                    MaybeDtcOutcome::new_loss(ConversionType::Checkmate, loss),
                ),
                (w, MaybeDtcOutcome::DRAW),
                (l, MaybeDtcOutcome::INVALID),
            ] {
                stats.record_outcome(outcome, local_idx, &egt, 5, 3);
            }
            stats
        };
        let mut builder = EgtStatsBuilder::new([5, 3]);
        builder.merge([
            part(idx[0], idx[1], 0, 0),
            part(idx[4], idx[3], 5, 3),
            part(idx[2], idx[1], 5, 3),
        ]);
        builder.merge([part(idx[0], idx[1], 4, 2), part(idx[0], idx[5], 5, 3)]);
        assert_eq!(builder.totals.candidates[0], [idx[4], idx[2], idx[0]]);
        assert_eq!(builder.totals.candidates[1], [idx[3], idx[1], idx[5]]);
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

    #[test]
    fn checkmate_examples_include_zero_but_zero_distance_wins_do_not() {
        let egt = table("KQ_K");
        let idx = (0..egt.index_range())
            .find(|&idx| {
                !egt.diagonal_symmetric(idx).is_some_and(|other| idx > other)
                    && egt.position_from_index(idx, Color::White).is_some()
            })
            .unwrap();
        let mut stats = TableStats::default();
        stats.record_outcome(
            MaybeDtcOutcome::new_win(ConversionType::Capture, 0),
            idx,
            &egt,
            0,
            0,
        );
        stats.record_outcome(
            MaybeDtcOutcome::new_loss(ConversionType::Checkmate, 0),
            idx,
            &egt,
            0,
            0,
        );
        assert!(stats.candidates[0].is_empty());
        assert_eq!(stats.candidates[1], [idx]);
        assert_eq!(
            stats.histograms,
            [BTreeMap::from([(0, 1)]), BTreeMap::from([(0, 1)])]
        );
    }

    #[test]
    fn diagonal_redundancy_is_excluded_from_histograms_and_candidates() {
        let egt = table("KQ_K");
        let idx = (0..egt.index_range())
            .find(|&idx| egt.diagonal_symmetric(idx).is_some_and(|other| idx > other))
            .unwrap();
        let mut stats = TableStats::default();
        stats.record_outcome(
            MaybeDtcOutcome::new_win(ConversionType::Checkmate, 5),
            idx,
            &egt,
            5,
            0,
        );
        assert_eq!(stats.invalid_or_redundant, 1);
        assert_eq!(stats.win, 0);
        assert!(stats.histograms[0].is_empty());
        assert!(stats.candidates[0].is_empty());
    }
}
