use shakmaty::{Color, File, Piece, Rank, Role, Square, Setup, CastlingMode, Chess, FromSetup};
use crate::piece_set::{EgtPiece, EgtSide};
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

// Initialize map to encode the possible 462 kings positions, considering symmetries.
fn initialize_kings_map() -> (Vec<(usize, usize)>, Vec<Vec<usize>>) {
    let mut m = vec![(0,0); 462]; // from index to kings positions
    let mut r = vec![vec![999; 64]; 28]; // from kings positions to index
    let mut i = 0;
    let mut wk;
    wk = 0; for bk in (1..7).chain(9..15).chain(17..23).chain(26..31).chain(35..39).chain(44..47).chain(53..55).chain(62..63) { m[i] = (wk, bk); r[wk][bk] = i; i += 1; }
    wk = 1; for bk in (2..7).chain(10..63) { m[i] = (wk, bk); r[wk][bk] = i; i += 1; }
    wk = 2; for bk in (0..1).chain(3..8).chain(11..63) { m[i] = (wk, bk); r[wk][bk] = i; i += 1; }
    wk = 3; for bk in (0..2).chain(4..9).chain(12..63) { m[i] = (wk, bk); r[wk][bk] = i; i += 1; }
    wk = 9; for bk in (3..8).chain(10..15).chain(18..23).chain(26..31).chain(35..39).chain(44..47).chain(53..55).chain(62..63) { m[i] = (wk, bk); r[wk][bk] = i; i += 1; }
    wk = 10; for bk in (0..1).chain(4..9).chain(11..16).chain(19..63) { m[i] = (wk, bk); r[wk][bk] = i; i += 1; }
    wk = 11; for bk in (0..2).chain(5..10).chain(12..17).chain(20..63) { m[i] = (wk, bk); r[wk][bk] = i; i += 1; }
    wk = 18; for bk in (0..8).chain(12..16).chain(19..23).chain(27..31).chain(35..39).chain(44..47).chain(53..55).chain(62..63) { m[i] = (wk, bk); r[wk][bk] = i; i += 1; }
    wk = 19; for bk in (0..10).chain(13..18).chain(20..25).chain(28..63) { m[i] = (wk, bk); r[wk][bk] = i; i += 1; }
    wk = 27; for bk in (0..8).chain(9..16).chain(21..24).chain(28..31).chain(36..39).chain(44..47).chain(53..55).chain(62..63) { m[i] = (wk, bk); r[wk][bk] = i; i += 1; }
    assert_eq!(i, 462);
    (m, r)
}

// An element of the set of pieces appearing in an endgame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct PieceSetElement {
    // The piece appearing in the endgame configuration.
    piece: EgtPiece,

    // Whether the piece is for the side-to-move or the side-not-to-move.
    side: EgtSide,

    // How many instances of this piece appear in the endgame.
    multiplicity: usize,

    // The number of squares that these pieces can occupy.
    n_squares: usize,

    // The number of ways to put the pieces on the available squares.
    // Equal to N_CHOOSE_K[multiplicity][n_squares].
    combinations: usize,

    // The indexes (begin, end) where information about these pieces is
    // stored in the buffer vectors. Normally
    //   span.1 - span.0 = multiplicity,
    // unless we are encoding en passant positions and one of the pieces
    // here is a pawn that can be captured en passant. In that case the
    // multplicity is reduced by one since the pawn that can be captured
    // en passant is not encoded explicitly.
    span: (usize, usize),
}

// Positions where en passant is possible are encoded separately from positions
// where en passant is not possible. One instance of this struct represents one
// option for the en passant status of positions, together with the range of
// indexes associated to these positions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EnpassantOption {
    // The en passant square if en passant is possible, or None.
    square: Option<Square>,

    // The index of the pawns (side-not-to-move) on the file where the
    // en passant capture is possible.
    pawn_idx_sntm: Option<usize>,

    // The index of the pawns (side-to-move) on the file where the
    // en passant capture is possible.
    pawn_idx_stm: Option<usize>,

    // The start of the range used to encode positions with this en passant status.
    range_start: usize,

    // The end of the range used to encode positions with this en passant status.
    range_end: usize,
}

// Helper object taking care of the efficient conversion from board
// positions to compact indexes representing the positions, and viceversa.
#[derive(Clone, Debug)]
pub struct Indexer {
    // The set of pieces appearing in this endgame table.
    piece_set: Vec<PieceSetElement>,

    // The number of pieces in the endgame.
    pub n_pieces: usize,

    // The number of unique pieces in the endgame.
    // Non-pawn pieces of the same color and type count as one unique piece.
    // Pawns of the same color on the same file count as one unique piece.
    pub n_unique_pieces: usize,

    // The number of pawns in the endgame.
    pub n_pawns: usize,

    // The number of unique pawns in the endgame.
    // Pawns of the same color on the same file count as one unique piece.
    pub n_unique_pawns: usize,

    // The total number of positions indexed in this endgame table.
    pub index_range: usize,

    // The following fields provide information on the en passant status.
    // They are adjusted based on the position that is being encoded/decoded.
    ep_options: Vec<EnpassantOption>,
    current_ep_option: EnpassantOption,

    // Buffers used to encode/decode positions.
    buffer_pos: Vec<(usize, usize)>,
    buffer_pidx: Vec<usize>,

    // Precomputed maps for encoding kings positions.
    kings_map_from_index: Vec<(usize, usize)>,
    kings_map_to_index: Vec<Vec<usize>>,
}

impl Indexer {
    // Initialize the indexer with the given set of pieces (type, side, multiplicity).
    pub fn from_pieces(pieces: &[(EgtPiece, EgtSide, usize)]) -> Result<Self, ()> {
        // Setup the piece set elements.
        let mut piece_set: Vec<_> = pieces.iter().map(|p| PieceSetElement{
            piece: p.0,
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

        let n_unique_pieces = piece_set.len();
        let n_pawns = piece_set.iter().filter(|p| p.piece.is_pawn()).map(|p| p.multiplicity).sum();
        let n_unique_pawns = piece_set.iter().filter(|p| p.piece.is_pawn()).count();

        // Compute n_squares, combinations, span.
        Self::compute_piece_squares(&mut piece_set);

        let combinations_without_ep = piece_set.iter().map(|p| p.combinations).product();

        // Check en passant options.
        let ep_options = Self::setup_ep_options(&mut piece_set[0..n_unique_pawns], combinations_without_ep);
        let current_ep_option = ep_options[0];

        //println!("pieces: {:?}", &piece_set);
        //println!("ep_options: {:?}", &ep_options);

        let index_range = ep_options.last().unwrap().range_end;

        let buffer_pos = vec![(0, 0); n_pieces];
        let buffer_pidx = vec![0; n_pieces];

        let (kings_map_from_index, kings_map_to_index) = initialize_kings_map();

        Ok(Indexer {
            piece_set,
            n_pieces,
            n_unique_pieces,
            n_pawns,
            n_unique_pawns,
            index_range,
            ep_options,
            current_ep_option,
            buffer_pos,
            buffer_pidx,
            kings_map_from_index,
            kings_map_to_index,
        })
    }

    fn compute_piece_squares(piece_set: &mut[PieceSetElement]) {
        let mut available_pawn_squares = [6; 8];
        let mut available_squares = 64;
        let mut pawnless = true;
        let mut span_end = 0;
        for p in piece_set.iter_mut() {
            let k = p.multiplicity;
            if p.piece.is_pawn() {
                pawnless = false;
                p.n_squares = available_pawn_squares[p.piece.to_index()];
                assert!(p.n_squares >= k);
                available_pawn_squares[p.piece.to_index()] -= k;
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
            // Encode the position of both kings using a single index in 0..462.
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
                        let i_file = pawns[i].piece.to_index();
                        let j_file = pawns[j].piece.to_index();
                        if i_file == j_file + 1 || i_file + 1 == j_file {
                            // There are positions where a pawn on i_file can be captured en passant.
                            // Let's allocate an index range to encode them.

                            let istm = if i > 0 && pawns[i-1].piece == pawns[i].piece {
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

    // Encodes the status of a board into an index, which represents the positions
    // of the pieces (up to symmetries).
    pub fn board_to_index(&mut self, board: &Setup) -> usize {
        //println!("board_to_index: {}", board);
        //println!("en passant: {:?}", board.en_passant());
        let ep_pawn_square = board.ep_square.map(|sq| Square::from_coords(sq.file(), Rank::Fifth));
        let index_offset = self.adjust_ep_from_board(ep_pawn_square);
        // Now `current_ep_option` reflects the en passant status of the board.
        // If there are pawns on the en passant file (different from the en passant
        // pawn on the 5th rank), they will be encoded with indexes in 0..3 instead of 0..6.

        self.board_to_pos(board);
        // Now `buffer_pos` has the coordinates (rank, file) of the pieces.
        //println!("buffer_pos: {:?}", self.buffer_pos);

        if self.n_pawns == 0 {
            self.reduce_symmetries();
            // Now the first item in `buffer_pos` has coordinates restricted to 10 squares.
            //println!("buffer_pos: {:?}", self.buffer_pos);
        }

        self.sort_pos_repeated_pieces();
        // Now `buffer_pos` has the sorted coordinates for repeated pieces.
        //println!("buffer_pos: {:?}", self.buffer_pos);

        self.pos_to_pidx();
        // Now `buffer_pidx` has the position indexes, with
        // non-overlapping values in [0..64, 0..64, 0..64, 0..64, ...].
        //println!("buffer_pidx: {:?}", self.buffer_pidx);

        Self::compact_pidx(&mut self.buffer_pidx);
        // Now `buffer_pidx` has compact positions indexes, with
        // values in [0..64, 0..63, 0..62, 0..61, ...].
        //println!("buffer_pidx: {:?}", self.buffer_pidx);

        if self.n_pawns > 0 {
            self.pawn_pos_to_cpidx();
            // Now the first `n_pawns` elements of `buffer_pidx` have
            // the pawn positions encoded in 0..6 (or 0..3), so the value
            // ranges look like [0..6, 0..6, 0..62, 0..61, ...].
            // The positions of pawns on the same file are non-overlapping.
            //println!("buffer_pidx: {:?}", self.buffer_pidx);

            for i in 1..self.n_unique_pawns {
                if self.piece_set[i-1].piece == self.piece_set[i].piece {
                    let span = (self.piece_set[i-1].span.0, self.piece_set[i].span.1);
                    Self::compact_pidx(&mut self.buffer_pidx[span.0..span.1]);
                }
            }
            // Now the positions of pawns on the same file are compacted, so
            // the value ranges look like [0..6, 0..5, 0..62, 0..61, ...].
            //println!("buffer_pidx: {:?}", self.buffer_pidx);
        } else {
            self.map_kings();
            // Now the first element in `buffer_pidx` has values in 0..462
            // and encodes the position of both kings.
            //println!("buffer_pidx: {:?}", self.buffer_pidx);
        }

        // Now that we have all compact position indexes in `buffer_pidx`,
        // we can aggregate them into a single value.
        index_offset + self.cpidx_to_index()
    }

    // Decodes an index and recreates a board with the corresponding positions of the
    // pieces in this endgame (up to symmetries). If the index represents an invalid
    // position, returns None.
    pub fn board_from_index(&mut self, index: usize, side_to_move: Color) -> Option<Setup> {
        //println!("board_from_index: {}", index);
        assert!(index < self.index_range);
        let index_offset = self.adjust_ep_from_index(index);
        // Now `current_ep_option` reflects the en passant status as encoded in the index.
        // If there are pawns on the en passant file (different from the en passant
        // pawn on the 5th rank), they will be decoded from indexes in 0..3 instead of 0..6.

        self.index_to_cpidx(index - index_offset);

        if self.n_pawns > 0 {
            // `buffer_pidx` has value ranges that look like [0..6, 0..5, 0..62, 0..61, ...].
            //println!("buffer_pidx: {:?}", self.buffer_pidx);
            for i in 1..self.n_unique_pawns {
                if self.piece_set[i-1].piece == self.piece_set[i].piece {
                    let span = (self.piece_set[i-1].span.0, self.piece_set[i].span.1);
                    Self::uncompact_cpidx(&mut self.buffer_pidx[span.0..span.1]);
                }
            }
            // Now the positions of pawns on the same file are uncompacted, so
            // the value ranges look like [0..6, 0..6, 0..62, 0..61, ...].
            //println!("buffer_pidx: {:?}", self.buffer_pidx);

            self.pawn_cpidx_to_pos();
            Self::compact_pidx(&mut self.buffer_pidx[0..self.n_pawns]);
            // Now the first `n_pawns` elements of buffer_pos are correct, and `buffer_pidx`
            // has values in [0..64, 0..63, 0..62, 0..61, ...].
            //println!("buffer_pidx: {:?}", self.buffer_pidx);
        } else {
            // `buffer_pidx` has values in [0..462, 0..1, 0..62, 0..61, ...].
            //println!("buffer_pidx: {:?}", self.buffer_pidx);
            self.unmap_kings();
            // Now `buffer_pidx` has values in [0..64, 0..63, 0..62, 0..61, ...].
            //println!("buffer_pidx: {:?}", self.buffer_pidx);
        }

        Self::uncompact_cpidx(&mut self.buffer_pidx);
        // Now `buffer_pidx` has the positions indexes, with
        // non-overlapping values in [0..64, 0..64, 0..64, 0..64, ...].
        //println!("buffer_pidx: {:?}", self.buffer_pidx);

        self.nonpawn_pidx_to_pos();
        // Now the non-pawn part of `buffer_pos` is correct.
        //println!("buffer_pos: {:?}", self.buffer_pos);

        // All positions are in `buffer_pos`, we can place the pieces on a new board.
        self.pos_to_board(side_to_move)
    }

    // Checks whether the position allows en passant and adjusts the internal
    // setup to encode it. Returns the index offset to apply to the encoded value.
    fn adjust_ep_from_board(&mut self, ep_square: Option<Square>) -> usize {
        if self.current_ep_option.square != ep_square {
            self.unapply_ep_option();
            self.current_ep_option = *self.ep_options.iter().find(|opt| opt.square == ep_square).unwrap();
            self.apply_ep_option();
        }
        self.current_ep_option.range_start
    }

    // Checks to which en passant option this index corresponds and adjusts the
    // internal setup to decode it. Returns the index offset to apply to decode the value.
    fn adjust_ep_from_index(&mut self, index: usize) -> usize {
        let new_ep_option = *self.ep_options.iter().find(|ep| index < ep.range_end).unwrap();
        if self.current_ep_option.square != new_ep_option.square {
            self.unapply_ep_option();
            self.current_ep_option = new_ep_option;
            self.apply_ep_option();
        }
        self.current_ep_option.range_start
    }

    fn apply_ep_option(&mut self) {
        if let Some(i) = self.current_ep_option.pawn_idx_sntm {
            // For pawns (side-not-to-move) on the file
            // where en passant capture is possible:
            // - the pawn on 5th rank does not need to be encoded.
            // - the remaining k-1 pawns cannot occupy the 5th, 6th
            //   or 7th rank.
            self.piece_set[i].multiplicity -= 1;
            self.piece_set[i].n_squares -= 3;
            self.piece_set[i].combinations = N_CHOOSE_K[self.piece_set[i].multiplicity][self.piece_set[i].n_squares];
        }
        if let Some(i) = self.current_ep_option.pawn_idx_stm {
            // For pawns (side-to-move) on the file
            // where en passant capture is possible:
            // - the pawns cannot occupy the 5th, 6th
            //   or 7th rank.
            self.piece_set[i].n_squares -= 3;
            self.piece_set[i].combinations = N_CHOOSE_K[self.piece_set[i].multiplicity][self.piece_set[i].n_squares];
        }
    }

    fn unapply_ep_option(&mut self) {
        if let Some(i) = self.current_ep_option.pawn_idx_sntm {
            self.piece_set[i].multiplicity += 1;
            self.piece_set[i].n_squares += 3;
            self.piece_set[i].combinations = N_CHOOSE_K[self.piece_set[i].multiplicity][self.piece_set[i].n_squares];
        }
        if let Some(i) = self.current_ep_option.pawn_idx_stm {
            self.piece_set[i].n_squares += 3;
            self.piece_set[i].combinations = N_CHOOSE_K[self.piece_set[i].multiplicity][self.piece_set[i].n_squares];
        }
    }

    // Extracts the coordinates of the pieces from the board and stores them in `buffer_pos`.
    fn board_to_pos(&mut self, board: &Setup) {
        assert_eq!(board.board.occupied().count(), self.n_pieces, "board_to_pos() called with a board that does not match the piece set");
        for p in &self.piece_set {
            let mut bb = match p.piece {
                EgtPiece::Pawn(f) => board.board.by_role(Role::Pawn) & shakmaty::Bitboard::from_file(f),
                EgtPiece::King => board.board.by_role(Role::King),
                EgtPiece::Queen => board.board.by_role(Role::Queen),
                EgtPiece::Rook => board.board.by_role(Role::Rook),
                EgtPiece::Bishop => board.board.by_role(Role::Bishop),
                EgtPiece::Knight => board.board.by_role(Role::Knight),
            };
            bb &= match p.side {
                EgtSide::SideToMove => board.board.by_color(board.turn),
                EgtSide::SideNotToMove => board.board.by_color(!board.turn),
            };
            assert_eq!(bb.count(), p.span.1 - p.span.0, "board_to_pos() called with a board that does not match the piece set");
            for (square, i) in bb.into_iter().zip(p.span.0..p.span.1) {
                self.buffer_pos[i] = (square.rank().to_usize(), square.file().to_usize());
            }
        }

        if self.n_pawns > 0 && board.turn == Color::Black {
            // Switch the perspective so that the ranks are encoded from the
            // point of view of the side to move.
            for i in 0..self.n_pieces {
                self.buffer_pos[i].0 = 7 - self.buffer_pos[i].0;
            }
        }
    }

    // Builds a board using the coordinates stored in `buffer_pos`.
    fn pos_to_board(&mut self, side_to_move: Color) -> Option<Setup> {
        if self.n_pawns > 0 && side_to_move == Color::Black {
            // The ranks are encoded from the point of view of the side to move. Switch them up.
            for i in 0..self.n_pieces {
                self.buffer_pos[i].0 = 7 - self.buffer_pos[i].0;
            }
        }

        let mut setup = Setup::empty();
        setup.turn = side_to_move;
        for p in &self.piece_set {
            let role = p.piece.to_role();
            let color = match p.side {
                EgtSide::SideToMove => side_to_move,
                EgtSide::SideNotToMove => !side_to_move,
            };
            let piece = Piece { color, role };
            for i in p.span.0..p.span.1 {
                let (r, f) = self.buffer_pos[i];
                let square = Square::from_coords(File::new(f as u32), Rank::new(r as u32));
                setup.board.set_piece_at(square, piece);
            }
        }

        if let Some(square) = self.current_ep_option.square {
            let target_rank = match side_to_move {
                Color::White => Rank::Sixth,
                Color::Black => Rank::Third,
            };
            let target_square = Square::from_coords(square.file(), target_rank);
            setup.ep_square = Some(target_square);
        }

        if Chess::from_setup(setup.clone(), CastlingMode::Standard).is_ok() {
            Some(setup)
        } else {
            None
        }
    }

    // For pawnless positions, this function applies symmetrical
    // transformations of the coordinates that keep the position unchanged.
    fn reduce_symmetries(&mut self) {
        if self.buffer_pos[0].0 > 3 {
            // flip around horizontal axis
            for (r, _) in self.buffer_pos.iter_mut() { *r = 7 - *r; };
        }
        if self.buffer_pos[0].1 > 3 {
            // flip around vertical axis
            for (_, f) in self.buffer_pos.iter_mut() { *f = 7 - *f; };
        }
        if self.buffer_pos[0].0 > self.buffer_pos[0].1 ||
            (self.buffer_pos[0].0 == self.buffer_pos[0].1 &&
            self.buffer_pos[1].0 > self.buffer_pos[1].1) {
            // flip diagonally
            for p in self.buffer_pos.iter_mut() { *p = (p.1, p.0); };
        }

        // Note that when the kings are both on the diagonal, there are
        // pairs of positions that are symmetrical but are encoded using
        // different indexes. This is a small inefficiency but resolving
        // it adds a bit of complexity which we avoid. To properly resolve
        // the symmetry taking into account piece multiplicity, the
        // correct proedure would be to:
        // - Take the diagonal reflection of the position
        // - Re-sort the repeated pieces into a standard order
        // - Compare with the initial position and select one of the two
        //   (e.g. the lowest) as the canonical position.
    }

    // Sort coordinates for repeated pieces from highest to lowest.
    fn sort_pos_repeated_pieces(&mut self) {
        for p in &self.piece_set {
            if p.span.1 - p.span.0 > 1 {
                self.buffer_pos[p.span.0..p.span.1].sort_by_key(|v| Reverse(*v));
            }
        }
    }

    // Converts the piece coordinates to indexes in 0..64.
    fn pos_to_pidx(&mut self) {
        for i in 0..self.n_pieces {
            self.buffer_pidx[i] = self.buffer_pos[i].0 * 8 + self.buffer_pos[i].1;
        }
    }

    // Converts indexes in 0..64 to piece coordinates for non-pawns.
    fn nonpawn_pidx_to_pos(&mut self) {
        for i in self.n_pawns..self.n_pieces {
            self.buffer_pos[i] = (self.buffer_pidx[i] / 8, self.buffer_pidx[i] % 8);
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

    // Converts pawn positions to compact position indexes (cpidx) in 0..6,
    // where 0 indicates that the pawn is on the 2nd rank, 5 indicates that
    // the pawn is on the 7th rank.
    fn pawn_pos_to_cpidx(&mut self) {
        for p in &self.piece_set[0..self.n_unique_pawns] {
            for i in p.span.0..p.span.1 {
                self.buffer_pidx[i] = self.buffer_pos[i].0 - 1;
            }
        }
    }

    // Converts compact position indexes (cpidx) for pawns (in 0..6) to indexes
    // in 0..64, and converts these indexes to piece coordinates.
    fn pawn_cpidx_to_pos(&mut self) {
        for p in &self.piece_set[0..self.n_unique_pawns] {
            for i in p.span.0..p.span.1 {
                self.buffer_pos[i] = (self.buffer_pidx[i] + 1, p.piece.to_index());
                self.buffer_pidx[i] = self.buffer_pos[i].0 * 8 + self.buffer_pos[i].1;
            }
        }
    }

    fn map_kings(&mut self) {
        self.buffer_pidx[0] = self.kings_map_to_index[self.buffer_pidx[0]][self.buffer_pidx[1]];
        self.buffer_pidx[1] = 0;
    }

    fn unmap_kings(&mut self) {
        (self.buffer_pidx[0], self.buffer_pidx[1]) = self.kings_map_from_index[self.buffer_pidx[0]];
    }

    // Computes the final index representing the position, by aggregating
    // the values of the compact position indexes (cpidx) into a single number.
    fn cpidx_to_index(&self) -> usize {
        let mut index = 0;
        for p in &self.piece_set {
            index *= p.combinations;
            let mut i = p.span.1;
            for k in 1..=p.multiplicity {
                i -= 1;
                assert!(self.buffer_pidx[i] < p.n_squares); // valid cpidx
                if k == 1 {
                    index += self.buffer_pidx[i];
                } else {
                    assert!(self.buffer_pidx[i] > self.buffer_pidx[i+1]); // decreasing cpidx
                    index += N_CHOOSE_K[k][self.buffer_pidx[i]];
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
    fn index_to_cpidx(&mut self, index: usize) {
        let mut v = index;
        for p in self.piece_set.iter().rev() {
            let mut x = v % p.combinations;
            v /= p.combinations;
            let mut n = p.n_squares;
            if p.span.1 - p.span.0 > p.multiplicity {
                // There is a non-encoded en passant pawn.
                self.buffer_pidx[p.span.0] = p.n_squares;
            }
            for k in (1..=p.multiplicity).rev() {
                if k == 1 {
                    self.buffer_pidx[p.span.1-1] = x;
                } else {
                    // Search for the largest n such that N_CHOOSE_K[k][n] <= x
                    let row = &N_CHOOSE_K[k][0..n];
                    n = match row.binary_search(&(x + 1)) {
                        Ok(found_index) => found_index - 1,
                        Err(insert_index) => insert_index - 1,
                    };
                    x -= N_CHOOSE_K[k][n];
                    self.buffer_pidx[p.span.1-k] = n;
                }
            }
        }
        assert_eq!(v, 0);
    }
}

#[cfg(test)]
mod tests {
    use crate::egt::Egt;
    use super::*;

    #[test]
    fn index_to_cpidx_min() {
        // If all pieces occupy the first available square, the index is 0.
        let mut egt = Egt::from_tablename("KQR_KQQQ").unwrap();
        egt.indexer.buffer_pidx = vec![0, 0, 0, 2, 1, 0, 0];
        println!("buffer_pidx: {:?}", egt.indexer.buffer_pidx);
        assert_eq!(egt.indexer.cpidx_to_index(), 0);
    }

    #[test]
    fn index_to_cpidx_max() {
        // If all pieces occupy the last available square, the index is index_range-1.
        let mut egt = Egt::from_tablename("KQR_KQQQ").unwrap();
        egt.indexer.buffer_pidx = egt.indexer.piece_set.iter()
            .flat_map(|p| (1..=p.multiplicity).map(|k| p.n_squares - k))
            .collect();
        println!("buffer_pidx: {:?}", egt.indexer.buffer_pidx);
        assert_eq!(egt.indexer.cpidx_to_index(), egt.indexer.index_range - 1);
    }

    #[test]
    fn decode_encode_kpa_k() {
        let mut egt = Egt::from_tablename("KPa_K").unwrap();

        for i in 0..egt.index_range() {
            if let Some(board) = egt.indexer.board_from_index(i, Color::White) {
                assert_eq!(i, egt.indexer.board_to_index(&board));
            }
            if let Some(board) = egt.indexer.board_from_index(i, Color::Black) {
                assert_eq!(i, egt.indexer.board_to_index(&board));
            }
        }
    }

    #[test]
    fn decode_encode_kb_k() {
        let mut egt = Egt::from_tablename("KB_K").unwrap();

        for i in 0..egt.index_range() {
            if let Some(board) = egt.indexer.board_from_index(i, Color::White) {
                assert_eq!(i, egt.indexer.board_to_index(&board));
            }
            if let Some(board) = egt.indexer.board_from_index(i, Color::Black) {
                assert_eq!(i, egt.indexer.board_to_index(&board));
            }
        }
    }

    #[test]
    fn decode_encode_kqr_kqr() {
        let mut egt = Egt::from_tablename("KQ_KR").unwrap();

        for i in 0..egt.index_range() {
            if let Some(board) = egt.indexer.board_from_index(i, Color::White) {
                assert_eq!(i, egt.indexer.board_to_index(&board));
            }
            if let Some(board) = egt.indexer.board_from_index(i, Color::Black) {
                assert_eq!(i, egt.indexer.board_to_index(&board));
            }
        }
    }

    #[test]
    fn decode_encode_kqqq_k() {
        let mut egt = Egt::from_tablename("KQQQ_K").unwrap();

        for i in 0..egt.index_range() {
            if let Some(board) = egt.indexer.board_from_index(i, Color::White) {
                assert_eq!(i, egt.indexer.board_to_index(&board));
            }
            if let Some(board) = egt.indexer.board_from_index(i, Color::Black) {
                assert_eq!(i, egt.indexer.board_to_index(&board));
            }
        }
    }

    #[test]
    fn decode_encode_kpdpe_kpepepe() {
        let mut egt = Egt::from_tablename("KPdPe_KPePePe").unwrap();

        for i in 0..egt.index_range() {
            if let Some(board) = egt.indexer.board_from_index(i, Color::White) {
                assert_eq!(i, egt.indexer.board_to_index(&board));
            }
            if let Some(board) = egt.indexer.board_from_index(i, Color::Black) {
                assert_eq!(i, egt.indexer.board_to_index(&board));
            }
        }
    }

    #[test]
    fn decode_encode_kpdpepf_kpepe() {
        let mut egt = Egt::from_tablename("KPdPePf_KPePe").unwrap();

        for i in 0..egt.index_range() {
            if let Some(board) = egt.indexer.board_from_index(i, Color::White) {
                assert_eq!(i, egt.indexer.board_to_index(&board));
            }
            if let Some(board) = egt.indexer.board_from_index(i, Color::Black) {
                assert_eq!(i, egt.indexer.board_to_index(&board));
            }
        }
    }

    #[test]
    fn decode_encode_kpepe_kpdpf() {
        let mut egt = Egt::from_tablename("KPePe_KPdPf").unwrap();

        for i in 0..egt.index_range() {
            if let Some(board) = egt.indexer.board_from_index(i, Color::White) {
                assert_eq!(i, egt.indexer.board_to_index(&board));
            }
            if let Some(board) = egt.indexer.board_from_index(i, Color::Black) {
                assert_eq!(i, egt.indexer.board_to_index(&board));
            }
        }
    }
}
