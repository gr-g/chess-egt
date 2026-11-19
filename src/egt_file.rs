mod batch;
mod cache;
mod frame;
pub(crate) mod writer;

pub(crate) use batch::BatchReader;
pub(crate) use cache::SharedFileCache;
use frame::Frame;

use crate::egt::Egt;
use crate::error::{EgtError, EgtResult};
use crate::storage::{Location, block_on};
use crate::table_id::TableId;
use crate::PackedOutcome;
#[cfg(test)]
use crate::ConversionType;
use object_store::{GetOptions, GetRange, ObjectStoreExt, path::Path as ObjectPath};
#[cfg(test)]
use shakmaty::{CastlingMode, Position};
use shakmaty::{Chess, Color};
use std::path::Path;
use rayon::prelude::*;

/// Resolves an existing table with input-first, missing-only fallback.
pub(crate) fn load_existing_file(
    base_path: &Location,
    input_path: Option<&Location>,
    id: &TableId,
) -> EgtResult<EgtFileReader> {
    if let Some(input) = input_path {
        match EgtFileReader::open_in(input, id) {
            Ok(file) => return Ok(file),
            Err(EgtError::FileNotFound(_)) => {}
            Err(error) => return Err(error),
        }
    }
    EgtFileReader::open_in(base_path, id)
}

/// Read-only storage and frame cache for one existing canonical table.
#[derive(Debug)]
pub struct EgtFileReader {
    egt: Egt,
    location: Location,
    key: ObjectPath,
    seek_table: zeekstd::SeekTable,
    /// `None` means the frame has not been loaded or has been evicted.
    frames: Vec<Option<Frame>>,
}

impl EgtFileReader {
    /// The immutable descriptor of this canonical table.
    pub fn egt(&self) -> &Egt {
        &self.egt
    }

    /// Opens an existing table without modifying storage. Data is read on demand.
    /// Objects must not be replaced while this reader is in use: reads are not
    /// pinned to an object version.
    pub fn new_from_file(base_path: &Path, tablename: &str) -> EgtResult<Self> {
        Self::open_in(&Location::parse(base_path)?, &TableId::parse(tablename)?)
    }

    pub(crate) fn open_in(location: &Location, id: &TableId) -> EgtResult<Self> {
        let egt = Egt::new(id)?;
        let index_range = egt.index_range();
        let key = location.key(&id.relative_path("ggegt"));
        location.validate_format()?;
        let seek_table = Self::read_seek_table(location, &key, index_range, egt.frame_size())?;
        let frames = vec![None; index_range.div_ceil(egt.frame_size())];
        Ok(Self {
            key,
            egt,
            location: location.clone(),
            seek_table,
            frames,
        })
    }

    fn read_seek_table(
        location: &Location,
        key: &ObjectPath,
        index_range: usize,
        frame_size: usize,
    ) -> EgtResult<zeekstd::SeekTable> {
        // Our checksum-free seek tables have 8-byte entries and 17 bytes of framing.
        let num_frames = index_range.div_ceil(frame_size);
        let table_size = 17 + 8 * num_frames as u64;
        let options = GetOptions::new().with_range(Some(GetRange::Suffix(table_size)));
        let result = block_on(location.store.get_opts(key, options))
            .map_err(|error| location.error(key, error))?;
        let table_start = result.range.start;
        let bytes = block_on(result.bytes()).map_err(|error| location.error(key, error))?;
        if bytes.len() as u64 != table_size {
            return Err(location.invalid_data(key, "truncated tablebase seek table"));
        }
        // zeekstd allocates from the footer count before checking the source length.
        let count = u32::from_le_bytes(bytes[bytes.len() - 9..bytes.len() - 5].try_into().unwrap());
        if count as usize != num_frames {
            return Err(location.invalid_data(key, "invalid tablebase frame count"));
        }
        let mut source = zeekstd::BytesWrapper::new(&bytes);
        let table = zeekstd::SeekTable::from_seekable(&mut source)
            .map_err(|error| location.invalid_data(key, error))?;
        if table.size_comp() != table_start {
            return Err(location.invalid_data(key, "invalid tablebase compressed length"));
        }
        for index in 0..num_frames {
            let len = frame_size.min(index_range - index * frame_size);
            let decompressed = table
                .frame_size_decomp(index as u32)
                .map_err(|error| location.invalid_data(key, error))?;
            let compressed = table
                .frame_size_comp(index as u32)
                .map_err(|error| location.invalid_data(key, error))?;
            if !frame::valid_length(decompressed, len) || compressed == 0 {
                return Err(location.invalid_data(key, format!("invalid frame {index} length")));
            }
        }
        Ok(table)
    }

    pub fn num_frames(&self) -> usize {
        self.frames.len()
    }

    /// Probes the outcome of a specific position.
    pub fn probe(&mut self, position: &Chess) -> EgtResult<PackedOutcome> {
        let index = self.map_position_to_index(position)?;
        self.read_from_index(index)
    }

    /// Probes without allocating, decompressing, or reading from disk.
    /// `None` means the frame is cold, not that the outcome is invalid/unknown.
    #[cfg(test)]
    pub(crate) fn probe_cached(&self, position: &Chess) -> EgtResult<Option<PackedOutcome>> {
        self.read_from_index_cached(self.map_position_to_index(position)?)
    }

    fn read_from_index_cached(&self, index: usize) -> EgtResult<Option<PackedOutcome>> {
        if index >= self.egt.index_range() {
            return Err(EgtError::IndexOutOfRange {
                index,
                range: self.egt.index_range(),
            });
        }
        let (frame_idx, offset) = self.locate_frame(index);
        Ok(self.frames[frame_idx]
            .as_ref()
            .map(|uncompressed| uncompressed.get(offset)))
    }

    /// Gets the resident palette and indices, loading the frame if needed.
    fn get_frame_data(&mut self, frame_idx: usize) -> EgtResult<&Frame> {
        self.ensure_uncompressed_frames(&[frame_idx])?;

        self.frames[frame_idx]
            .as_ref()
            .ok_or(EgtError::Internal("expected resident frame after loading"))
    }

    /// Expands the table-local indices `[start, end)` into owned outcomes, loading
    /// the frame if needed. The range must lie within a single frame; the cache
    /// retains its palette and indices rather than the expanded outcomes.
    pub fn frame_chunk(&mut self, start: usize, end: usize) -> EgtResult<Vec<PackedOutcome>> {
        if start > end || end > self.egt.index_range() {
            return Err(EgtError::IndexOutOfRange {
                index: end,
                range: self.egt.index_range(),
            });
        }
        if start == end {
            return Ok(Vec::new());
        }
        let (frame_idx, offset) = self.locate_frame(start);
        if end - start > self.frame_len(frame_idx) - offset {
            return Err(EgtError::Internal(
                "frame_chunk range crosses a frame boundary",
            ));
        }
        let data = self.get_frame_data(frame_idx)?;
        data.expand(offset..offset + (end - start))
    }

    fn frame_chunk_cached(
        &self,
        start: usize,
        end: usize,
    ) -> EgtResult<Option<Vec<PackedOutcome>>> {
        if start > end || end > self.egt.index_range() {
            return Err(EgtError::IndexOutOfRange {
                index: end,
                range: self.egt.index_range(),
            });
        }
        if start == end {
            return Ok(Some(Vec::new()));
        }
        let (frame_idx, offset) = self.locate_frame(start);
        if end - start > self.frame_len(frame_idx) - offset {
            return Err(EgtError::Internal(
                "frame_chunk range crosses a frame boundary",
            ));
        }
        self.frames[frame_idx]
            .as_ref()
            .map(|uncompressed| uncompressed.expand(offset..offset + end - start))
            .transpose()
    }

    /// Maps a position to the corresponding index. This is used when probing
    /// for a specific position.
    pub fn map_position_to_index(&self, position: &Chess) -> EgtResult<usize> {
        let (name, reflected) = TableId::from_position(position)?;
        if name != self.egt.id() {
            return Err(EgtError::PositionNotInTable {
                table: self.egt.id().to_string(),
            });
        }
        self.map_routed_position_to_index(position, reflected)
    }

    /// Indexes a position already routed to this table. The caller must have
    /// selected this identity and its reflection flag via `TableId::from_position`.
    pub(crate) fn map_routed_position_to_index(
        &self,
        position: &Chess,
        reflected: bool,
    ) -> EgtResult<usize> {
        let index = if reflected {
            self.egt.reflected_position_to_index(position)
        } else {
            self.egt.position_to_index(position)
        };
        if index >= self.egt.index_range() {
            return Err(EgtError::IndexOutOfRange {
                index,
                range: self.egt.index_range(),
            });
        }
        Ok(index)
    }

    /// Converts a table-local index to a position.
    pub fn index_to_position(&self, index: usize, side_to_move: Color) -> Option<Chess> {
        self.egt.position_from_index(index, side_to_move)
    }

    fn locate_frame(&self, index: usize) -> (usize, usize) {
        let frame_size = self.egt.frame_size();
        (index / frame_size, index % frame_size)
    }

    fn frame_len(&self, frame_idx: usize) -> usize {
        let frame_size = self.egt.frame_size();
        frame_size.min(self.egt.index_range() - frame_idx * frame_size)
    }

    fn frame_bytes(&self, frame_idx: usize) -> usize {
        self.frames[frame_idx].as_ref().map_or(0, Frame::bytes)
    }

    fn invalid_frame(
        &self,
        frame: usize,
        source: impl Into<Box<dyn std::error::Error + Send + Sync>>,
    ) -> EgtError {
        EgtError::InvalidData {
            location: format!(
                "{} (frame {frame}, indices {}..{})",
                self.location.describe(&self.key),
                frame * self.egt.frame_size(),
                frame * self.egt.frame_size() + self.frame_len(frame)
            ),
            source: source.into(),
        }
    }

    /// Reads an outcome directly by its table-local index.
    pub fn read_from_index(&mut self, index: usize) -> EgtResult<PackedOutcome> {
        if index >= self.egt.index_range() {
            return Err(EgtError::IndexOutOfRange {
                index,
                range: self.egt.index_range(),
            });
        }

        let (frame_idx, offset) = self.locate_frame(index);

        let data = self.get_frame_data(frame_idx)?;
        Ok(data.get(offset))
    }

    fn read_indices_cached(&self, indices: &[usize]) -> EgtResult<Option<Vec<PackedOutcome>>> {
        indices
            .iter()
            .map(|&index| self.read_from_index_cached(index))
            .collect()
    }

    /// Loads distinct requested frames with one multi-range call, skipping warm frames.
    fn ensure_uncompressed_frames(&mut self, frames: &[usize]) -> EgtResult<()> {
        let missing: Vec<_> = frames
            .iter()
            .copied()
            .filter(|&frame| self.frames[frame].is_none())
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        for (frame, data) in missing.iter().zip(self.read_frames(&missing)?) {
            self.frames[*frame] = Some(data);
        }
        Ok(())
    }

    /// Loads all frames using the current Rayon pool, with one decode per worker.
    fn load_all(&mut self) -> EgtResult<()> {
        let frames = (0..self.num_frames()).into_par_iter().map(|frame| {
            self.read_frames(&[frame])?.into_iter().next()
                .map(Some).ok_or(EgtError::Internal("missing loaded frame"))
        }).collect::<EgtResult<Vec<_>>>()?;
        self.frames = frames;
        Ok(())
    }

    fn read_frames(&self, missing: &[usize]) -> EgtResult<Vec<Frame>> {
        let table = &self.seek_table;
        let ranges = missing
            .iter()
            .map(|&frame| {
                Ok(table.frame_start_comp(frame as u32)?..table.frame_end_comp(frame as u32)?)
            })
            .collect::<EgtResult<Vec<_>>>()?;
        // Local get_ranges reports partial EOF differently from get_range.
        let compressed = if let [range] = ranges.as_slice() {
            block_on(self.location.store.get_range(&self.key, range.clone()))
                .map(|bytes| vec![bytes])
        } else {
            block_on(self.location.store.get_ranges(&self.key, &ranges))
        }
        .map_err(|error| {
            // A backend may reject a short range instead of returning partial bytes.
            // Only classify it as corrupt data when truncation is confirmed.
            if let Ok(meta) = block_on(self.location.store.head(&self.key)) {
                if let Some(index) = ranges.iter().position(|range| range.end > meta.size) {
                    return self.invalid_frame(missing[index], error);
                }
            }
            self.location.error(&self.key, error)
        })?;
        if compressed.len() != missing.len() {
            return Err(EgtError::Internal("invalid tablebase range count"));
        }
        missing.iter().copied().zip(ranges).zip(compressed).map(|((frame, range), bytes)| {
            if bytes.len() as u64 != range.end - range.start {
                return Err(self.invalid_frame(frame, "truncated tablebase range"));
            }
            let encoded_len = table.frame_size_decomp(frame as u32)? as usize;
            decode_frame(&bytes, self.frame_len(frame), encoded_len).map_err(|error| match error {
                EgtError::WorkingMemoryAllocation { .. } => error,
                other => self.invalid_frame(frame, other),
            })
        }).collect()
    }
}

fn decode_frame(compressed: &[u8], len: usize, encoded_len: usize) -> EgtResult<Frame> {
    let mut data = Vec::new();
    data.try_reserve_exact(encoded_len)
        .map_err(|error| EgtError::WorkingMemoryAllocation {
            context: "frame decompression buffer".to_owned(),
            bytes: encoded_len as u128,
            reason: error.to_string(),
        })?;
    data.resize(encoded_len, 0);
    // One-shot decoding is bounded by both slices and rejects truncated input.
    // zeekstd's streaming decoder can otherwise loop at premature EOF.
    let decoded = zstd_safe::decompress(&mut data[..], compressed).map_err(|code| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "invalid compressed tablebase frame: {}",
                zstd_safe::get_error_name(code)
            ),
        )
    })?;
    if decoded != data.len() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "decompressed tablebase frame has wrong length",
        )
        .into());
    }
    Frame::decode(data, len)
}


/// Mirrors a chess position horizontally for independent indexing checks.
#[cfg(test)]
pub fn mirror_horizontally(position: &Chess) -> EgtResult<Chess> {
    let mut setup = position.to_setup(shakmaty::EnPassantMode::Legal);
    setup.board.flip_horizontal();
    setup.ep_square = setup.ep_square.map(|sq| sq.flip_horizontal());
    setup
        .position(CastlingMode::Standard)
        .map_err(|e| EgtError::InvalidPosition(format!("mirrored position is invalid: {:?}", e)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egt_file::writer::write_fixture;
    use shakmaty::Color;
    use shakmaty::fen::Fen;

    fn table_path(base: &Path, name: &str) -> std::path::PathBuf {
        let path = base.join(TableId::parse(name).unwrap().relative_path("ggegt"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        path
    }

    #[test]
    fn pawn_orders_route_to_distinct_tables_and_their_twins() {
        for name in ["KPa_KPa", "KPao_KPao", "KPaPa_KPb", "KPa_KPbPb"] {
            let id = TableId::parse(name).unwrap();
            let egt = Egt::new(&id).unwrap();
            let (twin, _) = id.twin();
            let twin_egt = Egt::new(&twin).unwrap();
            let stride = (egt.index_range() / 64).max(1);
            for turn in [Color::White, Color::Black] {
                for index in (0..egt.index_range()).step_by(stride) {
                    let Some(position) = egt.position_from_index(index, turn) else {
                        continue;
                    };
                    assert_eq!(TableId::from_position(&position).unwrap().0, id);
                    assert_eq!(egt.position_to_index(&position), index);
                    for mv in position.legal_moves() {
                        if mv.is_capture() || mv.is_promotion() {
                            continue;
                        }
                        let successor = position.clone().play(mv).unwrap();
                        let (routed, reflected) = TableId::from_position(&successor).unwrap();
                        assert_eq!(routed, twin);
                        let local = if reflected {
                            twin_egt.reflected_position_to_index(&successor)
                        } else {
                            twin_egt.position_to_index(&successor)
                        };
                        assert_eq!(
                            twin_egt
                                .position_from_index(local, successor.turn())
                                .unwrap(),
                            if reflected {
                                mirror_horizontally(&successor).unwrap()
                            } else {
                                successor
                            }
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn requires_format_and_uses_exact_hierarchical_namespace() {
        let dir = crate::TestDir::new("exact_namespace");
        let legacy = dir.0.join("KP_K.ggegt");
        std::fs::write(&legacy, b"legacy indexing").unwrap();
        assert!(EgtFileReader::new_from_file(&dir.0, "KPa_K").is_err());
        assert!(!dir.0.join("format.json").exists());
        let file = write_fixture(&dir.0, "KPa_K", |_| PackedOutcome::INVALID);
        assert!(table_path(&dir.0, &file.egt().id().name()).exists());
        assert_eq!(std::fs::read(legacy).unwrap(), b"legacy indexing");
        assert!(EgtFileReader::new_from_file(&dir.0, "KPa_K").is_ok());
        assert!(EgtFileReader::new_from_file(&dir.0, "KP_K").is_err());
    }

    #[test]
    fn cached_probe_does_not_warm_frames() {
        let dir = crate::TestDir::new("cached_probe_does_not_warm_frames");
        let position: Chess = "8/8/8/8/8/8/P7/K6k w - - 0 1"
            .parse::<Fen>()
            .unwrap()
            .into_position(CastlingMode::Standard)
            .unwrap();
        for outcome in [
            PackedOutcome::INVALID,
            PackedOutcome::new_unknown(3),
            PackedOutcome::DRAW,
        ] {
            let mut file = write_fixture(&dir.0, "KPa_K", |_| outcome);
            assert_eq!(file.probe_cached(&position).unwrap(), None);
            assert!(file.frames.iter().all(Option::is_none));
            assert_eq!(file.probe(&position).unwrap(), outcome);
            assert_eq!(file.probe_cached(&position).unwrap(), Some(outcome));
        }
    }

    #[test]
    fn test_egt_file_creation() {
        let dir = crate::TestDir::new("single_table_creation");
        let file = write_fixture(&dir.0, "KPa_K", |_| PackedOutcome::INVALID);
        assert_eq!(file.egt().id().name().as_str(), "KPa_K");
        let other: Chess = "8/8/8/8/8/8/1P6/K6k w - - 0 1"
            .parse::<Fen>()
            .unwrap()
            .into_position(CastlingMode::Standard)
            .unwrap();
        assert!(matches!(
            file.map_position_to_index(&other),
            Err(EgtError::PositionNotInTable { .. })
        ));
        let other_material: Chess = "8/8/8/8/8/8/Q7/K6k w - - 0 1"
            .parse::<Fen>()
            .unwrap()
            .into_position(CastlingMode::Standard)
            .unwrap();
        assert!(matches!(
            file.map_position_to_index(&other_material),
            Err(EgtError::PositionNotInTable { .. })
        ));
        assert!(
            file.index_to_position(file.egt.index_range(), Color::White)
                .is_none()
        );
    }

    fn run_round_trip_test(tablename: &str, stride: usize) {
        let egt = Egt::new(&crate::TableId::parse(tablename).unwrap()).unwrap();
        for turn in [Color::White, Color::Black] {
            for index in (0..egt.index_range()).step_by(stride) {
                if let Some(position) = egt.position_from_index(index, turn) {
                    assert_eq!(egt.position_to_index(&position), index);
                }
            }
        }
    }

    #[test]
    fn egt_file_decode_encode_kpp_kpp() {
        run_round_trip_test("KPaPb_KPcPd", 100_000);
    }

    #[test]
    fn egt_file_decode_encode_kq_kr() {
        run_round_trip_test("KQ_KR", 100);
    }

    #[test]
    fn egt_file_decode_encode_kppp_k() {
        run_round_trip_test("KPaPbPc_K", 100);
    }

    #[test]
    fn egt_file_decode_encode_kpp_kp() {
        run_round_trip_test("KPaPb_KPc", 500);
    }

    #[test]
    fn test_palette_frame_roundtrip() {
        // Use a frame size that is a multiple of 8 (as in production).
        let n = 16;
        let mut inputs = vec![PackedOutcome::INVALID; n];
        inputs[4] = PackedOutcome::new_win(ConversionType::Checkmate, 67);
        inputs[6] = PackedOutcome::new_loss(ConversionType::Capture, 12);
        inputs[7] = PackedOutcome::new_win(ConversionType::Promotion, 4567);
        inputs[8] = PackedOutcome::DRAW;
        inputs[10] = PackedOutcome::new_loss(ConversionType::Checkmate, 30);
        inputs[11] = PackedOutcome::new_win(ConversionType::Capture, 1);
        inputs[12] = PackedOutcome::DRAW;
        inputs[13] = PackedOutcome::new_loss(ConversionType::Promotion, 1200);
        let encoded = frame::encode(&inputs).unwrap();
        let recovered = Frame::decode(encoded, n).unwrap().expand(0..n).unwrap();
        for i in 0..n {
            assert_eq!(
                inputs[i].to_u16(),
                recovered[i].to_u16(),
                "mismatch at i={}",
                i
            );
        }
    }

    #[test]
    fn published_file_probes_canonical_and_mirrored_positions() {
        let dir = crate::TestDir::new("published_file_probe");
        let position = "8/8/8/8/8/8/P7/K6k w - - 0 1"
            .parse::<Fen>()
            .unwrap()
            .into_position(CastlingMode::Standard)
            .unwrap();
        let egt = Egt::new(&crate::TableId::parse("KPa_K").unwrap()).unwrap();
        let index = egt.position_to_index(&position);
        let outcome = PackedOutcome::new_win(ConversionType::Checkmate, 987);
        let mut reader = write_fixture(&dir.0, "KPa_K", |i| {
            if i == index {
                outcome
            } else {
                PackedOutcome::INVALID
            }
        });
        assert!(reader.frames.iter().all(Option::is_none));
        for position in [&position, &mirror_horizontally(&position).unwrap()] {
            assert_eq!(reader.map_position_to_index(position).unwrap(), index);
            assert_eq!(reader.probe(position).unwrap(), outcome);
        }
        let mut reopened = EgtFileReader::new_from_file(&dir.0, "KPa_K").unwrap();
        assert_eq!(reopened.probe(&position).unwrap(), outcome);
    }

    #[test]
    fn published_file_scan_and_reload() {
        let dir = crate::TestDir::new("published_file_scan");
        let outcome = |idx| {
            if idx % 3 == 0 {
                PackedOutcome::new_win(ConversionType::Promotion, 4567)
            } else {
                PackedOutcome::DRAW
            }
        };
        let mut reader = write_fixture(&dir.0, "KPa_K", outcome);
        assert_eq!(
            reader.num_frames(),
            reader.egt.index_range().div_ceil(reader.egt.frame_size())
        );
        for _ in 0..2 {
            for idx in 0..reader.egt.index_range() {
                assert_eq!(reader.read_from_index(idx).unwrap(), outcome(idx));
            }
            reader.frames.fill(None);
        }
        assert_eq!(
            reader
                .frame_chunk(reader.egt.index_range(), reader.egt.index_range())
                .unwrap(),
            &[]
        );
    }

    #[test]
    fn full_frames_short_tail_and_table_boundaries() {
        let dir = crate::TestDir::new("frame_boundaries");
        let egt = Egt::new(&crate::TableId::parse("KQ_KR").unwrap()).unwrap();
        let length = egt.index_range();
        let frame_size = egt.frame_size();
        let indexes = [0, frame_size - 1, frame_size, frame_size + 1, length - 1];
        let mut reader = write_fixture(&dir.0, "KQ_KR", |idx| {
            indexes
                .iter()
                .position(|&i| i == idx)
                .map_or(PackedOutcome::INVALID, |n| {
                    PackedOutcome::new_loss(ConversionType::Capture, n as u16 + 1)
                })
        });
        assert!(reader.num_frames() > 1);
        assert_ne!(length % frame_size, 0);
        assert_eq!(reader.frame_len(reader.num_frames() - 1), length % frame_size);
        assert!(reader.frame_chunk(frame_size - 1, frame_size + 1).is_err());
        for (n, &idx) in indexes.iter().enumerate() {
            assert_eq!(
                reader.read_from_index(idx).unwrap(),
                PackedOutcome::new_loss(ConversionType::Capture, n as u16 + 1)
            );
        }
        let pawnful = write_fixture(&dir.0, "KPa_K", |_| PackedOutcome::INVALID);
        assert_eq!(pawnful.num_frames(), 1);
        assert_eq!(pawnful.locate_frame(0), (0, 0));
        assert_eq!(pawnful.frame_len(0), pawnful.egt.index_range());
        assert!(reader.read_from_index(reader.egt.index_range()).is_err());
        assert!(reader.frame_chunk(1, 0).is_err());
        assert!(reader.frame_chunk(0, reader.egt.index_range() + 1).is_err());
    }

    #[test]
    fn rejects_invalid_frame_layout() {
        let dir = crate::TestDir::new("invalid_frame_layout");
        let file = write_fixture(&dir.0, "K_K", |_| PackedOutcome::INVALID);
        let frame_size = file.egt.frame_size();
        for bytes in [0, 3, 922, 926, frame_size * 2, frame_size * 2 + 2] {
            let output = std::fs::File::create(table_path(&dir.0, "K_K")).unwrap();
            let mut encoder = zeekstd::EncodeOptions::new().into_encoder(output).unwrap();
            encoder.compress(&vec![0; bytes]).unwrap();
            encoder.finish().unwrap();
            assert!(EgtFileReader::new_from_file(&dir.0, "K_K")
                .and_then(|mut reader| reader.read_from_index(0)).is_err());
        }
        // A correct payload with an extra empty frame has the wrong frame count.
        let output = std::fs::File::create(table_path(&dir.0, "K_K")).unwrap();
        let mut encoder = zeekstd::EncodeOptions::new().into_encoder(output).unwrap();
        encoder.compress(&frame::encode(&vec![PackedOutcome::INVALID; file.egt.index_range()]).unwrap()).unwrap();
        encoder.end_frame().unwrap();
        encoder.finish().unwrap();
        assert!(EgtFileReader::new_from_file(&dir.0, "K_K").is_err());
    }

    #[test]
    fn rejects_malformed_seek_table_suffixes() {
        let dir = crate::TestDir::new("malformed_seek_table_suffixes");
        let path = table_path(&dir.0, "K_K");
        write_fixture(&dir.0, "K_K", |_| PackedOutcome::INVALID);
        let valid = std::fs::read(&path).unwrap();
        let table_start = valid.len() - 25;

        for length in [0, 8, 9, 24] {
            std::fs::write(&path, &valid[valid.len() - length..]).unwrap();
            assert!(matches!(EgtFileReader::new_from_file(&dir.0, "K_K"),
                Err(EgtError::InvalidData { location, .. }) if location.ends_with("/K_K.ggegt")));
        }
        // Reject the declared count before zeekstd can allocate from it.
        let mut inflated = valid.clone();
        let count_start = inflated.len() - 9;
        inflated[count_start..count_start + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        std::fs::write(&path, inflated).unwrap();
        assert!(matches!(
            EgtFileReader::new_from_file(&dir.0, "K_K"),
            Err(EgtError::InvalidData { location, source })
                if location.ends_with("/K_K.ggegt") && source.to_string() == "invalid tablebase frame count"
        ));

        // zeekstd, rather than our loader, validates magic and reserved bits.
        for offset in [valid.len() - 1, valid.len() - 5, table_start] {
            let mut corrupt = valid.clone();
            corrupt[offset] ^= 0x04;
            std::fs::write(&path, corrupt).unwrap();
            assert!(matches!(
                EgtFileReader::new_from_file(&dir.0, "K_K"),
                Err(EgtError::InvalidData { .. })
            ));
        }
        let mut prefixed = vec![0];
        prefixed.extend_from_slice(&valid);
        std::fs::write(&path, prefixed).unwrap();
        assert!(matches!(
            EgtFileReader::new_from_file(&dir.0, "K_K"),
            Err(EgtError::InvalidData { source, .. })
                if source.to_string() == "invalid tablebase compressed length"
        ));

        // The reader deliberately accepts only our checksum-free seek-table format.
        let mut checksummed = valid.clone();
        checksummed.splice(table_start + 16..table_start + 16, [0; 4]);
        checksummed[table_start + 4..table_start + 8].copy_from_slice(&21u32.to_le_bytes());
        let descriptor = checksummed.len() - 5;
        checksummed[descriptor] |= 0x80;
        std::fs::write(&path, checksummed).unwrap();
        assert!(EgtFileReader::new_from_file(&dir.0, "K_K").is_err());

        std::fs::write(&path, valid).unwrap();
        EgtFileReader::new_from_file(&dir.0, "K_K").unwrap();
    }

    #[test]
    fn rejects_corrupt_frames_and_short_ranges() {
        let dir = crate::TestDir::new("short_frame_range");
        for name in ["K_K", "KQ_KR"] {
            let path = table_path(&dir.0, name);
            let mut reader = write_fixture(&dir.0, name, |_| PackedOutcome::INVALID);
            let valid = std::fs::read(&path).unwrap();
            let last = reader.num_frames() - 1;
            let frame_end = reader.seek_table.frame_end_comp(last as u32).unwrap();
            let mut corrupt = valid.clone();
            corrupt[0] ^= 0xff;
            std::fs::write(&path, corrupt).unwrap();
            assert!(matches!(reader.read_from_index(0),
                Err(EgtError::InvalidData { location, source })
                    if location.contains(&format!("{name}.ggegt (frame 0, indices 0.."))
                        && source.to_string().contains("invalid compressed tablebase frame")));
            std::fs::write(&path, valid).unwrap();
            std::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .unwrap()
                .set_len(frame_end - 1)
                .unwrap();
            let frames: Vec<_> = (0..reader.num_frames()).collect();
            assert!(matches!(reader.ensure_uncompressed_frames(&frames),
                Err(EgtError::InvalidData { location, .. })
                    if location.contains(&format!("{name}.ggegt (frame {last}, indices "))));
        }
    }
}
