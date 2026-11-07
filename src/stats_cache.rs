//! Optional hierarchy summaries, rebuilt exclusively from per-table sidecars.
//!
//! Puts are atomic per object, not across a subtree. Do not run cache rebuilds
//! concurrently with generation or other rebuilds. Publishers must invalidate
//! a table's ancestor summaries before publishing its per-table statistics.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use object_store::{ObjectStoreExt, PutMode, PutOptions};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::selection::{hierarchy_prefixes, resolve_selectors};
use crate::stats::{AggregateStats, StatsRecord, read_record};
use crate::storage::{Location, block_on};
use crate::table_id::TableId;
use crate::{EgtError, EgtResult};

/// Metadata queries and hierarchy-cache rebuilding for one tablebase root.
///
/// Statistics do not read table frames and have no frame-cache budget or input
/// overlay: each instance describes exactly one root. All operations, including
/// construction, are synchronous; async callers should use `spawn_blocking`.
pub struct EgtStatistics {
    location: Location,
}

impl EgtStatistics {
    /// Resolves a local tablebase directory or supported object-store URL once.
    pub fn new(path: impl AsRef<Path>) -> EgtResult<Self> {
        Ok(Self::from_location(Location::parse(path)?))
    }

    pub(crate) fn from_location(location: Location) -> Self {
        Self { location }
    }

    /// Reads metadata without modifying storage. Missing sidecars contribute
    /// theoretical index ranges; computed totals are null for incomplete selections.
    /// Hierarchy selections use cached summaries when available.
    pub fn stats(&self, selectors: &[&str]) -> EgtResult<AggregateStats> {
        query(&self.location, selectors)
    }

    /// Creates missing placeholders and rebuilds selected hierarchy summaries.
    /// Does not scan table contents or replace complete per-table metadata.
    /// Do not run concurrently with generation or another cache rebuild.
    pub fn cache_stats(&self, selectors: &[&str]) -> EgtResult<AggregateStats> {
        cache(&self.location, selectors)
    }
}

/// Binds cached totals to their canonical hierarchy prefix, including its '/'.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Summary {
    hierarchy: String,
    stats: AggregateStats,
}

#[derive(Clone, Copy)]
enum ReadMode {
    ReadOnly,
    Create,
    Rebuild,
}

/// Reads summaries when available, without creating placeholders or caches.
pub(crate) fn query(location: &Location, selectors: &[&str]) -> EgtResult<AggregateStats> {
    let prefixes = hierarchy_prefixes(selectors)?;
    // Validate nonhierarchical selectors before touching storage, too.
    let mut tables = if prefixes.is_none() {
        resolve_selectors(selectors)?
    } else {
        Vec::new()
    };
    location.validate_format()?;
    let mut total = AggregateStats::default();
    for prefix in prefixes.into_iter().flatten() {
        if let Some(summary) = read_summary(location, &prefix)? {
            total.merge(&summary);
        } else {
            tables.extend(resolve_selectors(&[&prefix])?);
        }
    }
    visit_records(location, &tables, ReadMode::ReadOnly, |_, record| {
        total.add_record(&record);
    })?;
    total.validate()?;
    Ok(total)
}

/// Rebuilds selected summaries and creates only absent per-table sidecars.
pub(crate) fn cache(location: &Location, selectors: &[&str]) -> EgtResult<AggregateStats> {
    let prefixes = hierarchy_prefixes(selectors)?;
    let tables = resolve_selectors(selectors)?;
    location.ensure_writable()?;
    location.ensure_format()?;
    // Invalidate outside ancestors before changing any sidecars or summaries.
    let outside: BTreeSet<_> = prefixes
        .iter()
        .flatten()
        .flat_map(|prefix| ancestors(prefix.trim_end_matches('/')))
        .collect();
    for prefix in outside {
        delete_summary(location, &prefix)?;
    }
    let mode = if prefixes.is_some() {
        ReadMode::Rebuild
    } else {
        ReadMode::Create
    };
    let mut total = AggregateStats::default();
    let mut summaries: BTreeMap<String, AggregateStats> = BTreeMap::new();
    visit_records(location, &tables, mode, |id, record| {
        total.add_record(&record);
        for ancestor in ancestors(&id.relative_path("json")) {
            if prefixes
                .iter()
                .flatten()
                .any(|prefix| ancestor.starts_with(prefix))
            {
                summaries.entry(ancestor).or_default().add_record(&record);
            }
        }
    })?;
    // Reverse lexical order publishes descendants before their parents.
    for (hierarchy, stats) in summaries.into_iter().rev() {
        stats.validate()?;
        let key = location.key(&format!("{hierarchy}stats.json"));
        let bytes = serde_json::to_vec_pretty(&Summary { hierarchy, stats })?;
        block_on(location.store.put(&key, bytes.into()))
            .map_err(|error| location.error(&key, error))?;
    }
    total.validate()?;
    Ok(total)
}

/// Invalidates the four hierarchy summaries before publishing a table sidecar.
pub(crate) fn invalidate(location: &Location, id: &TableId) -> EgtResult<()> {
    for prefix in ancestors(&id.relative_path("json")) {
        delete_summary(location, &prefix)?;
    }
    Ok(())
}

// Input is either a table path or a hierarchy directory without its trailing /.
fn ancestors(path: &str) -> impl Iterator<Item = String> + '_ {
    path.match_indices('/')
        .map(|(index, _)| path[..=index].to_owned())
}

fn delete_summary(location: &Location, prefix: &str) -> EgtResult<()> {
    let key = location.key(&format!("{prefix}stats.json"));
    match block_on(location.store.delete(&key)) {
        Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
        Err(error) => Err(location.error(&key, error)),
    }
}

fn read_summary(location: &Location, prefix: &str) -> EgtResult<Option<AggregateStats>> {
    let key = location.key(&format!("{prefix}stats.json"));
    let result = match block_on(location.store.get(&key)) {
        Ok(result) => result,
        Err(object_store::Error::NotFound { .. }) => return Ok(None),
        Err(error) => return Err(location.error(&key, error)),
    };
    let bytes = block_on(result.bytes()).map_err(|error| location.error(&key, error))?;
    let summary: Summary =
        serde_json::from_slice(&bytes).map_err(|error| location.invalid_data(&key, error))?;
    if summary.hierarchy != prefix || summary.stats.selected_tables == 0 {
        return Err(location.invalid_data(
            &key,
            "cached summary must match the requested hierarchy and contain tables",
        ));
    }
    summary
        .stats
        .validate()
        .map_err(|error| location.invalid_data(&key, error))?;
    Ok(Some(summary.stats))
}

fn record(location: &Location, id: &TableId, mode: ReadMode) -> EgtResult<StatsRecord> {
    match read_record(location, id) {
        Ok(record) => return Ok(record),
        Err(EgtError::FileNotFound(_)) => {}
        Err(error) => return Err(error),
    }
    let missing = StatsRecord::missing(id)?;
    if matches!(mode, ReadMode::ReadOnly) {
        return Ok(missing);
    }
    if matches!(mode, ReadMode::Create) {
        invalidate(location, id)?;
    }
    let key = location.key(&id.relative_path("json"));
    let bytes = serde_json::to_vec_pretty(&missing)?;
    let options = PutOptions {
        mode: PutMode::Create,
        ..Default::default()
    };
    match block_on(location.store.put_opts(&key, bytes.into(), options)) {
        Ok(_) => Ok(missing),
        // Never overwrite a complete record published after our missing read.
        Err(object_store::Error::AlreadyExists { .. }) => read_record(location, id),
        Err(error) => Err(location.error(&key, error)),
    }
}

fn visit_records(
    location: &Location,
    tables: &[TableId],
    mode: ReadMode,
    mut visit: impl FnMut(&TableId, StatsRecord),
) -> EgtResult<()> {
    if tables.is_empty() {
        return Ok(());
    }
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(8)
        .build()
        .map_err(|_| EgtError::Internal("failed to build statistics thread pool"))?;
    for batch in tables.chunks(32) {
        let results: Vec<_> = pool.install(|| {
            batch
                .par_iter()
                .map(|id| record(location, id, mode))
                .collect()
        });
        for (id, result) in batch.iter().zip(results) {
            visit(id, result?);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn location() -> Location {
        let location = Location::parse("memory:///nested/stats-cache").unwrap();
        location.ensure_format().unwrap();
        location
    }

    fn put(location: &Location, path: &str, bytes: Vec<u8>) {
        block_on(location.store.put(&location.key(path), bytes.into())).unwrap();
    }

    fn absent(location: &Location, path: &str) -> bool {
        matches!(
            block_on(location.store.head(&location.key(path))),
            Err(object_store::Error::NotFound { .. })
        )
    }

    #[test]
    fn validates_before_storage_and_queries_missing_records_read_only() {
        let mut location = Location::parse("memory:///empty").unwrap();
        for operation in [query, cache] {
            assert!(matches!(
                operation(&location, &["2", "K_K"]),
                Err(EgtError::InvalidEndgameName { .. })
            ));
        }
        assert!(absent(&location, "format.json"));
        assert!(query(&location, &[]).is_err());
        assert_eq!(cache(&location, &[]).unwrap(), AggregateStats::default());
        location.writable = false;
        let id = TableId::parse("K_K").unwrap();
        let summary = query(&location, &["2"]).unwrap();
        assert_eq!(summary, query(&location, &["K_K"]).unwrap());
        assert_eq!((summary.selected_tables, summary.available_tables), (1, 0));
        let missing = StatsRecord::missing(&id).unwrap();
        assert_eq!(summary.index_range, missing.index_range());
        assert_eq!(summary.bytes, None);
        assert!(absent(&location, &id.relative_path("json")));
        assert!(absent(&location, "2/stats.json"));
        assert!(cache(&location, &["2"]).is_err());
    }

    #[test]
    fn builds_all_levels_and_uses_summaries_for_overlapping_selections() {
        let location = location();
        let selectors = ["3/2v1/op0", "2", "3/", "3/2v1", "2/1v1/op0/0pawns"];
        let expected = query(&location, &["2", "3"]).unwrap();
        assert_eq!(cache(&location, &selectors).unwrap(), expected);
        let mut prefixes = BTreeSet::new();
        for id in resolve_selectors(&["2", "3"]).unwrap() {
            prefixes.extend(ancestors(&id.relative_path("json")));
            put(&location, &id.relative_path("json"), b"broken".to_vec());
        }
        for prefix in prefixes {
            let expected = read_summary(&location, &prefix).unwrap().unwrap();
            assert_eq!(query(&location, &[&prefix]).unwrap(), expected);
        }
        assert_eq!(query(&location, &selectors).unwrap(), expected);
        assert!(cache(&location, &selectors).is_err());
    }

    #[test]
    fn subtree_rebuild_and_placeholder_creation_invalidate_ancestors() {
        let location = location();
        cache(&location, &["3"]).unwrap();
        let sibling_prefix = "3/2v1/op0/1pawns/";
        let sibling = read_summary(&location, sibling_prefix).unwrap();
        let prefix = "3/2v1/op0/0pawns/";
        let expected = query(&location, &[prefix]).unwrap();
        let path = format!("{prefix}stats.json");
        put(&location, &path, b"broken".to_vec());
        assert_eq!(cache(&location, &[prefix]).unwrap(), expected);
        for parent in ancestors(prefix.trim_end_matches('/')) {
            assert!(absent(&location, &format!("{parent}stats.json")));
        }
        assert_eq!(read_summary(&location, sibling_prefix).unwrap(), sibling);
        cache(&location, &["2"]).unwrap();
        let id = TableId::parse("K_K").unwrap();
        let key = location.key(&id.relative_path("json"));
        block_on(location.store.delete(&key)).unwrap();
        let summary = cache(&location, &["K_K", "K_K"]).unwrap();
        assert_eq!(summary.available_tables, 0);
        for prefix in ancestors(&id.relative_path("json")) {
            assert!(absent(&location, &format!("{prefix}stats.json")));
        }
        invalidate(&location, &id).unwrap();
    }

    #[test]
    fn malformed_records_and_summary_envelopes_are_errors() {
        let location = location();
        let id = TableId::parse("K_K").unwrap();
        for bytes in [b"broken".to_vec(), b"{}".to_vec()] {
            put(&location, &id.relative_path("json"), bytes);
            for result in [query(&location, &["K_K"]), cache(&location, &["2"])] {
                assert!(matches!(result, Err(EgtError::InvalidData { location, .. })
                    if location.ends_with("/K_K.json")));
            }
        }
        let missing = serde_json::to_vec(&StatsRecord::missing(&id).unwrap()).unwrap();
        put(&location, &id.relative_path("json"), missing);
        let stats = query(&location, &["K_K"]).unwrap();
        let envelope = serde_json::json!({ "hierarchy": "2/", "stats": stats });
        let empty = serde_json::to_value(AggregateStats::default()).unwrap();
        let mut invalid = vec![serde_json::json!({}), empty.clone()];
        for (field, value) in [
            ("hierarchy", "2".into()),
            ("hierarchy", "3/".into()),
            ("unknown", true.into()),
            ("stats", empty),
        ] {
            let mut summary = envelope.clone();
            summary[field] = value;
            invalid.push(summary);
        }
        for value in invalid {
            let bytes = serde_json::to_vec(&value).unwrap();
            put(&location, "2/stats.json", bytes);
            assert!(matches!(query(&location, &["2"]),
                Err(EgtError::InvalidData { location, .. }) if location.ends_with("/2/stats.json")));
            assert!(query(&location, &["K_K"]).is_ok());
        }
        assert!(cache(&location, &["2"]).is_ok());
    }
}
