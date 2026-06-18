use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use shakmaty::{File, Setup};
use crate::MaybeDtcOutcome;
use crate::egt::Egt;
use crate::piece_set::{EgtPiece, EgtSide};

// 16k positions per frame, corresponding to 32k bytes per frame.
const DEFAULT_FRAME_SIZE: usize = 16384;

/// Represents the state of a single frame in an EgtFile.
#[derive(Debug, Clone)]
pub enum FrameState {
    /// The frame is not allocated or calculated yet.
    Unallocated,
    /// Only the compressed representation of the frame is stored in memory.
    Compressed(Vec<u8>),
    /// The frame is fully uncompressed in memory.
    Uncompressed {
        /// Cached compressed bytes to avoid re-compression if not dirty on eviction.
        compressed: Option<Vec<u8>>,
        /// Uncompressed MaybeDtcOutcome values.
        uncompressed: Vec<MaybeDtcOutcome>,
        /// True if the frame has been modified since it was loaded or created.
        dirty: bool,
    },
}

/// A simple stub for the memory Arena that manages a fixed pool of memory.
#[derive(Debug)]
pub struct Arena {
    capacity: usize,
    used: usize,
}

impl Arena {
    /// Creates a new Arena with the given capacity in bytes.
    pub fn new(capacity: usize) -> Self {
        Self { capacity, used: 0 }
    }

    /// Attempts to allocate memory from the arena.
    /// In a full implementation, this would trigger eviction of LRU frames if capacity is exceeded.
    pub fn allocate(&mut self, size: usize) -> bool {
        if self.used + size <= self.capacity {
            self.used += size;
            true
        } else {
            false
        }
    }

    /// Returns memory to the arena.
    pub fn deallocate(&mut self, size: usize) {
        self.used = self.used.saturating_sub(size);
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn used(&self) -> usize {
        self.used
    }
}

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
#[allow(dead_code)]
#[derive(Debug)]
pub struct EgtFile {
    /// Path to the file on disk.
    pub path: PathBuf,
    /// The sub-tables (Egt objects) that compose this file.
    pub egts: Vec<Egt>,
    /// Map from PawnKey to its index in the `egts` vector.
    pub egt_map: HashMap<PawnKey, usize>,
    /// The frames of the file.
    frames: Vec<FrameState>,
    /// Number of positions per frame (e.g., 16384).
    frame_size: usize,
    /// Total number of positions across all Egts in this file.
    pub total_positions: usize,
}

impl EgtFile {
    /// Creates a new EgtFile for a given piece configuration and path.
    ///
    /// If starting from scratch, frames are initialized as `Unallocated`.
    /// If starting from an existing file, frames are initialized as `Compressed` with data from disk.
    pub fn new(path: PathBuf, tablename: &str, from_scratch: bool) -> Result<Self, ()> {
        let (stm_pawns, sntm_pawns, other_pieces) = parse_top_level_tablename(tablename)?;

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

        let total_positions: usize = egts.iter().map(|egt| egt.index_range()).sum();
        let frame_size = DEFAULT_FRAME_SIZE;
        let num_frames = (total_positions + frame_size - 1) / frame_size;

        let mut frames = Vec::with_capacity(num_frames);
        if from_scratch {
            for _ in 0..num_frames {
                frames.push(FrameState::Unallocated);
            }
        } else {
            // Stub: In a full implementation, we would read the seekable Zstd index
            // from disk and populate the frames with their compressed bytes.
            for _ in 0..num_frames {
                frames.push(FrameState::Compressed(vec![]));
            }
        }

        Ok(Self {
            path,
            egts,
            egt_map,
            frames,
            frame_size,
            total_positions,
        })
    }

    /// Probes the outcome of a specific board position.
    pub fn probe(&mut self, board: &Setup, arena: &mut Arena) -> Option<MaybeDtcOutcome> {
        let (egt_idx, local_index) = self.map_position_to_egt(board)?;
        let global_index = self.get_global_index(egt_idx, local_index);

        if global_index >= self.total_positions {
            return None;
        }

        let frame_idx = global_index / self.frame_size;
        let offset = global_index % self.frame_size;

        self.ensure_uncompressed(frame_idx, arena);

        if let FrameState::Uncompressed { uncompressed, .. } = &self.frames[frame_idx] {
            Some(uncompressed[offset])
        } else {
            None
        }
    }

    /// Writes the outcome of a specific board position.
    pub fn write_outcome(&mut self, board: &Setup, outcome: MaybeDtcOutcome, arena: &mut Arena) -> Result<(), ()> {
        let (egt_idx, local_index) = self.map_position_to_egt(board).ok_or(())?;
        let global_index = self.get_global_index(egt_idx, local_index);

        if global_index >= self.total_positions {
            return Err(());
        }

        let frame_idx = global_index / self.frame_size;
        let offset = global_index % self.frame_size;

        self.ensure_uncompressed(frame_idx, arena);

        if let FrameState::Uncompressed { uncompressed, dirty, .. } = &mut self.frames[frame_idx] {
            uncompressed[offset] = outcome;
            *dirty = true;
            Ok(())
        } else {
            Err(())
        }
    }

    /// Counts the number of wins, draws, losses, and invalid positions in the EgtFile.
    pub fn count_outcomes(&mut self, arena: &mut Arena) -> (usize, usize, usize, usize) {
        let mut wins = 0;
        let mut draws = 0;
        let mut losses = 0;
        let mut invalid = 0;

        for frame_idx in 0..self.frames.len() {
            if let FrameState::Unallocated = &self.frames[frame_idx] {
                invalid += self.frame_size;
                continue;
            }

            self.ensure_uncompressed(frame_idx, arena);
            if let FrameState::Uncompressed { uncompressed, .. } = &self.frames[frame_idx] {
                for &outcome in uncompressed {
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
        }

        (wins, draws, losses, invalid)
    }

    /// Prints table-specific statistics (wins, draws, losses, compression) and returns the number of canonical positions.
    pub fn print_single_table_stats(&mut self, arena: &mut Arena) -> usize {
        let (wins, draws, losses, _) = self.count_outcomes(arena);
        let canonical_positions = wins + draws + losses;
        let compressed_size_bytes = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        let compressed_size_mb = compressed_size_bytes as f64 / (1024.0 * 1024.0);
        let bits_per_pos = if canonical_positions > 0 {
            (compressed_size_bytes as f64 * 8.0) / canonical_positions as f64
        } else {
            0.0
        };

        let tablename = self.path.file_stem().unwrap().to_str().unwrap();

        println!(
            "Generated table {} with {} positions: {} wins, {} draws, {} losses. Compressed size: {:.0}MB ({:.2} bits/pos).",
            tablename,
            canonical_positions,
            wins,
            draws,
            losses,
            compressed_size_mb,
            bits_per_pos
        );

        canonical_positions
    }

    /// Saves the entire EgtFile to disk using seekable Zstd compression.
    pub fn save_to_disk(&mut self, arena: &mut Arena) -> Result<(), ()> {
        use std::fs::File;
        use std::io::BufWriter;
        use zeekstd::Encoder;

        let file = File::create(&self.path).map_err(|_| ())?;
        let writer = BufWriter::new(file);
        let mut encoder = Encoder::new(writer).map_err(|_| ())?;

        for frame_idx in 0..self.frames.len() {
            // Ensure the frame is loaded (either uncompressed or compressed)
            // If it is Unallocated, we treat it as all zeros (unknown/invalid)
            let uncompressed_data = match &mut self.frames[frame_idx] {
                FrameState::Uncompressed { uncompressed, .. } => uncompressed.clone(),
                FrameState::Compressed(_compressed_bytes) => {
                    self.ensure_uncompressed(frame_idx, arena);
                    if let FrameState::Uncompressed { uncompressed, .. } = &self.frames[frame_idx] {
                        uncompressed.clone()
                    } else {
                        unreachable!()
                    }
                }
                FrameState::Unallocated => {
                    vec![MaybeDtcOutcome::INVALID; self.frame_size]
                }
            };

            // Transpose (bit-slice) the frame
            let transposed = transpose_frame(&uncompressed_data);

            // Compress the transposed frame
            encoder.compress(&transposed).map_err(|_| ())?;
            encoder.end_frame().map_err(|_| ())?;
        }

        // Finish the seekable Zstd file (writes the seek table)
        encoder.finish().map_err(|_| ())?;

        Ok(())
    }

    /// Saves the entire EgtFile to disk and evicts all uncompressed frames,
    /// returning their memory to the arena.
    pub fn save_and_evict_all(&mut self, arena: &mut Arena) -> Result<(), ()> {
        self.save_to_disk(arena)?;

        // Evict all frames
        for frame_idx in 0..self.frames.len() {
            if let FrameState::Uncompressed { .. } = &self.frames[frame_idx] {
                self.frames[frame_idx] = FrameState::Compressed(vec![]);
                arena.deallocate(self.frame_size * 2);
            }
        }

        Ok(())
    }

    /// Maps a board position to the corresponding Egt index and local index.
    pub fn map_position_to_egt(&mut self, board: &Setup) -> Option<(usize, usize)> {
        let stm_color = board.turn;
        let sntm_color = !stm_color;

        let stm_pawns_bb = board.board.pawns() & board.board.by_color(stm_color);
        let sntm_pawns_bb = board.board.pawns() & board.board.by_color(sntm_color);

        let mut stm_files: Vec<File> = stm_pawns_bb.into_iter().map(|sq| sq.file()).collect();
        let mut sntm_files: Vec<File> = sntm_pawns_bb.into_iter().map(|sq| sq.file()).collect();

        stm_files.sort_by_key(|f| f.to_usize());
        sntm_files.sort_by_key(|f| f.to_usize());

        let canonical = is_canonical(&stm_files, &sntm_files);

        let (target_stm_files, target_sntm_files, target_board) = if canonical {
            (stm_files, sntm_files, board.clone())
        } else {
            let stm_ref = reflect_files(&stm_files);
            let sntm_ref = reflect_files(&sntm_files);
            let mirrored = mirror_setup_horizontally(board);
            (stm_ref, sntm_ref, mirrored)
        };

        let key = PawnKey::new(&target_stm_files, &target_sntm_files);
        let &egt_idx = self.egt_map.get(&key)?;
        let local_index = self.egts[egt_idx].board_to_index(&target_board);

        Some((egt_idx, local_index))
    }

    /// Computes the global index in the file given an Egt index and local index.
    pub fn get_global_index(&self, egt_idx: usize, local_index: usize) -> usize {
        let offset: usize = self.egts[0..egt_idx].iter().map(|egt| egt.index_range()).sum();
        offset + local_index
    }

    /// Reads an outcome directly by its global index.
    pub fn read_by_global_index(&mut self, global_index: usize, arena: &mut Arena) -> Option<MaybeDtcOutcome> {
        if global_index >= self.total_positions {
            return None;
        }

        let frame_idx = global_index / self.frame_size;
        let offset = global_index % self.frame_size;

        self.ensure_uncompressed(frame_idx, arena);

        if let FrameState::Uncompressed { uncompressed, .. } = &self.frames[frame_idx] {
            Some(uncompressed[offset])
        } else {
            None
        }
    }

    /// Writes an outcome directly by its global index.
    pub fn write_by_global_index(&mut self, global_index: usize, outcome: MaybeDtcOutcome, arena: &mut Arena) -> Result<(), ()> {
        if global_index >= self.total_positions {
            return Err(());
        }

        let frame_idx = global_index / self.frame_size;
        let offset = global_index % self.frame_size;

        self.ensure_uncompressed(frame_idx, arena);

        if let FrameState::Uncompressed { uncompressed, dirty, .. } = &mut self.frames[frame_idx] {
            uncompressed[offset] = outcome;
            *dirty = true;
            Ok(())
        } else {
            Err(())
        }
    }

    /// Returns the total number of positions across all Egts in this file.
    pub fn total_positions(&self) -> usize {
        self.total_positions
    }

    /// Fills the EgtFile with random (but somewhat realistic) DtcOutcome values.
    /// This serves as a stub for retrograde analysis and a basis for testing compression.
    pub fn generate_random_outcomes(&mut self, arena: &mut Arena) {
        let mut prng = SimplePrng::new(42); // Seed with a constant for reproducibility

        for frame_idx in 0..self.frames.len() {
            self.ensure_uncompressed(frame_idx, arena);

            if let FrameState::Uncompressed { uncompressed, dirty, .. } = &mut self.frames[frame_idx] {
                for val in uncompressed.iter_mut() {
                    // 70% Draw, 15% Win, 15% Loss
                    let r = prng.next_range(0, 99);
                    let outcome = if r < 70 {
                        MaybeDtcOutcome::DRAW
                    } else {
                        // Random conversion type
                        let ct_r = prng.next_range(0, 2);
                        let ct = match ct_r {
                            0 => crate::ConversionType::Checkmate,
                            1 => crate::ConversionType::Capture,
                            _ => crate::ConversionType::Promotion,
                        };
                        let dist = prng.next_range(1, 100) as u16;
                        if r < 85 {
                            MaybeDtcOutcome::new_win(ct, dist)
                        } else {
                            MaybeDtcOutcome::new_loss(ct, dist)
                        }
                    };
                    *val = outcome;
                }
                *dirty = true;
            }
        }
    }

    /// Ensures that the frame at `frame_idx` is in the `Uncompressed` state.
    /// Allocates memory from the arena and decompresses if necessary.
    fn ensure_uncompressed(&mut self, frame_idx: usize, arena: &mut Arena) {
        match &self.frames[frame_idx] {
            FrameState::Uncompressed { .. } => {
                // Already uncompressed, update LRU status in full implementation
            }
            FrameState::Compressed(compressed_bytes) => {
                // Allocate memory from arena (frame_size * 2 bytes for u16 array)
                let mem_size = self.frame_size * 2;
                arena.allocate(mem_size);

                let mut transposed = vec![0u8; self.frame_size * 2];

                if compressed_bytes.is_empty() {
                    // Load and decompress from disk on demand!
                    use std::fs::File;
                    use zeekstd::Decoder;

                    let file = File::open(&self.path).expect("Failed to open EgtFile");
                    let mut decoder = Decoder::new(file).expect("Failed to create Decoder");

                    let uncompressed_offset = (frame_idx * self.frame_size * 2) as u64;
                    decoder.seek(SeekFrom::Start(uncompressed_offset)).expect("Failed to seek");
                    decoder.read_exact(&mut transposed).expect("Failed to read");
                } else {
                    // Decompress from memory!
                    use zeekstd::{BytesWrapper, Decoder};

                    let wrapper = BytesWrapper::new(compressed_bytes);
                    let mut decoder = Decoder::new(wrapper).expect("Failed to create Decoder");
                    decoder.read_exact(&mut transposed).expect("Failed to read");
                }

                // Detranspose the frame
                let uncompressed = detranspose_frame(&transposed, self.frame_size);

                self.frames[frame_idx] = FrameState::Uncompressed {
                    compressed: if compressed_bytes.is_empty() { None } else { Some(compressed_bytes.clone()) },
                    uncompressed,
                    dirty: false,
                };
            }
            FrameState::Unallocated => {
                // Allocate memory from arena
                let mem_size = self.frame_size * 2;
                arena.allocate(mem_size);

                let uncompressed = vec![MaybeDtcOutcome::INVALID; self.frame_size];

                self.frames[frame_idx] = FrameState::Uncompressed {
                    compressed: None,
                    uncompressed,
                    dirty: true,
                };
            }
        }
    }
}

/// A simple self-contained Linear Congruential Generator (LCG) PRNG.
/// This avoids adding external dependencies like `rand` to Cargo.toml.
struct SimplePrng {
    state: u32,
}

impl SimplePrng {
    fn new(seed: u32) -> Self {
        Self { state: seed }
    }

    fn next(&mut self) -> u32 {
        // LCG parameters (Numerical Recipes)
        self.state = self.state.wrapping_mul(1664525).wrapping_add(1013904223);
        self.state
    }

    fn next_range(&mut self, min: u32, max: u32) -> u32 {
        let val = self.next();
        min + (val % (max - min + 1))
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

/// Parses the top-level tablename (e.g., "KRP_KP") to extract:
/// - Number of pawns for SideToMove
/// - Number of pawns for SideNotToMove
/// - List of non-pawn pieces
fn parse_top_level_tablename(tablename: &str) -> Result<(usize, usize, Vec<(EgtPiece, EgtSide, usize)>), ()> {
    let (stm, sntm) = tablename.split_once('_').ok_or(())?;
    let mut stm_pawns = 0;
    let mut sntm_pawns = 0;
    let mut other_pieces = Vec::new();

    for (s, side, pawns) in [
        (stm, EgtSide::SideToMove, &mut stm_pawns),
        (sntm, EgtSide::SideNotToMove, &mut sntm_pawns),
    ] {
        let mut count = [0; crate::piece_set::ALL_EGT_PIECES.len()];
        for c in s.chars() {
            match c {
                'P' => *pawns += 1,
                'K' => count[EgtPiece::King.to_index()] += 1,
                'Q' => count[EgtPiece::Queen.to_index()] += 1,
                'R' => count[EgtPiece::Rook.to_index()] += 1,
                'B' => count[EgtPiece::Bishop.to_index()] += 1,
                'N' => count[EgtPiece::Knight.to_index()] += 1,
                _ => return Err(()),
            }
        }
        if count[EgtPiece::King.to_index()] != 1 {
            return Err(());
        }
        for piece in crate::piece_set::ALL_EGT_PIECES {
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
    other_pieces: &[(EgtPiece, EgtSide, usize)],
) -> Vec<(EgtPiece, EgtSide, usize)> {
    let mut pieces = other_pieces.to_vec();

    // Count stm pawns by file
    let mut stm_pawn_counts = [0; 8];
    for f in stm_files {
        stm_pawn_counts[f.to_usize()] += 1;
    }
    for (idx, &count) in stm_pawn_counts.iter().enumerate() {
        if count > 0 {
            pieces.push((EgtPiece::Pawn(File::new(idx as u32)), EgtSide::SideToMove, count));
        }
    }

    // Count sntm pawns by file
    let mut sntm_pawn_counts = [0; 8];
    for f in sntm_files {
        sntm_pawn_counts[f.to_usize()] += 1;
    }
    for (idx, &count) in sntm_pawn_counts.iter().enumerate() {
        if count > 0 {
            pieces.push((EgtPiece::Pawn(File::new(idx as u32)), EgtSide::SideNotToMove, count));
        }
    }

    pieces
}

/// Mirrors a chess board horizontally.
pub fn mirror_setup_horizontally(setup: &Setup) -> Setup {
    let mut mirrored = setup.clone();
    mirrored.board.flip_horizontal();
    mirrored.ep_square = mirrored.ep_square.map(|sq| sq.flip_horizontal());
    mirrored
}

/// Prints detailed statistics about a pair of generated tables (or a single table if symmetric).
pub fn print_generation_stats_pair(
    file_a: &mut EgtFile,
    file_b: Option<&mut EgtFile>,
    duration: std::time::Duration,
    arena: &mut Arena,
) {
    let mut total_canonical = file_a.print_single_table_stats(arena);

    if let Some(fb) = file_b {
        total_canonical += fb.print_single_table_stats(arena);
    }

    let us_per_pos = if total_canonical > 0 {
        duration.as_micros() as f64 / total_canonical as f64
    } else {
        0.0
    };

    let total_secs = duration.as_secs();
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    let format_dur = format!("{:02}h:{:02}m:{:02}s", hours, minutes, seconds);

    println!(
        "Time used {} ({:.2} μs/pos). Memory usage: {:.0}MB.",
        format_dur,
        us_per_pos,
        arena.used() as f64 / (1024.0 * 1024.0)
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    use shakmaty::fen::Fen;
    use shakmaty::Color;

    #[test]
    fn test_parse_top_level_tablename() {
        let (stm_pawns, sntm_pawns, other_pieces) = parse_top_level_tablename("KRP_KP").unwrap();
        assert_eq!(stm_pawns, 1);
        assert_eq!(sntm_pawns, 1);
        assert_eq!(other_pieces.len(), 3); // King STM, King SNTM, Rook STM
        // Wait, other_pieces has King STM, Rook STM, King SNTM.
        // Let's check the exact pieces
        assert!(other_pieces.contains(&(EgtPiece::King, EgtSide::SideToMove, 1)));
        assert!(other_pieces.contains(&(EgtPiece::Rook, EgtSide::SideToMove, 1)));
        assert!(other_pieces.contains(&(EgtPiece::King, EgtSide::SideNotToMove, 1)));
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
        let egt_file = EgtFile::new(path, "KP_K", true).unwrap();

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
        let mut egt_file = EgtFile::new(path, "KP_K", true).unwrap();
        let mut arena = Arena::new(1024 * 1024);

        // A canonical position: White pawn on a2, White king on a1, Black king on h8
        // FEN: 8/8/8/8/8/8/P7/K6k w - - 0 1
        let board = Fen::from_str("8/8/8/8/8/8/P7/K6k w - - 0 1").map(Setup::from).unwrap();

        // Probing should succeed (returns Invalid because the frame is initialized to 0/invalid)
        let outcome = egt_file.probe(&board, &mut arena);
        assert_eq!(outcome, Some(MaybeDtcOutcome::INVALID));

        // Write an outcome
        let expected_outcome = MaybeDtcOutcome::new_win(crate::ConversionType::Checkmate, 12);
        egt_file.write_outcome(&board, expected_outcome, &mut arena).unwrap();

        // Probe again
        let outcome = egt_file.probe(&board, &mut arena);
        assert_eq!(outcome, Some(expected_outcome));
    }

    #[test]
    fn egt_file_probing_mirrored() {
        let path = PathBuf::from("test_kp_k.egt");
        let mut egt_file = EgtFile::new(path, "KP_K", true).unwrap();
        let mut arena = Arena::new(1024 * 1024);

        // A mirrored (non-canonical) position: White pawn on h2, White king on h1, Black king on a1
        // This is the horizontal reflection of the canonical position above.
        // FEN: 8/8/8/8/8/8/7P/k6K w - - 0 1
        let board_mirrored = Fen::from_str("8/8/8/8/8/8/7P/k6K w - - 0 1").map(Setup::from).unwrap();
        let board_canonical = Fen::from_str("8/8/8/8/8/8/P7/K6k w - - 0 1").map(Setup::from).unwrap();

        // Write to the canonical position
        let expected_outcome = MaybeDtcOutcome::new_win(crate::ConversionType::Checkmate, 12);
        egt_file.write_outcome(&board_canonical, expected_outcome, &mut arena).unwrap();

        // Probing the mirrored position should return the same outcome because it gets canonicalized/mirrored!
        let outcome = egt_file.probe(&board_mirrored, &mut arena);
        assert_eq!(outcome, Some(expected_outcome));
    }

    fn run_round_trip_test(tablename: &str, stride: usize) {
        let path = PathBuf::from(format!("test_{}.egt", tablename.to_lowercase()));
        let mut egt_file = EgtFile::new(path, tablename, true).unwrap();

        let mut offset = 0;
        let num_egts = egt_file.egts.len();
        for egt_idx in 0..num_egts {
            let range = egt_file.egts[egt_idx].index_range();
            let mut local_index = 0;
            while local_index < range {
                let global_index = offset + local_index;
                if let Some(board) = egt_file.egts[egt_idx].board_from_index(local_index, Color::White) {
                    let (mapped_egt_idx, mapped_local_index) = egt_file.map_position_to_egt(&board).unwrap();
                    let mapped_global_index = egt_file.get_global_index(mapped_egt_idx, mapped_local_index);
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
        let path = PathBuf::from("test_compression.egt");
        let mut egt_file = EgtFile::new(path.clone(), "KP_K", true).unwrap();
        let mut arena = Arena::new(16 * 1024 * 1024);

        // Generate random outcomes
        egt_file.generate_random_outcomes(&mut arena);

        // Sample a position
        let board = Fen::from_str("8/8/8/8/8/8/P7/K6k w - - 0 1").map(Setup::from).unwrap();
        let original_outcome = egt_file.probe(&board, &mut arena);

        // Save and evict
        egt_file.save_and_evict_all(&mut arena).unwrap();
        assert_eq!(arena.used(), 0);

        // Probe again (triggers on-demand decompression from disk)
        let loaded_outcome = egt_file.probe(&board, &mut arena);
        assert_eq!(original_outcome, loaded_outcome);

        // Clean up
        let _ = std::fs::remove_file(&path);
    }
}
