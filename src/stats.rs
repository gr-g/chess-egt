//! Versioned per-table sidecars and deterministic summaries of selected tables.

use std::collections::BTreeMap;
use std::io;

use object_store::ObjectStoreExt;
use serde::{Deserialize, Serialize};

use crate::storage::{DEFAULT_FRAME_SIZE, Location, block_on};
use crate::table_id::TableId;
use crate::{EgtError, EgtResult};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LongestDtcPosition {
    pub epd: String,
    pub ply: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EgtStats {
    pub tablename: String,
    pub bytes: u64,
    pub sha256: String,
    pub index_range: usize,
    pub frame_size: usize,
    pub num_frames: usize,
    pub unique_positions: usize,
    pub win: usize,
    pub draw: usize,
    pub loss: usize,
    pub invalid_or_redundant: usize,
    pub histogram_win: BTreeMap<u16, usize>,
    pub histogram_loss: BTreeMap<u16, usize>,
    pub longest_dtc_win: Vec<LongestDtcPosition>,
    pub longest_dtc_loss: Vec<LongestDtcPosition>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AggregateStats {
    pub selected_tables: usize,
    pub loaded_tables: usize,
    pub bytes: u64,
    pub index_range: usize,
    pub num_frames: usize,
    pub unique_positions: usize,
    pub win: usize,
    pub draw: usize,
    pub loss: usize,
    pub invalid_or_redundant: usize,
    pub histogram_win: BTreeMap<u16, usize>,
    pub histogram_loss: BTreeMap<u16, usize>,
    pub longest_dtc_win: Vec<LongestDtcPosition>,
    pub longest_dtc_loss: Vec<LongestDtcPosition>,
}

impl AggregateStats {
    /// Adds one loaded table. The caller sets `selected_tables` and records any
    /// missing identities separately, so selection and loading remain distinct.
    pub fn add(&mut self, stats: &EgtStats) {
        self.loaded_tables += 1;
        self.bytes += stats.bytes;
        self.index_range += stats.index_range;
        self.num_frames += stats.num_frames;
        self.unique_positions += stats.unique_positions;
        self.win += stats.win;
        self.draw += stats.draw;
        self.loss += stats.loss;
        self.invalid_or_redundant += stats.invalid_or_redundant;
        merge_histogram(&mut self.histogram_win, &stats.histogram_win);
        merge_histogram(&mut self.histogram_loss, &stats.histogram_loss);
        merge_examples(
            &mut self.longest_dtc_win,
            &stats.longest_dtc_win,
            self.histogram_win.last_key_value().map(|(&ply, _)| ply),
        );
        merge_examples(
            &mut self.longest_dtc_loss,
            &stats.longest_dtc_loss,
            self.histogram_loss.last_key_value().map(|(&ply, _)| ply),
        );
    }
}

fn merge_histogram(target: &mut BTreeMap<u16, usize>, source: &BTreeMap<u16, usize>) {
    for (&ply, &count) in source {
        *target.entry(ply).or_default() += count;
    }
}

fn merge_examples(
    target: &mut Vec<LongestDtcPosition>,
    source: &[LongestDtcPosition],
    maximum: Option<u16>,
) {
    target.extend(source.iter().cloned());
    target.retain(|position| Some(position.ply) == maximum);
    target.sort_unstable_by(|a, b| a.epd.cmp(&b.epd).then(a.ply.cmp(&b.ply)));
    target.dedup();
}

pub(crate) fn read_stats(location: &Location, id: &TableId) -> EgtResult<EgtStats> {
    location.validate_format()?;
    let key = location.key(&id.relative_path("json"));
    let result = block_on(location.store.get(&key)).map_err(|error| location.error(&key, error))?;
    let bytes = block_on(result.bytes()).map_err(|error| location.error(&key, error))?;
    let stats: EgtStats = serde_json::from_slice(&bytes)?;
    stats.validate(id)?;
    Ok(stats)
}

impl EgtStats {
    fn validate(&self, id: &TableId) -> EgtResult<()> {
        let invalid = |reason: &str| {
            EgtError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid statistics for {}: {reason}", id.tablename()),
            ))
        };
        if self.tablename != id.tablename() {
            return Err(invalid("tablename does not match the requested table"));
        }
        if self.sha256.len() != 64 || !self.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(invalid("sha256 must contain 64 hexadecimal digits"));
        }
        if self.bytes == 0
            || self.index_range == 0
            || self.frame_size != DEFAULT_FRAME_SIZE
            || self.num_frames != self.index_range.div_ceil(self.frame_size)
        {
            return Err(invalid("inconsistent file size or frame geometry"));
        }
        if self
            .win
            .checked_add(self.draw)
            .and_then(|n| n.checked_add(self.loss))
            != Some(self.unique_positions)
            || self.unique_positions.checked_add(self.invalid_or_redundant)
                != Some(self.index_range)
        {
            return Err(invalid(
                "outcome counts do not match position and index totals",
            ));
        }
        for (histogram, count, examples) in [
            (&self.histogram_win, self.win, &self.longest_dtc_win),
            (&self.histogram_loss, self.loss, &self.longest_dtc_loss),
        ] {
            if histogram.values().any(|&count| count == 0)
                || histogram
                    .values()
                    .try_fold(0usize, |sum, &n| sum.checked_add(n))
                    != Some(count)
            {
                return Err(invalid("histogram does not match its outcome count"));
            }
            let maximum = histogram.last_key_value().map(|(&ply, _)| ply);
            if examples
                .iter()
                .any(|position| position.epd.trim().is_empty() || Some(position.ply) != maximum)
            {
                return Err(invalid(
                    "longest-DTC example does not match the histogram maximum",
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats() -> EgtStats {
        EgtStats {
            tablename: "KQ_K".into(),
            bytes: 100,
            sha256: "ab".repeat(32),
            index_range: 10,
            frame_size: DEFAULT_FRAME_SIZE,
            num_frames: 1,
            unique_positions: 6,
            win: 3,
            draw: 1,
            loss: 2,
            invalid_or_redundant: 4,
            histogram_win: BTreeMap::from([(1, 1), (3, 2)]),
            histogram_loss: BTreeMap::from([(0, 1), (2, 1)]),
            longest_dtc_win: vec![example("z", 3)],
            longest_dtc_loss: vec![example("l", 2)],
        }
    }

    fn example(epd: &str, ply: u16) -> LongestDtcPosition {
        LongestDtcPosition {
            epd: epd.into(),
            ply,
        }
    }

    fn location() -> Location {
        let location = Location::parse("memory:///stats-tests").unwrap();
        location.ensure_format().unwrap();
        location
    }

    fn put(location: &Location, bytes: Vec<u8>) {
        let key = location.key(&TableId::parse("KQ_K").unwrap().relative_path("json"));
        block_on(location.store.put(&key, bytes.into())).unwrap();
    }

    #[test]
    fn merging_sums_counts_histograms_and_tracks_loaded_tables() {
        let stats = stats();
        let mut total = AggregateStats {
            selected_tables: 3,
            ..Default::default()
        };
        total.add(&stats);
        total.add(&stats);
        assert_eq!(total.selected_tables, 3);
        assert_eq!(total.loaded_tables, 2);
        assert_eq!(total.bytes, 200);
        assert_eq!(total.index_range, 20);
        assert_eq!(total.num_frames, 2);
        assert_eq!(total.unique_positions, 12);
        assert_eq!(
            (
                total.win,
                total.draw,
                total.loss,
                total.invalid_or_redundant
            ),
            (6, 2, 4, 8)
        );
        assert_eq!(total.histogram_win, BTreeMap::from([(1, 2), (3, 4)]));
        assert_eq!(total.histogram_loss, BTreeMap::from([(0, 2), (2, 2)]));
        assert_eq!(total.longest_dtc_win, stats.longest_dtc_win);
    }

    #[test]
    fn merging_examples_keeps_only_global_maxima_in_deterministic_order() {
        let low = stats();
        let mut high = stats();
        high.histogram_win = BTreeMap::from([(5, 3)]);
        high.longest_dtc_win = vec![example("b", 5), example("a", 5)];
        high.histogram_loss = BTreeMap::from([(0, 2)]);
        high.longest_dtc_loss = vec![example("checkmate", 0)];
        let mut tied = high.clone();
        tied.longest_dtc_win = vec![example("c", 5), example("a", 5)];
        let mut forward = AggregateStats::default();
        let mut reverse = AggregateStats::default();
        for stats in [&low, &high, &tied] {
            forward.add(stats);
        }
        for stats in [&tied, &high, &low] {
            reverse.add(stats);
        }
        assert_eq!(forward, reverse);
        assert_eq!(
            forward.longest_dtc_win,
            vec![example("a", 5), example("b", 5), example("c", 5)]
        );
        assert_eq!(forward.longest_dtc_loss, low.longest_dtc_loss);
        let mut zero = AggregateStats::default();
        zero.add(&high);
        assert_eq!(zero.longest_dtc_loss, high.longest_dtc_loss);
    }

    #[test]
    fn reads_versioned_sidecar_and_reports_missing_without_creating_it() {
        let location = location();
        let id = TableId::parse("KQ_K").unwrap();
        assert!(matches!(
            read_stats(&location, &id),
            Err(EgtError::FileNotFound(_))
        ));
        put(&location, serde_json::to_vec(&stats()).unwrap());
        assert_eq!(read_stats(&location, &id).unwrap(), stats());
    }

    #[test]
    fn rejects_inconsistent_geometry_counts_histograms_and_examples() {
        let id = TableId::parse("KQ_K").unwrap();
        for field in [
            "bytes",
            "frame_size",
            "num_frames",
            "index_range",
            "unique_positions",
            "win",
            "invalid_or_redundant",
        ] {
            let mut json = serde_json::to_value(stats()).unwrap();
            json[field] = serde_json::json!(0);
            let invalid: EgtStats = serde_json::from_value(json).unwrap();
            assert!(invalid.validate(&id).is_err(), "accepted invalid {field}");
        }
        let mut invalid = stats();
        invalid.sha256 = "not a digest".into();
        assert!(invalid.validate(&id).is_err());
        invalid = stats();
        invalid.histogram_win.insert(7, 1);
        assert!(invalid.validate(&id).is_err());
        invalid = stats();
        invalid.longest_dtc_win[0].ply = 1;
        assert!(invalid.validate(&id).is_err());
        invalid = stats();
        invalid.win = usize::MAX;
        assert!(invalid.validate(&id).is_err());
    }

    #[test]
    fn root_format_is_validated_before_reading_sidecars() {
        let location = Location::parse("memory:///no-format").unwrap();
        let id = TableId::parse("KQ_K").unwrap();
        put(&location, serde_json::to_vec(&stats()).unwrap());
        assert!(
            matches!(read_stats(&location, &id), Err(EgtError::FileNotFound(path)) if path.ends_with("format.json"))
        );
    }
}
