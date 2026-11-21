//! Per-table metadata and deterministic summaries of selected tables.

mod cache;

use std::collections::BTreeMap;

use object_store::ObjectStoreExt;
use serde::{Deserialize, Serialize, Serializer, ser::SerializeStruct};

use crate::storage::{Location, block_on};
use crate::table_id::TableId;
use crate::{Egt, EgtError, EgtResult};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LongestDtcPosition {
    pub epd: String,
    pub ply: u16,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Totals {
    pub bytes: u64,
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TableWire {
    tablename: String,
    #[serde(deserialize_with = "required_nullable")]
    bytes: Option<u64>,
    #[serde(deserialize_with = "required_nullable")]
    sha256: Option<String>,
    index_range: usize,
    #[serde(deserialize_with = "required_nullable")]
    frame_size: Option<usize>,
    #[serde(deserialize_with = "required_nullable")]
    num_frames: Option<usize>,
    #[serde(deserialize_with = "required_nullable")]
    unique_positions: Option<usize>,
    #[serde(deserialize_with = "required_nullable")]
    win: Option<usize>,
    #[serde(deserialize_with = "required_nullable")]
    draw: Option<usize>,
    #[serde(deserialize_with = "required_nullable")]
    loss: Option<usize>,
    #[serde(deserialize_with = "required_nullable")]
    invalid_opposite_check: Option<usize>,
    #[serde(deserialize_with = "required_nullable")]
    invalid_ep_squares_occupied: Option<usize>,
    #[serde(deserialize_with = "required_nullable")]
    invalid_illegal_ep_capture: Option<usize>,
    #[serde(deserialize_with = "required_nullable")]
    redundant: Option<usize>,
    #[serde(deserialize_with = "required_nullable")]
    histogram_win: Option<BTreeMap<u16, usize>>,
    #[serde(deserialize_with = "required_nullable")]
    histogram_loss: Option<BTreeMap<u16, usize>>,
    #[serde(deserialize_with = "required_nullable")]
    longest_dtc_win: Option<Vec<LongestDtcPosition>>,
    #[serde(deserialize_with = "required_nullable")]
    longest_dtc_loss: Option<Vec<LongestDtcPosition>>,
}

fn serialize_table<S: Serializer>(
    serializer: S,
    tablename: &str,
    index_range: usize,
    stats: Option<&EgtStats>,
) -> Result<S::Ok, S::Error> {
    let mut wire = serializer.serialize_struct("TableStats", 18)?;
    wire.serialize_field("tablename", tablename)?;
    if stats.is_none() {
        wire.serialize_field("index_range", &index_range)?;
    }
    wire.serialize_field("bytes", &stats.map(|stats| &stats.totals.bytes))?;
    wire.serialize_field("sha256", &stats.map(|stats| &stats.sha256))?;
    if stats.is_some() {
        wire.serialize_field("index_range", &index_range)?;
    }
    wire.serialize_field("frame_size", &stats.map(|stats| stats.frame_size))?;
    wire.serialize_field("num_frames", &stats.map(|stats| &stats.totals.num_frames))?;
    wire.serialize_field(
        "unique_positions",
        &stats.map(|stats| &stats.totals.unique_positions),
    )?;
    wire.serialize_field("win", &stats.map(|stats| &stats.totals.win))?;
    wire.serialize_field("draw", &stats.map(|stats| &stats.totals.draw))?;
    wire.serialize_field("loss", &stats.map(|stats| &stats.totals.loss))?;
    wire.serialize_field(
        "invalid_opposite_check",
        &stats.map(|stats| &stats.totals.invalid_opposite_check),
    )?;
    wire.serialize_field(
        "invalid_ep_squares_occupied",
        &stats.map(|stats| &stats.totals.invalid_ep_squares_occupied),
    )?;
    wire.serialize_field(
        "invalid_illegal_ep_capture",
        &stats.map(|stats| &stats.totals.invalid_illegal_ep_capture),
    )?;
    wire.serialize_field("redundant", &stats.map(|stats| &stats.totals.redundant))?;
    wire.serialize_field(
        "histogram_win",
        &stats.map(|stats| &stats.totals.histogram_win),
    )?;
    wire.serialize_field(
        "histogram_loss",
        &stats.map(|stats| &stats.totals.histogram_loss),
    )?;
    wire.serialize_field(
        "longest_dtc_win",
        &stats.map(|stats| &stats.totals.longest_dtc_win),
    )?;
    wire.serialize_field(
        "longest_dtc_loss",
        &stats.map(|stats| &stats.totals.longest_dtc_loss),
    )?;
    wire.end()
}

impl TryFrom<TableWire> for StatsRecord {
    type Error = &'static str;

    fn try_from(wire: TableWire) -> Result<Self, Self::Error> {
        let TableWire {
            tablename,
            index_range,
            sha256,
            frame_size,
            bytes,
            num_frames,
            unique_positions,
            win,
            draw,
            loss,
            invalid_opposite_check,
            invalid_ep_squares_occupied,
            invalid_illegal_ep_capture,
            redundant,
            histogram_win,
            histogram_loss,
            longest_dtc_win,
            longest_dtc_loss,
        } = wire;
        match (
            sha256,
            frame_size,
            bytes,
            num_frames,
            unique_positions,
            win,
            draw,
            loss,
            invalid_opposite_check,
            invalid_ep_squares_occupied,
            invalid_illegal_ep_capture,
            redundant,
            histogram_win,
            histogram_loss,
            longest_dtc_win,
            longest_dtc_loss,
        ) {
            (
                Some(sha256),
                Some(frame_size),
                Some(bytes),
                Some(num_frames),
                Some(unique_positions),
                Some(win),
                Some(draw),
                Some(loss),
                Some(invalid_opposite_check),
                Some(invalid_ep_squares_occupied),
                Some(invalid_illegal_ep_capture),
                Some(redundant),
                Some(histogram_win),
                Some(histogram_loss),
                Some(longest_dtc_win),
                Some(longest_dtc_loss),
            ) => Ok(Self::Complete(EgtStats {
                tablename,
                index_range,
                sha256,
                frame_size,
                totals: Totals {
                    bytes,
                    num_frames,
                    unique_positions,
                    win,
                    draw,
                    loss,
                    invalid_opposite_check,
                    invalid_ep_squares_occupied,
                    invalid_illegal_ep_capture,
                    redundant,
                    histogram_win,
                    histogram_loss,
                    longest_dtc_win,
                    longest_dtc_loss,
                },
            })),
            (
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            ) => Ok(Self::Missing {
                tablename,
                index_range,
            }),
            _ => Err("inconsistent table stats nullability"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "TableWire")]
pub struct EgtStats {
    pub tablename: String,
    pub sha256: String,
    pub index_range: usize,
    pub frame_size: usize,
    pub totals: Totals,
}

impl Serialize for EgtStats {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serialize_table(serializer, &self.tablename, self.index_range, Some(self))
    }
}

impl TryFrom<TableWire> for EgtStats {
    type Error = &'static str;

    fn try_from(wire: TableWire) -> Result<Self, Self::Error> {
        match StatsRecord::try_from(wire)? {
            StatsRecord::Complete(stats) => Ok(stats),
            StatsRecord::Missing { .. } => Err("requested statistics are unavailable"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "TableWire")]
pub(crate) enum StatsRecord {
    Complete(EgtStats),
    Missing {
        tablename: String,
        index_range: usize,
    },
}

impl Serialize for StatsRecord {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Complete(stats) => stats.serialize(serializer),
            Self::Missing {
                tablename,
                index_range,
            } => serialize_table(serializer, tablename, *index_range, None),
        }
    }
}

impl StatsRecord {
    pub(crate) fn missing(id: &TableId) -> EgtResult<Self> {
        Ok(Self::Missing {
            tablename: id.to_string(),
            index_range: Egt::new(id)?.index_range(),
        })
    }

    #[cfg(test)]
    fn index_range(&self) -> usize {
        match self {
            Self::Complete(stats) => stats.index_range,
            Self::Missing { index_range, .. } => *index_range,
        }
    }
}

// Missing fields are errors even when explicit nulls are allowed.
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
    pub totals: Option<Totals>,
}

impl Default for AggregateStats {
    fn default() -> Self {
        Self {
            selected_tables: 0,
            available_tables: 0,
            index_range: 0,
            totals: Some(Totals::default()),
        }
    }
}

impl AggregateStats {
    /// Adds one selected, available table. As with `merge`, selections must be disjoint.
    pub fn add(&mut self, stats: &EgtStats) {
        self.selected_tables += 1;
        self.available_tables += 1;
        self.index_range += stats.index_range;
        if let Some(totals) = &mut self.totals {
            totals.add(&stats.totals);
        }
    }

    pub(crate) fn add_record(&mut self, record: &StatsRecord) {
        match record {
            StatsRecord::Complete(stats) => self.add(stats),
            StatsRecord::Missing { index_range, .. } => {
                self.selected_tables += 1;
                self.index_range += index_range;
                self.totals = None;
            }
        }
    }

    /// Combines disjoint selections; missing metadata propagates through all totals.
    pub fn merge(&mut self, other: &Self) {
        self.selected_tables += other.selected_tables;
        self.available_tables += other.available_tables;
        self.index_range += other.index_range;
        match (&mut self.totals, &other.totals) {
            (Some(totals), Some(other)) => totals.add(other),
            _ => self.totals = None,
        }
    }

    pub(crate) fn validate(&self) -> EgtResult<()> {
        let complete = self.available_tables == self.selected_tables;
        if self.available_tables > self.selected_tables
            || (self.selected_tables == 0) != (self.index_range == 0)
            || self.index_range < self.selected_tables
            || self.totals.is_some() != complete
        {
            return Err(invalid_data("inconsistent aggregate counts or nullability"));
        }
        let Some(totals) = &self.totals else {
            return Ok(());
        };
        if self.selected_tables == 0 && self != &Self::default() {
            return Err(invalid_data("nonzero metadata in an empty aggregate"));
        }
        if totals.bytes < self.available_tables as u64
            || totals.num_frames < self.available_tables
            || totals.num_frames > self.index_range
        {
            return Err(invalid_data("inconsistent aggregate file totals"));
        }
        totals.validate(self.index_range)
    }
}

impl Totals {
    pub(crate) fn add(&mut self, other: &Self) {
        self.bytes += other.bytes;
        self.num_frames += other.num_frames;
        self.unique_positions += other.unique_positions;
        self.win += other.win;
        self.draw += other.draw;
        self.loss += other.loss;
        self.invalid_opposite_check += other.invalid_opposite_check;
        self.invalid_ep_squares_occupied += other.invalid_ep_squares_occupied;
        self.invalid_illegal_ep_capture += other.invalid_illegal_ep_capture;
        self.redundant += other.redundant;
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

    fn validate(&self, index_range: usize) -> EgtResult<()> {
        validate_outcomes(
            index_range,
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

fn merge_distribution(
    histogram: &mut BTreeMap<u16, usize>,
    source: &BTreeMap<u16, usize>,
    examples: &mut Vec<LongestDtcPosition>,
    source_examples: &[LongestDtcPosition],
) {
    for (&ply, &count) in source {
        *histogram.entry(ply).or_default() += count;
    }
    let maximum = histogram.last_key_value().map(|(&ply, _)| ply);
    examples.extend(source_examples.iter().cloned());
    examples.retain(|position| Some(position.ply) == maximum);
    examples.sort_unstable_by(|a, b| a.epd.cmp(&b.epd).then(a.ply.cmp(&b.ply)));
    examples.dedup();
}

/// Publishes a sidecar after its binary table, invalidating hierarchy summaries first.
pub(crate) fn write_stats(location: &Location, id: &TableId, stats: &EgtStats) -> EgtResult<()> {
    location.ensure_format()?;
    let key = location.key(&id.relative_path("json"));
    let json = serde_json::to_vec_pretty(stats)?;
    cache::invalidate(location, id)?;
    block_on(location.store.put(&key, json.into())).map_err(|error| location.error(&key, error))?;
    Ok(())
}

pub(crate) fn read_record(location: &Location, id: &TableId) -> EgtResult<StatsRecord> {
    location.validate_format()?;
    let key = location.key(&id.relative_path("json"));
    let result = block_on(location.store.get(&key)).map_err(|error| location.error(&key, error))?;
    let bytes = block_on(result.bytes()).map_err(|error| location.error(&key, error))?;
    let record: StatsRecord =
        serde_json::from_slice(&bytes).map_err(|error| location.invalid_data(&key, error))?;
    match &record {
        StatsRecord::Complete(stats) => stats
            .validate(id)
            .map_err(|error| location.invalid_data(&key, error.into_data_source()))?,
        StatsRecord::Missing { .. } if record != StatsRecord::missing(id)? => {
            return Err(location.invalid_data(
                &key,
                "placeholder identity or index range does not match table",
            ));
        }
        StatsRecord::Missing { .. } => {}
    }
    Ok(record)
}

#[cfg(test)]
pub(crate) fn read_stats(location: &Location, id: &TableId) -> EgtResult<EgtStats> {
    match read_record(location, id)? {
        StatsRecord::Complete(stats) => Ok(stats),
        StatsRecord::Missing { .. } => Err(invalid_data("requested statistics are unavailable")),
    }
}

impl EgtStats {
    fn validate(&self, id: &TableId) -> EgtResult<()> {
        let frame_size = Egt::new(id)?.frame_size();
        if self.tablename != id.name().as_str()
            || self.sha256.len() != 64
            || !self.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
            || self.totals.bytes == 0
            || self.index_range == 0
            || self.frame_size != frame_size
            || self.totals.num_frames != self.index_range.div_ceil(self.frame_size)
        {
            return Err(invalid_data(
                "invalid table identity, SHA-256, or frame geometry",
            ));
        }
        self.totals.validate(self.index_range)
    }
}

fn invalid_data(reason: &str) -> EgtError {
    EgtError::invalid_data("statistics", reason)
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
            sha256: "ab".repeat(32),
            index_range: 16,
            frame_size: Egt::new(&TableId::parse("KQ_K").unwrap())
                .unwrap()
                .frame_size(),
            totals: Totals {
                bytes: 100,
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
            },
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
        let mut direct = AggregateStats::default();
        direct.add(&stats());
        direct.validate().unwrap();
        assert_eq!((direct.selected_tables, direct.available_tables), (1, 1));
        assert_eq!(direct, aggregate(&[StatsRecord::Complete(stats())]));
        let low = StatsRecord::Complete(stats());
        let mut high = stats();
        high.totals.histogram_win = BTreeMap::from([(5, 3)]);
        high.totals.longest_dtc_win = vec![example("b", 5), example("a", 5), example("a", 5)];
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
        let totals = total.totals.as_ref().unwrap();
        assert_eq!((totals.bytes, totals.unique_positions), (200, 12));
        assert_eq!(totals.invalid_opposite_check, 2);
        assert_eq!(totals.invalid_ep_squares_occupied, 4);
        assert_eq!(totals.invalid_illegal_ep_capture, 6);
        assert_eq!(totals.redundant, 8);
        assert_eq!(
            totals.histogram_win,
            BTreeMap::from([(1, 1), (3, 2), (5, 3)])
        );
        assert_eq!(
            totals.longest_dtc_win,
            vec![example("a", 5), example("b", 5)]
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
            assert_eq!(
                total.index_range,
                stats().index_range + missing.index_range()
            );
            assert_eq!(
                serde_json::to_value(total).unwrap()["totals"],
                serde_json::Value::Null
            );
        }
    }

    #[test]
    fn validates_file_metadata_and_cached_totals() {
        let id = TableId::parse("KQ_K").unwrap();
        stats().validate(&id).unwrap();
        let mut wrong_frame_size = stats();
        wrong_frame_size.frame_size += 1;
        assert!(wrong_frame_size.validate(&id).is_err());
        for field in [
            "bytes",
            "num_frames",
            "index_range",
            "win",
            "unique_positions",
        ] {
            let mut json = serde_json::to_value(stats()).unwrap();
            json[field] = 0.into();
            let invalid: EgtStats = serde_json::from_value(json).unwrap();
            assert!(invalid.validate(&id).is_err(), "{field}");
        }
        let mut invalid = stats();
        invalid.sha256.clear();
        assert!(invalid.validate(&id).is_err());
        invalid = stats();
        invalid.totals.histogram_win.insert(9, 1);
        assert!(invalid.validate(&id).is_err());
        invalid = stats();
        invalid.totals.longest_dtc_win[0].ply = 1;
        assert!(invalid.validate(&id).is_err());
        let mut total = aggregate(&[StatsRecord::Complete(stats())]);
        total.available_tables = 0;
        assert!(total.validate().is_err());
        total = aggregate(&[StatsRecord::Complete(stats())]);
        total.totals.as_mut().unwrap().win = 0;
        assert!(total.validate().is_err());
        total = AggregateStats::default();
        total.totals.as_mut().unwrap().bytes = 1;
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
            let mut missing = json.clone();
            missing.as_object_mut().unwrap().remove("totals");
            assert!(serde_json::from_value::<AggregateStats>(missing).is_err());
            let mut inconsistent = json.clone();
            inconsistent["totals"] = if total.totals.is_some() {
                serde_json::Value::Null
            } else {
                serde_json::to_value(Totals::default()).unwrap()
            };
            assert!(
                serde_json::from_value::<AggregateStats>(inconsistent)
                    .unwrap()
                    .validate()
                    .is_err()
            );
            if total.totals.is_some() {
                for field in PARTITION_FIELDS {
                    let mut invalid = json.clone();
                    invalid["totals"].as_object_mut().unwrap().remove(field);
                    assert!(serde_json::from_value::<AggregateStats>(invalid.clone()).is_err());
                    invalid["totals"][field] = serde_json::Value::Null;
                    assert!(serde_json::from_value::<AggregateStats>(invalid).is_err());
                }
            }
            let mut old = json;
            old.as_object_mut().unwrap().remove("totals");
            old["bytes"] = serde_json::Value::Null;
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
                json["totals"][field] = count.into();
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
