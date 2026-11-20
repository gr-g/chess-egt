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
use object_store::{GetOptions, GetRange, ObjectStoreExt, path::Path as ObjectPath};
#[cfg(test)]
use shakmaty::{CastlingMode, Position};
use shakmaty::Chess;
use rayon::prelude::*;

/// Resolves existing tables with input-first, missing-only fallback.
#[derive(Debug)]
pub(crate) struct Source {
    base: Location,
    input: Option<Location>,
}

impl Source {
    pub(crate) fn new(base: Location) -> Self {
        Self { base, input: None }
    }

    pub(crate) fn with_input(&self, input: Option<Location>) -> Self {
        Self {
            base: self.base.clone(),
            input,
        }
    }

    pub(crate) fn base(&self) -> &Location {
        &self.base
    }

    pub(crate) fn open(&self, id: &TableId) -> EgtResult<TableFile> {
        if let Some(input) = &self.input {
            match TableFile::open(input, id) {
                Ok(file) => return Ok(file),
                Err(EgtError::FileNotFound(_)) => {}
                Err(error) => return Err(error),
            }
        }
        TableFile::open(&self.base, id)
    }
}

/// File metadata and frame I/O for one existing canonical table.
#[derive(Debug)]
pub(crate) struct TableFile {
    egt: Egt,
    location: Location,
    key: ObjectPath,
    seek_table: zeekstd::SeekTable,
}

impl TableFile {
    /// The immutable descriptor of this canonical table.
    pub(crate) fn egt(&self) -> &Egt {
        &self.egt
    }

    /// Opens an existing table without modifying storage. Data is read on demand.
    /// Objects must not be replaced while this reader is in use: reads are not
    /// pinned to an object version.
    pub(crate) fn open(location: &Location, id: &TableId) -> EgtResult<Self> {
        let egt = Egt::new(id)?;
        let index_range = egt.index_range();
        let key = location.key(&id.relative_path("ggegt"));
        location.validate_format()?;
        let seek_table = Self::read_seek_table(location, &key, index_range, egt.frame_size())?;
        Ok(Self {
            key,
            egt,
            location: location.clone(),
            seek_table,
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

    pub(crate) fn num_frames(&self) -> usize {
        self.egt.index_range().div_ceil(self.egt.frame_size())
    }

    /// Indexes a position already routed to this table. The caller must have
    /// selected this identity and its reflection flag via `TableId::from_position`.
    pub(crate) fn index(&self, position: &Chess, reflected: bool) -> EgtResult<usize> {
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

    fn locate(&self, index: usize) -> (usize, usize) {
        let frame_size = self.egt.frame_size();
        (index / frame_size, index % frame_size)
    }

    fn frame_len(&self, frame_idx: usize) -> usize {
        let frame_size = self.egt.frame_size();
        frame_size.min(self.egt.index_range() - frame_idx * frame_size)
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

    /// Reads all frames using the current Rayon pool, with one decode per worker.
    pub(crate) fn read_all(&self) -> EgtResult<Vec<Frame>> {
        (0..self.num_frames())
            .into_par_iter()
            .map(|frame| Ok(self.read_frames(&[frame])?.pop().expect("one requested frame")))
            .collect()
    }

    /// Reads and decodes frames in the requested order.
    fn read_frames(&self, frames: &[usize]) -> EgtResult<Vec<Frame>> {
        if frames.is_empty() {
            return Ok(Vec::new());
        }
        let table = &self.seek_table;
        let ranges = frames
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
                    return self.invalid_frame(frames[index], error);
                }
            }
            self.location.error(&self.key, error)
        })?;
        if compressed.len() != frames.len() {
            return Err(EgtError::Internal("invalid tablebase range count"));
        }
        frames.iter().copied().zip(ranges).zip(compressed).map(|((frame, range), bytes)| {
            if bytes.len() as u64 != range.end - range.start {
                return Err(self.invalid_frame(frame, "truncated tablebase range"));
            }
            let encoded_len = table.frame_size_decomp(frame as u32)? as usize;
            decode_frame(&bytes, self.frame_len(frame), encoded_len).map_err(|error| match error {
                EgtError::WorkingMemoryAllocation { .. } => error,
                other => self.invalid_frame(frame, other.into_data_source()),
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
        EgtError::invalid_data(
            "tablebase frame",
            format!(
                "invalid compressed tablebase frame: {}",
                zstd_safe::get_error_name(code)
            ),
        )
    })?;
    if decoded != data.len() {
        return Err(EgtError::invalid_data(
            "tablebase frame",
            "decompressed tablebase frame has wrong length",
        ));
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
    use crate::outcome::PackedOutcome;
    use crate::ConversionType;
    use shakmaty::Color;
    use shakmaty::fen::Fen;
    use std::path::Path;

    fn open_local(base: &Path, name: &str) -> EgtResult<TableFile> {
        TableFile::open(&Location::parse(base)?, &TableId::parse(name)?)
    }

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
        assert!(open_local(&dir.0, "KPa_K").is_err());
        assert!(!dir.0.join("format.json").exists());
        let file = write_fixture(&dir.0, "KPa_K", |_| PackedOutcome::INVALID);
        assert!(table_path(&dir.0, &file.egt().id().name()).exists());
        assert_eq!(std::fs::read(legacy).unwrap(), b"legacy indexing");
        assert!(open_local(&dir.0, "KPa_K").is_ok());
        assert!(open_local(&dir.0, "KP_K").is_err());
    }

    #[test]
    fn reads_unassigned_and_draw_outcomes() {
        let dir = crate::TestDir::new("unassigned_and_draw_outcomes");
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
            let file = write_fixture(&dir.0, "KPa_K", |_| outcome);
            let (_, reflected) = TableId::from_position(&position).unwrap();
            let (frame, offset) = file.locate(file.index(&position, reflected).unwrap());
            assert_eq!(file.read_frames(&[frame]).unwrap()[0].get(offset), outcome);
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
        assert_ne!(TableId::from_position(&other).unwrap().0, file.egt().id());
        let other_material: Chess = "8/8/8/8/8/8/Q7/K6k w - - 0 1"
            .parse::<Fen>()
            .unwrap()
            .into_position(CastlingMode::Standard)
            .unwrap();
        assert_ne!(TableId::from_position(&other_material).unwrap().0, file.egt().id());
        assert!(
            file.egt().position_from_index(file.egt.index_range(), Color::White)
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
        let reader = write_fixture(&dir.0, "KPa_K", |i| {
            if i == index {
                outcome
            } else {
                PackedOutcome::INVALID
            }
        });
        for position in [&position, &mirror_horizontally(&position).unwrap()] {
            let (id, reflected) = TableId::from_position(position).unwrap();
            assert_eq!(id, reader.egt().id());
            assert_eq!(reader.index(position, reflected).unwrap(), index);
            let (frame, offset) = reader.locate(index);
            assert_eq!(reader.read_frames(&[frame]).unwrap()[0].get(offset), outcome);
        }
        let prober = crate::Tablebase::new(&dir.0).unwrap();
        assert_eq!(prober.probe(&position).unwrap(), crate::DtcOutcome::try_from(outcome).unwrap());
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
        let reader = write_fixture(&dir.0, "KPa_K", outcome);
        assert_eq!(
            reader.num_frames(),
            reader.egt.index_range().div_ceil(reader.egt.frame_size())
        );
        let id = reader.egt().id();
        let location = Location::parse(&dir.0).unwrap();
        let range = reader.egt().index_range();
        for _ in 0..2 {
            let frames = reader.read_all().unwrap();
            for idx in 0..range {
                let (frame, offset) = reader.locate(idx);
                assert_eq!(frames[frame].get(offset), outcome(idx));
            }
        }
        let batch = BatchReader::open(&Source::new(location), &[id]).unwrap();
        assert!(batch.read_chunk(&id, range, range).unwrap().is_empty());
    }

    #[test]
    fn full_frames_short_tail_and_table_boundaries() {
        let dir = crate::TestDir::new("frame_boundaries");
        let egt = Egt::new(&crate::TableId::parse("KQ_KR").unwrap()).unwrap();
        let length = egt.index_range();
        let frame_size = egt.frame_size();
        let indexes = [0, frame_size - 1, frame_size, frame_size + 1, length - 1];
        let reader = write_fixture(&dir.0, "KQ_KR", |idx| {
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
        let id = reader.egt().id();
        let mut batch = BatchReader::default();
        let frames = reader.read_all().unwrap();
        batch.insert(reader, frames);
        assert!(batch.read_chunk(&id, frame_size - 1, frame_size + 1).is_err());
        for (n, &idx) in indexes.iter().enumerate() {
            assert_eq!(
                batch.read_chunk(&id, idx, idx + 1).unwrap()[0],
                PackedOutcome::new_loss(ConversionType::Capture, n as u16 + 1)
            );
        }
        let pawnful = write_fixture(&dir.0, "KPa_K", |_| PackedOutcome::INVALID);
        assert_eq!(pawnful.num_frames(), 1);
        assert_eq!(pawnful.locate(0), (0, 0));
        assert_eq!(pawnful.frame_len(0), pawnful.egt.index_range());
        assert!(batch.read_chunk(&id, length, length + 1).is_err());
        assert!(batch.read_chunk(&id, 1, 0).is_err());
        assert!(batch.read_chunk(&id, 0, length + 1).is_err());
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
            assert!(open_local(&dir.0, "K_K")
                .and_then(|reader| reader.read_frames(&[0])).is_err());
        }
        // A correct payload with an extra empty frame has the wrong frame count.
        let output = std::fs::File::create(table_path(&dir.0, "K_K")).unwrap();
        let mut encoder = zeekstd::EncodeOptions::new().into_encoder(output).unwrap();
        encoder.compress(&frame::encode(&vec![PackedOutcome::INVALID; file.egt.index_range()]).unwrap()).unwrap();
        encoder.end_frame().unwrap();
        encoder.finish().unwrap();
        assert!(open_local(&dir.0, "K_K").is_err());
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
            assert!(matches!(open_local(&dir.0, "K_K"),
                Err(EgtError::InvalidData { location, .. }) if location.ends_with("/K_K.ggegt")));
        }
        // Reject the declared count before zeekstd can allocate from it.
        let mut inflated = valid.clone();
        let count_start = inflated.len() - 9;
        inflated[count_start..count_start + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        std::fs::write(&path, inflated).unwrap();
        assert!(matches!(
            open_local(&dir.0, "K_K"),
            Err(EgtError::InvalidData { location, source })
                if location.ends_with("/K_K.ggegt") && source.to_string() == "invalid tablebase frame count"
        ));

        // zeekstd, rather than our loader, validates magic and reserved bits.
        for offset in [valid.len() - 1, valid.len() - 5, table_start] {
            let mut corrupt = valid.clone();
            corrupt[offset] ^= 0x04;
            std::fs::write(&path, corrupt).unwrap();
            assert!(matches!(
                open_local(&dir.0, "K_K"),
                Err(EgtError::InvalidData { .. })
            ));
        }
        let mut prefixed = vec![0];
        prefixed.extend_from_slice(&valid);
        std::fs::write(&path, prefixed).unwrap();
        assert!(matches!(
            open_local(&dir.0, "K_K"),
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
        assert!(open_local(&dir.0, "K_K").is_err());

        std::fs::write(&path, valid).unwrap();
        open_local(&dir.0, "K_K").unwrap();
    }

    #[test]
    fn rejects_corrupt_frames_and_short_ranges() {
        let dir = crate::TestDir::new("short_frame_range");
        for name in ["K_K", "KQ_KR"] {
            let path = table_path(&dir.0, name);
            let reader = write_fixture(&dir.0, name, |_| PackedOutcome::INVALID);
            let valid = std::fs::read(&path).unwrap();
            let last = reader.num_frames() - 1;
            let frame_end = reader.seek_table.frame_end_comp(last as u32).unwrap();
            let mut corrupt = valid.clone();
            corrupt[0] ^= 0xff;
            std::fs::write(&path, corrupt).unwrap();
            assert!(matches!(reader.read_frames(&[0]),
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
            assert!(matches!(reader.read_frames(&frames),
                Err(EgtError::InvalidData { location, .. })
                    if location.contains(&format!("{name}.ggegt (frame {last}, indices "))));
        }
    }
}
