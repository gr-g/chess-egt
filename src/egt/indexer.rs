use shakmaty::{Color, File, Piece, Rank, Role, Square, Setup, CastlingMode, Position, Chess};
use arrayvec::ArrayVec;
use crate::piece_set::{EgtElement, EgtSide, MAX_PIECES, validate_elements};
use crate::error::EgtResult;
use std::cmp::Reverse;


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

// The number of canonical king pairs in a pawnless endgame.
const N_KING_PAIRS: usize = 462;

// Map to encode the possible king positions under pawnless symmetries.
// The second king breaks diagonal symmetry when the first king is on the diagonal,
// but positions where both kings are on the a1-h8 diagonal are intentionally not
// canonicalized using other pieces.
//
// This is a property of the board alone, so it is shared by all `Indexer`s rather
// than rebuilt and stored per sub-table. The tables are flat to keep the lookups
// down to a single indexed load.
struct KingsMap {
    // Canonical king pair index -> (first king square, compacted second king square).
    from_index: [(usize, usize); N_KING_PAIRS],

    // (first king square) * 64 + (compacted second king square) -> canonical
    // king pair index. Invalid pairs are left as `u16::MAX`.
    to_index: [u16; 64 * 64],

    // Whether both kings of a canonical king pair stand on the a1-h8 diagonal,
    // i.e. whether the pair index represents 4 equivalent positions rather than 8.
    on_diagonal: [bool; N_KING_PAIRS],
}

static KINGS_MAP: std::sync::LazyLock<KingsMap> = std::sync::LazyLock::new(|| {
    let mut from_index = [(0, 0); N_KING_PAIRS];
    let mut to_index = [u16::MAX; 64 * 64];
    let mut on_diagonal = [false; N_KING_PAIRS];
    let mut unique_pairs = std::collections::BTreeSet::new();

    for wk in 0..64 {
        for bk in 0..64 {
            if !kings_adjacent_or_same(wk, bk) {
                unique_pairs.insert(reduce_king_pair(wk, bk));
            }
        }
    }
    assert_eq!(unique_pairs.len(), N_KING_PAIRS);

    for (i, &(wk, bk)) in unique_pairs.iter().enumerate() {
        let bk_compacted = if wk <= bk { bk - 1 } else { bk };
        from_index[i] = (wk, bk_compacted);
        to_index[wk * 64 + bk_compacted] = i as u16;
        on_diagonal[i] = wk / 8 == wk % 8 && bk / 8 == bk % 8;
    }

    KingsMap { from_index, to_index, on_diagonal }
});

// A logical EGT element together with its derived indexing metadata.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct IndexerElement {
    // A file-wide ordered pawn group, or identical non-pawn pieces.
    pub element: EgtElement,

    // How many pieces in the group are encoded explicitly (excluding an EP victim).
    pub encoded_multiplicity: usize,

    // The number of squares that these pieces can occupy.
    pub n_squares: usize,

    // The number of ways to put the pieces on the available squares.
    // Equal to N_CHOOSE_K[encoded_multiplicity][n_squares].
    pub combinations: usize,

    // The indexes (begin, end) where information about these pieces is
    // stored in the buffer vectors. Normally
    //   span.1 - span.0 = encoded_multiplicity,
    // unless we are encoding en passant positions and one of the pieces
    // here is a pawn that can be captured en passant. In that case the
    // encoded_multiplicity is reduced by one since the pawn that can be captured
    // en passant is not encoded explicitly.
    pub span: (usize, usize),
}

// Positions where en passant is possible are encoded separately from positions
// where en passant is not possible. One instance of this struct represents one
// option for the en passant status of positions, together with the range of
// indexes associated to these positions.
// Each option carries its own pre-adjusted `piece_set`, with the `encoded_multiplicity`,
// `n_squares` and `combinations` of the pawn group on the en passant file
// already reduced to reflect the constraints imposed by that option (the pawn
// on the 5th rank is not encoded, and the remaining pawns on that file cannot
// occupy the 5th, 6th or 7th rank). `span` is kept at its original value so that
// the ep-pawn slot stays reserved in the buffers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnpassantOption {
    // The file of the en passant pawn if en passant is possible, or None.
    // The rank is implicit: from the side-to-move's perspective, the en
    // passant pawn is always on the 5th rank.
    pub file: Option<File>,

    // The piece set adjusted for this en passant option.
    pub piece_set: Vec<IndexerElement>,

    // The start of the range used to encode positions with this en passant status.
    pub range_start: usize,

    // The end of the range used to encode positions with this en passant status.
    pub range_end: usize,
}

// Stack-allocated buffers used to encode/decode positions without heap allocation.
// Both are sized to `MAX_PIECES`, the maximum number of pieces supported by an endgame.
type CoordBuf = ArrayVec<(usize, usize), MAX_PIECES>;
type PidxBuf = ArrayVec<usize, MAX_PIECES>;

// Helper object taking care of the efficient conversion from positions to
// compact indexes representing the positions, and viceversa.
#[derive(Clone, Debug)]
pub struct Indexer {
    // The number of pieces in the endgame.
    pub n_pieces: usize,

    // The number of pawns in the endgame.
    pub n_pawns: usize,

    // The number of occupied pawn files (one ordered group per file).
    pub n_pawn_groups: usize,

    // The total number of positions indexed in this endgame table.
    pub index_range: usize,

    // Coordinate slots in normalized colors: White denotes the side to move.
    pieces: ArrayVec<Piece, MAX_PIECES>,

    // The following field provides information on the en passant status.
    ep_options: Vec<EnpassantOption>,

    // For pawnless endgames, the number of combinations of all pieces after the
    // two kings. The king pair index is the quotient of the index by this value,
    // which makes it cheap to recover without a full decode.
    nonking_combinations: usize,
}

impl Indexer {
    /// Builds the indexing plan in canonical order: pawn files, kings, then
    /// other role/side groups. The element multiplicities describe physical pieces.
    pub fn from_elements(elements: &[EgtElement]) -> EgtResult<Self> {
        validate_elements(elements)?;
        let n_pieces = elements.iter().map(EgtElement::multiplicity).sum();
        let n_pawns = elements.iter().filter_map(|element| match element {
            EgtElement::PawnGroup { order, .. } => Some(order.len()),
            EgtElement::PieceGroup { .. } => None,
        }).sum();
        let n_pawn_groups = elements.iter().filter(|element| matches!(element, EgtElement::PawnGroup { .. })).count();

        let mut elements = elements.to_vec();
        elements.sort();
        let mut piece_set: Vec<_> = elements.into_iter().map(|element| IndexerElement {
            encoded_multiplicity: element.multiplicity(),
            element,
            n_squares: 0,
            combinations: 0,
            span: (0, 0),
        }).collect();

        // Compute n_squares, combinations, span.
        Self::compute_piece_squares(&mut piece_set);
        let mut pieces = ArrayVec::new();
        for p in &piece_set {
            let (role, sides): (_, ArrayVec<_, MAX_PIECES>) = match &p.element {
                EgtElement::PawnGroup { order, .. } => (Role::Pawn, order.iter().rev().copied().collect()),
                EgtElement::PieceGroup { role, side, multiplicity } => (*role, std::iter::repeat_n(*side, *multiplicity).collect()),
            };
            pieces.extend(sides.into_iter().map(|side| Piece {
                role,
                color: if side == EgtSide::SideToMove { Color::White } else { Color::Black },
            }));
        }

        let combinations_without_ep = piece_set.iter().map(|p| p.combinations).product();

        // Check en passant options.
        let ep_options = Self::setup_ep_options(&piece_set, n_pawn_groups, combinations_without_ep);

        //println!("pieces: {:?}", &piece_set);
        //println!("ep_options: {:?}", &ep_options);

        let index_range = ep_options.last().unwrap().range_end;

        // In a pawnless endgame the kings occupy the first two piece set entries
        // and contribute a factor of 462 * 1 to the mixed-radix encoding, so the
        // remaining factor identifies the king pair.
        let nonking_combinations = if n_pawns == 0 {
            piece_set[2..].iter().map(|p| p.combinations).product()
        } else {
            0
        };

        Ok(Indexer {
            n_pieces,
            n_pawns,
            n_pawn_groups,
            index_range,
            pieces,
            ep_options,
            nonking_combinations,
        })
    }

    // Returns the piece set adjusted for the given en passant option.
    #[inline]
    fn piece_set(&self, ep_idx: usize) -> &Vec<IndexerElement> {
        &self.ep_options[ep_idx].piece_set
    }

    // Returns the en passant option at the given index.
    #[inline]
    fn ep_option(&self, ep_idx: usize) -> &EnpassantOption {
        &self.ep_options[ep_idx]
    }

    // Builds a `CoordBuf` pre-filled with `(0, 0)` for `n` pieces.
    #[inline]
    fn filled_coord(n: usize) -> CoordBuf {
        (0..n).map(|_| (0, 0)).collect()
    }

    // Builds a `PidxBuf` pre-filled with `0` for `n` pieces.
    #[inline]
    fn filled_pidx(n: usize) -> PidxBuf {
        (0..n).map(|_| 0).collect()
    }

    fn compute_piece_squares(piece_set: &mut[IndexerElement]) {
        let mut available_squares = 64;
        let mut pawnless = true;
        let mut span_end = 0;
        for p in piece_set.iter_mut() {
            let k = p.encoded_multiplicity;
            if matches!(&p.element, EgtElement::PawnGroup { .. }) {
                pawnless = false;
                p.n_squares = 6;
                debug_assert!(p.n_squares >= k);
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
            debug_assert_eq!(piece_set[0].encoded_multiplicity, 1); // white king
            debug_assert_eq!(piece_set[1].encoded_multiplicity, 1); // black king
            // Encode the position of both kings using a single canonical index.
            piece_set[0].n_squares = 462;
            piece_set[0].combinations = 462;
            piece_set[1].n_squares = 1;
            piece_set[1].combinations = 1;
        }
    }

    fn setup_ep_options(base_piece_set: &[IndexerElement], n_pawn_groups: usize, combinations_without_ep: usize) -> Vec<EnpassantOption> {
        let mut ep_options = vec![EnpassantOption {
            file: None,
            piece_set: base_piece_set.to_vec(),
            range_start: 0,
            range_end: combinations_without_ep,
        }];
        let mut range_start = combinations_without_ep;
        let pawns = &base_piece_set[..n_pawn_groups];
        for (i, pawn) in pawns.iter().enumerate() {
            let EgtElement::PawnGroup { file, order } = &pawn.element else { unreachable!() };
            // The double-pushed pawn is highest on its file. Its starting and
            // transit squares must be empty, leaving ranks 2..4 for other pawns.
            if order.last() != Some(&EgtSide::SideNotToMove) || order.len() > 4 {
                continue;
            }
            let has_neighbor = pawns.iter().any(|p| {
                let EgtElement::PawnGroup { file: other_file, order } = &p.element else { unreachable!() };
                file.to_usize().abs_diff(other_file.to_usize()) == 1
                    && order.contains(&EgtSide::SideToMove)
            });
            if !has_neighbor {
                continue;
            }

            let mut option_piece_set = base_piece_set.to_vec();
            let p = &mut option_piece_set[i];
            // Keep the full order and span, reserving their first (highest) slot
            // for the victim. Only the remaining pawns contribute to the radix.
            p.encoded_multiplicity -= 1;
            p.n_squares = 3;
            p.combinations = N_CHOOSE_K[p.encoded_multiplicity][p.n_squares];
            let combinations = combinations_without_ep / pawn.combinations * p.combinations;
            let range_end = range_start + combinations;
            ep_options.push(EnpassantOption {
                file: Some(*file),
                piece_set: option_piece_set,
                range_start,
                range_end,
            });
            range_start = range_end;
        }
        ep_options
    }

    // Encodes the position into an index, which represents the position up to symmetries.
    pub fn position_to_index(&self, position: &Chess) -> usize {
        self.position_to_index_impl::<false>(position)
    }

    // Encodes the horizontal reflection without constructing another Chess.
    pub(crate) fn reflected_position_to_index(&self, position: &Chess) -> usize {
        self.position_to_index_impl::<true>(position)
    }

    fn position_to_index_impl<const REFLECTED: bool>(&self, position: &Chess) -> usize {
        let ep_file = position.legal_ep_square().map(|sq| {
            if REFLECTED { sq.flip_horizontal().file() } else { sq.file() }
        });
        let (index_offset, ep_idx) = self.adjust_ep_from_position(ep_file);
        // Now the en passant option reflects the en passant status of the position.
        // If there are pawns on the en passant file (different from the en passant
        // pawn on the 5th rank), they will be encoded with indexes in 0..3 instead of 0..6.

        let mut buffer_coord = Self::filled_coord(self.n_pieces);
        let mut buffer_pidx = Self::filled_pidx(self.n_pieces);

        self.position_to_coord::<REFLECTED>(ep_idx, &mut buffer_coord, position);
        // Now `buffer_coord` has the coordinates (rank, file) of the pieces.
        //println!("buffer_coord: {:?}", buffer_coord);

        if self.n_pawns == 0 {
            self.reduce_symmetries(&mut buffer_coord);
            // Now the first item in `buffer_coord` has coordinates restricted to 10 squares.
            //println!("buffer_coord: {:?}", buffer_coord);
        }

        self.sort_coord_repeated_pieces(ep_idx, &mut buffer_coord);
        // Now `buffer_coord` has the sorted coordinates for repeated pieces.
        //println!("buffer_coord: {:?}", buffer_coord);

        self.coord_to_pidx(&buffer_coord, &mut buffer_pidx);
        // Now `buffer_pidx` has the position indexes, with
        // non-overlapping values in [0..64, 0..64, 0..64, 0..64, ...].
        //println!("buffer_pidx: {:?}", buffer_pidx);

        Self::compact_pidx(&mut buffer_pidx);
        // Now `buffer_pidx` has compact positions indexes, with
        // values in [0..64, 0..63, 0..62, 0..61, ...].
        //println!("buffer_pidx: {:?}", buffer_pidx);

        if self.n_pawns > 0 {
            self.pawn_pidx_to_cpidx(ep_idx, &buffer_coord, &mut buffer_pidx);
            // Now the first `n_pawns` elements of `buffer_pidx` have
            // the pawn positions encoded in 0..6 (or 0..3), so the value
            // ranges look like [0..6, 0..6, 0..62, 0..61, ...].
            // The positions of pawns on the same file are non-overlapping.
            //println!("buffer_pidx: {:?}", buffer_pidx);
        } else {
            Self::map_kings(&mut buffer_pidx);
            // Now the first element in `buffer_pidx` has values in 0..462
            // and encodes the position of both kings.
            //println!("buffer_pidx: {:?}", buffer_pidx);
        }

        // Now that we have all compact position indexes in `buffer_pidx`,
        // we can aggregate them into a single value.
        index_offset + self.cpidx_to_index(ep_idx, &buffer_pidx)
    }

    // Decodes an index and recreates the corresponding position for this endgame (up to symmetries).
    // If the index represents an invalid position, returns None.
    pub fn position_from_index(&self, index: usize, side_to_move: Color) -> Option<Chess> {
        //println!("position_from_index: {}", index);
        if index >= self.index_range {
            return None;
        }
        debug_assert!(index < self.index_range);
        let (index_offset, ep_idx) = self.adjust_ep_from_index(index);
        // Now the en passant option reflects the en passant status as encoded in the index.
        // If there are pawns on the en passant file (different from the en passant
        // pawn on the 5th rank), they will be decoded from indexes in 0..3 instead of 0..6.

        let mut buffer_coord = Self::filled_coord(self.n_pieces);
        let mut buffer_pidx = Self::filled_pidx(self.n_pieces);

        self.index_to_cpidx(ep_idx, &mut buffer_pidx, index - index_offset);

        if self.n_pawns > 0 {
            self.pawn_cpidx_to_pidx(ep_idx, &mut buffer_coord, &mut buffer_pidx);
            Self::compact_pidx(&mut buffer_pidx[0..self.n_pawns]);
            // Now the first `n_pawns` elements of buffer_coord are correct, and `buffer_pidx`
            // has values in [0..64, 0..63, 0..62, 0..61, ...].
            //println!("buffer_pidx: {:?}", buffer_pidx);
        } else {
            // `buffer_pidx` has values in [0..462, 0..1, 0..62, 0..61, ...].
            //println!("buffer_pidx: {:?}", buffer_pidx);
            Self::unmap_kings(&mut buffer_pidx);
            // Now `buffer_pidx` has values in [0..64, 0..63, 0..62, 0..61, ...].
            //println!("buffer_pidx: {:?}", buffer_pidx);
        }

        Self::uncompact_cpidx(&mut buffer_pidx);
        // Now `buffer_pidx` has the positions indexes, with
        // non-overlapping values in [0..64, 0..64, 0..64, 0..64, ...].
        //println!("buffer_pidx: {:?}", buffer_pidx);

        self.nonpawn_pidx_to_coord(&mut buffer_coord, &buffer_pidx);
        // Now the non-pawn part of `buffer_coord` is correct.
        //println!("buffer_coord: {:?}", buffer_coord);

        // All coordinates are in `buffer_coord`, we can place the pieces to create a new position.
        self.coord_to_position(ep_idx, &buffer_coord, side_to_move)
    }

    // Checks whether the position allows en passant and selects the corresponding
    // en passant option. Returns the index offset to apply to the encoded value
    // and the index of the selected en passant option.
    fn adjust_ep_from_position(&self, ep_file: Option<File>) -> (usize, usize) {
        let ep_idx = self.ep_options.iter().position(|opt| opt.file == ep_file).unwrap();
        (self.ep_options[ep_idx].range_start, ep_idx)
    }

    // Checks to which en passant option this index corresponds and selects it.
    // Returns the index offset to apply to decode the value and the index of the
    // selected en passant option.
    fn adjust_ep_from_index(&self, index: usize) -> (usize, usize) {
        let ep_idx = self.ep_options.iter().position(|ep| index < ep.range_end).unwrap();
        (self.ep_options[ep_idx].range_start, ep_idx)
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
    pub fn diagonal_symmetric(&self, index: usize) -> Option<usize> {
        if self.n_pawns != 0 {
            return None;
        }

        // Whether both kings are on the diagonal depends only on the king pair,
        // which is the leading factor of the mixed-radix encoding. Recovering it
        // with a single division rejects the common case without paying for a
        // full decode of the index.
        debug_assert_eq!(self.ep_options.len(), 1, "a pawnless endgame has no en passant options");
        if !KINGS_MAP.on_diagonal[index / self.nonking_combinations] {
            return None;
        }

        let (index_offset, ep_idx) = self.adjust_ep_from_index(index);

        let mut buffer_coord = Self::filled_coord(self.n_pieces);
        let mut buffer_pidx = Self::filled_pidx(self.n_pieces);

        self.index_to_cpidx(ep_idx, &mut buffer_pidx, index - index_offset);
        Self::unmap_kings(&mut buffer_pidx);
        Self::uncompact_cpidx(&mut buffer_pidx);
        self.nonpawn_pidx_to_coord(&mut buffer_coord, &buffer_pidx);

        let kings_on_diagonal = buffer_coord[0..2].iter().all(|(r, f)| *r == *f);
        debug_assert!(kings_on_diagonal, "the fast path must agree with the decoded position");
        if kings_on_diagonal {
            // Swap coordinates and recode
            for p in buffer_coord.iter_mut() { *p = (p.1, p.0); };
            self.sort_coord_repeated_pieces(ep_idx, &mut buffer_coord);
            self.coord_to_pidx(&buffer_coord, &mut buffer_pidx);
            Self::compact_pidx(&mut buffer_pidx);
            Self::map_kings(&mut buffer_pidx);
            Some(self.cpidx_to_index(ep_idx, &buffer_pidx))
        } else {
            None
        }
    }



    // Extracts the coordinates of the pieces from the position and stores them in `buffer_coord`.
    fn position_to_coord<const REFLECTED: bool>(&self, ep_idx: usize, buffer_coord: &mut CoordBuf, position: &Chess) {
        assert_eq!(position.board().occupied().count(), self.n_pieces, "position_to_coord() called with a position that does not match the piece set");
        for p in self.piece_set(ep_idx) {
            match &p.element {
                EgtElement::PawnGroup { file, order } => {
                    // Descending normalized ranks: White descends squares, Black ascends.
                    // Piece groups describe target files, not reflected source files.
                    let source_file = if REFLECTED { File::new(7 - file.to_u32()) } else { *file };
                    let mut bb = position.board().by_role(Role::Pawn) & shakmaty::Bitboard::from_file(source_file);
                    assert_eq!(bb.count(), order.len(), "pawn file does not match the piece set");
                    for (i, expected_side) in (p.span.0..p.span.1).zip(order.iter().rev()) {
                        let square = if position.turn() == Color::Black { bb.pop_front() } else { bb.pop_back() }.unwrap();
                        debug_assert_eq!(position.board().by_color(position.turn()).contains(square),
                            *expected_side == EgtSide::SideToMove, "pawn color order does not match the piece set");
                        let rank = square.rank().to_usize();
                        let rank = if position.turn() == Color::Black { 7 - rank } else { rank };
                        buffer_coord[i] = (rank, file.to_usize());
                    }
                }
                EgtElement::PieceGroup { role, side, .. } => {
                    let color = if *side == EgtSide::SideToMove { position.turn() } else { !position.turn() };
                    let bb = position.board().by_role(*role) & position.board().by_color(color);
                    assert_eq!(bb.count(), p.span.1 - p.span.0, "position_to_coord() called with a position that does not match the piece set");
                    for (square, i) in bb.into_iter().zip(p.span.0..p.span.1) {
                        let square = if REFLECTED { square.flip_horizontal() } else { square };
                        let rank = square.rank().to_usize();
                        let rank = if self.n_pawns > 0 && position.turn() == Color::Black { 7 - rank } else { rank };
                        buffer_coord[i] = (rank, square.file().to_usize());
                    }
                }
            }
        }
    }

    // Builds a position using the coordinates stored in `buffer_coord`.
    fn coord_to_position(&self, ep_idx: usize, buffer_coord: &CoordBuf, side_to_move: Color) -> Option<Chess> {
        let mut setup = Setup::empty();
        setup.turn = side_to_move;
        for (&(mut r, f), mut piece) in buffer_coord.iter().zip(self.pieces.iter().copied()) {
            if side_to_move == Color::Black {
                piece.color = !piece.color;
                if self.n_pawns > 0 { r = 7 - r; }
            }
            let square = Square::from_coords(File::new(f as u32), Rank::new(r as u32));
            setup.board.set_piece_at(square, piece);
        }

        let ep_square = if let Some(file) = self.ep_option(ep_idx).file {
            let target_rank = match side_to_move {
                Color::White => Rank::Sixth,
                Color::Black => Rank::Third,
            };
            Some(Square::from_coords(file, target_rank))
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
    fn reduce_symmetries(&self, buffer_coord: &mut CoordBuf) {
        if buffer_coord[0].0 > 3 {
            // flip around horizontal axis
            for (r, _) in buffer_coord.iter_mut() { *r = 7 - *r; };
        }
        if buffer_coord[0].1 > 3 {
            // flip around vertical axis
            for (_, f) in buffer_coord.iter_mut() { *f = 7 - *f; };
        }
        if buffer_coord[0].0 > buffer_coord[0].1 ||
            (buffer_coord[0].0 == buffer_coord[0].1 &&
            buffer_coord[1].0 > buffer_coord[1].1) {
            // flip diagonally. If both kings are on the diagonal, we intentionally do not
            // diagonal-reflect based on the remaining pieces.
            for p in buffer_coord.iter_mut() { *p = (p.1, p.0); };
        }
    }

    // Sort repeated non-pawns from highest to lowest; pawns are already ordered.
    fn sort_coord_repeated_pieces(&self, ep_idx: usize, buffer_coord: &mut CoordBuf) {
        for p in &self.piece_set(ep_idx)[self.n_pawn_groups..] {
            if p.span.1 - p.span.0 > 1 {
                buffer_coord[p.span.0..p.span.1].sort_by_key(|v| Reverse(*v));
            }
        }
    }

    // Converts the piece coordinates to indexes in 0..64.
    fn coord_to_pidx(&self, buffer_coord: &CoordBuf, buffer_pidx: &mut PidxBuf) {
        for i in 0..self.n_pieces {
            buffer_pidx[i] = buffer_coord[i].0 * 8 + buffer_coord[i].1;
        }
    }

    // Converts indexes in 0..64 to piece coordinates for non-pawns.
    fn nonpawn_pidx_to_coord(&self, buffer_coord: &mut CoordBuf, buffer_pidx: &PidxBuf) {
        for i in self.n_pawns..self.n_pieces {
            buffer_coord[i] = (buffer_pidx[i] / 8, buffer_pidx[i] % 8);
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
    fn pawn_pidx_to_cpidx(&self, ep_idx: usize, buffer_coord: &CoordBuf, buffer_pidx: &mut PidxBuf) {
        for p in &self.piece_set(ep_idx)[0..self.n_pawn_groups] {
            for i in p.span.0..p.span.1 {
                buffer_pidx[i] = buffer_coord[i].0 - 1;
            }
        }
    }

    // Converts compact position indexes (cpidx) for pawns (in 0..6) to indexes
    // in 0..64, and converts these indexes to piece coordinates.
    fn pawn_cpidx_to_pidx(&self, ep_idx: usize, buffer_coord: &mut CoordBuf, buffer_pidx: &mut PidxBuf) {
        for p in &self.piece_set(ep_idx)[0..self.n_pawn_groups] {
            let EgtElement::PawnGroup { file, .. } = &p.element else { unreachable!() };
            for i in p.span.0..p.span.1 {
                buffer_coord[i] = (buffer_pidx[i] + 1, file.to_usize());
                buffer_pidx[i] = buffer_coord[i].0 * 8 + buffer_coord[i].1;
            }
        }
    }

    fn map_kings(buffer_pidx: &mut PidxBuf) {
        buffer_pidx[0] = KINGS_MAP.to_index[buffer_pidx[0] * 64 + buffer_pidx[1]] as usize;
        debug_assert!(buffer_pidx[0] < N_KING_PAIRS, "kings on an invalid pair of squares");
        buffer_pidx[1] = 0;
    }

    fn unmap_kings(buffer_pidx: &mut PidxBuf) {
        (buffer_pidx[0], buffer_pidx[1]) = KINGS_MAP.from_index[buffer_pidx[0]];
    }

    // Computes the final index representing the position, by aggregating
    // the values of the compact position indexes (cpidx) into a single number.
    fn cpidx_to_index(&self, ep_idx: usize, buffer_pidx: &PidxBuf) -> usize {
        let mut index = 0;
        for p in self.piece_set(ep_idx) {
            index *= p.combinations;
            let mut i = p.span.1;
            for k in 1..=p.encoded_multiplicity {
                i -= 1;
                debug_assert!(buffer_pidx[i] < p.n_squares); // valid cpidx
                if k == 1 {
                    index += buffer_pidx[i];
                } else {
                    debug_assert!(buffer_pidx[i] > buffer_pidx[i+1]); // decreasing cpidx
                    index += N_CHOOSE_K[k][buffer_pidx[i]];
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
    fn index_to_cpidx(&self, ep_idx: usize, buffer_pidx: &mut PidxBuf, index: usize) {
        let mut v = index;
        for p in self.piece_set(ep_idx).iter().rev() {
            let mut x = v % p.combinations;
            v /= p.combinations;
            let mut n = p.n_squares;
            if p.span.1 - p.span.0 > p.encoded_multiplicity {
                // There is a non-encoded en passant pawn.
                buffer_pidx[p.span.0] = p.n_squares;
            }
            for k in (1..=p.encoded_multiplicity).rev() {
                if k == 1 {
                    buffer_pidx[p.span.1-1] = x;
                } else {
                    // Search for the largest n such that N_CHOOSE_K[k][n] <= x
                    let row = &N_CHOOSE_K[k][0..n];
                    n = match row.binary_search(&(x + 1)) {
                        Ok(found_index) => found_index - 1,
                        Err(insert_index) => insert_index - 1,
                    };
                    x -= N_CHOOSE_K[k][n];
                    buffer_pidx[p.span.1-k] = n;
                }
            }
        }
        assert_eq!(v, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use EgtSide::{SideNotToMove as SNTM, SideToMove as STM};

    fn pawn_group(file: File, order: &[EgtSide]) -> EgtElement {
        EgtElement::PawnGroup { file, order: order.iter().copied().collect() }
    }

    fn indexer(groups: &[EgtElement]) -> Indexer {
        // Deliberately unsorted: the indexer must retain the old group order.
        let mut elements = vec![
            EgtElement::PieceGroup { role: Role::King, side: SNTM, multiplicity: 1 },
            EgtElement::PieceGroup { role: Role::King, side: STM, multiplicity: 1 },
        ];
        elements.extend_from_slice(groups);
        Indexer::from_elements(&elements).unwrap()
    }

    #[test]
    fn combined_pawn_groups_encode_every_rank_subset_and_color_order() {
        let mut legal_positions = 0;
        for k in 1..=6 {
            for colors in 0usize..1 << k {
                let order: ArrayVec<_, 6> = (0..k).map(|i| if colors & (1 << i) == 0 { STM } else { SNTM }).collect();
                let indexer = indexer(&[pawn_group(File::E, &order)]);
                let pawn = &indexer.piece_set(0)[0];
                assert_eq!(indexer.n_pawns, k);
                assert_eq!(indexer.n_pawn_groups, 1);
                assert_eq!(pawn.combinations, N_CHOOSE_K[k][6]);
                assert_eq!(pawn.span, (0, k));
                assert_eq!(indexer.ep_options.len(), 1);
                let king_combinations = (64 - k) * (63 - k);
                assert_eq!(indexer.index_range, N_CHOOSE_K[k][6] * king_combinations);
                let mut seen = std::collections::BTreeSet::new();
                for ranks in 1u32..64 {
                    if ranks.count_ones() as usize != k {
                        continue;
                    }
                    let mut cpidx = Indexer::filled_pidx(k + 2);
                    let mut coord = Indexer::filled_coord(k + 2);
                    for (i, rank) in (0..6).rev().filter(|r| ranks & (1 << r) != 0).enumerate() {
                        cpidx[i] = rank;
                        coord[i] = (rank + 1, File::E.to_usize());
                    }
                    let index = indexer.cpidx_to_index(0, &cpidx);
                    assert!(seen.insert(index));
                    let mut decoded = Indexer::filled_pidx(k + 2);
                    indexer.index_to_cpidx(0, &mut decoded, index);
                    assert_eq!(decoded, cpidx);

                    coord[k] = (0, 0);
                    coord[k + 1] = (7, 7);
                    for turn in [Color::White, Color::Black] {
                        if let Some(position) = indexer.coord_to_position(0, &coord, turn) {
                            let mut gathered = Indexer::filled_coord(k + 2);
                            indexer.position_to_coord::<false>(0, &mut gathered, &position);
                            assert_eq!(gathered, coord);
                            let index = indexer.position_to_index(&position);
                            let reflected = crate::egt_file::mirror_horizontally(&position).unwrap();
                            assert_eq!(indexer.reflected_position_to_index(&reflected), index);
                            assert_eq!(indexer.position_from_index(index, turn), Some(position));
                            legal_positions += 1;
                        }
                    }
                }
                assert_eq!(seen.len(), N_CHOOSE_K[k][6]);
            }
        }
        assert!(legal_positions > 100);
    }

    #[test]
    fn combined_en_passant_groups_reserve_highest_slot_and_round_trip() {
        use crate::egt_file::mirror_horizontally;

        for k in 1usize..=4 {
            for colors in 0usize..1 << (k - 1) {
                let mut order: ArrayVec<_, 6> = (0..k - 1).map(|i| if colors & (1 << i) == 0 { STM } else { SNTM }).collect();
                order.push(SNTM);
                let groups = [pawn_group(File::D, &[STM]), pawn_group(File::E, &order)];
                let indexer = indexer(&groups);
                assert_eq!(indexer.ep_options.len(), 2);
                let option = indexer.ep_option(1);
                assert_eq!(option.file, Some(File::E));
                let pawn = &option.piece_set[1];
                assert_eq!(pawn.element, groups[1]);
                assert_eq!(pawn.span, (1, k + 1));
                assert_eq!(pawn.encoded_multiplicity, k - 1);
                assert_eq!(pawn.element.multiplicity(), k);
                assert_eq!(pawn.n_squares, 3);
                assert_eq!(pawn.combinations, N_CHOOSE_K[k - 1][3]);
                assert_eq!(option.range_end - option.range_start, 6 * N_CHOOSE_K[k - 1][3] * (63 - k) * (62 - k));

                for ranks in 0u32..8 {
                    if ranks.count_ones() as usize != k - 1 {
                        continue;
                    }
                    let mut coord = Indexer::filled_coord(k + 3);
                    coord[0] = (4, 3); // Neighboring STM pawn on d5.
                    coord[1] = (4, 4); // Fixed SNTM victim on e5.
                    for (i, rank) in (0..3).rev().filter(|r| ranks & (1 << r) != 0).enumerate() {
                        coord[i + 2] = (rank + 1, 4);
                    }
                    coord[k + 1] = (0, 0);
                    coord[k + 2] = (7, 7);
                    for turn in [Color::White, Color::Black] {
                        let position = indexer.coord_to_position(1, &coord, turn).unwrap();
                        assert!(position.legal_ep_square().is_some());
                        let index = indexer.position_to_index(&position);
                        assert!((option.range_start..option.range_end).contains(&index));
                        assert_eq!(indexer.position_from_index(index, turn), Some(position.clone()));
                        let mut cpidx = Indexer::filled_pidx(k + 3);
                        indexer.index_to_cpidx(1, &mut cpidx, index - option.range_start);
                        assert_eq!(cpidx[1], 3); // +1 maps the reserved slot to rank 5.
                        let reflected = mirror_horizontally(&position).unwrap();
                        assert_eq!(indexer.reflected_position_to_index(&reflected), index);
                    }

                    // A neighbor elsewhere on the file does not make EP legal.
                    coord[0].0 = 3;
                    assert!(indexer.coord_to_position(1, &coord, Color::White).is_none());
                }
            }
        }
    }

    #[test]
    fn en_passant_requires_highest_sntm_pawn_space_and_stm_neighbor() {
        for groups in [
            vec![pawn_group(File::D, &[STM]), pawn_group(File::E, &[SNTM, STM])],
            vec![pawn_group(File::D, &[STM]), pawn_group(File::E, &[SNTM; 5])],
            vec![pawn_group(File::E, &[SNTM])],
            vec![pawn_group(File::D, &[SNTM]), pawn_group(File::E, &[SNTM])],
            vec![pawn_group(File::C, &[STM]), pawn_group(File::E, &[SNTM])],
        ] {
            assert_eq!(indexer(&groups).ep_options.len(), 1, "{groups:?}");
        }
    }

    #[test]
    fn indexer_rejects_invalid_elements() {
        let kings = [
            EgtElement::PieceGroup { role: Role::King, side: STM, multiplicity: 1 },
            EgtElement::PieceGroup { role: Role::King, side: SNTM, multiplicity: 1 },
        ];
        for extra in [
            vec![pawn_group(File::E, &[])],
            vec![pawn_group(File::E, &[STM]), pawn_group(File::E, &[SNTM])],
            vec![EgtElement::PieceGroup { role: Role::Pawn, side: STM, multiplicity: 1 }],
            vec![EgtElement::PieceGroup { role: Role::Queen, side: STM, multiplicity: 0 }],
            vec![EgtElement::PieceGroup { role: Role::Queen, side: STM, multiplicity: usize::MAX }],
        ] {
            let mut elements = kings.to_vec();
            elements.extend(extra);
            assert!(Indexer::from_elements(&elements).is_err());
        }
        assert!(Indexer::from_elements(&kings[..1]).is_err());
    }

    #[test]
    fn pawnless_mapping_keeps_existing_indices() {
        use shakmaty::fen::Fen;

        let elements = [
            EgtElement::PieceGroup { role: Role::Rook, side: STM, multiplicity: 1 },
            EgtElement::PieceGroup { role: Role::King, side: SNTM, multiplicity: 1 },
            EgtElement::PieceGroup { role: Role::King, side: STM, multiplicity: 1 },
        ];
        let indexer = Indexer::from_elements(&elements).unwrap();
        assert_eq!(indexer.index_range, 462 * 62);
        for fen in ["8/8/8/8/8/8/1R6/K1k5 w - - 0 1", "8/8/8/8/8/8/1r6/k1K5 b - - 0 1"] {
            let position: Chess = fen.parse::<Fen>().unwrap().into_position(CastlingMode::Standard).unwrap();
            assert_eq!(indexer.position_to_index(&position), 7);
            assert_eq!(indexer.position_from_index(7, position.turn()), Some(position));
        }
        assert_eq!(indexer.diagonal_symmetric(773), Some(801));
        assert_eq!(indexer.diagonal_symmetric(801), Some(773));
        assert_eq!(indexer.diagonal_symmetric(805), Some(805));
    }
}
