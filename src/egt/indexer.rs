use shakmaty::{Color, File, Piece, Rank, Role, Square, Setup, CastlingMode, Position, Chess};
use crate::piece_set::{EgtRole, EgtSide};
use std::cmp::Reverse;

// Supporting up to 8 pieces should be fine for some time :)
const MAX_PIECES: usize = 8;

// Precompute the values of the binomial coefficients C(n, k) and
// store them as N_CHOOSE_K[k][n]
const fn initialize_nchoosek() -> [[usize; 65]; MAX_PIECES+1] {
    let mut c = [[0; 65]; MAX_PIECES+1];
    let mut n = 0;
    while n < 65 {
        c[0][n] = 1;
        n += 1;
    }
    let mut k = 1;
    while k < MAX_PIECES+1 {
        n = k;
        while n < 65 {
            c[k][n] = c[k-1][n-1] + c[k][n-1];
            n += 1;
        }
        k += 1;
    }
    c
}

static N_CHOOSE_K: [[usize; 65]; MAX_PIECES+1] = initialize_nchoosek();

fn kings_adjacent_or_same(a: usize, b: usize) -> bool {
    let ar = a / 8;
    let af = a % 8;
    let br = b / 8;
    let bf = b % 8;
    (ar as isize - br as isize).abs() <= 1 && (af as isize - bf as isize).abs() <= 1
}

fn reduce_king_pair(wk: usize, bk: usize) -> (usize, usize) {
    let mut pos = [(wk / 8, wk % 8), (bk / 8, bk % 8)];

    if pos[0].0 > 3 {
        for (r, _) in pos.iter_mut() { *r = 7 - *r; }
    }
    if pos[0].1 > 3 {
        for (_, f) in pos.iter_mut() { *f = 7 - *f; }
    }
    if pos[0].0 > pos[0].1 ||
        (pos[0].0 == pos[0].1 && pos[1].0 > pos[1].1) {
        for p in pos.iter_mut() { *p = (p.1, p.0); }
    }

    (pos[0].0 * 8 + pos[0].1, pos[1].0 * 8 + pos[1].1)
}

// Initialize map to encode the possible king positions under pawnless symmetries.
// The second king breaks diagonal symmetry when the first king is on the diagonal,
// but positions where both kings are on the a1-h8 diagonal are intentionally not
// canonicalized using other pieces.
fn initialize_kings_map() -> (Vec<(usize, usize)>, Vec<Vec<usize>>) {
    let mut m = Vec::new();
    let mut r = vec![vec![999; 64]; 64];
    let mut unique_pairs = std::collections::BTreeSet::new();

    for wk in 0..64 {
        for bk in 0..64 {
            if !kings_adjacent_or_same(wk, bk) {
                unique_pairs.insert(reduce_king_pair(wk, bk));
            }
        }
    }

    for (i, &(wk, bk)) in unique_pairs.iter().enumerate() {
        let bk_compacted = if wk <= bk { bk - 1 } else { bk };
        m.push((wk, bk_compacted));
        r[wk][bk_compacted] = i;
    }

    assert_eq!(m.len(), 462);
    (m, r)
}

// An element of the set of pieces appearing in an endgame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PieceSetElement {
    // The piece role appearing in the endgame position.
    pub role: EgtRole,

    // Whether the piece is for the side-to-move or the side-not-to-move.
    pub side: EgtSide,

    // How many instances of this piece appear in the endgame.
    pub multiplicity: usize,

    // The number of squares that these pieces can occupy.
    pub n_squares: usize,

    // The number of ways to put the pieces on the available squares.
    // Equal to N_CHOOSE_K[multiplicity][n_squares].
    pub combinations: usize,

    // The indexes (begin, end) where information about these pieces is
    // stored in the buffer vectors. Normally
    //   span.1 - span.0 = multiplicity,
    // unless we are encoding en passant positions and one of the pieces
    // here is a pawn that can be captured en passant. In that case the
    // multplicity is reduced by one since the pawn that can be captured
    // en passant is not encoded explicitly.
    pub span: (usize, usize),
}

// Positions where en passant is possible are encoded separately from positions
// where en passant is not possible. One instance of this struct represents one
// option for the en passant status of positions, together with the range of
// indexes associated to these positions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EnpassantOption {
    // The en passant square if en passant is possible, or None.
    pub square: Option<Square>,

    // The index of the pawns (side-not-to-move) on the file where the
    // en passant capture is possible.
    pub pawn_idx_sntm: Option<usize>,

    // The index of the pawns (side-to-move) on the file where the
    // en passant capture is possible.
    pub pawn_idx_stm: Option<usize>,

    // The start of the range used to encode positions with this en passant status.
    pub range_start: usize,

    // The end of the range used to encode positions with this en passant status.
    pub range_end: usize,
}

// A mutable scratch buffer used to encode/decode positions.
#[derive(Clone, Debug)]
pub struct IndexerScratch {
    pub piece_set: Vec<PieceSetElement>,
    pub current_ep_option: EnpassantOption,
    pub buffer_coord: Vec<(usize, usize)>,
    pub buffer_pidx: Vec<usize>,
}

// Helper object taking care of the efficient conversion from positions to
// compact indexes representing the positions, and viceversa.
#[derive(Clone, Debug)]
pub struct Indexer {
    // The set of pieces appearing in this endgame table.
    pub piece_set: Vec<PieceSetElement>,

    // The number of pieces in the endgame.
    pub n_pieces: usize,

    // The number of pawns in the endgame.
    pub n_pawns: usize,

    // The number of unique pawns in the endgame.
    // Pawns of the same color on the same file count as one unique piece.
    pub n_unique_pawns: usize,

    // The total number of positions indexed in this endgame table.
    pub index_range: usize,

    // The following field provides information on the en passant status.
    ep_options: Vec<EnpassantOption>,

    // Precomputed maps for encoding kings positions.
    kings_map_from_index: Vec<(usize, usize)>,
    kings_map_to_index: Vec<Vec<usize>>,
}

impl Indexer {
    // Initialize the indexer with the given set of pieces (type, side, multiplicity).
    pub fn from_pieces(pieces: &[(EgtRole, EgtSide, usize)]) -> Result<Self, ()> {
        // Setup the piece set elements.
        let mut piece_set: Vec<_> = pieces.iter().map(|p| PieceSetElement{
            role: p.0,
            side: p.1,
            multiplicity: p.2,
            n_squares: 0, // filled in later
            combinations: 0, // filled in later
            span: (0, 0), // filled in later
        }).collect();

        let n_pieces = piece_set.iter().map(|p| p.multiplicity).sum();
        if n_pieces > MAX_PIECES {
            println!("too many pieces?");
            return Err(());
        }

        let n_pawns = piece_set.iter().filter(|p| p.role.is_pawn()).map(|p| p.multiplicity).sum();
        let n_unique_pawns = piece_set.iter().filter(|p| p.role.is_pawn()).count();

        // Compute n_squares, combinations, span.
        Self::compute_piece_squares(&mut piece_set);

        let combinations_without_ep = piece_set.iter().map(|p| p.combinations).product();

        // Check en passant options.
        let ep_options = Self::setup_ep_options(&mut piece_set[0..n_unique_pawns], combinations_without_ep);

        //println!("pieces: {:?}", &piece_set);
        //println!("ep_options: {:?}", &ep_options);

        let index_range = ep_options.last().unwrap().range_end;

        let (kings_map_from_index, kings_map_to_index) = initialize_kings_map();

        Ok(Indexer {
            piece_set,
            n_pieces,
            n_pawns,
            n_unique_pawns,
            index_range,
            ep_options,
            kings_map_from_index,
            kings_map_to_index,
        })
    }

    pub fn create_scratch(&self) -> IndexerScratch {
        IndexerScratch {
            piece_set: self.piece_set.clone(),
            current_ep_option: self.ep_options[0],
            buffer_coord: vec![(0, 0); self.n_pieces],
            buffer_pidx: vec![0; self.n_pieces],
        }
    }

    fn compute_piece_squares(piece_set: &mut[PieceSetElement]) {
        let mut available_pawn_squares = [6; 8];
        let mut available_squares = 64;
        let mut pawnless = true;
        let mut span_end = 0;
        for p in piece_set.iter_mut() {
            let k = p.multiplicity;
            if p.role.is_pawn() {
                pawnless = false;
                p.n_squares = available_pawn_squares[p.role.to_index()];
                assert!(p.n_squares >= k);
                available_pawn_squares[p.role.to_index()] -= k;
            } else {
                p.n_squares = available_squares;
            }
            available_squares -= k;
            p.combinations = N_CHOOSE_K[k][p.n_squares];
            p.span.0 = span_end;
            p.span.1 = p.span.0 + k;
            span_end = p.span.1;
        }
        if pawnless {
            assert_eq!(piece_set[0].multiplicity, 1); // white king
            assert_eq!(piece_set[1].multiplicity, 1); // black king
            // Encode the position of both kings using a single canonical index.
            piece_set[0].n_squares = 462;
            piece_set[0].combinations = 462;
            piece_set[1].n_squares = 1;
            piece_set[1].combinations = 1;
        }
    }

    fn setup_ep_options(pawns: &mut[PieceSetElement], combinations_without_ep: usize) -> Vec<EnpassantOption> {
        let mut ep_options = vec![];
        let mut range_start = 0;
        let mut range_end = combinations_without_ep;
        ep_options.push(EnpassantOption {
            square: None,
            pawn_idx_sntm: None,
            pawn_idx_stm: None,
            range_start,
            range_end,
        });
        range_start = range_end;
        for i in 0..pawns.len() {
            if pawns[i].side == EgtSide::SideNotToMove {
                for j in 0..pawns.len() {
                    if pawns[j].side == EgtSide::SideToMove {
                        let i_file = pawns[i].role.to_index();
                        let j_file = pawns[j].role.to_index();
                        if i_file == j_file + 1 || i_file + 1 == j_file {
                            // There are positions where a pawn on i_file can be captured en passant.
                            // Let's allocate an index range to encode them.

                            let istm = if i > 0 && pawns[i-1].role == pawns[i].role {
                                // There are also pawns of the side-to-move on the same
                                // file where en passant capture is possible.
                                Some(i-1)
                            } else {
                                None
                            };

                            let mut combinations = combinations_without_ep;
                            // For pawns (side-not-to-move) on the file
                            // where en passant capture is possible:
                            // - the pawn on 5th rank does not need to be encoded.
                            // - the remaining k-1 pawns cannot occupy the 5th, 6th
                            //   or 7th rank.
                            let k = pawns[i].multiplicity;
                            let n = pawns[i].n_squares;
                            combinations /= N_CHOOSE_K[k][n];
                            combinations *= N_CHOOSE_K[k-1][n-3];

                            // For pawns (side-to-move) on the file
                            // where en passant capture is possible:
                            // - the pawns cannot occupy the 5th, 6th
                            //   or 7th rank.
                            if let Some(x) = istm {
                                let k = pawns[x].multiplicity;
                                let n = pawns[x].n_squares;
                                combinations /= N_CHOOSE_K[k][n];
                                combinations *= N_CHOOSE_K[k][n-3];
                            }

                            range_end = range_start + combinations;
                            ep_options.push(EnpassantOption {
                                square: Some(Square::from_coords(File::new(i_file as u32), Rank::Fifth)),
                                pawn_idx_sntm: Some(i),
                                pawn_idx_stm: istm,
                                range_start,
                                range_end,
                            });
                            range_start = range_end;
                            break;
                        }
                    }
                }
            }
        }
        ep_options
    }

    // Encodes the position into an index, which represents the position up to symmetries.
    pub fn position_to_index(&self, scratch: &mut IndexerScratch, position: &Chess) -> usize {
        //println!("position_to_index: {}", position);
        //println!("en passant: {:?}", position.en_passant());
        let ep_pawn_square = position.legal_ep_square().map(|sq| Square::from_coords(sq.file(), Rank::Fifth));
        let index_offset = self.adjust_ep_from_position(scratch, ep_pawn_square);
        // Now `current_ep_option` reflects the en passant status of the position.
        // If there are pawns on the en passant file (different from the en passant
        // pawn on the 5th rank), they will be encoded with indexes in 0..3 instead of 0..6.

        self.position_to_coord(scratch, position);
        // Now `buffer_coord` has the coordinates (rank, file) of the pieces.
        //println!("buffer_coord: {:?}", scratch.buffer_coord);

        if self.n_pawns == 0 {
            self.reduce_symmetries(scratch);
            // Now the first item in `buffer_coord` has coordinates restricted to 10 squares.
            //println!("buffer_coord: {:?}", scratch.buffer_coord);
        }

        self.sort_coord_repeated_pieces(scratch);
        // Now `buffer_coord` has the sorted coordinates for repeated pieces.
        //println!("buffer_coord: {:?}", scratch.buffer_coord);

        self.coord_to_pidx(scratch);
        // Now `buffer_pidx` has the position indexes, with
        // non-overlapping values in [0..64, 0..64, 0..64, 0..64, ...].
        //println!("buffer_pidx: {:?}", scratch.buffer_pidx);

        Self::compact_pidx(&mut scratch.buffer_pidx);
        // Now `buffer_pidx` has compact positions indexes, with
        // values in [0..64, 0..63, 0..62, 0..61, ...].
        //println!("buffer_pidx: {:?}", scratch.buffer_pidx);

        if self.n_pawns > 0 {
            self.pawn_pidx_to_cpidx(scratch);
            // Now the first `n_pawns` elements of `buffer_pidx` have
            // the pawn positions encoded in 0..6 (or 0..3), so the value
            // ranges look like [0..6, 0..6, 0..62, 0..61, ...].
            // The positions of pawns on the same file are non-overlapping.
            //println!("buffer_pidx: {:?}", scratch.buffer_pidx);

            for i in 1..self.n_unique_pawns {
                if scratch.piece_set[i-1].role == scratch.piece_set[i].role {
                    let span = (scratch.piece_set[i-1].span.0, scratch.piece_set[i].span.1);
                    Self::compact_pidx(&mut scratch.buffer_pidx[span.0..span.1]);
                }
            }
            // Now the positions of pawns on the same file are compacted, so
            // the value ranges look like [0..6, 0..5, 0..62, 0..61, ...].
            //println!("buffer_pidx: {:?}", scratch.buffer_pidx);
        } else {
            self.map_kings(scratch);
            // Now the first element in `buffer_pidx` has values in 0..462
            // and encodes the position of both kings.
            //println!("buffer_pidx: {:?}", scratch.buffer_pidx);
        }

        // Now that we have all compact position indexes in `buffer_pidx`,
        // we can aggregate them into a single value.
        index_offset + self.cpidx_to_index(scratch)
    }

    // Decodes an index and recreates the corresponding position for this endgame (up to symmetries).
    // If the index represents an invalid position, returns None.
    pub fn position_from_index(&self, scratch: &mut IndexerScratch, index: usize, side_to_move: Color) -> Option<Chess> {
        //println!("position_from_index: {}", index);
        assert!(index < self.index_range);
        let index_offset = self.adjust_ep_from_index(scratch, index);
        // Now `current_ep_option` reflects the en passant status as encoded in the index.
        // If there are pawns on the en passant file (different from the en passant
        // pawn on the 5th rank), they will be decoded from indexes in 0..3 instead of 0..6.

        self.index_to_cpidx(scratch, index - index_offset);

        if self.n_pawns > 0 {
            // `buffer_pidx` has value ranges that look like [0..6, 0..5, 0..62, 0..61, ...].
            //println!("buffer_pidx: {:?}", scratch.buffer_pidx);
            for i in 1..self.n_unique_pawns {
                if scratch.piece_set[i-1].role == scratch.piece_set[i].role {
                    let span = (scratch.piece_set[i-1].span.0, scratch.piece_set[i].span.1);
                    Self::uncompact_cpidx(&mut scratch.buffer_pidx[span.0..span.1]);
                }
            }
            // Now the positions of pawns on the same file are uncompacted, so
            // the value ranges look like [0..6, 0..6, 0..62, 0..61, ...].
            //println!("buffer_pidx: {:?}", scratch.buffer_pidx);

            self.pawn_cpidx_to_pidx(scratch);
            Self::compact_pidx(&mut scratch.buffer_pidx[0..self.n_pawns]);
            // Now the first `n_pawns` elements of buffer_coord are correct, and `buffer_pidx`
            // has values in [0..64, 0..63, 0..62, 0..61, ...].
            //println!("buffer_pidx: {:?}", scratch.buffer_pidx);
        } else {
            // `buffer_pidx` has values in [0..462, 0..1, 0..62, 0..61, ...].
            //println!("buffer_pidx: {:?}", scratch.buffer_pidx);
            self.unmap_kings(scratch);
            // Now `buffer_pidx` has values in [0..64, 0..63, 0..62, 0..61, ...].
            //println!("buffer_pidx: {:?}", scratch.buffer_pidx);
        }

        Self::uncompact_cpidx(&mut scratch.buffer_pidx);
        // Now `buffer_pidx` has the positions indexes, with
        // non-overlapping values in [0..64, 0..64, 0..64, 0..64, ...].
        //println!("buffer_pidx: {:?}", scratch.buffer_pidx);

        self.nonpawn_pidx_to_coord(scratch);
        // Now the non-pawn part of `buffer_coord` is correct.
        //println!("buffer_coord: {:?}", scratch.buffer_coord);

        // All coordinates are in `buffer_coord`, we can place the pieces to create a new position.
        self.coord_to_position(scratch, side_to_move)
    }

    // Checks whether the position allows en passant and adjusts the internal
    // setup to encode it. Returns the index offset to apply to the encoded value.
    fn adjust_ep_from_position(&self, scratch: &mut IndexerScratch, ep_square: Option<Square>) -> usize {
        if scratch.current_ep_option.square != ep_square {
            self.unapply_ep_option(scratch);
            scratch.current_ep_option = *self.ep_options.iter().find(|opt| opt.square == ep_square).unwrap();
            self.apply_ep_option(scratch);
        }
        scratch.current_ep_option.range_start
    }

    // Checks to which en passant option this index corresponds and adjusts the
    // internal setup to decode it. Returns the index offset to apply to decode the value.
    fn adjust_ep_from_index(&self, scratch: &mut IndexerScratch, index: usize) -> usize {
        let new_ep_option = *self.ep_options.iter().find(|ep| index < ep.range_end).unwrap();
        if scratch.current_ep_option.square != new_ep_option.square {
            self.unapply_ep_option(scratch);
            scratch.current_ep_option = new_ep_option;
            self.apply_ep_option(scratch);
        }
        scratch.current_ep_option.range_start
    }

    // Pawnless positions are reduced to a canonical form through rotations and reflections
    // to take advantage of symmetries, so normally each index corresponds to 8 equivalent
    // position. However, if both kings are on a long diagonal, the diagonal symmetry is not
    // used and the correspoding index represents only 4 equivalent positions.
    // In particular, positions with the kings on the diagonal and some other piece outside
    // the diagonal have a symmetric that is in principle equivalent, but is assigned a
    // separate index.
    // `diagonal_symmetric(idx)` returns:
    //  - `None` if the corresponding position does not have both kings on a long diagonal
    //    (i.e. `idx` represents 8 equivalent positions)
    //  - `Some(other_idx)` if the corresponding position has both kings on a long diagonal
    //    (i.e. `idx` represents 4 equivalent positions), with `other_idx` being the index
    //    of the symmetrical position along the diagonal. `other_idx` will be equal to `idx`
    //    for positions with all pieces on the diagonal.
    pub fn diagonal_symmetric(&self, scratch: &mut IndexerScratch, index: usize) -> Option<usize> {
        if self.n_pawns != 0 {
            return None;
        }

        self.index_to_cpidx(scratch, index);
        self.unmap_kings(scratch);
        Self::uncompact_cpidx(&mut scratch.buffer_pidx);
        self.nonpawn_pidx_to_coord(scratch);

        let kings_on_diagonal = scratch.buffer_coord[0..2].iter().all(|(r, f)| *r == *f);
        if kings_on_diagonal {
            // Swap coordinates and recode
            for p in scratch.buffer_coord.iter_mut() { *p = (p.1, p.0); };
            self.sort_coord_repeated_pieces(scratch);
            self.coord_to_pidx(scratch);
            Self::compact_pidx(&mut scratch.buffer_pidx);
            self.map_kings(scratch);
            Some(self.cpidx_to_index(scratch))
        } else {
            None
        }


    }

    fn apply_ep_option(&self, scratch: &mut IndexerScratch) {
        if let Some(i) = scratch.current_ep_option.pawn_idx_sntm {
            // For pawns (side-not-to-move) on the file
            // where en passant capture is possible:
            // - the pawn on 5th rank does not need to be encoded.
            // - the remaining k-1 pawns cannot occupy the 5th, 6th
            //   or 7th rank.
            scratch.piece_set[i].multiplicity -= 1;
            scratch.piece_set[i].n_squares -= 3;
            scratch.piece_set[i].combinations = N_CHOOSE_K[scratch.piece_set[i].multiplicity][scratch.piece_set[i].n_squares];
        }
        if let Some(i) = scratch.current_ep_option.pawn_idx_stm {
            // For pawns (side-to-move) on the file
            // where en passant capture is possible:
            // - the pawns cannot occupy the 5th, 6th
            //   or 7th rank.
            scratch.piece_set[i].n_squares -= 3;
            scratch.piece_set[i].combinations = N_CHOOSE_K[scratch.piece_set[i].multiplicity][scratch.piece_set[i].n_squares];
        }
    }

    fn unapply_ep_option(&self, scratch: &mut IndexerScratch) {
        if let Some(i) = scratch.current_ep_option.pawn_idx_sntm {
            scratch.piece_set[i].multiplicity += 1;
            scratch.piece_set[i].n_squares += 3;
            scratch.piece_set[i].combinations = N_CHOOSE_K[scratch.piece_set[i].multiplicity][scratch.piece_set[i].n_squares];
        }
        if let Some(i) = scratch.current_ep_option.pawn_idx_stm {
            scratch.piece_set[i].n_squares += 3;
            scratch.piece_set[i].combinations = N_CHOOSE_K[scratch.piece_set[i].multiplicity][scratch.piece_set[i].n_squares];
        }
    }

    // Extracts the coordinates of the pieces from the position and stores them in `buffer_coord`.
    fn position_to_coord(&self, scratch: &mut IndexerScratch, position: &Chess) {
        assert_eq!(position.board().occupied().count(), self.n_pieces, "position_to_coord() called with a position that does not match the piece set");
        for p in &scratch.piece_set {
            let mut bb = match p.role {
                EgtRole::Pawn(f) => position.board().by_role(Role::Pawn) & shakmaty::Bitboard::from_file(f),
                EgtRole::King => position.board().by_role(Role::King),
                EgtRole::Queen => position.board().by_role(Role::Queen),
                EgtRole::Rook => position.board().by_role(Role::Rook),
                EgtRole::Bishop => position.board().by_role(Role::Bishop),
                EgtRole::Knight => position.board().by_role(Role::Knight),
            };
            bb &= match p.side {
                EgtSide::SideToMove => position.board().by_color(position.turn()),
                EgtSide::SideNotToMove => position.board().by_color(!position.turn()),
            };
            assert_eq!(bb.count(), p.span.1 - p.span.0, "position_to_coord() called with a position that does not match the piece set");
            for (square, i) in bb.into_iter().zip(p.span.0..p.span.1) {
                scratch.buffer_coord[i] = (square.rank().to_usize(), square.file().to_usize());
            }
        }

        if self.n_pawns > 0 && position.turn() == Color::Black {
            // Switch the perspective so that the ranks are encoded from the
            // point of view of the side to move.
            for i in 0..self.n_pieces {
                scratch.buffer_coord[i].0 = 7 - scratch.buffer_coord[i].0;
            }
        }
    }

    // Builds a position using the coordinates stored in `buffer_coord`.
    fn coord_to_position(&self, scratch: &mut IndexerScratch, side_to_move: Color) -> Option<Chess> {
        if self.n_pawns > 0 && side_to_move == Color::Black {
            // The ranks are encoded from the point of view of the side to move. Switch them up.
            for i in 0..self.n_pieces {
                scratch.buffer_coord[i].0 = 7 - scratch.buffer_coord[i].0;
            }
        }

        let mut setup = Setup::empty();
        setup.turn = side_to_move;
        for p in &scratch.piece_set {
            let role = p.role.to_role();
            let color = match p.side {
                EgtSide::SideToMove => side_to_move,
                EgtSide::SideNotToMove => !side_to_move,
            };
            let piece = Piece { color, role };
            for i in p.span.0..p.span.1 {
                let (r, f) = scratch.buffer_coord[i];
                let square = Square::from_coords(File::new(f as u32), Rank::new(r as u32));
                setup.board.set_piece_at(square, piece);
            }
        }

        let ep_square = if let Some(square) = scratch.current_ep_option.square {
            let target_rank = match side_to_move {
                Color::White => Rank::Sixth,
                Color::Black => Rank::Third,
            };
            Some(Square::from_coords(square.file(), target_rank))
        } else {
            None
        };
        setup.ep_square = ep_square;

        if let Ok(position) = setup.position::<Chess>(CastlingMode::Standard) {
            if position.legal_ep_square() == ep_square {
                Some(position)
            } else {
                None
            }
        } else {
            None
        }
    }

    // For pawnless positions, this function applies symmetrical
    // transformations of the coordinates that keep the position unchanged.
    fn reduce_symmetries(&self, scratch: &mut IndexerScratch) {
        if scratch.buffer_coord[0].0 > 3 {
            // flip around horizontal axis
            for (r, _) in scratch.buffer_coord.iter_mut() { *r = 7 - *r; };
        }
        if scratch.buffer_coord[0].1 > 3 {
            // flip around vertical axis
            for (_, f) in scratch.buffer_coord.iter_mut() { *f = 7 - *f; };
        }
        if scratch.buffer_coord[0].0 > scratch.buffer_coord[0].1 ||
            (scratch.buffer_coord[0].0 == scratch.buffer_coord[0].1 &&
            scratch.buffer_coord[1].0 > scratch.buffer_coord[1].1) {
            // flip diagonally. If both kings are on the diagonal, we intentionally do not
            // diagonal-reflect based on the remaining pieces.
            for p in scratch.buffer_coord.iter_mut() { *p = (p.1, p.0); };
        }
    }

    // Sort coordinates for repeated pieces from highest to lowest.
    fn sort_coord_repeated_pieces(&self, scratch: &mut IndexerScratch) {
        for p in &scratch.piece_set {
            if p.span.1 - p.span.0 > 1 {
                scratch.buffer_coord[p.span.0..p.span.1].sort_by_key(|v| Reverse(*v));
            }
        }
    }

    // Converts the piece coordinates to indexes in 0..64.
    fn coord_to_pidx(&self, scratch: &mut IndexerScratch) {
        for i in 0..self.n_pieces {
            scratch.buffer_pidx[i] = scratch.buffer_coord[i].0 * 8 + scratch.buffer_coord[i].1;
        }
    }

    // Converts indexes in 0..64 to piece coordinates for non-pawns.
    fn nonpawn_pidx_to_coord(&self, scratch: &mut IndexerScratch) {
        for i in self.n_pawns..self.n_pieces {
            scratch.buffer_coord[i] = (scratch.buffer_pidx[i] / 8, scratch.buffer_pidx[i] % 8);
        }
    }

    // Compact non-overlapping indexes in ranges 0..a, 0..b, 0..c, ...
    // to (possibly overlapping) indexed in ranges 0..a, 0..b-1, 0..c-2, ....
    // Note that values for repeated pieces are sorted from highest to
    // lowest, so they remain non-overlapping.
    fn compact_pidx(v: &mut[usize]) {
        let n = v.len();
        for i in 0..n-1 {
            for j in i+1..n {
                if v[i] < v[j] { v[j] -= 1; }
            }
        }
    }

    // Undo `compact_pidx`.
    fn uncompact_cpidx(v: &mut[usize]) {
        let n = v.len();
        for i in (0..n-1).rev() {
            for j in i+1..n {
                if v[i] <= v[j] { v[j] += 1; }
            }
        }
    }

    // Converts pawn indexes (pidx) to compact position indexes (cpidx) in 0..6,
    // where 0 indicates that the pawn is on the 2nd rank, 5 indicates that
    // the pawn is on the 7th rank.
    fn pawn_pidx_to_cpidx(&self, scratch: &mut IndexerScratch) {
        for p in &scratch.piece_set[0..self.n_unique_pawns] {
            for i in p.span.0..p.span.1 {
                scratch.buffer_pidx[i] = scratch.buffer_coord[i].0 - 1;
            }
        }
    }

    // Converts compact position indexes (cpidx) for pawns (in 0..6) to indexes
    // in 0..64, and converts these indexes to piece coordinates.
    fn pawn_cpidx_to_pidx(&self, scratch: &mut IndexerScratch) {
        for p in &scratch.piece_set[0..self.n_unique_pawns] {
            for i in p.span.0..p.span.1 {
                scratch.buffer_coord[i] = (scratch.buffer_pidx[i] + 1, p.role.to_index());
                scratch.buffer_pidx[i] = scratch.buffer_coord[i].0 * 8 + scratch.buffer_coord[i].1;
            }
        }
    }

    fn map_kings(&self, scratch: &mut IndexerScratch) {
        scratch.buffer_pidx[0] = self.kings_map_to_index[scratch.buffer_pidx[0]][scratch.buffer_pidx[1]];
        scratch.buffer_pidx[1] = 0;
    }

    fn unmap_kings(&self, scratch: &mut IndexerScratch) {
        (scratch.buffer_pidx[0], scratch.buffer_pidx[1]) = self.kings_map_from_index[scratch.buffer_pidx[0]];
    }

    // Computes the final index representing the position, by aggregating
    // the values of the compact position indexes (cpidx) into a single number.
    fn cpidx_to_index(&self, scratch: &IndexerScratch) -> usize {
        let mut index = 0;
        for p in &scratch.piece_set {
            index *= p.combinations;
            let mut i = p.span.1;
            for k in 1..=p.multiplicity {
                i -= 1;
                assert!(scratch.buffer_pidx[i] < p.n_squares); // valid cpidx
                if k == 1 {
                    index += scratch.buffer_pidx[i];
                } else {
                    assert!(scratch.buffer_pidx[i] > scratch.buffer_pidx[i+1]); // decreasing cpidx
                    index += N_CHOOSE_K[k][scratch.buffer_pidx[i]];
                }
            }
        }
        index
    }

    // Reads the index representing the piece configuration and writes the
    // corresponding compact position indexes (cpidx) to buffer_pidx:
    // - pawn positions encoded with an index in 0..6
    // - non-pawn positions encoded with an index representing the square to
    //   occupy among the set of non-already-occupied squares.
    fn index_to_cpidx(&self, scratch: &mut IndexerScratch, index: usize) {
        let mut v = index;
        for p in scratch.piece_set.iter().rev() {
            let mut x = v % p.combinations;
            v /= p.combinations;
            let mut n = p.n_squares;
            if p.span.1 - p.span.0 > p.multiplicity {
                // There is a non-encoded en passant pawn.
                scratch.buffer_pidx[p.span.0] = p.n_squares;
            }
            for k in (1..=p.multiplicity).rev() {
                if k == 1 {
                    scratch.buffer_pidx[p.span.1-1] = x;
                } else {
                    // Search for the largest n such that N_CHOOSE_K[k][n] <= x
                    let row = &N_CHOOSE_K[k][0..n];
                    n = match row.binary_search(&(x + 1)) {
                        Ok(found_index) => found_index - 1,
                        Err(insert_index) => insert_index - 1,
                    };
                    x -= N_CHOOSE_K[k][n];
                    scratch.buffer_pidx[p.span.1-k] = n;
                }
            }
        }
        assert_eq!(v, 0);
    }
}
