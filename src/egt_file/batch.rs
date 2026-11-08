use std::collections::HashMap;

use rayon::prelude::*;
use shakmaty::Chess;

use super::{EgtFileReader, load_existing_file};
use crate::storage::Location;
use crate::table_id::{TableId, position_table_name};
use crate::{DtcOutcome, EgtError, EgtResult, PackedOutcome};

/// Job-scoped tables loaded before workers borrow them immutably.
/// All enumerated dependencies must exist and their decompressed data must fit
/// in memory together. Probes never load data or update shared state.
#[derive(Default)]
pub(crate) struct BatchReader {
    files: HashMap<String, EgtFileReader>,
}

impl BatchReader {
    /// Opens all metadata first, so missing dependencies fail before bulk loading.
    pub(crate) fn open(
        base: &Location,
        input: Option<&Location>,
        ids: &[TableId],
    ) -> EgtResult<Self> {
        let mut reader = Self::default();
        for id in ids {
            if !reader.files.contains_key(id.tablename()) {
                let file =
                    load_existing_file(base, input, id.tablename()).map_err(
                        |source| match source {
                            EgtError::FileNotFound(_) => EgtError::DependencyUnavailable {
                                dependency: id.tablename().to_owned(),
                                source: Box::new(source),
                            },
                            other => other,
                        },
                    )?;
                reader.insert(file);
            }
        }
        Ok(reader)
    }

    /// Also accepts explicit outputs, which must override input-overlay copies.
    pub(crate) fn insert(&mut self, file: EgtFileReader) {
        self.files.insert(file.egt().tablename().to_owned(), file);
    }

    pub(crate) fn load(&mut self) -> EgtResult<()> {
        self.files.par_iter_mut().try_for_each(|(_, file)| {
            if file.frames.iter().any(Option::is_none) {
                file.load_all()?;
            }
            Ok(())
        })
    }

    /// Outcome storage only; metadata, working arrays and temporary buffers are extra.
    pub(crate) fn resident_bytes(&self) -> u128 {
        self.files
            .values()
            .map(|file| file.egt().index_range() as u128 * 2)
            .sum()
    }

    pub(crate) fn file(&self, name: &str) -> EgtResult<&EgtFileReader> {
        self.files.get(name).ok_or(EgtError::Internal(
            "table absent from prepared dependencies",
        ))
    }

    pub(crate) fn probe(&self, position: &Chess) -> EgtResult<PackedOutcome> {
        self.probe_indexed(position).map(|(_, outcome)| outcome)
    }

    pub(crate) fn probe_outcome(&self, position: &Chess) -> EgtResult<DtcOutcome> {
        let (index, outcome) = self.probe_indexed(position)?;
        outcome.to_outcome_at(index)
    }

    fn probe_indexed(&self, position: &Chess) -> EgtResult<(usize, PackedOutcome)> {
        let (name, reflected) = position_table_name(position)?;
        let file = self.file(name.as_str())?;
        let index = file.map_routed_position_to_index(position, reflected)?;
        let outcome = file
            .read_from_index_cached(index)?
            .ok_or(EgtError::Internal("cold frame in prepared dependencies"))?;
        Ok((index, outcome))
    }

    pub(crate) fn read_chunk(
        &self,
        name: &str,
        start: usize,
        end: usize,
    ) -> EgtResult<&[PackedOutcome]> {
        self.file(name)?
            .frame_chunk_cached(start, end)?
            .ok_or(EgtError::Internal("cold frame in prepared dependencies"))
    }
}
