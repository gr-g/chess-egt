# gg-EGT: A Chess Endgame Tablebase
gg-EGT is an experimental [chess endgame tablebase](https://en.wikipedia.org/wiki/Endgame_tablebase)
(EGT) written in Rust.

## Current status

All tables with up to five pieces have been generated and verified. The design
supports positions with up to eight pieces.

The library API, command-line interface, and file format are not yet finalized.

## Goals
- Provide a tablebase with depth-to-conversion (DTC) outcomes including the
  conversion type (checkmate, capture or promotion), which can be especially
  useful for studying endgame theory or practicing endgames against the
  computer.
- Provide readable Rust code that others can understand, modify, and reuse,
  while keeping performance close to that of more heavily optimized (but
  somewhat impenetrable) codebases.
- Use a modern approach for the generation and storage of endgame tables,
  producing separate tables for independent pawn configurations (generalizing
  the approach of the [op1](https://lichess.org/@/lichess/blog/op1-partial-8-piece-tablebase-available/1ptPBDpC)
  tablebase).
- Support scaling on cloud resources (compute-optimized instances and cloud
  object storage) to potentially reach 7-piece completeness and beyond.

This tablebase is intended as a resource for online lookup. Is is not intended
for use in chess engines, where the DTZ metric is most suitable and where
there compact win/draw/loss representations are needed.

## Usage

### Quick start

```sh
# Generate all tables with two or three pieces.
cargo run --release -- --generate 2/ 3/

# Probe a position.
cargo run --release -- '8/8/8/8/8/2K5/4Q3/7k w - -'
```

Tables are stored in `./ggegt` by default. Capture and promotion dependencies
must already exist or be explicitly selected.

### Additional commands

```sh
# Generate all 4-piece tables (assuming dependencies already exist), using 4 threads.
cargo run --release -- --path ./ggegt --generate 4/ --threads 4

# Generate selected tables locally, using existing dependencies from an S3 bucket.
cargo run --release -- --path ./ggegt --input-path s3://my-ggegt-bucket/ --generate KQ_KR KP_K

# Verify the internal consistency of existing tables.
cargo run --release -- --verify KQ_KR KR_KQ

# Enumerate theoretical tables having 5 pieces with 3 on one side.
cargo run --release -- --list 5/3v2/

# Report statistics on all 4-piece tables.
cargo run --release -- --stats 4/

# Store summary statistics for groups of tables for faster retrieval.
cargo run --release -- --cache-stats 2/ 3/ 4/ 5/
```

For S3 access, configure credentials through environment variables using the
conventions supported by [`object_store`](https://docs.rs/object_store/latest/object_store/aws/struct.AmazonS3Builder.html#method.from_env).

`--generate`, `--verify`, `--stats`, `--cache-stats`, and `--list` each accept
multiple `SELECTOR`s as arguments, in the following forms:

- **Exact names**, e.g. `KPa_K`: a table identified by its canonical name (see
  below for table naming conventions).
- **Material configurations**, e.g. `KP_K`: all tables for that material
  balance.
- **Hierarchy prefixes**, e.g.
  - `3/`: all tables with 3 pieces.
  - `4/2v2/op1`: all tables with 4 pieces, 2 per side, with one pair of opposing
    pawns.
  - `5/3v2/op1/2pawns`: all tables with 5 pieces, 3 on one side and 2 on the
    other, with one pair of opposing pawns and 2 pawns in total.

Tables are generated in pairs: `--generate KPa_K` will produce a file for
`KPa_K` (representing positions where the side-to-move has the pawn) and also a
file for the "twin" table `K_KPa` (representing positions where the
side-not-to-move has the pawn).

Run the test suite with:
```sh
cargo test
```

Two Python scripts are available to export statistics from a current tablebase.
```sh
cargo run --release -- --path ROOT --cache-stats 3/ 4/ 5/ 6/
python3 scripts/export_table_stats.py ROOT table_stats.csv
python3 scripts/export_summary_stats.py ROOT summary_stats.csv
```


### API server

Run an API server with tables read from an S3 bucket:

```sh
cargo run --release --bin chess-egt-server -- \
  --path s3://my-ggegt-bucket/ --cache-size 1GiB --bind localhost:44348

# Get statistics on 3-piece endgames
curl --get http://localhost:44348/stats?tables=3

# Query a position
curl --get http://localhost:44348/query?fen=8/8/8/8/8/2K5/6P1/7k_w_-_-
```

The JSON response with the position outcome is similar to the
[Lichess tablebase API](https://lichess.org/api#tag/tablebase):

```json
{
  "checkmate": false,
  "stalemate": false,
  "insufficient_material": false,
  "dtc": 9,
  "dtc_type": "promotion",
  "category": "win",
  "moves": [
    {
      "uci": "g2g4",
      "san": "g4",
      "conversion": false,
      "checkmate": false,
      "stalemate": false,
      "insufficient_material": false,
      "dtc": -8,
      "dtc_type": "promotion",
      "category": "loss"
    },
    ...
  ]
}
```

## Design and implementation

- [Scope](#scope)
- [High-level overview](#high-level-overview)
- [Outcome representation](#outcome-representation)
- [Tablebase organization and naming conventions](#tablebase-organization-and-naming-conventions)
- [Indexing layer](#indexing-layer)
- [Retrograde analysis](#retrograde-analysis)
- [Storage and memory layer](#storage-and-memory-layer)

### Scope
The tablebase supports valid positions with at most eight pieces and no
castling rights.

(According to traditional terminology, we should say _8 [men](https://en.wikipedia.org/wiki/Chess_piece#Definitions)_
instead of _8 pieces_ to make it clear that pawns are counted among the 8
pieces, but we do not make this distinction here and just use _pieces_
everywhere.)

As usual for the DTC metric, threefold repetition, the fifty-move rule, and
the seventy-five-move rule are ignored.

A position is considered _valid_ if it fulfills
[`shakmaty`'s validity requirements](https://docs.rs/shakmaty/latest/shakmaty/struct.PositionErrorKinds.html),
except that "impossible checks", such as three pieces giving check
simultaneously, are accepted.

In other words, within the piece-count and castling restrictions above, you
can construct a supported position with the following code:
```rust
use shakmaty::{fen::Epd, CastlingMode, Chess, PositionError};

let position: Chess = "8/8/8/8/8/8/P7/K6k w - -"
    .parse::<Epd>()
    .unwrap()
    .into_position(CastlingMode::Standard)
    .or_else(PositionError::ignore_impossible_check)
    .unwrap();
```

Positions with en passant rights use dedicated indices, distinct from those
for the same positions without en passant rights. These rights affect the
index only when at least one en passant capture is legal; otherwise, probing
ignores them.

### High-level overview
The following sections describe the tablebase's main building blocks:
- **Outcome representation**: Encodes win/draw/loss, DTC, and conversion type
  in a compact 16-bit value.
- **Tablebase organization and naming conventions**: Partitions positions into
  endgame tables and assigns each table a name and storage path.
- **Indexing layer (`Egt`, `Indexer`)**: Maps positions to a contiguous index
  range `[0, index_range)` within each table.
- **Retrograde analysis (`RetrogradeSolver`)**: Computes outcomes by propagating
  known results backward to predecessor positions.
- **Storage and memory layer (`EgtFileReader`, `EgtFileWriter`,
  `SharedFileCache`)**: Reads and writes compressed tables and caches
  decompressed frames.

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

### Outcome representation
Outcomes are measured from the perspective of the side to move. A ply is one
move by one player. While preserving the win/loss outcome, the winning side
minimizes the number of plies to conversion and the losing side maximizes it.
A conversion is a capture, promotion, or checkmate. Draws have no DTC.

At equal distances, wins prefer checkmate, then capture, then promotion;
losses prefer promotion, then capture, then checkmate. A capturing promotion
is classified as a capture.

The public `DtcOutcome` enum represents a win, draw, or loss. During generation
and storage, outcomes use a packed 16-bit representation, `MaybeDtcOutcome`,
which also represents invalid or unresolved entries. It is structured as
follows:
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

As a result, the working outcome array for a table with _n_ indexed locations
requires _2n_ bytes, which is the main memory cost of generation. Some
additional memory overhead comes from block summaries, scan buffers,
dependency-cache entries, and compression buffers.

### Tablebase organization and naming conventions

#### Table scopes and naming
The tablebase is organized as a collection of individual endgame tables. Each
table covers positions with:
1. Fixed material for the side-to-move (STM) and fixed material for the
   side-not-to-move (SNTM).
2. Pawns assigned to fixed files.
3. When multiple pawns are assigned to the same file, their relative order is
   also fixed.

Each table is assigned a canonical name (`TableId`) such as `KQ_KPc`, which
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

A move that is neither a capture nor a promotion preserves material, pawn
files, and the relative order of pawns within each file. As a result, until a
capture or promotion occurs, positions remain confined to the same table and
its twin (the table with the STM and SNTM swapped).

This partitioning lets the generator solve each table pair once its
dependencies are available. It also allows generation of tables with specific
configurations, such as the tables with _opposing pawns_ (as in the
[op1](https://lichess.org/@/lichess/blog/op1-partial-8-piece-tablebase-available/1ptPBDpC)
project).

#### On-disk layout

Each table is stored as one compressed file in a hierarchical directory
structure. Table paths have this form:
```text
<root>/<n>/<max>v<min>/op<K>/<N>pawns/<tablename>.ggegt
```

In the path, `n` counts all pieces including kings, `max`/`min` are the two
side piece counts in descending order, `K` counts mutually opposing pawn pairs
(once per pair), and `N` counts pawns. For example:
```text
<root>/4/2v2/op0/1pawns/KQ_KPc.ggegt
```

A simple JSON file named `format.json` at the root contains a format
descriptor. Currently this is: `{"format":"ggegt-v0.11"}`.

The hierarchical layout described above is designed to expose useful facts
about the dependencies between groups of tables and facilitate grouping tables
that can be generated together in a single batch. In fact, tables residing in
`<n>/<max>v<min>/op<K>/<N>pawns/` need the following dependent tables to be
available before they can be generated:
- Tables in `<n>/<max>v<min>/op<K>/<N-1>pawns/` (conversion by promotion
  without capture).
- Tables in `<n-1>/` (conversion by capture, including capturing promotions).
  Captures may also change the material split, pawn count, and number of
  opposing pawn pairs.

This means, for example, that if all 6-piece tables are available, then the
following groups of tables can be generated independently from each other:
- `7/4v3/op0/`
- `7/4v3/op1/`
- `7/4v3/op2/`
- `7/5v2/op0/`
- `7/5v2/op1/`
- `7/6v1/op0/`

#### Available statistics

In addition to the compressed table files, the directory structure also hosts
accompanying JSON files with summary statistics. Therefore, the complete
structure looks like this:

```text
<root>/format.json
<root>/<n>/<max>v<min>/op<K>/<N>pawns/<tablename>.ggegt
<root>/<n>/<max>v<min>/op<K>/<N>pawns/<tablename>.json
<root>/<n>/<max>v<min>/op<K>/<N>pawns/stats.json
<root>/<n>/<max>v<min>/op<K>/stats.json
<root>/<n>/<max>v<min>/stats.json
```

The `--stats` command reports statistics for individual tables
(`<tablename>.json`) or groups of tables (`stats.json`).

Key statistics include the number of unique positions covered by the selected
tables (`unique_positions`) and the breakdown into `win`, `draw`, and `loss`.
Indices excluded from `unique_positions` are also counted, with a split into
redundant representations and three categories of invalid positions:
- `redundant`: the index represents the diagonal reflection of a position
  represented by another index in the table. The position may itself be valid
  or invalid.
- `invalid_opposite_check`: the index represents a position where the side to
  move attacks the opposite king (including the case of adjacent kings).
- `invalid_ep_squares_occupied`: the index represents a position with en passant
  rights and a geometrical capturer, but the target square or the double-push
  origin square is occupied.
- `invalid_illegal_ep_capture`: the index represents a position with en passant
  rights where at least one pawn geometrically attacks the target square, but
  no en passant capture is legal.

These categories are exclusive: redundancy takes precedence over opposite
check, followed by occupied EP squares and illegality of all EP captures.
EP states without a geometrical capturing pawn are excluded from the index
range entirely.

The total number of indices in the table is stored in `index_range` and
is equal to the number of unique positions plus the four excluded counts.
Further information is also available (size, hash, number of frames, number of
wins/losses at each ply, longest wins/losses).

Note that the granular information on invalid indices can be useful to
reconcile position counts across tablebases that use different methodologies.
In fact, the value of `unique_positions + invalid_ep_squares_occupied +
invalid_illegal_ep_capture` matches exactly the
[Number of Unique Legal Positions in chess endgames](https://web.archive.org/web/20181125020747/http://kirill-kryukov.com/chess/nulp/results.html)
(also used as a source for the numbers in the
[Wikipedia page](https://en.wikipedia.org/wiki/Endgame_tablebase)). More
precisely, `unique_positions + invalid_ep_squares_occupied +
invalid_illegal_ep_capture` is equal to the number of _Positions_ minus the
number of _Positions with castling rights_.

### Indexing layer

#### Indexing and symmetries

For each endgame table, an `Indexer` maps positions to a contiguous index range
`[0, index_range)`. It uses board symmetries to reduce duplicate
representations, making sure that symmetry-equivalent positions share the same
index, except for the pawnless diagonal-symmetry case
described below.

The indexing strategy used by this EGT distinguishes three symmetry cases.

**Pawnless tables**. Positions without pawns are transformed using the
existing board symmetries (horizontal, vertical, diagonal) into _canonical_
positions with the STM king constrained to the _a1-d1-d4_ triangle. If the STM
king lies on the _a1-h8_ diagonal, the SNTM king breaks the remaining diagonal
symmetry by being constrained to the _a1-h1-h8_ triangle. When both kings lie
on the diagonal, duplicate representations are retained as described below.

**Tables with asymmetric pawn arrangements**. In tables such as `KPa_K`,
reflecting a position horizontally produces a position in a different table
(`KPh_K`). Only one table from each reflected pair is generated: `KPa_K`, for
example, rather than `KPh_K`. Positions in the reflected table are probed by
reflecting them into the generated table.

**Tables with symmetric pawn arrangements**. In tables such as `KPaPh_K`,
reflecting a position horizontally produces a position in the same table. The
indexer chooses a canonical orientation by requiring the STM and SNTM kings to
appear in a fixed order. Horizontal reflection reverses this order, so exactly
one orientation satisfies the requirement.

In general, the indexing approach is similar to the approaches categorized as
"Heinz" and "Wirth" in section 2 of [this paper](https://centaur.reading.ac.uk/4563/1/2001a_ACG9_NHH_Space-Efficient_Indexing.pdf),
especially for pawnless endgames. In pawnless tables, the kings are encoded
together as a single index in `[0, 462)` before the other pieces. The encoding
prevents pieces from overlapping and uses a combinatorial index for pieces of
the same type and side.

Pawnless positions with both kings on the a1–h8 diagonal are a special case:
diagonal reflection can produce a different index. Both indices are populated
during generation, but only one contributes to the unique-position statistics;
the other is counted as `redundant`.

In the case of tables with pawns, the indexing approach deviates a bit from
common strategies used by other EGTs, due to the choice of "pawn-first"
partitioning described above, which influences the indexing strategy. For
instance, kings are not encoded together as an index in `[0, 1806)`; instead,
they are encoded after the pawns without overlapping with them. This is
slightly inefficient with one pawn, but becomes better with more pawns. It
also has the very significant advantage of being a simple and general approach
with a relatively understandable implementation. Custom compact
representations through careful index partitioning, as in the [syzygy](https://github.com/syzygy1/tb8gen/blob/master/src/index.c)
or [op1](https://github.com/lichess-org/op1/blob/main/mbeval-sys/mbeval/src/mbeval.c)
tablebases, are more efficient, but also more cumbersome.

The positions of the pawns on the ranks and the en passant rights are encoded
together into a combined pawn index. A precomputed array maps a base-six
encoding of the pawn ranks and an en passant selector to this index.
For each combined pawn index, the positions of the other pieces are
combinatorially encoded without overlaps into a fixed-size range. The
`Indexer` exposes one contiguous index range for the entire table, combining
the pawn and non-pawn indices.

For the detailed indexing algorithm, see the module documentation in
[`src/egt/indexer.rs`](src/egt/indexer.rs).

### Retrograde analysis
Retrograde analysis generates endgame tablebases by working backward from
terminal positions (checkmates, stalemates, and conversions) to determine the
outcome and depth-to-conversion (DTC) for all other positions.

For any pair of tables (e.g., `KQ_KPa` and `KPa_KQ`), retrograde analysis runs
on both tables simultaneously. Both tables are initialized and propagated
together.

Dependencies are queried during the initialization phase: when positions have
available captures (including en passant) or promotions converting to a
simpler endgame, the corresponding dependencies are probed to determine the
outcome after conversion.

#### Move counters
During retrograde propagation, each unresolved position index has a
decremental move counter of its remaining legal moves after excluding those
that are certainly losing. To achieve zero memory overhead, this counter is
stored directly in the 13 unused bits of the 'invalid/unknown' state: an
'unknown' position with `C` remaining moves is represented as
`0b000 | (C << 3)`.

The move counter is generally initialized to the number of legal moves in a
position. In pawnless tables, symmetry reduction can change how often a move
is encountered during backward propagation. Initialization and propagation
adjust counter updates accordingly. For the formal explanation, see
"Move counters and diagonal symmetry" in the module documentation in
[`src/retrograde.rs`](src/retrograde.rs).

#### Initialization phase
Both tables in the pair being generated are initialized independently. For each of them, a full scan of the table is performed:
1. Indices are decoded, marking as invalid those that do not correspond to a
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
At iteration _n_, the solver scans both tables for positions resolved at ply
_n−1_ and propagates their outcomes to predecessor positions. Iterations start
at _n = 1_ and continue until no new positions are resolved:
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
5. At the end of the process, mark all 'unknown' positions as draws.

A phase only scans values at ply _n-1_ and only writes values at ply _n_ (or
counters), so the result does not depend on the order in which positions are
visited. Running the three conversion types separately makes the conversion
type of a loss deterministic (the last one to decrement its counter, i.e.
Promotion > Capture > Checkmate).

To make the scanning more efficient, a record is kept of the highest ply
resolved in each 4096-position block (`PlySummary`) and of the type of
resolution. In this way, blocks can be skipped if the record indicates that no
positions were resolved at that ply for the given resolution.

#### Alternative approach for initialization

An alternative to forward dependency probing would be to scan simpler endgames
and generate reverse capture/promotion moves to update their predecessors.
These unmove functions are not part of the current implementation.

If this approach were used, special care would be needed for unmoves
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
  positions, which are compressed independently, allowing seekable random
  access.
- A seek table (without checksum) of `17 + 8 * num_frames` bytes is appended
  in the footer frame. The footer frame count is checked before parsing to
  bound allocation; compressed and decompressed lengths are also validated.
- Readers cache the seek table. To load a frame not already cached, they fetch
  its compressed byte range and decompress it.
- Before applying Zstd compression to a frame of _N_ positions, the 16-bit
  `MaybeDtcOutcome` values are transposed to maximize compressibility. They are
  reshaped as a sequence of bytes by taking:
  1. First: the low byte of all _N_ outcomes (_N_ bytes).
  2. Second: the high byte of all _N_ outcomes, skipping the (unused) high
     byte for invalid and drawn positions (max _N_ bytes).
  The transposed buffer is always exactly _2*N_ bytes: after the values are
  transposed, the unused tail remains zero.

Frames are compressed in parallel and the results are collected. An
`EgtFileWriter` then streams them in order and appends one seek table through
the `object_store` buffered writer.

#### Memory management

Generation uses a pair of flat `Vec<AtomicU16>` arrays for the pair being
solved. These arrays are allocated with `try_reserve_exact`, so allocation
failure is reported as an error. Finalization processes one table at a time in
parallel frame batches. Batches use the same Rayon pool as generation, with at
most one encoder per configured thread.

Probing (`EgtProber`) uses a shared cache that keeps track of frames already
available in memory, evicting the least recently used stored frames to keep
retained frame data within the configured cache budget (`--cache-size`).
