//! Per-table metadata and deterministic summaries of selected tables.

use std::collections::BTreeMap;
use std::io;
use std::ops::Add;

use object_store::ObjectStoreExt;
use serde::{Deserialize, Serialize};

use crate::storage::{FRAME_SIZE, Location, block_on};
use crate::table_id::TableId;
use crate::{Egt, EgtError, EgtResult};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LongestDtcPosition {
    pub epd: String,
    pub ply: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// Nonredundant indices where the side to move attacks the opposite king.
    pub invalid_opposite_check: usize,
    /// EP states with a capturer but an occupied target or double-push origin square.
    pub invalid_ep_squares_occupied: usize,
    /// Structurally valid EP states with capturers, but no legal EP capture.
    pub invalid_illegal_ep_capture: usize,
    /// Discarded diagonal-symmetric indices, counted before invalidity checks.
    pub redundant: usize,
    pub histogram_win: BTreeMap<u16, usize>,
    pub histogram_loss: BTreeMap<u16, usize>,
    pub longest_dtc_win: Vec<LongestDtcPosition>,
    pub longest_dtc_loss: Vec<LongestDtcPosition>,
}

/// Unit fields accept only explicit JSON nulls, keeping placeholders all-or-nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlaceholderStats {
    tablename: String,
    index_range: usize,
    bytes: (),
    sha256: (),
    frame_size: (),
    num_frames: (),
    unique_positions: (),
    win: (),
    draw: (),
    loss: (),
    invalid_opposite_check: (),
    invalid_ep_squares_occupied: (),
    invalid_illegal_ep_capture: (),
    redundant: (),
    histogram_win: (),
    histogram_loss: (),
    longest_dtc_win: (),
    longest_dtc_loss: (),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged, try_from = "serde_json::Value")]
pub(crate) enum StatsRecord {
    Complete(EgtStats),
    Missing(PlaceholderStats),
}

// Deserialize through JSON so integer histogram keys retain JSON's key conversion.
impl TryFrom<serde_json::Value> for StatsRecord {
    type Error = serde_json::Error;

    fn try_from(value: serde_json::Value) -> Result<Self, Self::Error> {
        if value.get("bytes").is_some_and(serde_json::Value::is_null) {
            serde_json::from_value(value).map(Self::Missing)
        } else {
            serde_json::from_value(value).map(Self::Complete)
        }
    }
}

impl StatsRecord {
    pub(crate) fn missing(id: &TableId) -> EgtResult<Self> {
        Ok(Self::Missing(PlaceholderStats {
            tablename: id.tablename().to_owned(),
            index_range: Egt::from_elements(id.parse_elements())?.index_range(),
            ..Default::default()
        }))
    }

    pub(crate) fn index_range(&self) -> usize {
        match self {
            Self::Complete(stats) => stats.index_range,
            Self::Missing(stats) => stats.index_range,
        }
    }
}

// Unlike an ordinary Option, a missing field must not be mistaken for explicit null.
fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AggregateStats {
    pub selected_tables: usize,
    pub available_tables: usize,
    pub index_range: usize,
    #[serde(deserialize_with = "required_nullable")]
    pub bytes: Option<u64>,
    #[serde(deserialize_with = "required_nullable")]
    pub num_frames: Option<usize>,
    #[serde(deserialize_with = "required_nullable")]
    pub unique_positions: Option<usize>,
    #[serde(deserialize_with = "required_nullable")]
    pub win: Option<usize>,
    #[serde(deserialize_with = "required_nullable")]
    pub draw: Option<usize>,
    #[serde(deserialize_with = "required_nullable")]
    pub loss: Option<usize>,
    #[serde(deserialize_with = "required_nullable")]
    pub invalid_opposite_check: Option<usize>,
    #[serde(deserialize_with = "required_nullable")]
    pub invalid_ep_squares_occupied: Option<usize>,
    #[serde(deserialize_with = "required_nullable")]
    pub invalid_illegal_ep_capture: Option<usize>,
    #[serde(deserialize_with = "required_nullable")]
    pub redundant: Option<usize>,
    #[serde(deserialize_with = "required_nullable")]
    pub histogram_win: Option<BTreeMap<u16, usize>>,
    #[serde(deserialize_with = "required_nullable")]
    pub histogram_loss: Option<BTreeMap<u16, usize>>,
    #[serde(deserialize_with = "required_nullable")]
    pub longest_dtc_win: Option<Vec<LongestDtcPosition>>,
    #[serde(deserialize_with = "required_nullable")]
    pub longest_dtc_loss: Option<Vec<LongestDtcPosition>>,
}

impl Default for AggregateStats {
    fn default() -> Self {
        Self {
            selected_tables: 0,
            available_tables: 0,
            index_range: 0,
            bytes: Some(0),
            num_frames: Some(0),
            unique_positions: Some(0),
            win: Some(0),
            draw: Some(0),
            loss: Some(0),
            invalid_opposite_check: Some(0),
            invalid_ep_squares_occupied: Some(0),
            invalid_illegal_ep_capture: Some(0),
            redundant: Some(0),
            histogram_win: Some(BTreeMap::new()),
            histogram_loss: Some(BTreeMap::new()),
            longest_dtc_win: Some(Vec::new()),
            longest_dtc_loss: Some(Vec::new()),
        }
    }
}

impl AggregateStats {
    /// Adds one available table, leaving selection counting to the caller.
    pub fn add(&mut self, stats: &EgtStats) {
        self.merge(&Self {
            selected_tables: 0,
            available_tables: 1,
            index_range: stats.index_range,
            bytes: Some(stats.bytes),
            num_frames: Some(stats.num_frames),
            unique_positions: Some(stats.unique_positions),
            win: Some(stats.win),
            draw: Some(stats.draw),
            loss: Some(stats.loss),
            invalid_opposite_check: Some(stats.invalid_opposite_check),
            invalid_ep_squares_occupied: Some(stats.invalid_ep_squares_occupied),
            invalid_illegal_ep_capture: Some(stats.invalid_illegal_ep_capture),
            redundant: Some(stats.redundant),
            histogram_win: Some(stats.histogram_win.clone()),
            histogram_loss: Some(stats.histogram_loss.clone()),
            longest_dtc_win: Some(stats.longest_dtc_win.clone()),
            longest_dtc_loss: Some(stats.longest_dtc_loss.clone()),
        });
    }

    pub(crate) fn add_record(&mut self, record: &StatsRecord) {
        self.selected_tables += 1;
        match record {
            StatsRecord::Complete(stats) => self.add(stats),
            StatsRecord::Missing(_) => {
                self.index_range += record.index_range();
                self.bytes = None;
                self.num_frames = None;
                self.unique_positions = None;
                self.win = None;
                self.draw = None;
                self.loss = None;
                self.invalid_opposite_check = None;
                self.invalid_ep_squares_occupied = None;
                self.invalid_illegal_ep_capture = None;
                self.redundant = None;
                self.histogram_win = None;
                self.histogram_loss = None;
                self.longest_dtc_win = None;
                self.longest_dtc_loss = None;
            }
        }
    }

    /// Combines disjoint selections; missing metadata propagates through all totals.
    pub fn merge(&mut self, other: &Self) {
        self.selected_tables += other.selected_tables;
        self.available_tables += other.available_tables;
        self.index_range += other.index_range;
        self.bytes = sum(self.bytes, other.bytes);
        self.num_frames = sum(self.num_frames, other.num_frames);
        self.unique_positions = sum(self.unique_positions, other.unique_positions);
        self.win = sum(self.win, other.win);
        self.draw = sum(self.draw, other.draw);
        self.loss = sum(self.loss, other.loss);
        self.invalid_opposite_check =
            sum(self.invalid_opposite_check, other.invalid_opposite_check);
        self.invalid_ep_squares_occupied = sum(
            self.invalid_ep_squares_occupied,
            other.invalid_ep_squares_occupied,
        );
        self.invalid_illegal_ep_capture = sum(
            self.invalid_illegal_ep_capture,
            other.invalid_illegal_ep_capture,
        );
        self.redundant = sum(self.redundant, other.redundant);
        merge_distribution(
            &mut self.histogram_win,
            &other.histogram_win,
            &mut self.longest_dtc_win,
            &other.longest_dtc_win,
        );
        merge_distribution(
            &mut self.histogram_loss,
            &other.histogram_loss,
            &mut self.longest_dtc_loss,
            &other.longest_dtc_loss,
        );
    }

    pub(crate) fn validate(&self) -> EgtResult<()> {
        let complete = self.available_tables == self.selected_tables;
        let fields = [
            self.bytes.is_some(),
            self.num_frames.is_some(),
            self.unique_positions.is_some(),
            self.win.is_some(),
            self.draw.is_some(),
            self.loss.is_some(),
            self.invalid_opposite_check.is_some(),
            self.invalid_ep_squares_occupied.is_some(),
            self.invalid_illegal_ep_capture.is_some(),
            self.redundant.is_some(),
            self.histogram_win.is_some(),
            self.histogram_loss.is_some(),
            self.longest_dtc_win.is_some(),
            self.longest_dtc_loss.is_some(),
        ];
        if self.available_tables > self.selected_tables
            || (self.selected_tables == 0) != (self.index_range == 0)
            || self.index_range < self.selected_tables
            || fields.iter().any(|&present| present != complete)
        {
            return Err(invalid_data("inconsistent aggregate counts or nullability"));
        }
        if !complete {
            return Ok(());
        }
        if self.selected_tables == 0 {
            return if self == &Self::default() {
                Ok(())
            } else {
                Err(invalid_data("nonzero metadata in an empty aggregate"))
            };
        }
        if self.bytes.unwrap() < self.available_tables as u64
            || self.num_frames.unwrap() < self.available_tables
            || self.num_frames.unwrap() > self.index_range
        {
            return Err(invalid_data("inconsistent aggregate file totals"));
        }
        validate_outcomes(
            self.index_range,
            self.unique_positions.unwrap(),
            self.win.unwrap(),
            self.draw.unwrap(),
            self.loss.unwrap(),
            [
                self.invalid_opposite_check.unwrap(),
                self.invalid_ep_squares_occupied.unwrap(),
                self.invalid_illegal_ep_capture.unwrap(),
                self.redundant.unwrap(),
            ],
        )?;
        validate_distribution(
            self.histogram_win.as_ref().unwrap(),
            self.win.unwrap(),
            self.longest_dtc_win.as_ref().unwrap(),
        )?;
        validate_distribution(
            self.histogram_loss.as_ref().unwrap(),
            self.loss.unwrap(),
            self.longest_dtc_loss.as_ref().unwrap(),
        )
    }
}

fn sum<T: Add<Output = T>>(a: Option<T>, b: Option<T>) -> Option<T> {
    a.zip(b).map(|(a, b)| a + b)
}

fn merge_distribution(
    histogram: &mut Option<BTreeMap<u16, usize>>,
    source: &Option<BTreeMap<u16, usize>>,
    examples: &mut Option<Vec<LongestDtcPosition>>,
    source_examples: &Option<Vec<LongestDtcPosition>>,
) {
    if let (Some(histogram), Some(source), Some(examples), Some(source_examples)) = (
        histogram.as_mut(),
        source,
        examples.as_mut(),
        source_examples,
    ) {
        for (&ply, &count) in source {
            *histogram.entry(ply).or_default() += count;
        }
        let maximum = histogram.last_key_value().map(|(&ply, _)| ply);
        examples.extend(source_examples.iter().cloned());
        examples.retain(|position| Some(position.ply) == maximum);
        examples.sort_unstable_by(|a, b| a.epd.cmp(&b.epd).then(a.ply.cmp(&b.ply)));
        examples.dedup();
    } else {
        *histogram = None;
        *examples = None;
    }
}

/// Publishes a sidecar after its binary table, invalidating hierarchy summaries first.
pub(crate) fn write_stats(location: &Location, id: &TableId, stats: &EgtStats) -> EgtResult<()> {
    location.ensure_format()?;
    let key = location.key(&id.relative_path("json"));
    let json = serde_json::to_vec_pretty(stats)?;
    crate::stats_cache::invalidate(location, id)?;
    block_on(location.store.put(&key, json.into())).map_err(|error| location.error(&key, error))?;
    Ok(())
}

pub(crate) fn read_record(location: &Location, id: &TableId) -> EgtResult<StatsRecord> {
    location.validate_format()?;
    let key = location.key(&id.relative_path("json"));
    let result = block_on(location.store.get(&key)).map_err(|error| location.error(&key, error))?;
    let bytes = block_on(result.bytes()).map_err(|error| location.error(&key, error))?;
    let record: StatsRecord = serde_json::from_slice(&bytes)?;
    match &record {
        StatsRecord::Complete(stats) => stats.validate(id)?,
        StatsRecord::Missing(_) if record != StatsRecord::missing(id)? => {
            return Err(invalid_data(
                "placeholder identity or index range does not match table",
            ));
        }
        StatsRecord::Missing(_) => {}
    }
    Ok(record)
}

#[cfg(test)]
pub(crate) fn read_stats(location: &Location, id: &TableId) -> EgtResult<EgtStats> {
    match read_record(location, id)? {
        StatsRecord::Complete(stats) => Ok(stats),
        StatsRecord::Missing(_) => Err(invalid_data("requested statistics are unavailable")),
    }
}

impl EgtStats {
    fn validate(&self, id: &TableId) -> EgtResult<()> {
        if self.tablename != id.tablename()
            || self.sha256.len() != 64
            || !self.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
            || self.bytes == 0
            || self.index_range == 0
            || self.frame_size != FRAME_SIZE
            || self.num_frames != self.index_range.div_ceil(self.frame_size)
        {
            return Err(invalid_data(
                "invalid table identity, SHA-256, or frame geometry",
            ));
        }
        validate_outcomes(
            self.index_range,
            self.unique_positions,
            self.win,
            self.draw,
            self.loss,
            [
                self.invalid_opposite_check,
                self.invalid_ep_squares_occupied,
                self.invalid_illegal_ep_capture,
                self.redundant,
            ],
        )?;
        validate_distribution(&self.histogram_win, self.win, &self.longest_dtc_win)?;
        validate_distribution(&self.histogram_loss, self.loss, &self.longest_dtc_loss)
    }
}

fn invalid_data(reason: &str) -> EgtError {
    EgtError::Io(io::Error::new(io::ErrorKind::InvalidData, reason))
}

fn validate_outcomes(
    index_range: usize,
    unique_positions: usize,
    win: usize,
    draw: usize,
    loss: usize,
    invalid_and_redundant: [usize; 4],
) -> EgtResult<()> {
    if win.checked_add(draw).and_then(|n| n.checked_add(loss)) != Some(unique_positions)
        || invalid_and_redundant
            .iter()
            .try_fold(unique_positions, |sum, &count| sum.checked_add(count))
            != Some(index_range)
    {
        return Err(invalid_data(
            "outcome counts do not match position and index totals",
        ));
    }
    Ok(())
}

fn validate_distribution(
    histogram: &BTreeMap<u16, usize>,
    count: usize,
    examples: &[LongestDtcPosition],
) -> EgtResult<()> {
    let maximum = histogram.last_key_value().map(|(&ply, _)| ply);
    if histogram.values().any(|&count| count == 0)
        || histogram
            .values()
            .try_fold(0usize, |sum, &n| sum.checked_add(n))
            != Some(count)
        || examples
            .iter()
            .any(|position| position.epd.trim().is_empty() || Some(position.ply) != maximum)
    {
        return Err(invalid_data(
            "histogram or longest-DTC examples do not match outcomes",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARTITION_FIELDS: [&str; 4] = [
        "invalid_opposite_check",
        "invalid_ep_squares_occupied",
        "invalid_illegal_ep_capture",
        "redundant",
    ];

    fn stats() -> EgtStats {
        EgtStats {
            tablename: "KQ_K".into(),
            bytes: 100,
            sha256: "ab".repeat(32),
            index_range: 16,
            frame_size: FRAME_SIZE,
            num_frames: 1,
            unique_positions: 6,
            win: 3,
            draw: 1,
            loss: 2,
            invalid_opposite_check: 1,
            invalid_ep_squares_occupied: 2,
            invalid_illegal_ep_capture: 3,
            redundant: 4,
            histogram_win: BTreeMap::from([(1, 1), (3, 2)]),
            histogram_loss: BTreeMap::from([(0, 1), (2, 1)]),
            longest_dtc_win: vec![example("win", 3)],
            longest_dtc_loss: vec![example("loss", 2)],
        }
    }

    fn example(epd: &str, ply: u16) -> LongestDtcPosition {
        LongestDtcPosition {
            epd: epd.into(),
            ply,
        }
    }

    fn aggregate(records: &[StatsRecord]) -> AggregateStats {
        let mut total = AggregateStats::default();
        for record in records {
            total.add_record(record);
        }
        total.validate().unwrap();
        total
    }

    #[test]
    fn record_schema_accepts_complete_or_all_null_only() {
        let id = TableId::parse("KQ_K").unwrap();
        for record in [
            StatsRecord::Complete(stats()),
            StatsRecord::missing(&id).unwrap(),
        ] {
            let json = serde_json::to_value(&record).unwrap();
            assert_eq!(
                serde_json::from_value::<StatsRecord>(json.clone()).unwrap(),
                record
            );
            for field in ["bytes", "sha256", "histogram_win", "longest_dtc_loss"]
                .into_iter()
                .chain(PARTITION_FIELDS)
            {
                let mut invalid = json.clone();
                invalid.as_object_mut().unwrap().remove(field);
                assert!(serde_json::from_value::<StatsRecord>(invalid).is_err());
            }
        }
        for field in PARTITION_FIELDS {
            let mut complete = serde_json::to_value(stats()).unwrap();
            complete[field] = serde_json::Value::Null;
            assert!(
                serde_json::from_value::<StatsRecord>(complete).is_err(),
                "{field}"
            );
            let mut missing = serde_json::to_value(StatsRecord::missing(&id).unwrap()).unwrap();
            missing[field] = 0.into();
            assert!(
                serde_json::from_value::<StatsRecord>(missing).is_err(),
                "{field}"
            );
        }
        for record in [
            StatsRecord::Complete(stats()),
            StatsRecord::missing(&id).unwrap(),
        ] {
            let mut old = serde_json::to_value(record).unwrap();
            let fields = old.as_object_mut().unwrap();
            for field in PARTITION_FIELDS {
                fields.remove(field);
            }
            fields.insert("invalid_or_redundant".into(), serde_json::Value::Null);
            assert!(serde_json::from_value::<StatsRecord>(old).is_err());
        }
        let mut partial = serde_json::to_value(stats()).unwrap();
        partial["bytes"] = serde_json::Value::Null;
        assert!(serde_json::from_value::<StatsRecord>(partial).is_err());
    }

    #[test]
    fn aggregation_sums_counts_and_keeps_deterministic_global_maxima() {
        let low = StatsRecord::Complete(stats());
        let mut high = stats();
        high.histogram_win = BTreeMap::from([(5, 3)]);
        high.longest_dtc_win = vec![example("b", 5), example("a", 5), example("a", 5)];
        let high = StatsRecord::Complete(high);
        let total = aggregate(&[low.clone(), high.clone()]);
        assert_eq!(total, aggregate(&[high.clone(), low.clone()]));
        assert_eq!(
            (
                total.selected_tables,
                total.available_tables,
                total.index_range
            ),
            (2, 2, 32)
        );
        assert_eq!((total.bytes, total.unique_positions), (Some(200), Some(12)));
        assert_eq!(total.invalid_opposite_check, Some(2));
        assert_eq!(total.invalid_ep_squares_occupied, Some(4));
        assert_eq!(total.invalid_illegal_ep_capture, Some(6));
        assert_eq!(total.redundant, Some(8));
        assert_eq!(
            total.histogram_win,
            Some(BTreeMap::from([(1, 1), (3, 2), (5, 3)]))
        );
        assert_eq!(
            total.longest_dtc_win,
            Some(vec![example("a", 5), example("b", 5)])
        );
        let mut merged = aggregate(&[low]);
        merged.merge(&aggregate(&[high]));
        assert_eq!(merged, total);
        let missing = StatsRecord::missing(&TableId::parse("K_K").unwrap()).unwrap();
        for records in [
            [missing.clone(), StatsRecord::Complete(stats())],
            [StatsRecord::Complete(stats()), missing.clone()],
        ] {
            let total = aggregate(&records);
            assert_eq!((total.selected_tables, total.available_tables), (2, 1));
            assert_eq!(total.index_range, stats().index_range + missing.index_range());
            for (key, value) in serde_json::to_value(total).unwrap().as_object().unwrap() {
                if !["selected_tables", "available_tables", "index_range"].contains(&key.as_str()) {
                    assert!(value.is_null(), "{key}");
                }
            }
        }
    }

    #[test]
    fn validates_file_metadata_and_cached_totals() {
        let id = TableId::parse("KQ_K").unwrap();
        for field in [
            "bytes",
            "frame_size",
            "num_frames",
            "index_range",
            "win",
            "unique_positions",
        ]
        .into_iter()
        .chain(PARTITION_FIELDS)
        {
            let mut json = serde_json::to_value(stats()).unwrap();
            json[field] = 0.into();
            let invalid: EgtStats = serde_json::from_value(json).unwrap();
            assert!(invalid.validate(&id).is_err(), "{field}");
        }
        let mut invalid = stats();
        invalid.sha256.clear();
        assert!(invalid.validate(&id).is_err());
        invalid = stats();
        invalid.histogram_win.insert(9, 1);
        assert!(invalid.validate(&id).is_err());
        invalid = stats();
        invalid.longest_dtc_win[0].ply = 1;
        assert!(invalid.validate(&id).is_err());
        let mut total = aggregate(&[StatsRecord::Complete(stats())]);
        total.available_tables = 0;
        assert!(total.validate().is_err());
        total = aggregate(&[StatsRecord::Complete(stats())]);
        total.win = Some(0);
        assert!(total.validate().is_err());
        total = AggregateStats::default();
        total.bytes = Some(1);
        assert!(total.validate().is_err());
    }

    #[test]
    fn aggregate_schema_requires_all_partition_fields_and_consistent_nullability() {
        let missing = StatsRecord::missing(&TableId::parse("K_K").unwrap()).unwrap();
        for total in [
            AggregateStats::default(),
            aggregate(&[StatsRecord::Complete(stats())]),
            aggregate(&[missing]),
        ] {
            let json = serde_json::to_value(&total).unwrap();
            assert_eq!(
                serde_json::from_value::<AggregateStats>(json.clone()).unwrap(),
                total
            );
            for field in PARTITION_FIELDS {
                assert!(json.get(field).is_some(), "{field}");
                let mut invalid = json.clone();
                invalid.as_object_mut().unwrap().remove(field);
                assert!(
                    serde_json::from_value::<AggregateStats>(invalid).is_err(),
                    "{field}"
                );
                let mut invalid = json.clone();
                invalid[field] = if json[field].is_null() {
                    0.into()
                } else {
                    serde_json::Value::Null
                };
                let invalid: AggregateStats = serde_json::from_value(invalid).unwrap();
                assert!(invalid.validate().is_err(), "{field}");
            }
            let mut old = json;
            let fields = old.as_object_mut().unwrap();
            for field in PARTITION_FIELDS {
                fields.remove(field);
            }
            fields.insert("invalid_or_redundant".into(), serde_json::Value::Null);
            assert!(serde_json::from_value::<AggregateStats>(old).is_err());
        }
    }

    #[test]
    fn validates_each_partition_count_and_rejects_overflow() {
        let id = TableId::parse("KQ_K").unwrap();
        for field in PARTITION_FIELDS {
            for count in [0, usize::MAX] {
                let mut json = serde_json::to_value(stats()).unwrap();
                json[field] = count.into();
                let invalid: EgtStats = serde_json::from_value(json).unwrap();
                assert!(invalid.validate(&id).is_err(), "{field}: {count}");

                let mut json =
                    serde_json::to_value(aggregate(&[StatsRecord::Complete(stats())])).unwrap();
                json[field] = count.into();
                let invalid: AggregateStats = serde_json::from_value(json).unwrap();
                assert!(invalid.validate().is_err(), "{field}: {count}");
            }
        }
        assert!(validate_outcomes(0, 0, usize::MAX, 1, 0, [0; 4]).is_err());
        for index in 0..4 {
            let mut counts = [0; 4];
            counts[index] = usize::MAX;
            assert!(validate_outcomes(0, 1, 0, 1, 0, counts).is_err());
        }
        assert!(validate_outcomes(usize::MAX, 0, 0, 0, 0, [usize::MAX, 1, 0, 0]).is_err());
        assert!(validate_outcomes(usize::MAX, 0, 0, 0, 0, [0, 0, 0, usize::MAX]).is_ok());
    }

    #[test]
    fn reader_requires_root_and_validates_placeholder_identity_and_range() {
        let location = Location::parse("memory:///record-tests").unwrap();
        let id = TableId::parse("KQ_K").unwrap();
        assert!(read_record(&location, &id).is_err());
        location.ensure_format().unwrap();
        assert!(matches!(
            read_record(&location, &id),
            Err(EgtError::FileNotFound(_))
        ));
        let record = StatsRecord::missing(&id).unwrap();
        let mut json = serde_json::to_value(&record).unwrap();
        let key = location.key(&id.relative_path("json"));
        for (field, value) in [("index_range", 1.into()), ("tablename", "KR_K".into())] {
            let original = json[field].clone();
            json[field] = value;
            block_on(
                location
                    .store
                    .put(&key, serde_json::to_vec(&json).unwrap().into()),
            )
            .unwrap();
            assert!(read_record(&location, &id).is_err());
            json[field] = original;
        }
        block_on(
            location
                .store
                .put(&key, serde_json::to_vec(&record).unwrap().into()),
        )
        .unwrap();
        assert_eq!(read_record(&location, &id).unwrap(), record);
    }
}
