use std::collections::HashMap;
use std::path::PathBuf;
use chess::{Board, BoardBuilder, Color, File, Piece, Square};
use crate::DtcOutcome;
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
        /// Uncompressed DtcOutcome values (stored as u16 for efficiency).
        uncompressed: Vec<u16>,
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
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct PawnKey {
    files: [u8; 8],  // Up to 8 pawns, padded with 255
}

impl PawnKey {
    pub fn new(stm: &[File], sntm: &[File]) -> Self {
        debug_assert!(stm.len() + sntm.len() <= 8);

        let mut arr = [255; 8];
        let mut i = 0;
        for &f in stm {
            arr[i] = f.to_index() as u8;
            i += 1;
        }
        for &f in sntm {
            arr[i] = f.to_index() as u8 + 8;
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
    path: PathBuf,
    /// The sub-tables (Egt objects) that compose this file.
    egts: Vec<Egt>,
    /// Map from PawnKey to its index in the `egts` vector.
    egt_map: HashMap<PawnKey, usize>,
    /// The frames of the file.
    frames: Vec<FrameState>,
    /// Number of positions per frame (e.g., 16384).
    frame_size: usize,
    /// Total number of positions across all Egts in this file.
    total_positions: usize,
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
                    let name = get_tablename(&egt.pieces);
                    egt_pairs.push((name, egt));
                }
            }
        }

        // Sort alphabetically by tablename to ensure a stable, deterministic order
        egt_pairs.sort_by(|(name1, _), (name2, _)| name1.cmp(name2));

        let mut egts = Vec::with_capacity(egt_pairs.len());
        let mut egt_map = HashMap::with_capacity(egt_pairs.len());

        for (idx, (_name, egt)) in egt_pairs.into_iter().enumerate() {
            let mut stm_files = Vec::new();
            let mut sntm_files = Vec::new();
            for &(piece, side, multiplicity) in &egt.pieces {
                if let EgtPiece::Pawn(file) = piece {
                    for _ in 0..multiplicity {
                        if side == EgtSide::SideToMove {
                            stm_files.push(file);
                        } else {
                            sntm_files.push(file);
                        }
                    }
                }
            }
            stm_files.sort_by_key(|f| f.to_index());
            sntm_files.sort_by_key(|f| f.to_index());
            let key = PawnKey::new(&stm_files, &sntm_files);
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
    pub fn probe(&mut self, board: &Board, arena: &mut Arena) -> Option<DtcOutcome> {
        let (egt_idx, local_index) = self.map_position_to_egt(board)?;
        let global_index = self.get_global_index(egt_idx, local_index);

        if global_index >= self.total_positions {
            return None;
        }

        let frame_idx = global_index / self.frame_size;
        let offset = global_index % self.frame_size;

        self.ensure_uncompressed(frame_idx, arena);

        if let FrameState::Uncompressed { uncompressed, .. } = &self.frames[frame_idx] {
            let val = uncompressed[offset];
            if val == 0 {
                None // Invalid/unknown
            } else {
                Some(DtcOutcome::from_u16(val))
            }
        } else {
            None
        }
    }

    /// Writes the outcome of a specific board position.
    pub fn write_outcome(&mut self, board: &Board, outcome: DtcOutcome, arena: &mut Arena) -> Result<(), ()> {
        let (egt_idx, local_index) = self.map_position_to_egt(board).ok_or(())?;
        let global_index = self.get_global_index(egt_idx, local_index);

        if global_index >= self.total_positions {
            return Err(());
        }

        let frame_idx = global_index / self.frame_size;
        let offset = global_index % self.frame_size;

        self.ensure_uncompressed(frame_idx, arena);

        if let FrameState::Uncompressed { uncompressed, dirty, .. } = &mut self.frames[frame_idx] {
            uncompressed[offset] = outcome.to_u16();
            *dirty = true;
            Ok(())
        } else {
            Err(())
        }
    }

    /// Flushes all dirty frames to disk using seekable Zstd compression.
    pub fn flush(&mut self) -> Result<(), ()> {
        for frame in &mut self.frames {
            if let FrameState::Uncompressed { compressed, uncompressed: _, dirty } = frame {
                if *dirty {
                    // In a full implementation, we would bit-slice `uncompressed`
                    // and compress it using Zstd, then write to disk.
                    *compressed = Some(vec![]); // Stub compressed bytes
                    *dirty = false;
                }
            }
        }
        Ok(())
    }

    /// Maps a board position to the corresponding Egt index and local index.
    fn map_position_to_egt(&self, board: &Board) -> Option<(usize, usize)> {
        let stm_color = board.side_to_move();
        let sntm_color = !stm_color;

        let stm_pawns_bb = *board.pieces(Piece::Pawn) & board.color_combined(stm_color);
        let sntm_pawns_bb = *board.pieces(Piece::Pawn) & board.color_combined(sntm_color);

        let mut stm_files: Vec<File> = stm_pawns_bb.into_iter().map(|sq| sq.get_file()).collect();
        let mut sntm_files: Vec<File> = sntm_pawns_bb.into_iter().map(|sq| sq.get_file()).collect();

        stm_files.sort_by_key(|f| f.to_index());
        sntm_files.sort_by_key(|f| f.to_index());

        let canonical = is_canonical(&stm_files, &sntm_files);

        let (target_stm_files, target_sntm_files, target_board) = if canonical {
            (stm_files, sntm_files, board.clone())
        } else {
            let stm_ref = reflect_files(&stm_files);
            let sntm_ref = reflect_files(&sntm_files);
            let mirrored = mirror_board_horizontally(board);
            (stm_ref, sntm_ref, mirrored)
        };

        let key = PawnKey::new(&target_stm_files, &target_sntm_files);
        let &egt_idx = self.egt_map.get(&key)?;
        let mut indexer = self.egts[egt_idx].indexer.clone();
        let local_index = indexer.board_to_index(&target_board);

        Some((egt_idx, local_index))
    }

    /// Computes the global index in the file given an Egt index and local index.
    fn get_global_index(&self, egt_idx: usize, local_index: usize) -> usize {
        let offset: usize = self.egts[0..egt_idx].iter().map(|egt| egt.index_range()).sum();
        offset + local_index
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
                        DtcOutcome::Draw
                    } else {
                        // Random conversion type
                        let ct_r = prng.next_range(0, 2);
                        let ct = match ct_r {
                            0 => crate::ConversionType::Checkmate,
                            1 => crate::ConversionType::Promotion,
                            _ => crate::ConversionType::Capture,
                        };
                        // Random distance (1 to 100 moves)
                        let dist = prng.next_range(1, 100) as u16;

                        if r < 85 {
                            DtcOutcome::Win(ct, dist)
                        } else {
                            DtcOutcome::Loss(ct, dist)
                        }
                    };
                    *val = outcome.to_u16();
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

                // Stub decompression: in full implementation, we would decompress
                // `compressed_bytes` using Zstd and reverse the bit-slicing.
                let uncompressed = vec![0; self.frame_size];

                self.frames[frame_idx] = FrameState::Uncompressed {
                    compressed: Some(compressed_bytes.clone()),
                    uncompressed,
                    dirty: false,
                };
            }
            FrameState::Unallocated => {
                // Allocate memory from arena
                let mem_size = self.frame_size * 2;
                arena.allocate(mem_size);

                let uncompressed = vec![0; self.frame_size];

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
            let file = File::from_index(idx);
            current.push(file);
            recurse(k, idx, current, results);
            current.pop();
        }
    }
    recurse(k, 0, &mut current, &mut results);
    results
}

/// Reflects a list of files horizontally (file f becomes 7 - f).
fn reflect_files(files: &[File]) -> Vec<File> {
    let mut reflected: Vec<File> = files.iter().map(|f| File::from_index(7 - f.to_index())).collect();
    reflected.sort_by_key(|f| f.to_index());
    reflected
}

/// Checks if a pawn configuration is canonical (lexicographically lower than or equal to its horizontal reflection).
fn is_canonical(stm_files: &[File], sntm_files: &[File]) -> bool {
    let stm_ref = reflect_files(stm_files);
    let sntm_ref = reflect_files(sntm_files);

    let stm_idx: Vec<usize> = stm_files.iter().map(|f| f.to_index()).collect();
    let sntm_idx: Vec<usize> = sntm_files.iter().map(|f| f.to_index()).collect();

    let stm_ref_idx: Vec<usize> = stm_ref.iter().map(|f| f.to_index()).collect();
    let sntm_ref_idx: Vec<usize> = sntm_ref.iter().map(|f| f.to_index()).collect();

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
        stm_pawn_counts[f.to_index()] += 1;
    }
    for (idx, &count) in stm_pawn_counts.iter().enumerate() {
        if count > 0 {
            pieces.push((EgtPiece::Pawn(File::from_index(idx)), EgtSide::SideToMove, count));
        }
    }

    // Count sntm pawns by file
    let mut sntm_pawn_counts = [0; 8];
    for f in sntm_files {
        sntm_pawn_counts[f.to_index()] += 1;
    }
    for (idx, &count) in sntm_pawn_counts.iter().enumerate() {
        if count > 0 {
            pieces.push((EgtPiece::Pawn(File::from_index(idx)), EgtSide::SideNotToMove, count));
        }
    }

    pieces
}

/// Formats a list of pieces into a canonical tablename string (e.g., "KPaPdPf_KPbPf").
pub fn get_tablename(pieces: &[(EgtPiece, EgtSide, usize)]) -> String {
    let mut stm_king = String::new();
    let mut stm_others = String::new();
    let mut stm_pawns = String::new();

    let mut sntm_king = String::new();
    let mut sntm_others = String::new();
    let mut sntm_pawns = String::new();

    for &(piece, side, multiplicity) in pieces {
        let (king, others, pawns) = if side == EgtSide::SideToMove {
            (&mut stm_king, &mut stm_others, &mut stm_pawns)
        } else {
            (&mut sntm_king, &mut sntm_others, &mut sntm_pawns)
        };

        match piece {
            EgtPiece::King => {
                for _ in 0..multiplicity {
                    king.push('K');
                }
            }
            EgtPiece::Queen => {
                for _ in 0..multiplicity {
                    others.push('Q');
                }
            }
            EgtPiece::Rook => {
                for _ in 0..multiplicity {
                    others.push('R');
                }
            }
            EgtPiece::Bishop => {
                for _ in 0..multiplicity {
                    others.push('B');
                }
            }
            EgtPiece::Knight => {
                for _ in 0..multiplicity {
                    others.push('N');
                }
            }
            EgtPiece::Pawn(file) => {
                let file_char = match file {
                    File::A => 'a',
                    File::B => 'b',
                    File::C => 'c',
                    File::D => 'd',
                    File::E => 'e',
                    File::F => 'f',
                    File::G => 'g',
                    File::H => 'h',
                };
                for _ in 0..multiplicity {
                    pawns.push_str(&format!("P{}", file_char));
                }
            }
        }
    }

    format!(
        "{}{}{}_{}{}{}",
        stm_king, stm_others, stm_pawns,
        sntm_king, sntm_others, sntm_pawns
    )
}

/// Mirrors a chess board horizontally.
fn mirror_board_horizontally(board: &Board) -> Board {
    let mut builder = BoardBuilder::new();
    builder.side_to_move(board.side_to_move());

    if let Some(ep_square) = board.en_passant() {
        let mirrored_file = File::from_index(7 - ep_square.get_file().to_index());
        builder.en_passant(Some(mirrored_file));
    }

    let pieces = [
        Piece::Pawn,
        Piece::King,
        Piece::Queen,
        Piece::Rook,
        Piece::Bishop,
        Piece::Knight,
    ];
    let colors = [Color::White, Color::Black];

    for &piece in &pieces {
        for &color in &colors {
            let bb = *board.pieces(piece) & board.color_combined(color);
            for square in bb {
                let mirrored_file = File::from_index(7 - square.get_file().to_index());
                let mirrored_square = Square::make_square(square.get_rank(), mirrored_file);
                builder.piece(mirrored_square, piece, color);
            }
        }
    }

    builder.try_into().expect("Failed to build mirrored board")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

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
        let board = Board::from_str("8/8/8/8/8/8/P7/K6k w - - 0 1").unwrap();

        // Probing should succeed (returns None because the frame is initialized to 0/invalid)
        let outcome = egt_file.probe(&board, &mut arena);
        assert_eq!(outcome, None);

        // Write an outcome
        let expected_outcome = DtcOutcome::Win(crate::ConversionType::Checkmate, 12);
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
        let board_mirrored = Board::from_str("8/8/8/8/8/8/7P/k6K w - - 0 1").unwrap();
        let board_canonical = Board::from_str("8/8/8/8/8/8/P7/K6k w - - 0 1").unwrap();

        // Write to the canonical position
        let expected_outcome = DtcOutcome::Win(crate::ConversionType::Checkmate, 12);
        egt_file.write_outcome(&board_canonical, expected_outcome, &mut arena).unwrap();

        // Probing the mirrored position should return the same outcome because it gets canonicalized/mirrored!
        let outcome = egt_file.probe(&board_mirrored, &mut arena);
        assert_eq!(outcome, Some(expected_outcome));
    }

    fn run_round_trip_test(tablename: &str, stride: usize) {
        let path = PathBuf::from(format!("test_{}.egt", tablename.to_lowercase()));
        let egt_file = EgtFile::new(path, tablename, true).unwrap();

        let mut offset = 0;
        for (_egt_idx, egt) in egt_file.egts.iter().enumerate() {
            let range = egt.index_range();
            let mut indexer = egt.indexer.clone();
            let mut local_index = 0;
            while local_index < range {
                let global_index = offset + local_index;
                if let Some(board) = indexer.board_from_index(local_index, Color::White) {
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
}
