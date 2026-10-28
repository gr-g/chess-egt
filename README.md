# Chess Endgame Tablebases
Work-in-progress repository with experiments to build from scratch [Chess Endgame Tablebases (EGTs)](https://www.chessprogramming.org/Endgame_Tablebases).

Goals:
 - Provide tablebases with the Depth to Conversion (DTC) metric, particularly suitable for applications such as practicing endgames against the computer.
 - Provide a clean, readable Rust codebase that people interested in generating chess tablebase can understand, modify and use in other projects, even if performance is not super-optimized as in the state-of-the-art [Syzygy](https://www.chessprogramming.org/Syzygy_Bases) code.

# Usage

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
cargo run --release -- --list KP_K 4/2v2/op1

# Probe a table.
cargo run --release -- --path ./ggegt '8/8/8/8/8/2K5/4Q3/7k w - - 0 1'
```

`--generate`, `--verify`, `--stats`, and `--list` each accept multiple `SELECTOR`s:

- **Exact name**, e.g. `KPa_K`: one canonical table, with STM on the left.
- **Oriented material**, e.g. `KP_K`: all tables for that material balance (`K_KP` is not included).
- **Hierarchy prefix**, e.g. `3/`, `4/2v2/op1`, or `4/2v2/op1/2pawns`. A material or exact name may follow a prefix to intersect the selections.

Generating a table such as `KPa_K` will also generate the twin table `K_KPa`.
Verification and stats do not add twins.

Cloud credentials/configuration come from environment variables supported by
[`object_store`](https://docs.rs/object_store/latest/object_store/). HTTP roots
are read-only. `--input-path` is searched before `--path`, falling back only for
missing objects.

Run the test suite with:

```sh
cargo test --release
```

Chess rules and position validation use the upstream
[`shakmaty`](https://crates.io/crates/shakmaty) crate. Retrograde analysis
is implemented locally for the tablebase solver.

# Design Specifications

## 1. High-Level Architecture
The project is built from the following main components:
1. **Outcome Representation (`DtcOutcome`)**: Encodes the game outcome (Win/Loss/Draw), distance-to-conversion (DTC), and conversion type (Checkmate, Promotion, or Capture) into a compact 16-bit value.
2. **Logical Indexing Layer (`Egt`, `Indexer`)**: Maps canonical chess board positions to a contiguous index space `[0, index_range)`.
3. **Storage & Memory Layer (`EgtFileReader`, `EgtFileWriter`, `SharedFileCache`)**: Manages the physical access to files through `object_store`, seekable Zstd compression/decompression, and in-memory frame cache.
4. **Retrograde Analysis** (`RetrogradeSolver`): The backward-propagation algorithm to generate the outcomes, starting from terminal positions (checkmates and known winning/losing positions) and moving backwards to identify all other winning/losing positions.

## 2. Outcome Representation (`DtcOutcome`)
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
- **Bits 3-15 (Distance to Conversion)**:
  - A 13-bit unsigned integer representing the number of plies to conversion. This allows encoding distances up to $2^{13} - 1 = 8191$ plies.

## 3. Indexing & Symmetries

### 3.1 `Egt` and `Indexer`
An `Egt` contains positions with fixed material, pawn files, and the exact color order of pawns on each file. An `Egt` has a name such as `KQ_KPc` (king and queen versus king and pawn on the c file). The name has the side-to-move (STM) on the left and the side-not-to-move (SNTM) on the right.

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

The `Indexer` handles the mapping of positions to a contiguous local index range `[0, index_range)`.

En-passant positions are encoded separately in different ranges of indices (sub-tables) managed internally by the `Indexer`. The `Indexer` exposes a single unified index range for the entire `Egt`.

#### Local Indexing Algorithm (`board_to_index`)
The `Indexer` maps a chess position to a unique integer in `[0, index_range)` using a multi-step compaction and combinatorial encoding process:

   - The indexer checks whether a legal en-passant capture is available. A raw EP square without a legal capture is equivalent to no EP.
   - If so, it selects the corresponding sub-table and adjusts the `index_offset`. The SNTM victim is the highest pawn on its file in STM-normalized ranks, fixed on rank 5. The remaining $k-1$ pawns on that file occupy ranks 2–4, contributing $\binom{3}{k-1}$ combinations. The sequence must end in SNTM and contain at most four pawns. Decoding still checks that an en-passant capture is legal.
   - The board position is converted into coordinates `(rank, file)` for each piece.
   - Coordinates are ordered descending within each group. Pawns are gathered directly in rank order on their file; only repeated non-pawn pieces need sorting. Decoding uses precomputed piece/color slots for each pawn sequence and non-pawn group.
   - If there are no pawns, the first king's position is restricted to 10 canonical squares (representing one octant of the board) using horizontal, vertical, and diagonal reflections. All other pieces are reflected accordingly to preserve relative positions.
   - Each coordinate is converted to a raw square index in `0..64` (rank x 8 + file).
   - Since no two pieces can occupy the same square, the raw square indices are compacted to remove "holes" caused by already occupied squares. For each piece, its index is adjusted by subtracting the number of preceding pieces that occupy a lower square index. This maps the indices sequentially to ranges `[0..64, 0..63, 0..62, 0..61, ...]`.
   - Pawns are restricted to ranks 2-7 (6 possible squares on their designated file). Their positions are converted to compact indices in `0..6` ($rank - 1$).
   - A file containing $k$ pawns contributes $\binom{6}{k}$ rank subsets, with its fixed color sequence supplying the colors.
   - For pawnless endgames, the positions of the two kings are mapped together. Since they cannot stand on adjacent or identical squares, a precomputed lookup table (`kings_map_to_index`) maps the valid joint positions of both kings to a single index in `0..462`.
   - The compacted indices of each piece group are aggregated into a single integer using a mixed-radix system. For a group of $k$ identical pieces of a type that can occupy $n$ available squares, the number of combinations is given by the binomial coefficient $\binom{n}{k}$ (using a precomputed table).
   - The combination index for these $k$ pieces with compacted indices $c_0 > c_1 > \dots > c_{k-1}$ is computed using the combinatorial number system (combinadics):
     $$\text{group\_index} = \sum_{j=0}^{k-1} \binom{c_j}{k-j}$$
     *(Note: for $k=1$, it is simply $c_0$).*
   - These group indices are then combined using a mixed-radix base where the multiplier for each group is the total number of combinations of the subsequent groups.
   - The final index is:
     $$\text{local\_index} = \text{index\_offset} + \text{aggregated\_combinations}$$

### 3.2 Table identity and storage
`TableId` is a validated canonical name (`String`); logical elements are parsed
only for construction and planning. Probes build names in a stack buffer and
borrow them for cache lookup, allocating an owned name only on a cache miss.
`Egt` supplies the logical indexing; `EgtFileReader` holds one `Egt`, its identity,
and its storage/frame cache.

## 4. File Format & Compression
Each canonical table is one seekable Zstd object (via `zeekstd`), with an optional
JSON statistics file at the same relative path:

```text
<root>/format.json
<root>/<n>/<max>v<min>/op<K>/<N>pawns/<tablename>.ggegt
<root>/<n>/<max>v<min>/op<K>/<N>pawns/<tablename>.json
```

Here `n` counts all pieces including kings, `max`/`min` are the two side piece
counts in descending order, `K` counts mutually opposing pawn pairs (once per
pair), and `N` counts pawns.

The immutable root `format.json` declares `format: "chess-egt"`,
`format_version: 1`, `indexing_version: 1`, `layout_version: 1`, and
`frame_size: 262144`.

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

## 5. Memory Management

Generation uses separate flat `Vec<AtomicU16>` arrays for the pair being solved.
These arrays are allocated with `try_reserve_exact`, so allocation failure is
reported as an error. Finalization processes one table at a time in parallel
frame batches. Batches use the same Rayon pool as generation, with at most one
encoder per configured thread; level-19 encoder memory therefore grows with
`--threads`.

Selected jobs run serially while reusing one Rayon pool and a `SharedFileCache`
across jobs within each material family. Probing and verification use the same
cache implementation.

### 5.1 Frame Caching
A frame is either resident as uncompressed outcomes or absent. The shared cache
tracks resident frames across all files and evicts the least recently used
clean stored frame when over budget.

## 6. Retrograde Analysis
Retrograde analysis generates endgame tablebases by working backward from terminal positions (checkmates, stalemates, and conversions) to determine the outcome and distance-to-conversion (DTC) for all other positions.

### 6.1 Simultaneous Propagation
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

### 6.2 `MaybeDtcOutcome` & Zero-Overhead Move Counters
`MaybeDtcOutcome` is a wrapper around an `u16` used to represent the values
that are updated during retrograde analysis and that will eventually become a `DtcOutcome`.

During the retrograde propagation, each unresolved position must track a decremental move counter of its remaining legal moves after excluding those that are certainly losing. To achieve zero memory overhead, this counter is stored directly in the 13 unused bits of the 'invalid/unknown' state: an 'unknown' position with $C$ remaining moves is represented as `0b000 | (C << 3)`.

The move counter is generally initialized as the number of legal moves in a position. But note that there are interactions between the move counter initialization and the use of symmetries to canonicalize pawnless positions: there is the possibility that different legal moves result in positions that map to the same index, and similarly the retrograde analsys can find different reverse moves that map to the same index. For reference see section 4.6 [here](https://issuu.com/jespertk/docs/master_thesis).

A formal approach is the following. Let's say a canonical position `p` has `#p=8` if it represents 8 equivalent positions and `#p=4` if it represents 4 equivalent positions (with our choice of canonicalization, `#p=4` positions are positions with both kings on the a1-h8 diagonal). When initializing the counters, if there is a legal move `p -> p'` with `#p=8` and `#p'=4`, then there is a move (the symmetric along the diagonal) which goes from a non canonical position (the reflection of `p` along the diagonal) to a canonical position (the reflection of `p'` along the diagonal), which will be explored during backward propagation. To account for this, moves `p -> p'` with `#p=8` and `#p'=4` should increment the counter by 2 during initialization.
Similarly, when retrograde propagation from `p'` finds a reverse move `p -> p'` with `#p=4` and `#p'=8`, in addition to decrementing the counter for p, the counter for the reflection of `p` along the diagonal should also be decremented (since the symmetric move contributed to the counter for the reflection of `p` but led to a non-canonical position).

### 6.3 Initialization Phase
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

### 6.4 The Propagation Loop
Every resolved position stores its distance, so the positions to propagate at a given ply are found by scanning the tables for the values of the previous ply (no queues are needed). A per-block summary (the last ply resolved in each block of 4096 positions, aligned on each table's local indices) lets the scans skip blocks that cannot contain values of the scanned ply. The main loop runs for $n = 1, 2, \dots$ until no new positions are marked:
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

### 6.5 Predecessor Generation

`quiet_unmoves()` in `src/retrograde.rs` returns predecessor indices in the twin
table. Local pawnless and pawnful generators use reverse attacks onto empty
squares and reverse single/double pawn pushes. They construct each candidate
through Shakmaty's checked `Setup` API. Castling, captures, and promotions are
excluded.

### 6.6 Alternative Conversion Initialization

An alternative to forward dependency probing would scan simpler endgames and
generate reverse capture/promotion moves to update their predecessors. These
unmove functions are not part of the current implementation.

If this approach was used, note that special care should be given to unmoves from a pawnless successor (which has 8-way symmetry) to a pawned predecessor (which has 2-way horizontal symmetry). The process would be to reconstruct the 4 rotations of the canonical pawnless board, then call the retrograde unmove function on each of the 4 rotations. For each resulting predecessor board, if the newly placed pawn lands on files e–h, horizontally reflect the board to files a–d to canonicalize.
