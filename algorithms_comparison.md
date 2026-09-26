# Endgame Tablebase Generation: Comparative Analysis

A comparative study of the tablebase generation algorithms and architectures used across four projects:
- **chess-egt** (our Rust project: `chess-egt`)
- **Syzygy `tb`** by Ronald de Man (`https://github.com/syzygy1/tb`)
- **Prophet TB** by Markus (`https://github.com/markus7800/prophet_tb_gen_and_probe`)
- **chesstb** by noobpwnftw (`https://github.com/noobpwnftw/chesstb`)

Scaling to 6+ pieces (shared-memory vs. distributed parallelization) and the resulting implementation roadmap are covered in [`scaling_and_parallelization.md`](scaling_and_parallelization.md).

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
     - **Parallelism contention**: Decrementing shared counters is inherently non-idempotent. Every unmove reaching a predecessor $P$ must execute an atomic read-modify-write (`fetch_sub` or CAS loop). If multiple worker threads hit positions on the same cache line, the line is constantly invalidated and bounced across CPU cores.
       *(Re-assessed in [`scaling_and_parallelization.md`](scaling_and_parallelization.md) Section 4.1: the workload is compute-bound and the tables are accessed at scattered locations, so collisions should be rare and an uncontended atomic is cheap next to an unmove. The sweep also needs atomic writes in its propagation step. Not measured yet.)*
     - **Queue memory spikes**: Depth queues holding millions of `usize` indices can consume gigabytes of heap memory at peak iteration depths.

2. **Syzygy (`tb`), Prophet, and `chesstb` (Candidate Flag + Forward Verification)**:
   - **None of the other three engines use decremental move counters.**
   - Instead, they use a two-phase loop per ply:
     - **Step 1 (Loss $\to$ Win)**: For every position confirmed as a loss at ply $p-1$, generate unmoves and mark all predecessor positions as a Win at ply $p$.
     - **Step 2 (Win $\to$ Candidate Loss)**: For every position confirmed as a Win at ply $p$, generate unmoves and set a candidate flag on the predecessors:
       - Syzygy marks: `SET_CHANGED(table[idx])` (using an idempotent CAS or TTAS: if `UNKNOWN`, set to `CHANGED`).
       - Prophet marks: `LOSS_EGTB->TB[idx] = MAYBELOSS_IN(p+1)`.
       - chesstb marks: `add_flags(pred, DTC_FLAG_CHANGE)`.
       - *Why candidate flags avoid contention*: Marking `CHANGED` is **idempotent**. Threads use Test-and-Test-and-Set (TTAS): if the flag is already set, no write occurs, keeping the cache line in Shared (`S`) state and avoiding bus bouncing.
     - **Step 3 (Forward Verification of Candidate Losses)**: Sweep the candidate positions across non-overlapping index chunks (e.g. `par_chunks_mut`). Each candidate is only written by the thread owning its chunk, so this step needs no atomics. Steps 1-2 still write to the twin table at arbitrary indexes and do need atomics (CAS/TTAS).
       - Crucially, this sweep does **not** evaluate all `UNKNOWN` positions every ply: it inspects **only** positions tagged as `CHANGED` in Step 2.
       - For each marked position, generate its legal forward moves and check if *all* moves land on confirmed wins for the opponent.
       - **Early Exit**: As soon as **any single move** lands on `UNKNOWN` or a draw/win, the check aborts immediately (`break`), and the flag is reset to `UNKNOWN`.
       - If and only if **all** legal forward moves lead to opponent wins in $\le p$, the position is promoted to confirmed `LOSS_IN(p)`.

#### Measured comparison in `chess-egt` (single-threaded)

To get hard numbers, the Changed-Flag approach was re-implemented in `chess-egt` (`src/retrograde_sweep.rs`, selected with `--algorithm sweep`), sharing everything else with the counter implementation: indexing, `quiet_unmoves`, dependency probing, statistics and file output.

*Implementation notes:*
- The 13 spare bits of an `UNKNOWN` value hold a `CHANGED` flag, a `NO_LOSS` flag and two 2-bit conversion types. Initialization probes captures/promotions once (as the counter version does): if any conversion move does not lose, the position gets `NO_LOSS` and can never become a candidate; otherwise all conversions are known to lose, so forward verification only plays **quiet** moves against the pair tables and never probes dependency tables.
- Each ply runs a verification sweep (candidates flagged at the previous ply) followed by a propagation sweep (wins flag predecessors as `CHANGED`, losses mark predecessors as wins), frame by frame via `EgtFile::frame_chunk`.
- Output is **byte-identical** to the counter version (sha256-checked on all 3/4-piece tables and a 5-piece sample). This requires an order-independent tie-break for conversion types among moves realizing the distance: wins prefer Checkmate > Capture > Promotion, losses prefer Promotion > Capture > Checkmate (which is also what the prober's `move_value` ordering checks).
- Diagonal symmetry still requires marking the diagonal mirror of predecessors, but the `+2` counter adjustments disappear (marking is idempotent).

*Results* (one run each, dependencies read from existing tables, init + propagation time):

| Set / Endgame | Profile | Counters | Sweep | Δ time | Peak RSS counters | Peak RSS sweep |
| :--- | :--- | ---: | ---: | ---: | ---: | ---: |
| All 30 4-piece | mixed | 312 s | 351 s | +13% | 191 MiB | 152 MiB |
| `KBB_KN` | deep, pawnless (131 plies) | 169 s | 227 s | +34% | 822 MiB | 292 MiB |
| `KQB_KQ` | shallow, pawnless | 382 s | 430 s | +13% | 7495 MiB | 497 MiB |
| `KRB_KR` | shallow + long tail, pawnless | 241 s | 274 s | +14% | 3509 MiB | 498 MiB |
| `KNN_KP` | deep, thin frontier (228 plies, median 99) | 360 s | 551 s | +53% | 1629 MiB | 1092 MiB |
| `KRP_KQ` | shallow (92% of wins at ply 1) | 2181 s | 2612 s | +20% | 8427 MiB | 2059 MiB |
| `KQP_KQ` | shallow + long tail (227 plies) | 1477 s | 1840 s | +25% | 7506 MiB | 2059 MiB |
| **5-piece sample total** | | **4811 s** | **5934 s** | **+23%** | **8427 MiB** | **2059 MiB** |

*Findings:*
- **Speed:** the sweep was slower on every non-trivial endgame (propagation alone: +18% on 4-piece, +31% on the 5-piece sample). The worst case is as predicted by theory: deep tables with a thin frontier (`KNN_KP` +53%), where every ply scans the whole table. Shallow tables narrow the gap but never close it.
- **Early exit is weaker than claimed** ("refuted on the 1st or 2nd move"): in winning endgames 20–65% of candidates are confirmed losses and each candidate checks 3–5.3 quiet moves on average (`KRP_KQ`: 637M candidates, 19% confirmed, 5.29 moves). The claim holds only in drawish tables (`KQB_KQ`, `KRB_KR`, `KR_KR`: ~1.3–1.7 moves, 1–6% confirmed). Every candidate also pays a full decode (`from_setup`) before the first move.
- **Memory is the real advantage of the sweep**: its footprint is essentially the two tables (2 bytes/pos), while the counter version's queues (8-byte `usize` indices, with duplicates) peak at 3.5–8.4 GiB on shallow 5-piece tables, i.e. several times the tables themselves. This is prohibitive for 6-piece tables. Storing indices as `u32` would halve it, but only as chunk-relative indices: a 6-piece pawnless table has ~6.2 × 10⁹ positions per side, which is more than `u32::MAX`. The queues are not inherent to counters, though: they can be replaced by per-ply table scans (see Section 3.1).
- **Caveats**: the sweep is a first, unoptimized version (re-decodes every candidate, two passes per ply), while the counter version has been profiled. Parallel scalability, the sweep's main theoretical advantage, is not measured yet.

> **Note on BFS Queues vs. Table Sweeps**:
> The divergence between **queue-driven BFS** (tracking explicit predecessor index lists per ply) and **table array sweeps** (flat memory chunk iterations over flags) is a fundamental architectural choice. Table sweeps eliminate BFS queue memory overhead and enable simple chunk-parallel loops in shared memory, while BFS queues only touch reachable indices. This trade-off, and the choice between shared-memory multi-threading and distributed message-passing, is analyzed in [`scaling_and_parallelization.md`](scaling_and_parallelization.md). Note that the two dimensions are independent: "counters vs. candidate flags" (how losses are resolved) and "queues vs. scans" (how the positions to propagate are found) can be combined freely.

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
  - **Important distinction for generation vs. storage**: Retrograde solving inherently alternates between WTM and BTM, so **both sides must remain actively resident in memory during generation**. Dropping one side saves no RAM or cache pressure during active solving; the ~50% savings apply strictly to **final disk storage** and **prober memory footprint**.
- **Relaxed Bounds (chesstb)**:
  - If a position has a winning capture leading to an already-won sub-table, its exact stored value doesn't matter for game-theoretic correctness—the engine can see the winning capture at depth 1. `chesstb` treats these as "don't cares" during compression, allowing LZ algorithms to achieve significantly higher compression ratios.

---

## 3. Key Insights & Concrete Takeaways for `chess-egt`

Studying these three mature projects offers several high-value insights directly applicable to `chess-egt`:

### 1. Moving from Move Counters to `Candidate Loss (Changed) + Forward Check`
In `chess-egt`'s `AGENTS.md`, the complex interaction between move counters and diagonal symmetries is noted as a tricky area requiring special counter doubling ($+2$) and mirror decrements.
- **Insight**: If `chess-egt` replaces decremental counters with a `CHANGED` flag + forward `check_loss` (or uses candidate flags alongside counters):
  - Symmetry handling becomes trivial and robust: marking `CHANGED` or `WIN` is idempotent (multiple identical or symmetric reverse moves can mark the same cell without causing corruption).
  - Eliminates the need for large heap queues (`DepthQueues` vectors): measured 4x lower peak memory on 5-piece tables.
  - Avoids atomic subtracts in multi-threading (idempotent marks still need a CAS/TTAS).
- **Measured trade-off** (see Section 2.A): single-threaded, the sweep is 13% (4-piece) to 23% (5-piece sample) slower, up to +53% on deep thin-frontier tables. The decision therefore hinges on parallel scaling and on memory for 6-piece tables, not on single-threaded speed.
- **Possible middle grounds not yet measured**:
  - Candidate flags + forward verification, but with a queue (e.g. of chunk-relative `u32` indices) instead of full sweeps, which would remove the per-ply scans on deep tables while keeping idempotent marking.
  - **Decremental counters driven by per-ply table scans instead of queues** (the direction proposed in [`scaling_and_parallelization.md`](scaling_and_parallelization.md) Section 4.2). The positions to propagate at ply `p` are exactly those stored with distance `p - 1`, so the queues can be dropped. This keeps the counters' low work per edge (no forward verification of candidates) and has the sweep's memory footprint. Its updates never read remote state, so it also carries over to separate-memory workers. A per-block "last resolved ply" summary lets scans skip untouched blocks on deep tables.

### 2. Multi-Threading & Rayon Parallelization
Currently, `chess-egt` is single-threaded.
- **Insight**: Prophet and Syzygy demonstrate that flat array sweeps over index chunks (`[chunk_start..chunk_end]`) parallelize exceptionally well.
- In Rust, with `rayon`, an entire iteration step can be written as:
  ```rust
  table.par_chunks_mut(CHUNK_SIZE).for_each(|chunk| { ... });
  ```
  Using `AtomicU16` with `fetch_update` or CAS (like Syzygy's `lock cmpxchgb`) makes the propagation loop lock-free. Near-linear scaling is expected for this compute-bound workload but has not been measured. The same holds for atomic counter decrements. The output stays byte-identical only if the updates within a phase give the same result in any order. For counters, this requires resolving the conversion type of new losses per phase (see [`scaling_and_parallelization.md`](scaling_and_parallelization.md) Section 4.2).

### 3. Topological Slicing for Pawnful Tables
- **Insight**: Since pawns cannot move backward, pawn configurations form a Directed Acyclic Graph (DAG).
- As `chesstb` demonstrates, you do not need all pawn configurations of an endgame resident in memory at the same time. You can solve slices at higher ranks (e.g., pawns on 6th/7th rank) first, write them out, and then solve lower ranks. This eliminates the memory wall when scaling from 5-piece to 6-piece endgames.
- In `chess-egt`, pawns are the most significant digits of the index, so a pawn-rank slice of an `Egt` is a contiguous index range (apart from the en passant sub-tables). Successors in already solved slices must be injected at the matching ply, not during initialization. Slices are also the natural unit of independent jobs for distributed generation (see [`scaling_and_parallelization.md`](scaling_and_parallelization.md) Section 3).

### 4. Reverse Capture/Promotion Unmoves vs. Forward Probing
In `chess-egt`'s `TODO`, there is an item: *"Experiment with approach using capture/promotion unmoves for initialization."*
- **Insight**: Both Syzygy and Prophet validate this approach. By iterating over the (much smaller) dependency tables and unmoving captures/promotions into the target table, you eliminate the overhead of forward legality checking and sub-table probe searching for the vast majority of non-capture positions during initialization.

### 5. Probing and Storage Footprint (STM Dropping)
- **Insight**: In `chess-egt`, an `EgtFile` currently stores all positions for both sides. For 5-piece and upcoming 6-piece tables, dropping one side-to-move for asymmetric endgames (reconstructing via 1-ply search in the prober) will immediately reduce **final disk storage and prober memory footprint** by roughly 40–50%.
- Note that during the retrograde generation process itself, both sides must remain active in memory to propagate alternating plies, so this optimization does not reduce generation-time memory pressure.
