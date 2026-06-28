# AGENTS.md

- The goal of the project is to produce chess endgame tablebases (EGTs).
- The current status of the project is: there is an implementation of the file and table indexing (`EgtFile`, `Egt` and `Indexer` classes). There is an implementation of compression/decompression. There is no memory management yet (LRU-eviction of frames from memory) and no parallelization. The generation of tablebase outcomes through retrograde analysis of chess position is implemented (`RetrogradeSolver`) and looks pretty solid. Tablebases for all 3-piece, 4-piece and 5-piece endgames were generated and verified successfully. The exact library interface to expose and the command line interface are still to be defined.
- Always run `cargo test --release` for testing, otherwise it takes too much time.

TODO:
- Use object_store crate to use cloud storage in addition to local filesystem.
- Proper memory management and LRU-eviction. Keep track of number of uncompressed frames in EgtFile.
- Profiling with gungraun/valgrind. Benchmarking.
- Visibility and public interface.
- Add stats without en passant positions to EgtFileStats and implement EgtProber::verify_with_syzygy() to check our stats against the Syzygy stats available on the internet.
- Verify index_ranges table-by-table. Print counter during verification.
- Use compressed frames to generate compressed file (with zeekstd RawEncoder?).
- Put queues inside EgtHandle? Use something else instead of table_a == table_b?
- Experiment with approach using capture/promotion unmoves for initialization.
- Parallelization (rayon), distributed computing - mpi (e.g. ferrompi). alltoallv to exchange queues across workers. Run on EC2 cluster with S3 storage?
- Internalize quiet_unmoves() and use stock shakmaty?
- Frontend, cloning https://syzygy-tables.info/

# Design Specifications

## 1. High-Level Architecture
The project is built from the following main components:
1. **Outcome Representation (`DtcOutcome`)**: Encodes the game outcome (Win/Loss/Draw), distance-to-conversion (DTC), and conversion type (Checkmate, Promotion, or Capture) into a compact 16-bit value.
2. **Logical Indexing Layer (`Egt` & `Indexer`)**: Maps canonical chess board positions to a contiguous index space `[0, index_range)`.
3. **Storage & Memory Layer (`EgtFile`)**: Manages the physical files on disk, seekable Zstd compression/decompression, and the in-memory frame cache.
4. **Retrograde Analysis** (`RetrogradeSolver`): The recursive algorithm to generate the outcomes, starting from terminal positions (checkmates and known winning/losing positions) and moving backwards to identify all other winning/losing positions.

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
An `Egt` represents a tablebase with all positions with a given set of pieces and where pawns are fixed on specific files. These tablebases are identified by names such as `KQ_KPc`, where the left part represents the pieces for the side-to-move (one king and one queen) and the right part represents the pieces for the side-not-to-move (one king and one c-file pawn).
The `Indexer` handles the mapping of positions to a contiguous local index range `[0, index_range)`.
En-passant positions are encoded separately in different ranges of indices (sub-tables) managed internally by the `Indexer`. The `Indexer` exposes a single unified index range for the entire `Egt`.

#### Local Indexing Algorithm (`board_to_index`)
The `Indexer` maps a chess position to a unique integer in `[0, index_range)` using a multi-step compaction and combinatorial encoding process:

   - The indexer checks if there is an active en-passant option on the board.
   - If so, it selects the corresponding sub-table and adjusts the `index_offset`. In this sub-table, the pawn on the 5th rank that can be captured en-passant is fixed and not encoded, reducing the number of combinations.
   - The board position is converted into coordinates `(rank, file)` for each piece.
   - To avoid duplicate indexing of identical positions for indistinguishable pieces (e.g., two white knights, or multiple pawns of the same color on the same file), their coordinates are sorted in a descending standard order.
   - If there are no pawns, the first king's position is restricted to 10 canonical squares (representing one octant of the board) using horizontal, vertical, and diagonal reflections. All other pieces are reflected accordingly to preserve relative positions.
   - Each coordinate is converted to a raw square index in `0..64` (rank x 8 + file).
   - Since no two pieces can occupy the same square, the raw square indices are compacted to remove "holes" caused by already occupied squares. For each piece, its index is adjusted by subtracting the number of preceding pieces that occupy a lower square index. This maps the indices sequentially to ranges `[0..64, 0..63, 0..62, 0..61, ...]`.
   - Pawns are restricted to ranks 2-7 (6 possible squares on their designated file). Their positions are converted to compact indices in `0..6` ($rank - 1$).
   - If multiple pawns of the same color are on the same file, their indices are compacted relative to each other (mapping them to `[0..6, 0..5, ...]`).
   - For pawnless endgames, the positions of the two kings are mapped together. Since they cannot stand on adjacent or identical squares, a precomputed lookup table (`kings_map_to_index`) maps the valid joint positions of both kings to a single index in `0..462`.
   - The compacted indices of each piece group are aggregated into a single integer using a mixed-radix system. For a group of $k$ identical pieces of a type that can occupy $n$ available squares, the number of combinations is given by the binomial coefficient $\binom{n}{k}$ (using a precomputed table).
   - The combination index for these $k$ pieces with compacted indices $c_0 > c_1 > \dots > c_{k-1}$ is computed using the combinatorial number system (combinadics):
     $$\text{group\_index} = \sum_{j=0}^{k-1} \binom{c_j}{k-j}$$
     *(Note: for $k=1$, it is simply $c_0$).*
   - These group indices are then combined using a mixed-radix base where the multiplier for each group is the total number of combinations of the subsequent groups.
   - The final index is:
     $$\text{local\_index} = \text{index\_offset} + \text{aggregated\_combinations}$$

### 3.2 `EgtFile` Composition
An `EgtFile` represents a physical file on disk, storing the outcomes for a specific combination of pieces on the board (e.g., `KP_K`) and is composed of multiple `Egt` objects (e.g., `KPa_K`, `KPb_K`, `KPc_K`, `KPd_K`).
- The `EgtFile` maps its global index space sequentially to the constituent `Egt` objects.
- The global index is computed as:
  $$\text{global\_index} = \text{egt\_offset} + \text{local\_index}$$
  where $\text{egt\_offset}$ is the sum of the index ranges of all preceding `Egt` objects in a stable order.

## 4. File Format & Compression
On disk, an `EgtFile` is compressed using a seekable Zstd format (via the `zeekstd` library).
- The file is divided into **frames**, each containing a fixed number of positions (default: 16384).
- Each frame is compressed independently, allowing seekable random access.

Before applying Zstd compression to a frame of $N$ positions, the 16-bit `DtcOutcome` values are transposed to maximize compressibility. They are reshaped as a sequence of bytes by taking:
1. First: the low byte of all $N$ outcomes ($N$ bytes).
2. Second: the high byte of all $N$ outcomes, skipping the (unused) high byte for invalid and drawn positions (max $N$ bytes).

The new sequence of bytes is compressed using Zstd.

## 5. Memory Management

### 5.1 Frame States
Each frame in an `EgtFile` can be in one of three states:
1. **Unallocated**: The frame is not loaded or has not been calculated yet (all outcomes default to invalid/unknown).
2. **Compressed**: Only the compressed representation of the frame is stored in memory.
3. **Uncompressed**: The frame is fully uncompressed in memory as a contiguous array of `u16` values.

When a frame needs to be written to or read, its uncompressed buffer is allocated.
If the memory used reaches an assigned limit, the Least Recently Used (LRU) uncompressed frames are evicted:
- **If `dirty == true`**: The frame is bit-sliced, compressed using Zstd, and its state transitions to `Compressed`. The uncompressed memory is returned to the `Arena`.
- **If `dirty == false`**: The uncompressed memory is immediately freed without re-compression (using the cached `compressed` bytes).

## 6. Retrograde Analysis
Retrograde analysis is the recursive algorithm used to generate endgame tablebases by working backward from terminal positions (checkmates, stalemates, and conversions) to determine the outcome and distance-to-conversion (DTC) for all other positions.

### 6.1 Simultaneous Propagation
For any pair of tables (e.g., `KQ_KPa` and `KPa_KQ`), retrograde analysis is run on both tables simultaneously in a single unified loop. Both tables are initialized and propagated together.

### 6.2 `MaybeDtcOutcome` & Zero-Overhead Move Counters
`MaybeDtcOutcome` is a wrapper around an `u16` used to represent the values
that are updated during retrograde analysis and that will eventually become a `DtcOutcome`.

During the retrograde propagation, each unresolved position must track a decremental move counter of its remaining legal moves after excluding those that are certainly losing. To achieve zero memory overhead, this counter is stored directly in the 13 unused bits of the 'invalid/unknown' state: an 'unknown' position with $C$ remaining moves is represented as `0b000 | (C << 3)`.

The move counter is generally initialized as the number of legal moves in a position. But note that there are interactions between the move counter initialization and the use of symmetries to canonicalize pawnless positions: there is the possibility that different legal moves result in positions that map to the same index, and similarly the retrograde analsys can find different reverse moves that map to the same index. For reference see section 4.6 [here](https://issuu.com/jespertk/docs/master_thesis).

A formal approach is the following. Let's say a canonical position `p` has `#p=8` if it represents 8 equivalent positions and `#p=4` if it represents 4 equivalent positions (with our choice of canonicalization, `#p=4` positions are positions with both kings on the a1-h8 diagonal). When initializing the counters, if there is a legal move `p -> p'` with `#p=8` and `#p'=4`, then there is a move (the symmetric along the diagonal) which goes from a non canonical position (the reflection of `p` along the diagonal) to a canonical position (the reflection of `p'` along the diagonal), which will be explored during backward propagation. To account for this, moves `p -> p'` with `#p=8` and `#p'=4` should increment the counter by 2 during initialization.
Similarly, when retrograde propagation from `p'` finds a reverse move `p -> p'` with `#p=4` and `#p'=8`, in addition to decrementing the counter for p, the counter for the reflection of `p` along the diagonal should also be decremented (since the symmetric move contributed to the counter for the reflection of `p` but led to a non-canonical position).

### 6.3 Initialization Phase
Before starting the main retrograde loop, both tables in the pair are initialized:
1. **Invalid Positions:** Scan all indices and mark invalid positions as 'invalid'.
2. **Stalemate Positions:** Identify all valid positions with no legal moves that are not in check, and mark them as 'draw'.
3. **Checkmate Positions:** Identify all checkmated positions and mark them as 'loss (mate-in-0)'.
   * Perform the unmoves (`quiet_unmoves`). Mark the resulting predecessor positions as 'win (mate-in-1)' in the twin table.
4. **Move Counters:** Initialize the decremental move counter for all remaining valid positions (marked as 'unknown').
5. **Dependency Probing (Conversions):** For each table, probe the fully generated dependency tables (which have fewer pieces or fewer pawns) to resolve capture and promotion moves:
   * Scan the losing positions in the dependency tables and perform the corresponding unmoves (`capture_unmoves`, `promotion_unmoves`, `promotion_capture_unmoves`). Mark the resulting predecessor positions as 'win (capture-in-1)' or 'win(promotion-in-1)' if they are not already marked.
   * Scan the winning positions in the dependency tables and perform the corresponding unmoves (`capture_unmoves`, `promotion_unmoves`, `promotion_capture_unmoves`). If a predecessor position is 'unknown', decrement the counter. If the counter reaches zero, mark the predecessor as 'loss (capture-in-1)' or 'loss (promotion-in-1)'.
   * Here are some examples of dependency generation:
     - the table K_K has no dependencies.
     - the table K_KQ has one dependency: K_K (when the Q is captured)
     - the table KQ_KPa has one dependency: K_KQ (when the P is captured)
     - the table KPa_KQ has 9 dependencies:
       - K_KPa (when the Q is captured)
       - KQ_KQ (when the P promotes to Q)
       - KQ_KR (when the P promotes to R)
       - KQ_KB (when the P promotes to B)
       - KQ_KB (when the P promotes to N)
       - K_KQ (when the P promotes to Q capturing the Q)
       - K_KR (when the P promotes to R capturing the Q)
       - K_KB (when the P promotes to B capturing the Q)
       - K_KB (when the P promotes to N capturing the Q)

### 6.4 The Propagation Loop
The main loop runs for $n = 1, 2, \dots$ until no new positions are marked:
1. **Propagate Losses to Wins:**
   * For each position in Table A newly marked as 'loss (conversion_type, n)':
     * Call `quiet_unmoves`.
     * Mark the predecessors in Table B as 'win (conversion_type, n+1)' if they are currently 'unknown'.
   * Do the same for newly marked 'loss' positions in Table B, propagating them to Table A (unless A and B are the same table).
2. **Propagate Wins to Losses (decrement counters):**
   * For each position in Table A newly marked as 'win (conversion_type, n)':
     * Call `quiet_unmoves` .
     * Deduplicate the list of predecessor indices.
     * For each 'unknown' predecessor, decrement its move counter by 1.
     * If the counter reaches 0, mark the predecessor as 'loss (conversion_type, n+1)'.
   * Do the same for newly marked 'win' positions in Table B, propagating them to Table A (unless A and B are the same table).
3. If no new positions were marked in this iteration, stop.
4. Increment $n$.
5. At the end of the process mark all 'unknown' positions as draws.

### 6.5 Use of reverse move generation for transitions from a different endgame
The current approach relies only on `quiet_unmoves()`: a function that lists reverse moves without considering captures or promotions. This is enough during initialization, when we can scan all indexes and list the legal moves and consider transitions to simpler endgames. During this phase, for each capture/promotion move, we compute the index in the dependency and lookup the tablebase result, then use it for populating the queue of indexes to update.

In principle another approach is possible, which consists in scanning all indexes of simpler endgames, generating reverse capture/promotion moves from there (with unmove functions such as `capture_unmoves()`, `promotion_unmoves()`, or `promotion_capture_unmoves()`) and using these for populating the queue of indexes to update.

If this approach was used, note that special care should be given to unmoves from a pawnless successor (which has 8-way symmetry) to a pawned predecessor (which has 2-way horizontal symmetry). The process would be to reconstruct the 4 rotations of the canonical pawnless board, then call the retrograde unmove function on each of the 4 rotations. For each resulting predecessor board, if the newly placed pawn lands on files e–h, horizontally reflect the board to files a–d to canonicalize.
