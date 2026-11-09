use std::collections::{BTreeSet, HashMap};

use serde::Serialize;

use crate::{Egt, EgtResult, TableId, resolve_selectors};

/// Theoretical streaming memory for one selected table, in bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoryEstimate {
    pub tablename: String,
    pub generation_bytes: u64,
    pub verification_bytes: u64,
}

/// Estimates streaming memory without accessing storage, in selector resolution order.
/// Each table occupies two bytes per indexed entry. Generation retains the source
/// and its twin (once for self twins); verification retains the distinct union of
/// the twin and direct conversion dependencies, including the source for self twins.
/// These theoretical sizes exclude indexing, worker, buffer, and other overhead.
pub fn memory_estimates(selectors: &[&str]) -> EgtResult<Vec<MemoryEstimate>> {
    let mut sizes = HashMap::new();
    let mut table_bytes = |id: &TableId| -> EgtResult<u64> {
        if let Some(&bytes) = sizes.get(id) {
            return Ok(bytes);
        }
        let bytes = 2 * Egt::from_elements(id.parse_elements())?.index_range() as u64;
        sizes.insert(id.clone(), bytes);
        Ok(bytes)
    };
    resolve_selectors(selectors)?.into_iter().map(|id| {
        let twin = id.twin().0;
        let mut generation_bytes = table_bytes(&id)?;
        if twin != id {
            generation_bytes += table_bytes(&twin)?;
        }
        let dependencies: BTreeSet<_> = id.dependencies().into_iter()
            .chain(std::iter::once(twin)).collect();
        let mut verification_bytes = 0;
        for dependency in dependencies {
            verification_bytes += table_bytes(&dependency)?;
        }
        Ok(MemoryEstimate {
            tablename: id.tablename().to_owned(),
            generation_bytes,
            verification_bytes,
        })
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(names: &[&str]) -> u64 {
        names.iter().map(|name| {
            2 * Egt::from_tablename(name).unwrap().index_range() as u64
        }).sum()
    }

    #[test]
    fn estimates_capture_dependencies_and_self_twins() {
        for (name, generation, verification) in [
            ("KQ_K", vec!["KQ_K", "K_KQ"], vec!["K_KQ"]),
            ("KQ_KRR", vec!["KQ_KRR", "KRR_KQ"], vec!["KRR_KQ", "KR_KQ"]),
            ("KQ_KQ", vec!["KQ_KQ"], vec!["KQ_KQ", "K_KQ"]),
            ("K_K", vec!["K_K"], vec!["K_K"]),
        ] {
            assert_eq!(memory_estimates(&[name]).unwrap(), vec![MemoryEstimate {
                tablename: name.to_owned(),
                generation_bytes: bytes(&generation),
                verification_bytes: bytes(&verification),
            }]);
        }
    }

    #[test]
    fn estimates_direct_promotions_and_resolves_selectors() {
        let estimates = memory_estimates(&["KPa_K", "KQ_K", "KPa_K"]).unwrap();
        assert_eq!(estimates[0], memory_estimates(&["KQ_K"]).unwrap()[0]);
        assert_eq!(estimates[1], MemoryEstimate {
            tablename: "KPa_K".to_owned(),
            generation_bytes: bytes(&["KPa_K", "K_KPa"]),
            verification_bytes: bytes(&["K_KPa", "K_KQ", "K_KR", "K_KB", "K_KN"]),
        });
        assert_eq!(estimates.len(), 2);
        assert!(memory_estimates(&[]).unwrap().is_empty());
        assert!(memory_estimates(&["invalid"]).is_err());
    }
}
