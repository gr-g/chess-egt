use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use shakmaty::{CastlingMode, Chess, Color, File, Position};
use crate::{ConversionType, DtcOutcome};
use crate::egt::Egt;
use crate::piece_set::{EgtRole, EgtSide};

// 16k positions per frame, corresponding to 32k bytes per frame.
const DEFAULT_FRAME_SIZE: usize = 16384;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MaybeDtcOutcome(pub u16);

impl MaybeDtcOutcome {
    pub const INVALID: Self = Self(0b000);
    pub const DRAW: Self = Self(0b001);

    pub fn from_u16(value: u16) -> Self {
        Self(value)
    }

    pub fn to_u16(&self) -> u16 {
        self.0
    }

    pub fn is_assigned(&self) -> bool {
        (self.0 & 0b111) != 0b000
    }

    pub fn is_invalid(&self) -> bool {
        self.0 == 0b000
    }

    pub fn is_unknown(&self) -> bool {
        (self.0 & 0b111) == 0b000 && (self.0 >> 3) != 0
    }

    pub fn get_unknown_counter(&self) -> u16 {
        self.0 >> 3
    }

    pub fn conversion_type(&self) -> ConversionType {
        match self.0 & 0b110 {
            0b010 => ConversionType::Checkmate,
            0b100 => ConversionType::Capture,
            0b110 => ConversionType::Promotion,
            _ => panic!()
        }
    }

    pub fn is_draw(&self) -> bool {
        self.0 == 0b001
    }

    pub fn is_win(&self) -> bool {
        match self.0 & 0b111 {
            0b010 | 0b100 | 0b110 => true,
            _ => false,
        }
    }

    pub fn is_loss(&self) -> bool {
        match self.0 & 0b111 {
            0b011 | 0b101 | 0b111 => true,
            _ => false,
        }
    }

    pub fn new_win(ct: ConversionType, distance: u16) -> Self {
        let bits = match ct {
            ConversionType::Checkmate => 0b010,
            ConversionType::Capture => 0b100,
            ConversionType::Promotion => 0b110,
        };
        Self(bits | (distance << 3))
    }

    pub fn new_loss(ct: ConversionType, distance: u16) -> Self {
        let bits = match ct {
            ConversionType::Checkmate => 0b011,
            ConversionType::Capture => 0b101,
            ConversionType::Promotion => 0b111,
        };
        Self(bits | (distance << 3))
    }

    pub fn new_unknown(moves_counter: u16) -> Self {
        Self(moves_counter << 3)
    }

    pub fn to_outcome(self) -> Result<DtcOutcome, ()> {
        let n = self.0 >> 3;
        match self.0 & 0b111 {
            0b000 if n == 0 => Err(()),
            0b000 if n != 0 => Err(()),
            0b001 => Ok(DtcOutcome::Draw),
            0b010 => Ok(DtcOutcome::Win(ConversionType::Checkmate, n)),
            0b100 => Ok(DtcOutcome::Win(ConversionType::Capture, n)),
            0b110 => Ok(DtcOutcome::Win(ConversionType::Promotion, n)),
            0b011 => Ok(DtcOutcome::Loss(ConversionType::Checkmate, n)),
            0b101 => Ok(DtcOutcome::Loss(ConversionType::Capture, n)),
            0b111 => Ok(DtcOutcome::Loss(ConversionType::Promotion, n)),
            _ => unreachable!(),
        }
    }
}

/// Represents the state of a single frame in an EgtFile.
#[derive(Debug, Clone)]
pub enum FrameState {
    /// The frame is empty.
    Empty,
    /// The frame is compressed on file.
    CompressedOnFile,
    /// The frame is compressed in memory.
    Compressed(Vec<u8>),
    /// The frame is fully uncompressed in memory.
    Uncompressed {
        /// Cached compressed bytes to avoid re-compression if not dirty.
        compressed: Vec<u8>,
        /// Uncompressed values.
        uncompressed: Vec<MaybeDtcOutcome>,
        /// True if the compressed bytes are not updated.
        dirty: bool,
    },
}

/// A simple stub for the memory Arena that manages a fixed pool of memory.
//#[derive(Debug)]
//pub struct Arena {
//    capacity: usize,
//    used: usize,
//}

//impl Arena {
    /// Creates a new Arena with the given capacity in bytes.
    //pub fn new(capacity: usize) -> Self {
    //    Self { capacity, used: 0 }
    //}

    /// Attempts to allocate memory from the arena.
    /// In a full implementation, this would trigger eviction of LRU frames if capacity is exceeded.
    //pub fn allocate(&mut self, size: usize) -> bool {
    //    if self.used + size <= self.capacity {
    //        self.used += size;
    //        true
    //    } else {
    //        false
    //    }
    //}

    /// Returns memory to the arena.
    //pub fn deallocate(&mut self, size: usize) {
    //    self.used = self.used.saturating_sub(size);
    //}

    //pub fn capacity(&self) -> usize {
    //    self.capacity
    //}

    //pub fn used(&self) -> usize {
    //    self.used
    //}
//}

/// A stack-allocated, copyable key representing the pawn file assignments for both sides.
/// This avoids all string formatting and heap allocations during tablebase probing.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct PawnKey {
    files: [u8; 8],  // Up to 8 pawns, padded with 255
}

impl PawnKey {
    pub fn new(stm: &[File], sntm: &[File]) -> Self {
        debug_assert!(stm.len() + sntm.len() <= 8);

        let mut arr = [255; 8];
        let mut i = 0;
        for &f in stm {
            arr[i] = f.to_usize() as u8;
            i += 1;
        }
        for &f in sntm {
            arr[i] = f.to_usize() as u8 + 8;
            i += 1;
        }
        Self {
            files: arr,
        }
    }
}

/// Represents a file with endgame tablebases for a specific configuration of chess pieces.
#[derive(Debug)]
pub struct EgtFile {
    /// The name of this endgame file (e.g. "KQ_KP").
    pub endgame: String,

    /// Path to the file.
    pub path: PathBuf,

    /// The sub-tables (Egt objects) that compose this file.
    pub egts: Vec<Egt>,

    /// Map from PawnKey to its index in the `egts` vector.
    pub egt_map: HashMap<PawnKey, usize>,

    /// The frames of the file.
    frames: Vec<FrameState>,

    /// Number of indexed locations per frame.
    frame_size: usize,

    /// Total number of indexed locations across all Egts in this file.
    pub index_range: usize,
}

impl EgtFile {
    /// Creates a new EgtFile for a given piece configuration and path.
    ///
    /// The file starts with no memory allocated.
    pub fn new(base_path: &PathBuf, endgame: &str) -> Result<Self, ()> {
        let path = base_path.join(format!("{}.ggegt", endgame));

        let (stm_pawns, sntm_pawns, other_pieces) = parse_endgame_name(endgame)?;

        let stm_combos = get_file_combinations(stm_pawns);
        let sntm_combos = get_file_combinations(sntm_pawns);

        let mut egt_pairs = Vec::new();

        for stm_files in &stm_combos {
            for sntm_files in &sntm_combos {
                if is_canonical(stm_files, sntm_files) {
                    let pieces = build_pieces(stm_files, sntm_files, &other_pieces);
                    let egt = Egt::from_pieces(pieces)?;
                    let key = PawnKey::new(stm_files, sntm_files);
                    egt_pairs.push((key, egt));
                }
            }
        }

        // Sort by PawnKey to ensure a stable, deterministic order
        egt_pairs.sort_by_key(|(key, _)| *key);

        let mut egts = Vec::with_capacity(egt_pairs.len());
        let mut egt_map = HashMap::with_capacity(egt_pairs.len());

        for (idx, (key, egt)) in egt_pairs.into_iter().enumerate() {
            egt_map.insert(key, idx);
            egts.push(egt);
        }

        let index_range: usize = egts.iter().map(|egt| egt.index_range()).sum();
        let frame_size = DEFAULT_FRAME_SIZE;
        let num_frames = (index_range + frame_size - 1) / frame_size;

        let mut frames = Vec::with_capacity(num_frames);
        for _ in 0..num_frames {
            frames.push(FrameState::Empty);
        }

        Ok(Self {
            endgame: endgame.to_string(),
            path,
            egts,
            egt_map,
            frames,
            frame_size,
            index_range,
        })
    }

    /// Creates an EgtFile representing an existing file.
    ///
    /// The file starts with no memory allocated. Data is read from the file on demand.
    pub fn new_from_file(base_path: &PathBuf, endgame: &str) -> Result<Self, ()> {
        let mut egt_file = Self::new(base_path, endgame)?;
        let exists = std::fs::exists(&egt_file.path);
        if exists.is_err() || !exists.unwrap() {
            return Err(());
        }

        for f in 0..egt_file.frames.len() {
            egt_file.frames[f] = FrameState::CompressedOnFile;
        }

        Ok(egt_file)
    }

    /// Probes the outcome of a specific position.
    pub fn probe(&mut self, position: &Chess) -> Result<MaybeDtcOutcome, ()> {
        let index = self.map_position_to_index(position)?;
        self.read_from_index(index)
    }

    /// Counts the number of wins, draws, losses, and invalid positions in the EgtFile.
    pub fn count_outcomes(&mut self) -> (usize, usize, usize, usize) {
        let mut wins = 0;
        let mut draws = 0;
        let mut losses = 0;
        let mut invalid = 0;

        for frame_idx in 0..self.frames.len() {
            if let FrameState::Empty = &self.frames[frame_idx] {
                invalid += self.frame_size;
                continue;
            }

            for outcome in self.get_frame_data(frame_idx).unwrap() {
                if outcome.is_win() {
                    wins += 1;
                } else if outcome.is_loss() {
                    losses += 1;
                } else if outcome.is_draw() {
                    draws += 1;
                } else if outcome.is_invalid() {
                    invalid += 1;
                }
            }
        }

        (wins, draws, losses, invalid)
    }

    /// Save the entire EgtFile using seekable Zstd compression.
    /// Leaves the data in a compressed state in memory afterwards.
    pub fn save_to_file(&mut self) -> Result<u64, ()> {
        use std::fs::File;
        use std::io::BufWriter;
        use zeekstd::Encoder;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|_| ())?;
        }
        let file = File::create(&self.path).map_err(|_| ())?;
        let writer = BufWriter::new(file);
        let mut encoder = Encoder::new(writer).map_err(|_| ())?;

        // TODO: this does the encoding in one go with the uncompressed data,
        // but in principle we could reuse the compressed frames.
        for frame_idx in 0..self.frames.len() {
            self.ensure_uncompressed(frame_idx)?;

            // Transpose the frame
            if let FrameState::Uncompressed { uncompressed, .. } = &self.frames[frame_idx] {
                let transposed = transpose_frame(&uncompressed);

                // Compress the transposed frame
                encoder.compress(&transposed).map_err(|_| ())?;
                encoder.end_frame().map_err(|_| ())?;

                // Drop the uncompressed data from memory
                self.ensure_compressed(frame_idx)?;
            } else {
                unreachable!();
            }
        }

        // Finish the seekable Zstd file (writes the seek table)
        encoder.finish().map_err(|_| ())
    }

    /// Get uncompressed data for the frame at `frame_index`.
    fn get_frame_data(&mut self, frame_idx: usize) -> Result<&mut [MaybeDtcOutcome], ()> {
        self.ensure_uncompressed(frame_idx)?;

        if let FrameState::Uncompressed { uncompressed, .. } = &mut self.frames[frame_idx] {
            Ok(uncompressed)
        } else {
            unreachable!();
        }
    }

    /// Maps a position to the corresponding index. This is used when probing
    /// for a specific position.
    pub fn map_position_to_index(&mut self, position: &Chess) -> Result<usize, ()> {
        let stm_color = position.turn();
        let sntm_color = !stm_color;

        let stm_pawns_bb = position.board().pawns() & position.board().by_color(stm_color);
        let sntm_pawns_bb = position.board().pawns() & position.board().by_color(sntm_color);

        let mut stm_files: Vec<File> = stm_pawns_bb.into_iter().map(|sq| sq.file()).collect();
        let mut sntm_files: Vec<File> = sntm_pawns_bb.into_iter().map(|sq| sq.file()).collect();

        stm_files.sort_by_key(|f| f.to_usize());
        sntm_files.sort_by_key(|f| f.to_usize());

        let canonical = is_canonical(&stm_files, &sntm_files);

        let (target_stm_files, target_sntm_files, target_position) = if canonical {
            (stm_files, sntm_files, position.clone())
        } else {
            let stm_ref = reflect_files(&stm_files);
            let sntm_ref = reflect_files(&sntm_files);
            let mirrored = mirror_horizontally(position);
            (stm_ref, sntm_ref, mirrored)
        };

        let key = PawnKey::new(&target_stm_files, &target_sntm_files);
        let &egt_idx = self.egt_map.get(&key).ok_or(())?;
        let local_index = self.egts[egt_idx].position_to_index(&target_position);

        let global_index = self.get_global_index(egt_idx, local_index);
        if global_index >= self.index_range {
            return Err(());
        }

        Ok(global_index)
    }

    /// Computes the global index in the file given an Egt index and local index.
    pub fn get_global_index(&self, egt_idx: usize, local_index: usize) -> usize {
        let offset: usize = self.egts[0..egt_idx].iter().map(|egt| egt.index_range()).sum();
        offset + local_index
    }

    /// Converts a global index to a position.
    pub fn index_to_position(&mut self, index: usize, side_to_move: Color) -> Option<Chess> {
        if index >= self.index_range {
            return None;
        }

        let mut remaining_idx = index;
        let mut target_egt_idx = None;
        for (egt_idx, egt) in self.egts.iter().enumerate() {
            let range = egt.index_range();
            if remaining_idx < range {
                target_egt_idx = Some(egt_idx);
                break;
            }
            remaining_idx -= range;
        }

        let egt_idx = target_egt_idx?;
        self.egts[egt_idx].position_from_index(remaining_idx, side_to_move)
    }

    /// Reads an outcome directly by its global index.
    pub fn read_from_index(&mut self, index: usize) -> Result<MaybeDtcOutcome, ()> {
        if index >= self.index_range {
            return Err(());
        }

        let frame_idx = index / self.frame_size;
        let offset = index % self.frame_size;

        let data = self.get_frame_data(frame_idx)?;
        Ok(data[offset])
    }

    /// Writes an outcome directly by its global index.
    pub fn write_to_index(&mut self, index: usize, outcome: MaybeDtcOutcome) -> Result<(), ()> {
        if index >= self.index_range {
            return Err(());
        }

        let frame_idx = index / self.frame_size;
        let offset = index % self.frame_size;

        self.ensure_uncompressed(frame_idx)?;

        if let FrameState::Uncompressed { uncompressed, dirty, .. } = &mut self.frames[frame_idx] {
            uncompressed[offset] = outcome;
            *dirty = true;
            Ok(())
        } else {
            unreachable!();
        }
    }

    /// Ensures that the frame at `frame_idx` is in the `Uncompressed` state.
    /// Allocates memory and decompresses if necessary.
    fn ensure_uncompressed(&mut self, frame_idx: usize) -> Result<(), ()> {
        match &self.frames[frame_idx] {
            FrameState::Empty => {
                // TODO: Allocate memory from arena
                let uncompressed = vec![MaybeDtcOutcome::INVALID; self.frame_size];

                self.frames[frame_idx] = FrameState::Uncompressed {
                    compressed: vec![],
                    uncompressed,
                    dirty: true,
                };
                Ok(())
            },
            FrameState::CompressedOnFile => {
                // Load compressed data from file
                let file = std::fs::File::open(&self.path).map_err(|_| ())?;
                let mut decoder = zeekstd::Decoder::new(file).map_err(|_| ())?;
                let mut transposed = vec![0u8; self.frame_size * 2];

                let uncompressed_offset = (frame_idx * self.frame_size * 2) as u64;
                decoder.seek(SeekFrom::Start(uncompressed_offset)).map_err(|_| ())?;
                decoder.read_exact(&mut transposed).map_err(|_| ())?;

                // Detranspose the frame
                // TODO: allocate memory from arena
                let uncompressed = detranspose_frame(&transposed, self.frame_size);

                self.frames[frame_idx] = FrameState::Uncompressed {
                    compressed: vec![],
                    uncompressed,
                    dirty: true,
                };
                Ok(())
            },
            FrameState::Compressed(compressed_bytes) => {
                // TODO: Allocate memory from arena
                let mut transposed = vec![0u8; self.frame_size * 2];

                // Decompress from memory
                use zeekstd::{BytesWrapper, Decoder};

                let wrapper = BytesWrapper::new(compressed_bytes);
                let mut decoder = Decoder::new(wrapper).map_err(|_| ())?;
                decoder.read_exact(&mut transposed).map_err(|_| ())?;

                // Detranspose the frame
                let uncompressed = detranspose_frame(&transposed, self.frame_size);

                self.frames[frame_idx] = FrameState::Uncompressed {
                    compressed: vec![],
                    uncompressed,
                    dirty: true,
                };
                Ok(())
            },
            FrameState::Uncompressed { .. } => {
                // Already uncompressed
                // TODO: update LRU status
                Ok(())
            }
        }
    }

    /// Ensures that the frame at `frame_idx` is in the `Compressed` state.
    fn ensure_compressed(&mut self, frame_idx: usize) -> Result<(), ()> {
        match &self.frames[frame_idx] {
            FrameState::Empty | FrameState::CompressedOnFile => {
                self.ensure_uncompressed(frame_idx)?;
                self.ensure_compressed(frame_idx)
            }
            FrameState::Compressed(_) => {
                // Already compressed
                Ok(())
            },
            FrameState::Uncompressed { uncompressed, dirty, .. } => {
                if !dirty {
                    let mut temp = FrameState::Empty;
                    std::mem::swap(&mut self.frames[frame_idx], &mut temp);
                    if let FrameState::Uncompressed { compressed, .. } = temp {
                        self.frames[frame_idx] = FrameState::Compressed(compressed);
                    }
                } else {
                    // Regenerate the compressed bytes
                    // TODO: Use the exact compressed frame bytes that would be used for the
                    // full compressed file.
                    use zeekstd::Encoder;

                    let mut compressed_bytes = vec![];
                    let mut encoder = Encoder::new(&mut compressed_bytes).map_err(|_| ())?;

                    // Transpose (bit-slice) the frame
                    let transposed = transpose_frame(&uncompressed);

                    // Compress the transposed frame
                    encoder.compress(&transposed).map_err(|_| ())?;
                    encoder.end_frame().map_err(|_| ())?;
                    encoder.finish().map_err(|_| ())?;

                    self.frames[frame_idx] = FrameState::Compressed(compressed_bytes);
                }
                Ok(())
            },
        }
    }
}

/// Transposes (bit-slices) a frame of N positions to maximize Zstd compressibility.
pub fn transpose_frame(uncompressed: &[MaybeDtcOutcome]) -> Vec<u8> {
    let n = uncompressed.len();
    let mut output = vec![0u8; n * 2];

    let set_bit = |slice: &mut [u8], bit_idx: usize, bit_val: u16| {
        if bit_val != 0 {
            slice[bit_idx / 8] |= 1 << (bit_idx % 8);
        }
    };

    // Slice offsets in bytes
    let offset_0 = 0;
    let offset_1 = n / 8;
    let offset_2 = (n / 8) * 2;
    let offset_3 = (n / 8) * 3;
    let offset_4 = offset_3 + (n / 2);
    let offset_5 = offset_4 + (n / 2);

    for i in 0..n {
        let val = uncompressed[i].to_u16();

        // Slice 0 (bit 0)
        set_bit(&mut output[offset_0..], i, val & 1);

        // Slice 1 (bit 1)
        set_bit(&mut output[offset_1..], i, val & 2);

        // Slice 2 (bit 2)
        set_bit(&mut output[offset_2..], i, val & 4);

        // Slice 3 (bits 3-6)
        let val_3 = (val >> 3) & 0xF;
        for b in 0..4 {
            set_bit(&mut output[offset_3..], i * 4 + b, val_3 & (1 << b));
        }

        // Slice 4 (bits 7-10)
        let val_4 = (val >> 7) & 0xF;
        for b in 0..4 {
            set_bit(&mut output[offset_4..], i * 4 + b, val_4 & (1 << b));
        }

        // Slice 5 (bits 11-15)
        let val_5 = (val >> 11) & 0x1F;
        for b in 0..5 {
            set_bit(&mut output[offset_5..], i * 5 + b, val_5 & (1 << b));
        }
    }

    output
}

/// Reverses the transposition (un-bit-slices) of a frame.
pub fn detranspose_frame(transposed: &[u8], frame_size: usize) -> Vec<MaybeDtcOutcome> {
    let n = frame_size;
    let mut output = vec![MaybeDtcOutcome::INVALID; n];

    let get_bit = |slice: &[u8], bit_idx: usize| -> u16 {
        let byte = slice[bit_idx / 8];
        ((byte >> (bit_idx % 8)) & 1) as u16
    };

    // Slice offsets in bytes
    let offset_0 = 0;
    let offset_1 = n / 8;
    let offset_2 = (n / 8) * 2;
    let offset_3 = (n / 8) * 3;
    let offset_4 = offset_3 + (n / 2);
    let offset_5 = offset_4 + (n / 2);

    for i in 0..n {
        let mut val = 0u16;

        // Slice 0 (bit 0)
        val |= get_bit(&transposed[offset_0..], i);

        // Slice 1 (bit 1)
        val |= get_bit(&transposed[offset_1..], i) << 1;

        // Slice 2 (bit 2)
        val |= get_bit(&transposed[offset_2..], i) << 2;

        // Slice 3 (bits 3-6)
        let mut val_3 = 0u16;
        for b in 0..4 {
            val_3 |= get_bit(&transposed[offset_3..], i * 4 + b) << b;
        }
        val |= val_3 << 3;

        // Slice 4 (bits 7-10)
        let mut val_4 = 0u16;
        for b in 0..4 {
            val_4 |= get_bit(&transposed[offset_4..], i * 4 + b) << b;
        }
        val |= val_4 << 7;

        // Slice 5 (bits 11-15)
        let mut val_5 = 0u16;
        for b in 0..5 {
            val_5 |= get_bit(&transposed[offset_5..], i * 5 + b) << b;
        }
        val |= val_5 << 11;

        output[i] = MaybeDtcOutcome::from_u16(val);
    }

    output
}

/// Parses the endgame name (e.g., "KRP_KP") to extract:
/// - Number of pawns for SideToMove
/// - Number of pawns for SideNotToMove
/// - List of non-pawn pieces
fn parse_endgame_name(endgame: &str) -> Result<(usize, usize, Vec<(EgtRole, EgtSide, usize)>), ()> {
    let (stm, sntm) = endgame.split_once('_').ok_or(())?;
    let mut stm_pawns = 0;
    let mut sntm_pawns = 0;
    let mut other_pieces = Vec::new();

    for (s, side, pawns) in [
        (stm, EgtSide::SideToMove, &mut stm_pawns),
        (sntm, EgtSide::SideNotToMove, &mut sntm_pawns),
    ] {
        let mut count = [0; crate::piece_set::ALL_EGT_ROLES.len()];
        for c in s.chars() {
            match c {
                'P' => *pawns += 1,
                'K' => count[EgtRole::King.to_index()] += 1,
                'Q' => count[EgtRole::Queen.to_index()] += 1,
                'R' => count[EgtRole::Rook.to_index()] += 1,
                'B' => count[EgtRole::Bishop.to_index()] += 1,
                'N' => count[EgtRole::Knight.to_index()] += 1,
                _ => return Err(()),
            }
        }
        if count[EgtRole::King.to_index()] != 1 {
            return Err(());
        }
        for piece in crate::piece_set::ALL_EGT_ROLES {
            if !piece.is_pawn() {
                let multiplicity = count[piece.to_index()];
                if multiplicity > 0 {
                    other_pieces.push((piece, side, multiplicity));
                }
            }
        }
    }
    Ok((stm_pawns, sntm_pawns, other_pieces))
}

/// Generates all combinations with repetition of size `k` from the 8 files.
fn get_file_combinations(k: usize) -> Vec<Vec<File>> {
    let mut results = Vec::new();
    let mut current = Vec::new();
    fn recurse(k: usize, start_file_idx: usize, current: &mut Vec<File>, results: &mut Vec<Vec<File>>) {
        if current.len() == k {
            results.push(current.clone());
            return;
        }
        for idx in start_file_idx..8 {
            let file = File::new(idx as u32);
            current.push(file);
            recurse(k, idx, current, results);
            current.pop();
        }
    }
    recurse(k, 0, &mut current, &mut results);
    results
}

/// Reflects a list of files horizontally (file f becomes 7 - f).
pub fn reflect_files(files: &[File]) -> Vec<File> {
    let mut reflected: Vec<File> = files.iter().map(|f| File::new(7 - f.to_usize() as u32)).collect();
    reflected.sort_by_key(|f| f.to_usize());
    reflected
}

/// Checks if a pawn configuration is canonical (lexicographically lower than or equal to its horizontal reflection).
pub fn is_canonical(stm_files: &[File], sntm_files: &[File]) -> bool {
    let stm_ref = reflect_files(stm_files);
    let sntm_ref = reflect_files(sntm_files);

    let stm_idx: Vec<usize> = stm_files.iter().map(|f| f.to_usize()).collect();
    let sntm_idx: Vec<usize> = sntm_files.iter().map(|f| f.to_usize()).collect();

    let stm_ref_idx: Vec<usize> = stm_ref.iter().map(|f| f.to_usize()).collect();
    let sntm_ref_idx: Vec<usize> = sntm_ref.iter().map(|f| f.to_usize()).collect();

    (stm_idx.clone(), sntm_idx.clone()) <= (stm_ref_idx, sntm_ref_idx)
}

/// Builds the pieces vector by combining pawn file assignments and non-pawn pieces.
fn build_pieces(
    stm_files: &[File],
    sntm_files: &[File],
    other_pieces: &[(EgtRole, EgtSide, usize)],
) -> Vec<(EgtRole, EgtSide, usize)> {
    let mut pieces = other_pieces.to_vec();

    // Count stm pawns by file
    let mut stm_pawn_counts = [0; 8];
    for f in stm_files {
        stm_pawn_counts[f.to_usize()] += 1;
    }
    for (idx, &count) in stm_pawn_counts.iter().enumerate() {
        if count > 0 {
            pieces.push((EgtRole::Pawn(File::new(idx as u32)), EgtSide::SideToMove, count));
        }
    }

    // Count sntm pawns by file
    let mut sntm_pawn_counts = [0; 8];
    for f in sntm_files {
        sntm_pawn_counts[f.to_usize()] += 1;
    }
    for (idx, &count) in sntm_pawn_counts.iter().enumerate() {
        if count > 0 {
            pieces.push((EgtRole::Pawn(File::new(idx as u32)), EgtSide::SideNotToMove, count));
        }
    }

    pieces
}

/// Mirrors a chess position horizontally.
pub fn mirror_horizontally(position: &Chess) -> Chess {
    let mut setup = position.to_setup(shakmaty::EnPassantMode::Legal);
    setup.board.flip_horizontal();
    setup.ep_square = setup.ep_square.map(|sq| sq.flip_horizontal());
    setup.position(CastlingMode::Standard).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use shakmaty::fen::Fen;
    use shakmaty::Color;

    #[test]
    fn test_parse_endgame_name() {
        let (stm_pawns, sntm_pawns, other_pieces) = parse_endgame_name("KRP_KP").unwrap();
        assert_eq!(stm_pawns, 1);
        assert_eq!(sntm_pawns, 1);
        assert_eq!(other_pieces.len(), 3); // King STM, King SNTM, Rook STM
        // Wait, other_pieces has King STM, Rook STM, King SNTM.
        // Let's check the exact pieces
        assert!(other_pieces.contains(&(EgtRole::King, EgtSide::SideToMove, 1)));
        assert!(other_pieces.contains(&(EgtRole::Rook, EgtSide::SideToMove, 1)));
        assert!(other_pieces.contains(&(EgtRole::King, EgtSide::SideNotToMove, 1)));
    }

    #[test]
    fn test_get_file_combinations() {
        let combos_1 = get_file_combinations(1);
        assert_eq!(combos_1.len(), 8);

        let combos_2 = get_file_combinations(2);
        assert_eq!(combos_2.len(), 36);
    }

    #[test]
    fn test_is_canonical() {
        // [a] vs [c] is canonical
        assert!(is_canonical(&[File::A], &[File::C]));
        // [h] vs [f] is not canonical (reflection is [a] vs [c])
        assert!(!is_canonical(&[File::H], &[File::F]));
    }

    #[test]
    fn test_egt_file_creation() {
        let path = PathBuf::from("test_kp_k.egt");
        let egt_file = EgtFile::new(&path, "KP_K").unwrap();

        // KP_K has 1 pawn for STM, 0 for SNTM.
        // Files for STM pawn: a, b, c, d are canonical. e, f, g, h are reflected.
        // So we expect exactly 4 Egts: KPa_K, KPb_K, KPc_K, KPd_K
        assert_eq!(egt_file.egts.len(), 4);
        assert!(egt_file.egt_map.contains_key(&PawnKey::new(&[File::A], &[])));
        assert!(egt_file.egt_map.contains_key(&PawnKey::new(&[File::B], &[])));
        assert!(egt_file.egt_map.contains_key(&PawnKey::new(&[File::C], &[])));
        assert!(egt_file.egt_map.contains_key(&PawnKey::new(&[File::D], &[])));
    }

    #[test]
    fn egt_file_probing_canonical() {
        let path = PathBuf::from("test_kp_k.egt");
        let mut egt_file = EgtFile::new(&path, "KP_K").unwrap();

        // A canonical position: White pawn on a2, White king on a1, Black king on h8
        // FEN: 8/8/8/8/8/8/P7/K6k w - - 0 1
        let fen: Fen = "8/8/8/8/8/8/P7/K6k w - - 0 1".parse().unwrap();
        let position = fen.into_position(CastlingMode::Standard).unwrap();

        // Probing should succeed (returns Invalid because the frame is initialized to 0/invalid)
        let outcome = egt_file.probe(&position);
        assert_eq!(outcome, Ok(MaybeDtcOutcome::INVALID));

        // Write an outcome
        let expected_outcome = MaybeDtcOutcome::new_win(crate::ConversionType::Checkmate, 12);
        let idx = egt_file.map_position_to_index(&position).unwrap();
        egt_file.write_to_index(idx, expected_outcome).unwrap();

        // Probe again
        let outcome = egt_file.probe(&position);
        assert_eq!(outcome, Ok(expected_outcome));
    }

    #[test]
    fn egt_file_probing_mirrored() {
        let path = PathBuf::from("test_kp_k.egt");
        let mut egt_file = EgtFile::new(&path, "KP_K").unwrap();

        // A mirrored (non-canonical) position: White pawn on h2, White king on h1, Black king on a1
        // This is the horizontal reflection of the canonical position above.
        // FEN: 8/8/8/8/8/8/7P/k6K w - - 0 1
        let fen_mirrored: Fen = "8/8/8/8/8/8/7P/k6K w - - 0 1".parse().unwrap();
        let position_mirrored = fen_mirrored.into_position(CastlingMode::Standard).unwrap();
        let fen_canonical: Fen = "8/8/8/8/8/8/P7/K6k w - - 0 1".parse().unwrap();
        let position_canonical = fen_canonical.into_position(CastlingMode::Standard).unwrap();

        // Write to the canonical position
        let expected_outcome = MaybeDtcOutcome::new_win(crate::ConversionType::Checkmate, 12);
        let idx_canonical = egt_file.map_position_to_index(&position_canonical).unwrap();
        egt_file.write_to_index(idx_canonical, expected_outcome).unwrap();

        // Probing the mirrored position should return the same outcome because it gets canonicalized/mirrored
        let outcome = egt_file.probe(&position_mirrored);
        assert_eq!(outcome, Ok(expected_outcome));
    }

    fn run_round_trip_test(endgame: &str, stride: usize) {
        let path = PathBuf::from(".");
        let mut egt_file = EgtFile::new(&path, endgame).unwrap();

        let mut offset = 0;
        let num_egts = egt_file.egts.len();
        for egt_idx in 0..num_egts {
            let range = egt_file.egts[egt_idx].index_range();
            let mut local_index = 0;
            while local_index < range {
                let global_index = offset + local_index;
                if let Some(position) = egt_file.egts[egt_idx].position_from_index(local_index, Color::White) {
                    let mapped_global_index = egt_file.map_position_to_index(&position).unwrap();
                    assert_eq!(global_index, mapped_global_index);
                }
                local_index += stride;
            }
            offset += range;
        }
    }

    #[test]
    fn egt_file_decode_encode_kpp_kpp() {
        // KPP_KPP has 2.35 billion positions across 656 sub-tables.
        run_round_trip_test("KPP_KPP", 100_000);
    }

    #[test]
    fn egt_file_decode_encode_kq_kr() {
        // KQ_KR has 1.7 million positions across 1 sub-table.
        run_round_trip_test("KQ_KR", 100);
    }

    #[test]
    fn egt_file_decode_encode_kppp_k() {
        // KPPP_K has ~5 million positions across 56 sub-tables.
        run_round_trip_test("KPPP_K", 100);
    }

    #[test]
    fn egt_file_decode_encode_kpp_kp() {
        // KPP_KP has ~15 million positions across 156 sub-tables.
        run_round_trip_test("KPP_KP", 500);
    }

    #[test]
    fn egt_file_compression_decompression() {
        let base_path = PathBuf::from(".");
        let mut egt_file = EgtFile::new(&base_path, "KP_K").unwrap();

        // Sample a position
        let fen: Fen = "8/8/8/8/8/8/P7/K6k w - - 0 1".parse().unwrap();
        let position = fen.into_position(CastlingMode::Standard).unwrap();
        let outcome = MaybeDtcOutcome::new_win(ConversionType::Checkmate, 987);
        let idx = egt_file.map_position_to_index(&position).unwrap();
        egt_file.write_to_index(idx, outcome).unwrap();
        assert_eq!(egt_file.probe(&position), Ok(outcome));

        // Save to file
        egt_file.save_to_file().unwrap();
        for f in 0..egt_file.frames.len() {
            match egt_file.frames[f] {
                FrameState::Compressed(_) => {},
                _ => panic!("frame {} should be compressed on file", f),
            }
        }

        // Probe again (triggers on-demand decompression from file)
        let mut another_egt_file = EgtFile::new_from_file(&base_path, "KP_K").unwrap();
        let loaded_outcome = another_egt_file.probe(&position);
        assert_eq!(loaded_outcome, Ok(outcome));

        // Clean up
        let _ = std::fs::remove_file(&egt_file.path);
    }

    #[test]
    fn egt_memory_compression_decompression() {
        let path = PathBuf::from(".");
        let mut egt_file = EgtFile::new(&path, "KP_K").unwrap();

        // Sample a position
        let fen: Fen = "8/8/8/8/8/8/P7/K6k w - - 0 1".parse().unwrap();
        let position = fen.into_position(CastlingMode::Standard).unwrap();
        let outcome = MaybeDtcOutcome::new_win(ConversionType::Checkmate, 987);
        let idx = egt_file.map_position_to_index(&position).unwrap();
        egt_file.write_to_index(idx, outcome).unwrap();
        assert_eq!(egt_file.probe(&position), Ok(outcome));

        for f in 0..egt_file.frames.len() {
            egt_file.ensure_compressed(f).unwrap();
            match egt_file.frames[f] {
                FrameState::Compressed(_) => {},
                _ => panic!(),
            }
        }

        // Probe again (triggers decompression from memory)
        let loaded_outcome = egt_file.probe(&position);
        assert_eq!(loaded_outcome, Ok(outcome));

        // Clean up
        let _ = std::fs::remove_file(&path);
    }
}
