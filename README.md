# gg-EGT - a Chess Endgame Tablebase
Work-in-progress repository with experiments to build from scratch a [Chess
Endgame Tablebase](https://en.wikipedia.org/wiki/Endgame_tablebase) (EGT).

## Goals
- Provide a tablebase with the Depth to Conversion (DTC) metric including the
  conversion type (checkmate, capture or promotion), particularly suited to
  applications such as studying endgame theory or practicing endgames against
  the computer.
- Provide a clean, readable Rust codebase that people interested in generating
  chess tablebases can understand, modify and use in other projects, with
  performance not far from more optimized (but also more obscure) codebases.
- Use a modern approach for the generation and storage of endgame tables,
  producing separate tables for independent pawn configurations (generalizing
  the approach of the [op1](https://lichess.org/@/lichess/blog/op1-partial-8-piece-tablebase-available/1ptPBDpC)
  tablebase).
- Support scaling on cloud resources (compute-optimized instances and cloud
  object storage) to potentially reach 7-piece completeness and beyond.

## Usage
```sh
# Generate all endgame tables up to 3 pieces locally.
cargo run --release -- --generate 2/ 3/

# Generate all 4-piece tables (assuming dependencies already exist), using 4 threads.
cargo run --release -- --path ./ggegt --generate 4/ --threads 4

# Generate selected tables locally, using existing dependencies from an S3 bucket
# (you can pass credentials through environment variables using `object_store`
# conventions, e.g. see [here](https://docs.rs/object_store/latest/object_store/aws/struct.AmazonS3Builder.html#method.from_env)).
cargo run --release -- --path ./ggegt --input-path s3://my-ggegt-bucket/ --generate KQ_KR KP_K

# Verify the internal consistency of existing tables.
cargo run --release -- --verify KQ_KR KR_KQ

# Enumerate theoretical tables having 5 pieces with 3 on one side.
cargo run --release -- --list 5/3v2/

# Report statistics on all 4-piece tables.
cargo run --release -- --stats 5/

# Store summary statistics for groups of tables for faster retrieval.
cargo run --release -- --cache-stats 2/ 3/ 4/ 5/

# Probe a position.
cargo run --release -- '8/8/8/8/8/2K5/4Q3/7k w - -'
```

`--generate`, `--verify`, `--stats`, `--cache-stats`, and `--list` each accept
multiple `SELECTOR`s as arguments, which can be either:

- **Exact names**, e.g. `KPa_K`: identifying a table by its canonical name (see
  below for table naming conventions).
- **Material configurations**, e.g. `KP_K`: all tables for that material
  balance.
- **Hierarchy prefixes**, e.g.
 - `3/`: all tables with 3 pieces.
 - `4/2v2/op1`: all tables with 4 pieces, 2 per side, with one pair of opposing
   pawns.
 - `5/3v2/op1/2pawns`: all tables with 5 pieces, 3 pieces on one, with one pair
   of opposing pawns and 2 pawns in total.

Tables are generated in pairs: `--generate KPa_K` will produce a file for
`KPa_K` (representing positions where the side-to-move has the pawn) and also a
file for the "twin" table `K_KPa` (representing positions where the
side-not-to-move has the pawn).

Run the test suite with:
```sh
cargo test --release
```

Two Python scripts are available to export statistics from a current tablebase.
```sh
cargo run --release -- --path ROOT --cache-stats 3/ 4/ 5/ 6/
python3 scripts/export_table_stats.py ROOT table_stats.csv
python3 scripts/export_summary_stats.py ROOT summary_stats.csv
```

## Current Status

All tables up to 5 pieces have been generated and verified. Aspects of the
library interface, the command line interface, and of the file format still
need to be finalized before proceeding further.

## Design Specifications

### Scope
The tablebase is designed to store DTC outcomes for a set of chess positions
(_queryable positions_) defined as: _valid positions_ with up to 8 pieces
without castling rights.

(According to traditional terminology we should say _8 [men](https://en.wikipedia.org/wiki/Chess_piece#Definitions)_
instead of _8 pieces_ to make it clear that pawns are counted among the 8
pieces, but we do not make this distinction here and just use _pieces_
everywhere.)

A position is considered _valid_ if it fulfills the validity requirements
defined by the `shakmaty` library [here](https://docs.rs/shakmaty/latest/shakmaty/struct.PositionErrorKinds.html),
except that positions with "impossible checks" (such as positions where three
pieces give a check at the same time) are also considered valid. In other
words, the `shakmaty::Chess` objects (with up to 8 pieces and without castling
rights) returned from code such as the following correspond to the positions
that the EGT theoretically supports and that can be queried.
```rust
let position: Chess = "8/8/8/8/8/8/P7/K6k w - -"
    .parse::<Epd>()
    .unwrap()
    .into_position(CastlingMode::Standard)
    .or_else(PositionError::ignore_impossible_check)
	.unwrap()
```

Positions with en-passant rights can also be queried: their outcomes are stored
in dedicated indexes in the tablebase (different from the indexes of the same
positions without en-passant rights). However, note that this applies only for
positions where the en-passant capture is a legal move. If a position is marked
as having en-passant rights but the en-passant capture is not legal, the
en-passant rigths are ignored when querying.

### High-Level overview
The project is built upon these logical building blocks, which are covered in
the following sections:
- **Tablebase organization and naming conventions**: Define how chess positions
  are partitioned into independent endgame tables, with assigned names and with
  an assigned location in a hierarchical folder structure.
- **Outcome representation**: Defines how the game outcome (Win/Loss/Draw),
  depth-to-conversion (DTC), and conversion type (checkmate, capture or
  promotion) are represented into a compact 16-bit value used for in-memory
  operations.
- **Indexing layer (`Egt`, `Indexer`)**: Defines the mapping between chess
  positions and a contiguous index space `[0, index_range)` for each table.
- **Retrograde analysis (`RetrogradeSolver`)**: Explains the back-propagation
  algorithm used to generate the outcomes, starting from terminal positions
  (checkmates and known winning/losing positions) and moving backwards to
  identify all other winning/losing positions.
- **Storage and memory layer (`EgtFileReader`, `EgtFileWriter`,
  `SharedFileCache`)**: Manages the physical access to files, seekable Zstd
  compression/decompression, and in-memory frame cache.

The project focuses on the core tablebase architecture, table generation, and
probing. Various other aspects rely on external dependencies:
- [`shakmaty`](https://crates.io/crates/shakmaty) for chess board manipulation
  and legal move generation. Note: for efficiency, a `shakmaty` fork is used
  that adds a function `Chess::from_setup_unchecked`, used to create positions
  when validity requirements are already known to be fulfilled.
- [`object_store`](https://crates.io/crates/object_store) for cloud-aware file
  input/output.
- [`zeekstd`](https://crates.io/crates/zeekstd) and
  [`zstd`](https://crates.io/crates/zstd) for table compression and
  decompression.

Parallel processing is implemented through [`Rayon`](https://crates.io/crates/rayon).

### Tablebase organization and naming conventions

#### Table scopes and naming
The tablebase is organized as a collection of individual endgame tables. Each
table covers positions with:
1. Fixed material for the side-to-move (STM) and fixed material for the
   side-not-to-move (SNTM).
2. Pawns assigned to fixed files.
3. When multiple pawns are assigned to the same file, their relative order is
   also fixed.

Each table is assigned a canonical name (`TableID`) such as `KQ_KPc`, which
uniquely identifies the material and pawn constraints. The part on the left
(`KQ`) indicates the STM material (king and queen) and the part on the right
(`KPc`) indicates the SNTM material (king and pawn on the _c_ file).

To uniquely establish the relative order of pawns in the same file, the
following convention applies: pawn identifiers (`Pa` through `Ph`) are marked
with a trailing `o` ("opposed") if the **nearest pawn ahead on that file** is
an enemy pawn. A friendly pawn ahead, or no pawn ahead, leaves the identifier
unmarked. Within each file, each side's identifiers run from its own backrank
toward promotion. For example:

| Pawn order from the STM backrank on file _a_ | Table |
| --- | --- |
| SNTM, STM | `KPa_KPa` |
| STM, SNTM | `KPao_KPao` |
| SNTM, STM, STM | `KPaPa_KPa` |
| STM, STM, SNTM | `KPaPao_KPao` |
| STM, SNTM, STM | `KPaoPa_KPao` |

Note that valid `TableId`s must have the same number of `o` markers on both
sides.

The split into individual tables by pawn files and pawn order within the files
has the significant advantage of creating tables that are logically
independent: moves applied to positions within each table remain within the
same table (and the "twin" table with the STM and SNTM swapped) until a
conversion to a simpler endgame occurs. In particular, this support independent
generation of specific tables, such as the tables with _opposing pawns_ (as in
the [op1](https://lichess.org/@/lichess/blog/op1-partial-8-piece-tablebase-available/1ptPBDpC)
project).

#### On-disk layout

On disk, tables are stored in a compressed format inside a hierarchical folder
structure, one file per table. The structure follows the following pattern:
```text
<root>/<n>/<max>v<min>/op<K>/<N>pawns/<tablename>.ggegt
```

In the path, `n` counts all pieces including kings, `max`/`min` are the two
side piece counts in descending order, `K` counts mutually opposing pawn pairs
(once per pair), and `N` counts pawns. So you can have for instance:
```text
<root>/4/2v2/op0/1pawns/KQ_KPc.ggegt
```

A simple JSON file named `format.json` at the root contians a format
descriptor. Currently this is: `{"format":"ggegt-v0.10"}`.

The hierarchical layout described above is designed to expose useful facts
about the dependencies between groups of tables and facilitate grouping tables
that can be generated together in a single batch. In fact, tables residing in
`<n>/<max>v<min>/op<K>/<N>pawns/` need the following dependent tables to be
available before they can be generated:
- Tables in `<n>/<max>v<min>/op<K>/**<N-1>**pawns/` (conversion by promotion
  without capture).
- Tables in `**<n-1>**/` with `max`/`min` reduced at most by 1, with `K`
  changed at most by +/-1, and with `N` reduced at most by 1 (conversion by
  capture).

This means, for example, that if all 6-piece tables are available, then the
following groups of tables can be generated independently from each other:
- `7/4v3/op0/`
- `7/4v3/op1/`
- `7/4v3/op2/`
- `7/5v2/op0/`
- `7/5v2/op1/`
- `7/6v1/op0/`

#### Available statistics

In addition to the compressed table files, the folder structure also hosts
accompaying JSON files with summary statistics. Therefore, the complete
structure looks like this:

```text
<root>/format.json
<root>/<n>/<max>v<min>/op<K>/<N>pawns/<tablename>.ggegt
<root>/<n>/<max>v<min>/op<K>/<N>pawns/<tablename>.json
<root>/<n>/<max>v<min>/op<K>/<N>pawns/stats.json
<root>/<n>/<max>v<min>/op<K>/stats.json
<root>/<n>/<max>v<min>/stats.json
```

The JSON files provide the statistics that are presented when running the
`--stats` command. These can be either at the level of the individual file
(`<tablename>.json`) or at the level of the sub-folder (`stats.json`).

Key statistics include the number of unique positions covered by the selected
table(s) (`unique_positions`) and its split into `win`, `draw`, `loss`. The
number of indexes that do not represent any valid position are also available,
with a split into five categories:
- `redundant`: the index represents a position that is the diagonal-symmetric
  of another position that is represented in the table.
- `invalid_opposite_check`: the index represents a position where the side to
  move attacks the opposite king (including the case of adjacent kings).
- `invalid_no_ep_capture`: the index represents a position with en-passant
  rights but no side-to-move pawn geometrically attacks the target square.
- `invalid_ep_squares_occupied`: the index represents a position with en-passant
  rights and a geometrical capturer, but the target square or the double-push
  origin square is occupied.
- `invalid_illegal_ep_capture`: the index represents a position with en-passant
  rights and at least one pawn that can capture en-passant exists, but none
  of the en-passant captures are legal.

These categories are exclusive: opposite check takes precedence over an
absent capturing pawn, which takes precedence over EP structure failures.

The total number of indexes of the table is stored in `index_range` and
is equal to the number of unique positions plus the five excluded counts.
Further information is also available (size, hash, number of frames, number of wins/losses at each ply, longest wins/losses).

For instance, running `--stats 4/` gives:
```text
  ...
  "index_range": 156098268,
  "bytes": 13931509,
  "num_frames": 688,
  "unique_positions": 125132316,
  "win": 43726797,
  "draw": 37376378,
  "loss": 44029141,
  "invalid_opposite_check": 29591009,
  "invalid_no_ep_capture": 115499,
  "invalid_ep_squares_occupied": 1124,
  "invalid_illegal_ep_capture": 0,
  "redundant": 1258320,
  ...
```

Note that the granular information on invalid indexes can be useful to
reconcile position counts across tablebases that use different methodologies.
In fact, the value of `unique_positions + invalid_ep_squares_occupied` matches
exactly the "Number of Unique Legal Positions in chess endgames"
[here](https://web.archive.org/web/20181125020747/http://kirill-kryukov.com/chess/nulp/results.html)
(also used as a source for the numbers in the
[Wikipedia page](https://en.wikipedia.org/wiki/Endgame_tablebase)). More
precisely, `unique_positions + invalid_ep_squares_occupied` is equal to the
number of _Positions_ minus the number of _Positions with castling rights_.

### Outcome representation
Each position's outcome is represented by a 16-bit `DtcOutcome` value,
structured as follows:
- **Bits 0-2 (WDL and Conversion Type)**:
  - `0b000`: Invalid / Unknown (used for invalid/uncalculated positions)
  - `0b001`: Draw
  - `0b010`: Win - Checkmate can be forced in n plies
  - `0b100`: Win - A capture can be forced in n plies converting to a winning position
  - `0b110`: Win - A promotion can be forced in n plies converting to a winning position
  - `0b011`: Loss - Opponent can force checkmate in n plies (n = 0 for checkmated positions)
  - `0b101`: Loss - Opponent can force a capture in n plies converting to a losing position
  - `0b111`: Loss - Opponent can force a promotion in n plies converting to a losing position
- **Bits 3-15 (Depth to Conversion)**:
  - A 13-bit unsigned integer representing the number of plies to conversion.
    This allows encoding distances up to $2^{13} - 1 = 8191$ plies.

As a result, during generation a table with `n` indexed locations requires
`2n` bytes of allocated memory.

### Indexing layer

#### Indexing and symmetries

For each endgame table, an `Indexer` object handles the mapping of positions
to a contiguous index range `[0, index_range)`. One of the key feature of the
indexing algorithm is that equivalent positions (positions that are the same
up to symmetries) are mapped to the same index, so the outcome for equivalent
position is stored only once.

The indexing strategy used by this EGT distinguishes three symmetry cases.

**Pawnless tables**. Positions without pawns are transformed using the
existing board symmetries (horizontal, vertical, diagonal) into _canonical_
positions with the STM king constrained on the _a1-d1-d4_ triangle and the SNTM
king constrained on the _a1-h1-h8_ triangle, before the position is assigned to
an index.

**Tables with asymmetric pawn arrangements**. These are table like `KPa_K`,
where applying a horizontal reflection to the position produces a position in a
different table (`KPh_K`). For these tables no canonicalization is done, but
the symmetry is exploited by generating only one of the horizontally reflected
tables is generated (`KPa_K` is generated, while `KPh_K` is not).

**Table with symmetric pawn arrangements**: these are tables like `KPaPh_K`,
where applying a horizontal reflection to the position produces a position in
the same table. For these tables the positions are canonicalized by requiring
that the STM and SNTM kings are in a specific order, defined so that the order
is reversed by a horizontal reflection.

In general, the indexing approach is similar to the approaches categorised in
section 2 [here](https://centaur.reading.ac.uk/4563/1/2001a_ACG9_NHH_Space-Efficient_Indexing.pdf)
as "Heinz" and "Wirth", especially for pawnless endgames. Without pawns, the
kings are encoded together by a single index in `[0, 462)` before other pieces
are encoded, the indexing makes sure that no pieces can overlap, and pieces of
the same type and sides are encoded together with a combinatorial index.

Note that there is a special case for pawnless endgames: positions with both
kings on the long diagonal can have a symmetric position (reflecting along the
diagonal) with a different index. The choice made in this project is not
to canonicalize these positions. Instead, both indexes are populated during
generation. However, only one of those indexes counts for the statistics, while
the other is counted as `redundant`.

In the case of tables with pawns, the indexing approach deviates a bit from
common strategies used by other EGTs, due to the choice of "pawn-first"
partitioning described above, which influences the indexing strategy. For
instance, kings are not encoded together as an index in `[0, 1806)`, instead
they are encoded individually after pawns are encoded. This is slightly less
compact with one pawn (1806 < 63 * 62 / 2), but becomes comparable with more
pawns. It also has the very significant advantage of being a simple approach
with a relatively understandable implementation, avoiding
[monsters like this](https://github.com/lichess-org/op1/blob/main/mbeval-sys/mbeval/src/mbeval.c).

En-passant positions are encoded separately in different ranges of indexes
(sub-tables) managed internally by the `Indexer`. The `Indexer` exposes a
single unified index range for the entire table.

##### Detailed indexing algorithm (`position_to_index`)
The `Indexer` maps a chess position to a unique integer in `0..index_range`
using a compaction and combinatorial encoding process:
- Only for symmetric pawn tables, a horizontal mirroring is applied if needed
  to make sure the kings are in the correct order.
- The indexer checks whether a legal en-passant capture is available. A raw EP
  square without a legal capture is equivalent to no EP.
- If so, it selects the corresponding sub-table. The SNTM target pawn is the
  highest pawn on its file in STM-normalized ranks, fixed on rank 5, and is
  ignored by later encoding steps as its presence is implicit. The remaining
  pawns on that file can only occupy ranks 2–4.
- The board position of each piece is converted into coordinates
  `(rank, file)`.
- For each file, pawns are gathered in descending rank order on that file
  (irrespective of side) and form a group. Repeated non-pawn pieces of the same
  side are sorted by descending square number and form a group.
- If there are no pawns, the STM king position is restricted to 10 canonical
  squares (representing one octant of the board) using horizontal, vertical,
  and diagonal reflections. All other pieces are reflected accordingly to
  preserve relative positions.
- Each coordinate is converted to a raw square index in `0..64`:
  `rank * 8 + file`, except for symmetric pawn tables, for which square numbers
  are `8 * file + rank` on files a–d and `8 * file + (7 - rank)` on e–h. In
  this way, horizontal reflection sends square number `i` to `63 - i`, so that
  exactly one of the reflected or non-reflected positions will have the STM and
  SNTM king in the right order.
- Since no two pieces can occupy the same square, the raw square indices are
  compacted to remove "holes" caused by already occupied squares. For each
  piece, its index is adjusted by subtracting the number of preceding pieces
  that occupy a lower square index. This maps the indices sequentially to
  ranges `[0..64, 0..63, 0..62, 0..61, ...]`.
- Pawns are restricted to ranks 2-7 (6 possible squares on their designated
  file). Their positions are converted to compact indices in `0..6` (or `0..3`
  in case of en-passant).
- A group of $k$ pawns on a file is encoded together using $\binom{6}{k}$
  indexes (or $\binom{3}{k-1}$ in case of en-passant).
- For pawnless endgames, a precomputed lookup table (`KINGS_MAP`) maps valid
  canonical king pairs (excluding adjacent or identical squares) to a single
  index in `0..462`; symmetric pawn tables encode the two kings as a single
  group of two identical pieces.
- Groups of pieces are encoded together into a single integer using a
  mixed-radix system. A group of $k$ pieces contributes $\binom{n}{k}$
  combinations among $n$ available squares.
- The combination index for these $k$ pieces with compacted indices
  $c_0 > c_1 > \dots > c_{k-1}$ is computed using the combinatorial number
  system (combinadics): $\text{group\_index} = \sum_{j=0}^{k-1} \binom{c_j}{k-j}$
- These group indexes are then combined using a mixed-radix base where the
  multiplier for each group is the total number of combinations of the
  subsequent groups.
- The resulting index specific to the selected (en-passant-specific) sub-table
  is then mapped to a table-specific index.

### Retrograde analysis
Retrograde analysis generates endgame tablebases by working backward from
terminal positions (checkmates, stalemates, and conversions) to determine the
outcome and depth-to-conversion (DTC) for all other positions.

For any pair of tables (e.g., `KQ_KPa` and `KPa_KQ`), retrograde analysis runs
on both tables simultaneously. Both tables are initialized and propagated
together.

Dependencies are queried during the initialization phase: when positions have
available captures (including en-passant) or promotions converting to a
simpler endgame, the corresponding dependencies are probed to determine the
outcome after conversion.

#### Move counters
During the retrograde propagation, each unresolved position-index has a
decremental move counter of its remaining legal moves after excluding those
that are certainly losing. To achieve zero memory overhead, this counter is
stored directly in the 13 unused bits of the 'invalid/unknown' state: an
'unknown' position with `C` remaining moves is represented as
`0b000 | (C << 3)`.

The move counter is generally initialized as the number of legal moves in a
position. But note that there are interactions between the move counter
initialization and the use of symmetries to canonicalize pawnless positions:
there is the possibility that different legal moves result in positions that
map to the same index, and similarly the retrograde analsys can find different
reverse moves that map to the same index. For reference see section 4.6
[here](https://issuu.com/jespertk/docs/master_thesis).

A formal approach is the following. Let's say a position `p` has `#p=8` if it
is equivalent to 8 positions and `#p=4` if it is equivalent to 4 positions
(with our choice of canonicalization, `#p=4` positions are positions with both
kings on a long diagonal). When initializing the counters for an index
corresponding to a canonical position `p`, if there is a legal move `p -> p'`
with `#p=8` and `#p'=4`, then there is another move (the symmetric along the
diagonal) which goes from a non canonical position (the reflection of `p`
along the diagonal) to a canonical position (the reflection of `p'` along the
diagonal), which will be explored during backward propagation. To account for
this, moves `p -> p'` with `#p=8` and `#p'=4` should increment the counter by 2
during initialization. Similarly, when retrograde propagation from an index
corresponding to a canonical position `p'` finds a move `p -> p'` with `#p=4`
and `#p'=8`, in addition to decrementing the counter for `p`, the counter for
the reflection of `p` along the diagonal should also be decremented (since the
symmetric move contributed to the counter for the reflection of `p` but led to
a non-canonical position).

#### Initialization phase
Both tables in the pair being generated are initialized independently. For each of them, a full scan of the table is performed:
1. Indexes are decoded, marking as invalid those that do not correspond to a
   valid position.
2. For valid positions, legal moves are generated. An empty list means
   checkmate when in check (position resolved as a loss at ply 0), or
   stalemate (draw) otherwise.
3. The move counters are initialized with the number of legal moves, with the
   pawnless symmetry adjustment described above.
4. Legal captures/promotions are played and the associated dependency tables
   are probed. A losing successor resolves the position as a win at ply 1.
   Each winning successor removes one move from its counter. A zero counter
   resolves the position as a loss at ply 1.

#### The propagation loop
The positions to propagate at ply _n_ are found by scanning the tables for
the the value of positions resolved at ply _n-1_. The main loop runs for $n =
1, 2, \dots$ until no new positions are marked:
1. **Propagate losses to wins:**
   - Scan Table A for positions marked as 'loss (conversion_type, n-1)'. For
     each of them:
     - Generate reverse non-capture non-promotion moves (`quiet_unmoves`).
     - Mark the predecessors in Table B as 'win (conversion_type, n)' if they
       are currently 'unknown'. A predecessor reached by several losses keeps
       the preferred conversion type (Checkmate > Capture > Promotion).
   - Do the same for Table B, propagating to Table A (unless A and B are the
     same table).
2. **Propagate wins to losses (decrement counters):** for each conversion
   type, in the order Checkmate, Capture, Promotion:
   * Scan Table A for positions marked as 'win (conversion_type, n-1)'. For
     each of them:
     * Generate reverse non-capture non-promotion moves (`quiet_unmoves`).
     * For each 'unknown' predecessor, decrement its move counter by 1.
     * If the counter reaches 0, mark the predecessor as 'loss
       (conversion_type, n)'.
   * Do the same for Table B, propagating to Table A (unless A and B are the
     same table).
3. If no new positions were marked in this iteration, stop.
4. Increment _n_.
5. At the end of the process mark all 'unknown' positions as draws.

A phase only scans values at ply _n-1_ and only writes values at ply _n_ (or
counters), so the result does not depend on the order in which positions are
visited. Running the three conversion types separately makes the conversion
type of a loss deterministic (the last one to decrement its counter, i.e.
Promotion > Capture > Checkmate).

To make the scanning more efficient, a record is kept of the highest ply
resolved in each 4096-positions block (`PlySummary`), and the type of
resolution. In this way, blocks can be skipped if the record indicates that no
positions were resolved at that ply for the given resolution.

#### Alternative approach for initialization

An alternative to forward dependency probing would be to scan simpler endgames
and generate reverse capture/promotion moves to update their predecessors.
These unmove functions are not part of the current implementation.

If this approach was used, note that special care should be given to unmoves
from a pawnless successor (which has 8-way symmetry) to a predecessor with
pawns (which has 2-way horizontal symmetry). The process would be to
reconstruct the 4 rotations of the canonical pawnless board, then call the
retrograde unmove function on each of the 4 rotations. For each resulting
predecessor board, if the newly placed pawn lands on files e–h, horizontally
reflect the board to files a–d to canonicalize.

### Storage and memory layer

#### File format and compression
Each canonical table is one seekable Zstd object (via `zeekstd`).

The format of the compressed tables (`.ggegt`) has the following characteristics:
- The uncompressed data is split into frames of `256 * 1024`
  positions which are compressed independently, allowing seekable random
  access.
- A seek table (without checksum) of `17 + 8 * num_frames` bytes is appended
  in the footer frame. The footer frame count is checked before parsing to
  bound allocation; compressed and decompressed lengths are also validated.
- Readers cache the seek table and fetch one compressed range per each frame
  no already uncompressed in memory.
- Before applying Zstd compression to a frame of _N_ positions, the 16-bit
  `DtcOutcome` values are transposed to maximize compressibility. They are
  reshaped as a sequence of bytes by taking:
  1. First: the low byte of all _N_ outcomes (_N_ bytes).
  2. Second: the high byte of all _N_ outcomes, skipping the (unused) high
     byte for invalid and drawn positions (max _N_ bytes).
  The transposed buffer is always exactly _2*N_ bytes: after the values are
  transposed, the unused tail remains zero.

Frames are compressed in parallel and the result are collected. An
`EgtFileWriter` then streams them in order and appends one seek table through
the `object_store` buffered writer.

#### Memory management

Generation uses a pair of flat `Vec<AtomicU16>` arrays for the pair being
solved. These arrays are allocated with `try_reserve_exact`, so allocation
failure is reported as an error. Finalization processes one table at a time in
parallel frame batches. Batches use the same Rayon pool as generation, with at
most one encoder per configured thread.

Probing (`EgtProber`) uses a shared cache that keeps track of frames already
available in memory, evicting the least recently used stored frames when
memory use exceeds a set threshold (`--cache-size`).
