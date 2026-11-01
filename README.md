# GG Endgame Tablebase
Work-in-progress repository with experiments to build from scratch a [Chess Endgame Tablebase](https://www.chessprogramming.org/Endgame_Tablebases).

Goals:
 - Provide tablebases with the Depth to Conversion (DTC) metric, particularly suitable for applications such as practicing endgames against the computer.
 - Provide a clean, readable Rust codebase that people interested in generating chess tablebase can understand, modify and use in other projects, even if performance is not super-optimized as in the state-of-the-art [Syzygy](https://www.chessprogramming.org/Syzygy_Bases) code.

## Usage

```sh
# Generate the prerequisites and all 3-piece tables locally.
cargo run --release -- --generate 2/ 3/

# Generate all 4-piece tables once their dependencies exist, using 4 threads.
cargo run --release -- --path ./ggegt --generate 4/ --threads 4

# Generate selected tables using existing dependencies from S3.
cargo run --release -- --path ./ggegt --input-path s3://ggegt/ --generate KQ_KR KP_K

# Verify existing tables.
cargo run --release -- --verify KQ_KR KR_KQ

# Enumerate theoretical tables.
cargo run --release -- --list 3/ 4/2v2/op1

# Report statistics, including theoretical sizes of missing tables.
cargo run --release -- --stats 4/

# Store summary statistics for groups of tables for faster retrieval.
cargo run --release -- --cache-stats 4/

# Probe a table.
cargo run --release -- --path ./ggegt '8/8/8/8/8/2K5/4Q3/7k w - -'
```

`--generate`, `--verify`, `--stats`, `--cache-stats`, and `--list` each accept multiple `SELECTOR`s of a single kind:

- **Exact name**, e.g. `KPa_K`: one canonical table, with STM on the left.
- **Oriented material**, e.g. `KP_K`: all tables for that material balance (`K_KP` is not included).
- **Hierarchy prefix**, e.g. `3/`, `4/2v2/op1`, or `4/2v2/op1/2pawns`. The trailing slash is optional.

Generating a table such as `KPa_K` will also generate the twin table `K_KPa`.
Verification, statistics, and statistics caching do not consider twin tables.

Cloud credentials/configuration come from environment variables supported by
[`object_store`](https://docs.rs/object_store/latest/object_store/). HTTP roots
are read-only. For generation, verification, and probing, `--input-path` is
searched before `--path`, falling back only for missing objects.

Run the test suite with:

```sh
cargo test --release
```

Chess rules and position validation use a `shakmaty` fork that adds
`Chess::from_setup_unchecked` for callers that already establish position
invariants.

Two Python scripts export statistics from a **local, current tablebase root**.

```sh
cargo run --release -- --path ROOT --cache-stats 3/ 4/ 5/ 6/
python3 scripts/export_table_stats.py ROOT table_stats.csv
python3 scripts/export_summary_stats.py ROOT summary_stats.csv
```

## Design Specifications

### 1. High-Level Architecture
The project is built from the following main components:
1. **Outcome Representation (`DtcOutcome`)**: Encodes the game outcome (Win/Loss/Draw), distance-to-conversion (DTC), and conversion type (Checkmate, Promotion, or Capture) into a compact 16-bit value.
2. **Logical Indexing Layer (`Egt`, `Indexer`)**: Maps canonical chess board positions to a contiguous index space `[0, index_range)`.
3. **Storage & Memory Layer (`EgtFileReader`, `EgtFileWriter`, `SharedFileCache`)**: Manages the physical access to files (via `object_store`), seekable Zstd compression/decompression (via `zeekstd`), and in-memory frame cache.
4. **Retrograde Analysis (`RetrogradeSolver`)**: The backward-propagation algorithm to generate the outcomes, starting from terminal positions (checkmates and known winning/losing positions) and moving backwards to identify all other winning/losing positions.

### 2. Outcome Representation (`DtcOutcome`)
Each position's outcome is represented by a 16-bit `DtcOutcome` value.

The 16 bits of a `DtcOutcome` are structured as follows:
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
  - A 13-bit unsigned integer representing the number of plies to conversion. This allows encoding distances up to $2^{13} - 1 = 8191$ plies.

### 3. Indexing

The tablebase is designed to store DTC outcomes for a set of chess positions (_queryable positions_) defined as: _valid positions_ with up to 8 pieces without castling rights.

For this purpose, a position is considered _valid_ if it fulfills the validity requirements defined by the `shakmaty` library [here](https://docs.rs/shakmaty/latest/shakmaty/struct.PositionErrorKinds.html), except that positions with "impossible checks" (such as positions where three pieces give a check at the same time) are also considered valid. In other words, `shakmaty::Chess` objects (with up to 8 pieces and without castling rights) returned from code such as the following correspond to positions that can be successfully queried.

```rust
let position: Chess = "8/8/8/8/8/8/P7/K6k w - -"
    .parse::<Epd>()
    .unwrap()
    .into_position(CastlingMode::Standard)
    .or_else(PositionError::ignore_impossible_check)?
```

Positions with en-passant rights can also be queried: their outcomes are stored in dedicated indexes in the tablebase (different from the indexes of the same positions without en-passant rights). However this applies only for positions where the en-passant capture is a legal move. If not, the en-passant rights are ignored.

#### 3.1 `Egt` and `Indexer`
An `Egt` contains positions with fixed material, pawn files, and the exact color order of pawns on each file. An `Egt` has a name such as `KQ_KPc` (king and queen versus king and pawn on the _c_ file). The name has the side-to-move (STM) on the left and the side-not-to-move (SNTM) on the right.

Pawn tokens are `Pa` through `Ph`, with a trailing `o` ("opposed") if the **nearest pawn ahead on that file** is an enemy pawn. A friendly pawn ahead, or no pawn ahead, leaves the token unmarked. Within each file, each side's tokens run from its own backrank toward promotion. For example:

| Pawn order from the STM backrank on file a | Table |
| --- | --- |
| SNTM, STM | `KPa_KPa` |
| STM, SNTM | `KPao_KPao` |
| SNTM, STM, STM | `KPaPa_KPa` |
| STM, STM, SNTM | `KPaPao_KPao` |
| STM, SNTM, STM | `KPaoPa_KPao` |

For each file, both sides must have the same number of `o` markers. Together with the token order this uniquely determines the color sequence.

Internally, material is represented by a vector of `EgtElement`: an element encodes either a group of pawns on the same file with a specific sequence of colors, or a group of non-pawn pieces of one type and color.

The `Indexer` handles the mapping of positions to a contiguous local index range `[0, index_range)` with three symmetry cases:

- **Pawnless tables:** use the existing board symmetries (horizontal, vertical, diagonal) to assign equivalent positions to the same index.
- **Tables with asymmetric pawn key:** only one of the two horizontally reflected tables is generated.
- **Table with symmetric pawn key:** the complete pawn arrangement on each file equals that on its reflected file. Keep one position per horizontal-reflection pair by ordering the STM and SNTM kings, and encode them jointly as a two-square combination.

For symmetric pawn tables, square numbers are `8 * file + rank` on files a–d and `8 * file + (7 - rank)` on e–h, using zero-based files and STM-normalized ranks. In this way horizontal reflection sends square number `i` to `63 - i`, so that exactly one of the reflected or non-reflected positions will have the STM and SNTM king in the right order.

En-passant positions are encoded separately in different ranges of indices (sub-tables) managed internally by the `Indexer`. The `Indexer` exposes a single unified index range for the entire `Egt`.

##### Local Indexing Algorithm (`position_to_index`)
The `Indexer` maps a chess position to a unique integer in `[0, index_range)` using a multi-step compaction and combinatorial encoding process:

   - For symmetric pawn tables, choose the horizontal orientation from the king order first, including the reflected EP file.
   - The indexer checks whether a legal en-passant capture is available. A raw EP square without a legal capture is equivalent to no EP.
   - If so, it selects the corresponding sub-table and adjusts the `index_offset`. The SNTM victim is the highest pawn on its file in STM-normalized ranks, fixed on rank 5. The remaining $k-1$ pawns on that file occupy ranks 2–4, contributing $\binom{3}{k-1}$ combinations. The sequence must end in SNTM and contain at most four pawns. Decoding still checks that an en-passant capture is legal.
   - The board position is converted into coordinates `(rank, file)` for each piece.
   - Pawns are gathered in descending rank order on their file. Repeated non-pawn pieces are sorted by descending square number; the joint king group already has its canonical order. Decoding uses precomputed piece/color slots.
   - If there are no pawns, the first king's position is restricted to 10 canonical squares (representing one octant of the board) using horizontal, vertical, and diagonal reflections. All other pieces are reflected accordingly to preserve relative positions.
   - Each coordinate is converted to a raw square index in `0..64`: `rank * 8 + file`, except for the reflection-reversing numbering used by symmetric pawn tables.
   - Since no two pieces can occupy the same square, the raw square indices are compacted to remove "holes" caused by already occupied squares. For each piece, its index is adjusted by subtracting the number of preceding pieces that occupy a lower square index. This maps the indices sequentially to ranges `[0..64, 0..63, 0..62, 0..61, ...]`.
   - Pawns are restricted to ranks 2-7 (6 possible squares on their designated file). Their positions are converted to compact indices in `0..6` ($rank - 1$).
   - A file containing $k$ pawns contributes $\binom{6}{k}$ rank subsets, with its fixed color sequence supplying the colors.
   - Pawnless and symmetric pawn tables both use an internal `IndexerGroup::KingPair`. For pawnless endgames, a precomputed lookup table (`KINGS_MAP`) maps valid canonical king pairs (excluding adjacent or identical squares) to a single index in `0..462`; symmetric pawn tables use the two-square combination described above.
   - The compacted indices of each piece group are aggregated into a single integer using a mixed-radix system. A group of $k$ identical pieces, or the ordered king pair in a symmetric pawn table, contributes $\binom{n}{k}$ combinations among $n$ available squares (using a precomputed table).
   - The combination index for these $k$ pieces with compacted indices $c_0 > c_1 > \dots > c_{k-1}$ is computed using the combinatorial number system (combinadics):
     $$\text{group\_index} = \sum_{j=0}^{k-1} \binom{c_j}{k-j}$$
     *(Note: for $k=1$, it is simply $c_0$).*
   - These group indices are then combined using a mixed-radix base where the multiplier for each group is the total number of combinations of the subsequent groups.
   - The final index is:
     $$\text{local\_index} = \text{index\_offset} + \text{aggregated\_combinations}$$

#### 3.2 Table identity and storage
`TableId` is a validated canonical name (`String`); logical elements are parsed
only for construction and planning. Probes build names in a stack buffer and
borrow them for cache lookup, allocating an owned name only on a cache miss.
`Egt` supplies the logical indexing; `EgtFileReader` holds one `Egt`, its identity,
and its storage/frame cache.

### 4. File Format & Compression
Each canonical table is one seekable Zstd object (via `zeekstd`), with an optional
JSON statistics file at the same relative path:

```text
<root>/format.json
<root>/<n>/<max>v<min>/op<K>/<N>pawns/<tablename>.ggegt
<root>/<n>/<max>v<min>/op<K>/<N>pawns/<tablename>.json
<root>/<hierarchy-prefix>/stats.json
```

The root descriptor is `{"format":"ggegt-v0.10"}`. Different formats are rejected.

In the path, `n` counts all pieces including kings, `max`/`min` are the two side piece
counts in descending order, `K` counts mutually opposing pawn pairs (once per
pair), and `N` counts pawns.

In the JSON files, statistics include a partition of the index range into `win`, `draw`, `loss`, and four categories of excluded indexes:
- `redundant`: the discarded member of a diagonal-symmetric index pair.
- `invalid_opposite_check`: the side to move attacks the opposite king (including
  adjacent kings).
- `invalid_ep_structure`: an invalid EP state, including an occupied target or
  double-push origin, or no side-to-move pawn geometrically able to capture EP.
- `invalid_illegal_ep_capture`: EP structure and a capturing pawn exist, but none
  of the EP captures is legal (for example, due to a pin or an unresolved check).

`unique_positions = win + draw + loss`; adding the four excluded counts gives
`index_range`.

The on-disk format of the compressed tables (`.ggegt`) has the following characteristics.
- Frames contain at most `256 * 1024` positions, with a short, unpadded final frame.
- Each frame is compressed independently, allowing seekable random access. The reader constructs the frame layout from the single table's index range and validates it against the seek table.
- Seek tables are checksum-free: `17 + 8 * num_frames` bytes. Checksum-bearing seek tables are not supported. The footer frame count is checked before parsing to bound allocation; compressed and decompressed lengths are also validated.
- Readers cache the seek table and fetch one compressed range per cold frame.
Before applying Zstd compression to a frame of $N$ positions, the 16-bit `DtcOutcome` values are transposed to maximize compressibility. They are reshaped as a sequence of bytes by taking:
1. First: the low byte of all $N$ outcomes ($N$ bytes).
2. Second: the high byte of all $N$ outcomes, skipping the (unused) high byte for invalid and drawn positions (max $N$ bytes).

The transposed buffer is always exactly `2 * N` bytes: high bytes for non-invalid, non-drawn outcomes are packed after the low bytes, and the unused tail remains zero.

`EgtFileWriter` retains one table's compressed frame batches, then streams them
in frame order without recompression and appends one seek table through the
`object_store` buffered writer.

### 5. Memory Management

Generation uses separate flat `Vec<AtomicU16>` arrays for the pair being solved.
These arrays are allocated with `try_reserve_exact`, so allocation failure is
reported as an error. Finalization processes one table at a time in parallel
frame batches. Batches use the same Rayon pool as generation, with at most one
encoder per configured thread; level-19 encoder memory therefore grows with
`--threads`.

Selected jobs run serially while reusing one Rayon pool and a `SharedFileCache`
across jobs within each material family. Probing and verification use the same
cache implementation.

#### 5.1 Frame Caching
A frame is either resident as uncompressed outcomes or absent. The shared cache
tracks resident frames across all files and evicts the least recently used
clean stored frame when over budget.

### 6. Retrograde Analysis
Retrograde analysis generates endgame tablebases by working backward from terminal positions (checkmates, stalemates, and conversions) to determine the outcome and distance-to-conversion (DTC) for all other positions.

#### 6.1 Simultaneous Propagation
For any pair of tables (e.g., `KQ_KPa` and `KPa_KQ`), retrograde analysis is run on both tables simultaneously in a single unified loop. Both tables are initialized and propagated together.

Quiet moves preserve the pawn color order on every file, so propagation stays within one table/twin pair. Captures (including en passant) and promotions probe existing dependency tables during initialization. Only one pair's working arrays are active; each finalized table is published independently.

Generation supports shared-memory parallelism through Rayon. Use `--threads N`
(or `EgtGenerator::with_threads(N)` in the library). Omitting `--threads` or
setting it to `0` uses Rayon's default: `RAYON_NUM_THREADS` if set, otherwise
the number of available logical CPUs.

Draw finalization, statistics, and compression run in frame batches.
Publication, SHA-256, and JSON remain serial, with JSON written after its
individual tablebase. Post-generation verification checks independent 4096-position
blocks in the generation pool; standalone verification creates its own pool. See Step 4 in
[`scaling_and_parallelization.md`](scaling_and_parallelization.md) for
validation and measured scaling.

#### 6.2 `MaybeDtcOutcome` & Zero-Overhead Move Counters
`MaybeDtcOutcome` is a wrapper around an `u16` used to represent the values
that are updated during retrograde analysis and that will eventually become a `DtcOutcome`.

During the retrograde propagation, each unresolved position must track a decremental move counter of its remaining legal moves after excluding those that are certainly losing. To achieve zero memory overhead, this counter is stored directly in the 13 unused bits of the 'invalid/unknown' state: an 'unknown' position with $C$ remaining moves is represented as `0b000 | (C << 3)`.

The move counter is generally initialized as the number of legal moves in a position. But note that there are interactions between the move counter initialization and the use of symmetries to canonicalize pawnless positions: there is the possibility that different legal moves result in positions that map to the same index, and similarly the retrograde analsys can find different reverse moves that map to the same index. For reference see section 4.6 [here](https://issuu.com/jespertk/docs/master_thesis).

A formal approach is the following. Let's say a canonical position `p` has `#p=8` if it represents 8 equivalent positions and `#p=4` if it represents 4 equivalent positions (with our choice of canonicalization, `#p=4` positions are positions with both kings on the a1-h8 diagonal). When initializing the counters, if there is a legal move `p -> p'` with `#p=8` and `#p'=4`, then there is a move (the symmetric along the diagonal) which goes from a non canonical position (the reflection of `p` along the diagonal) to a canonical position (the reflection of `p'` along the diagonal), which will be explored during backward propagation. To account for this, moves `p -> p'` with `#p=8` and `#p'=4` should increment the counter by 2 during initialization.
Similarly, when retrograde propagation from `p'` finds a reverse move `p -> p'` with `#p=4` and `#p'=8`, in addition to decrementing the counter for p, the counter for the reflection of `p` along the diagonal should also be decremented (since the symmetric move contributed to the counter for the reflection of `p` but led to a non-canonical position).

#### 6.3 Initialization Phase
Both tables in the pair are initialized independently, with each position updating
only its own entry:

1. **Decode and validate:** scan all indices and mark invalid arrangements as invalid.
2. **Generate legal moves:** an empty list means checkmate (loss at ply 0)
   when in check, or stalemate (draw) otherwise.
3. **Initialize counters:** count legal moves, with the pawnless symmetry adjustment
   described above.
4. **Probe conversions forward:** play legal captures/promotions and probe their
   dependency tables. A losing successor resolves the position as a win at ply 1;
   each winning successor removes one move from its counter. A zero counter
   resolves a loss at ply 1. Conversions to draws remain in the counter.

Dependencies have fewer pieces after captures or fewer pawns after promotions.
Quiet predecessors of checkmates are visited in the first propagation phase;
initialization does not generate capture or promotion unmoves.

#### 6.4 The Propagation Loop
Every resolved position stores its distance, so the positions to propagate at a given ply are found by scanning the tables for the values of the previous ply (no queues are needed). Per-block summaries let scans skip blocks that cannot contain matching values.

`PlySummary` stores four `AtomicU16` upper bounds per 4096-position block:
all losses, Checkmate wins, Capture wins, and Promotion wins. Zero means empty;
otherwise the bound encodes maximum ply plus one. Initialization chunks produce
accurate initial summaries. W scans use the loss bound; each L sub-phase uses
its win-type bound. Bounds increase monotonically; same-ply upgrades record the
new type without increasing newly resolved totals, retaining old bounds conservatively.
See [Step 2](scaling_and_parallelization.md#step-2-per-block-ply-summary) for details.

The main loop runs for $n = 1, 2, \dots$ until no new positions are marked:
1. **Propagate Losses to Wins:**
   * Scan Table A for positions marked as 'loss (conversion_type, n-1)'. For each of them:
     * Call `quiet_unmoves`.
     * Mark the predecessors in Table B as 'win (conversion_type, n)' if they are currently 'unknown'. A predecessor reached by several losses keeps the preferred conversion type (Checkmate > Capture > Promotion).
   * Do the same for Table B, propagating to Table A (unless A and B are the same table).
2. **Propagate Wins to Losses (decrement counters):** for each conversion type, in the order Checkmate, Capture, Promotion:
   * Scan Table A for positions marked as 'win (conversion_type, n-1)'. For each of them:
     * Call `quiet_unmoves`.
     * For each 'unknown' predecessor, decrement its move counter by 1.
     * If the counter reaches 0, mark the predecessor as 'loss (conversion_type, n)'.
   * Do the same for Table B, propagating to Table A (unless A and B are the same table).
3. If no new positions were marked in this iteration, stop.
4. Increment $n$.
5. At the end of the process mark all 'unknown' positions as draws.

A phase only scans values at ply `n-1` and only writes values at ply `n` (or counters), so the result does not depend on the order in which positions are visited. Running the three conversion types separately makes the conversion type of a loss deterministic (the last one to decrement its counter, i.e. Promotion > Capture > Checkmate).

#### 6.5 Predecessor Generation

`quiet_unmoves()` in `src/retrograde.rs` returns predecessor indices in the twin
table. Local pawnless and pawnful generators use reverse attacks onto empty
squares and reverse single/double pawn pushes. They reject opposite check and
pawn unmoves onto backranks before constructing candidates with
`Chess::from_setup_unchecked`. Material and king counts are preserved by the
unmove geometry; `IMPOSSIBLE_CHECK` is accepted just as in indexing. En-passant
states require an empty target and double-push origin, the pushed pawn, and a
legal en-passant capture. Castling, captures, and promotions are excluded.

#### 6.6 Alternative Conversion Initialization

An alternative to forward dependency probing would scan simpler endgames and
generate reverse capture/promotion moves to update their predecessors. These
unmove functions are not part of the current implementation.

If this approach was used, note that special care should be given to unmoves from a pawnless successor (which has 8-way symmetry) to a pawned predecessor (which has 2-way horizontal symmetry). The process would be to reconstruct the 4 rotations of the canonical pawnless board, then call the retrograde unmove function on each of the 4 rotations. For each resulting predecessor board, if the newly placed pawn lands on files e–h, horizontally reflect the board to files a–d to canonicalize.
