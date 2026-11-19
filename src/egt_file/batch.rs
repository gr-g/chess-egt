use std::collections::HashMap;

use rayon::prelude::*;
use shakmaty::Chess;

use super::{EgtFileReader, load_existing_file};
use crate::storage::Location;
use crate::table_id::TableId;
use crate::{DtcOutcome, EgtError, EgtResult, PackedOutcome};

/// Job-scoped tables loaded before workers borrow them immutably.
/// All enumerated dependencies must exist and their decompressed data must fit
/// in memory together. Probes never load data or update shared state.
#[derive(Default)]
pub(crate) struct BatchReader {
    files: HashMap<TableId, EgtFileReader>,
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
            if !reader.files.contains_key(id) {
                let file =
                    load_existing_file(base, input, id).map_err(
                        |source| match source {
                            EgtError::FileNotFound(_) => EgtError::DependencyUnavailable {
                                dependency: id.to_string(),
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
        self.files.insert(file.egt().id(), file);
    }

    pub(crate) fn load(&mut self) -> EgtResult<()> {
        self.files.par_iter_mut().try_for_each(|(_, file)| {
            if file.frames.iter().any(Option::is_none) {
                file.load_all()?;
            }
            Ok(())
        })
    }

    /// Retained palettes and indices; metadata, working arrays and temporary buffers are extra.
    pub(crate) fn resident_bytes(&self) -> u128 {
        self.files
            .values()
            .flat_map(|file| file.frames.iter().flatten())
            .map(|frame| frame.bytes() as u128)
            .sum()
    }

    pub(crate) fn file(&self, name: &TableId) -> EgtResult<&EgtFileReader> {
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
        let (name, reflected) = TableId::from_position(position)?;
        let file = self.file(&name)?;
        let index = file.map_routed_position_to_index(position, reflected)?;
        let outcome = file
            .read_from_index_cached(index)?
            .ok_or(EgtError::Internal("cold frame in prepared dependencies"))?;
        Ok((index, outcome))
    }

    pub(crate) fn read_chunk(
        &self,
        name: &TableId,
        start: usize,
        end: usize,
    ) -> EgtResult<Vec<PackedOutcome>> {
        self.file(name)?
            .frame_chunk_cached(start, end)?
            .ok_or(EgtError::Internal("cold frame in prepared dependencies"))
    }
}
