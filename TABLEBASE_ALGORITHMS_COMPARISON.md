# Endgame Tablebase Generation: Comparative Analysis

A comparative study of the tablebase generation algorithms and architectures used across four projects:
- **chess-egt** (our Rust project: `chess-egt`)
- **Syzygy `tb`** by Ronald de Man (`https://github.com/syzygy1/tb`)
- **Prophet TB** by Markus (`https://github.com/markus7800/prophet_tb_gen_and_probe`)
- **chesstb** by noobpwnftw (`https://github.com/noobpwnftw/chesstb`)

---

## 1. High-Level Comparison Table

| Feature / Dimension | **chess-egt** (Current) | **Syzygy (`tb`)** (Ronald de Man) | **Prophet** (Markus) | **chesstb** (noobpwnftw) |
| :--- | :--- | :--- | :--- | :--- |
| **Primary Metric** | **DTC** (Distance to Conversion) | **WDL** + **DTZ50** (Distance to Zeroing) | **DTM** (Distance to Mate, no 50MR) | **WDL, DTZ, DTC, DTM, DTM50** (All 5 metrics) |
| **Working Representation** | `u16` (16 bits): 3-bit status + 13-bit counter / DTC | `uint8_t` (8 bits): tightly packed status & ply codes | `int16_t` (16 bits): signed mate distance + flags | Custom bit-packed struct / byte per slice group |
| **Memory Footprint (per pos)** | 2 bytes/pos (both STM tables live in memory) | **1 byte/pos** (tightly packed, fits 6-man in ~16 GB) | 2 bytes/pos (WTM + BTM tables) | **Configurable Soft Cap (`--mem`)**: pages groups to disk |
| **Loss Resolution Mechanism** | **Decremental Move Counter** (stored in `UNKNOWN` bits) | **`CHANGED` flag + forward `check_loss` verification** | **`MAYBELOSS` flag + forward `check_loss` verification** | **`CHANGED` flag + bound check + forward `check_loss`** |
| **Propagation Engine** | **BFS Queues** (`DepthQueues` of `usize` indices) | **Table Iteration Scans** (flat array sweeps per ply) | **Table Iteration Scans** (OpenMP flat parallel loops) | **Group Paging & Chunked Parallel Iterators** |
| **Parallelization** | None currently (single-threaded) | Pthreads / C11 threads + atomic CAS (`lock cmpxchgb`) | OpenMP (`#pragma omp parallel for`) | Custom thread pool (`Thread_Pool`) with work-stealing chunks |
| **Dependency Resolution** | Forward scanning during init (probes children) | Scans sub-tables & reverse-marks into parent table | Forward scanning during init (or `retrograde_promotion_tbs`) | Forward & reverse hybrid, topological pawn slice ordering |
| **Output Compression** | Seekable Zstd (`zeekstd`) with bit-transposition | Custom Huffman + rank compression + LZ4 / Zstd | Block-compressed Zstd (32K pos / block) | LZ4-HC (WDL) / LZMA rank streams (DTZ/DTM), 64KB/1MB blocks |

---

## 2. Deep Dive into the Four Algorithms

### A. Resolution Strategy: Decremental Counters vs. Changed-Flag Verification

This is the **single most fundamental algorithmic difference**:

1. **`chess-egt` (Decremental Move Counters)**:
   - When initialized, every unresolved position stores the number of legal moves $C$ in its unused 13 bits (`0b000 | (C << 3)`).
   - During propagation, whenever a successor position is proved to be a win for the opponent, the predecessor’s index is queued, and its move counter is decremented by 1.
   - When the counter reaches 0, all legal moves have been proven losing, so the position is marked as a Loss.
   - *Challenges*:
     - **Symmetry hazards**: With diagonal reflections in pawnless tables, moves transitioning between 8-way and 4-way orbits require artificial $+2$ counter weighting, mirror decrements, and unmove deduplication. If a single unmove is duplicated or missed, the counter never reaches 0 (turning a loss into a draw) or drops too fast (false loss).
     - **Parallelism contention**: Decrementing shared counters from multiple worker threads requires atomic fetch-and-sub or spinlocks, causing severe memory bus cache-line bouncing.
     - **Queue memory spikes**: Depth queues holding millions of `usize` indices can consume gigabytes of heap memory at peak iteration depths.

2. **Syzygy (`tb`), Prophet, and `chesstb` (Candidate Flag + Forward Verification)**:
   - **None of the other three engines use decremental move counters.**
   - Instead, they use a two-phase loop per ply:
     - **Step 1 (Loss $\to$ Win)**: For every position confirmed as a loss at ply $p-1$, generate unmoves and mark all predecessor positions as a Win at ply $p$.
     - **Step 2 (Win $\to$ Candidate Loss)**: For every position confirmed as a Win at ply $p$, generate unmoves and set a candidate flag on the predecessors:
       - Syzygy marks: `SET_CHANGED(table[idx])` (using an idempotent atomic CAS: if `UNKNOWN`, set to `CHANGED`).
       - Prophet marks: `LOSS_EGTB->TB[idx] = MAYBELOSS_IN(p+1)`.
       - chesstb marks: `add_flags(pred, DTC_FLAG_CHANGE)`.
     - **Step 3 (Forward Verification of Candidate Losses)**: Sweep the candidate positions. For each marked position, generate its legal forward moves and check if *all* moves land on confirmed wins for the opponent:
       - **Crucial optimization (Early Exit)**: In forward `check_loss`, as soon as **any single move** lands on `UNKNOWN` or a draw/win, the check aborts immediately (`break`), and the flag is reset to `UNKNOWN`. Most candidate positions are refuted on their 1st or 2nd legal move!
       - If and only if **all** legal forward moves lead to opponent wins in $\le p$, the position is promoted to confirmed `LOSS_IN(p)`.

---

### B. Memory Allocation & Scaling to 6+ Pieces

- **Syzygy (`tb`)**:
  - Uses only **1 byte (`uint8_t`) per position**.
  - WDL and DTZ are generated in separate passes. For WDL, 1 byte easily stores the state (`ILLEGAL`, `UNKNOWN`, `CHANGED`, `MATE`, `LOSS_IN_ONE`, etc.).
  - Has a `--disk` flag that writes intermediate tables to disk so that even 6-piece tables could be generated on machines with just 16 GB RAM in 2013!
- **Prophet**:
  - Uses `int16_t` (2 bytes per position) for WTM and BTM in memory.
  - Generates DTM directly. A 6-piece table generation required 96 GB RAM.
  - Uses OpenMP parallel loops across the entire flat index range with chunks of 2,048 entries.
- **chesstb**:
  - Implements a full **virtual memory pager**: the position space is decomposed into slices (by king and pawn positions) and grouped into slice bundles.
  - With `--mem 64`, it will page groups to `--tmp` scratch disk, keeping only the active slice group and its "king-neighbor reach" (and pawn push targets) resident in RAM.
  - This allows generating even 7-piece and 8-piece endgames under a strict memory cap!

---

### C. Symmetry & Invariant Handling

- **Pawnless Endgames**:
  - All four engines exploit the 8-fold dihedral symmetry (10 canonical king squares, or the 462 non-attacking king pairs).
  - In Syzygy, diagonal symmetry is handled cleanly during the candidate marking phase using macro helpers (`MARK_PIVOT0`, `MARK_PIVOT1`): if a king is on the a1–h8 diagonal, the mirrored index `PIVOT_MIRROR(idx)` is marked simultaneously. Because marking `CHANGED` or `WIN` is idempotent, duplicate unmoves are completely harmless!
- **Pawnful Endgames**:
  - Pawns break vertical and diagonal symmetry (leaving only 2-fold horizontal file-mirror symmetry: files a–d).
  - `chesstb` orders pawn slices **topologically**: pawns only move forward! Therefore, a pawn slice at rank 6 never receives retrograde transitions from rank 5. This allows solving pawn slices in topological batches, drastically reducing the active working set.

---

### D. Capture & Promotion Dependencies (Sub-Tables)

1. **`chess-egt`**:
   - Currently, during table initialization, every valid position is decoded, legal captures and promotions are played forward, and the lower-order dependency table is probed (`dep_cache.get_or_load()?.probe()`).
2. **Syzygy (`rtbgen.c` / `tbgenp.c`)**:
   - Instead of scanning all parent positions and doing forward probes into child tables, it iterates over the possible captured pieces in the already-generated child table (`probe_captures_w`, `probe_captures_b`), doing reverse captures to directly mark predecessors in the parent table.
3. **Prophet (`retrograde_promotion_tbs`)**:
   - Uses a hybrid approach: if promotion tables are too large to hold resident in RAM alongside the current table, it runs a reverse promotion pass from the sub-tables to compute lower bounds before deallocating the sub-tables.

---

### E. Compression and Probing Optimizations

- **STM Color Dropping (`shrink` in chesstb, single-sided files in Syzygy/Prophet)**:
  - An endgame table has two sides to move (e.g., White to move and Black to move).
  - For asymmetric endgames, you do not need to store both sides on disk! At probe time, the missing color can be reconstructed in **1 ply of minimax**: generate all legal moves; quiet moves stay in the stored table of the same material, while captures reach smaller sub-tables.
  - `chesstb`'s `shrink` tool drops the larger compressed STM color automatically, cutting disk footprint almost in half.
- **Relaxed Bounds (chesstb)**:
  - If a position has a winning capture leading to an already-won sub-table, its exact stored value doesn't matter for game-theoretic correctness—the engine can see the winning capture at depth 1. `chesstb` treats these as "don't cares" during compression, allowing LZ algorithms to achieve significantly higher compression ratios.

---

## 3. Key Insights & Concrete Takeaways for `chess-egt`

Studying these three mature projects offers several high-value insights directly applicable to `chess-egt`:

### 1. Moving from Move Counters to `Candidate Loss (Changed) + Forward Check`
In `chess-egt`'s `AGENTS.md`, the complex interaction between move counters and diagonal symmetries is noted as a tricky area requiring special counter doubling ($+2$) and mirror decrements.
- **Insight**: If `chess-egt` replaces decremental counters with a `CHANGED` flag + forward `check_loss` (or uses candidate flags alongside counters):
  - Symmetry handling becomes trivial and robust: marking `CHANGED` or `WIN` is idempotent (multiple identical or symmetric reverse moves can mark the same cell without causing corruption).
  - Eliminates the need for large heap queues (`DepthQueues` vectors).
  - Enables effortless multi-threading without lock contention or atomic subtracts.
  - Forward `check_loss` refutes non-losses almost instantaneously with 1–2 legal move evaluations.

### 2. Multi-Threading & Rayon Parallelization
Currently, `chess-egt` is single-threaded.
- **Insight**: Prophet and Syzygy demonstrate that flat array sweeps over index chunks (`[chunk_start..chunk_end]`) parallelize exceptionally well.
- In Rust, with `rayon`, an entire iteration step can be written as:
  ```rust
  table.par_chunks_mut(CHUNK_SIZE).for_each(|chunk| { ... });
  ```
  Using `AtomicU16` with `fetch_update` or CAS (like Syzygy's `lock cmpxchgb`) makes the propagation loop completely lock-free and scales linearly across all CPU cores.

### 3. Topological Slicing for Pawnful Tables
- **Insight**: Since pawns cannot move backward, pawn configurations form a Directed Acyclic Graph (DAG).
- As `chesstb` demonstrates, you do not need all pawn configurations of an endgame resident in memory at the same time. You can solve slices at higher ranks (e.g., pawns on 6th/7th rank) first, write them out, and then solve lower ranks. This eliminates the memory wall when scaling from 5-piece to 6-piece endgames.

### 4. Reverse Capture/Promotion Unmoves vs. Forward Probing
In `chess-egt`'s `TODO`, there is an item: *"Experiment with approach using capture/promotion unmoves for initialization."*
- **Insight**: Both Syzygy and Prophet validate this approach. By iterating over the (much smaller) dependency tables and unmoving captures/promotions into the target table, you eliminate the overhead of forward legality checking and sub-table probe searching for the vast majority of non-capture positions during initialization.

### 5. Probing and Storage Footprint (STM Dropping)
- **Insight**: In `chess-egt`, an `EgtFile` currently stores all positions for both sides. For 5-piece and upcoming 6-piece tables, dropping one side-to-move for asymmetric endgames (reconstructing via 1-ply search in the prober) will immediately reduce disk storage and generation cache pressure by roughly 40–50%.
