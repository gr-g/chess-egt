use std::collections::{HashMap, hash_map::Entry};

use rayon::prelude::*;
use shakmaty::Chess;

use super::{Frame, Source, TableFile};
use crate::table_id::TableId;
use crate::outcome::PackedOutcome;
use crate::{DtcOutcome, Egt, EgtError, EgtResult};

/// Job-scoped tables loaded before workers borrow them immutably.
/// All enumerated dependencies must exist and their decompressed data must fit
/// in memory together. Probes never load data or update shared state.
#[derive(Default)]
pub(crate) struct BatchReader {
    files: HashMap<TableId, (TableFile, Vec<Frame>)>,
}

impl BatchReader {
    /// Opens all metadata first, so missing dependencies fail before bulk loading.
    pub(crate) fn open(source: &Source, ids: &[TableId]) -> EgtResult<Self> {
        let mut files = HashMap::new();
        for &id in ids {
            if let Entry::Vacant(entry) = files.entry(id) {
                let file = source.open(&id).map_err(|source| match source {
                    EgtError::FileNotFound(_) => EgtError::DependencyUnavailable {
                        dependency: id.to_string(),
                        source: Box::new(source),
                    },
                    other => other,
                })?;
                entry.insert(file);
            }
        }
        let files = files
            .into_par_iter()
            .map(|(id, file)| {
                let frames = file.read_all()?;
                Ok((id, (file, frames)))
            })
            .collect::<EgtResult<_>>()?;
        Ok(Self { files })
    }

    /// Also accepts explicit outputs, which must override input-overlay copies.
    pub(crate) fn insert(&mut self, file: TableFile, frames: Vec<Frame>) {
        debug_assert_eq!(file.num_frames(), frames.len());
        self.files.insert(file.egt().id(), (file, frames));
    }

    /// Retained palettes and indices; metadata, working arrays and temporary buffers are extra.
    pub(crate) fn resident_bytes(&self) -> u128 {
        self.files
            .values()
            .flat_map(|(_, frames)| frames)
            .map(|frame| frame.bytes() as u128)
            .sum()
    }

    fn table(&self, id: &TableId) -> &(TableFile, Vec<Frame>) {
        self.files
            .get(id)
            .expect("table absent from prepared dependencies")
    }

    pub(crate) fn egt(&self, id: &TableId) -> &Egt {
        self.table(id).0.egt()
    }

    pub(crate) fn probe(&self, position: &Chess) -> EgtResult<PackedOutcome> {
        self.probe_indexed(position).map(|(_, outcome)| outcome)
    }

    pub(crate) fn probe_outcome(&self, position: &Chess) -> EgtResult<DtcOutcome> {
        let (index, outcome) = self.probe_indexed(position)?;
        outcome.to_outcome_at(index)
    }

    fn probe_indexed(&self, position: &Chess) -> EgtResult<(usize, PackedOutcome)> {
        let (id, reflected) = TableId::from_position(position)?;
        let (file, frames) = self.table(&id);
        let index = file.index(position, reflected)?;
        let (frame, offset) = file.locate(index);
        Ok((index, frames[frame].get(offset)))
    }

    pub(crate) fn read_chunk(
        &self,
        id: &TableId,
        start: usize,
        end: usize,
    ) -> EgtResult<Vec<PackedOutcome>> {
        let (file, frames) = self.table(id);
        let range = file.egt().index_range();
        if start > end || end > range {
            return Err(EgtError::IndexOutOfRange { index: end, range });
        }
        if start == end {
            return Ok(Vec::new());
        }
        let (frame, offset) = file.locate(start);
        frames[frame].expand(offset..offset + (end - start))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egt_file::writer::write_fixture;
    use crate::storage::Location;
    use crate::ConversionType;
    use shakmaty::{CastlingMode, fen::Fen};

    #[test]
    fn explicit_table_replaces_loaded_overlay() {
        let base = crate::TestDir::new("batch_output");
        let input = crate::TestDir::new("batch_input");
        let output = PackedOutcome::new_win(ConversionType::Checkmate, 3);
        let file = write_fixture(&base.0, "KQ_K", |_| output);
        write_fixture(&input.0, "KQ_K", |_| PackedOutcome::DRAW);
        let id = file.egt().id();
        let source = Source::new(Location::parse(&base.0).unwrap())
            .with_input(Some(Location::parse(&input.0).unwrap()));
        let mut batch = BatchReader::open(&source, &[id]).unwrap();
        let position = "8/8/8/8/8/8/Q7/K6k w - - 0 1"
            .parse::<Fen>()
            .unwrap()
            .into_position(CastlingMode::Standard)
            .unwrap();
        assert_eq!(batch.probe(&position).unwrap(), PackedOutcome::DRAW);
        let frames = file.read_all().unwrap();
        batch.insert(file, frames);
        assert_eq!(batch.probe(&position).unwrap(), output);
    }
}
