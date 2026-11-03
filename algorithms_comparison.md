# Endgame Tablebase Generation: Comparative Analysis

A comparison of the current generation algorithms and storage architectures in:
- **chess-egt** (our Rust project)
- **Syzygy `tb`** by Ronald de Man (`https://github.com/syzygy1/tb`)
- **Prophet TB** (`https://github.com/markus7800/prophet_tb_gen_and_probe`)
- **chesstb** (`https://github.com/noobpwnftw/chesstb`)

Quantitative scale, measured performance, and resource/cost estimates are covered in
[`scaling.md`](scaling.md). Our implementation is described in
[the README](README.md#retrograde-analysis) and [`src/retrograde.rs`](src/retrograde.rs).
The comparisons below are based on source inspection, not cross-engine benchmarks.

---

## 1. High-Level Comparison Table

| Feature / Dimension | **chess-egt** | **Syzygy (`tb`)** | **Prophet** | **chesstb** |
| :--- | :--- | :--- | :--- | :--- |
| **Metrics** | DTC (Distance to Conversion) | WDL + DTZ with 50-move-rule information | DTM, ignoring the 50-move rule | WDL, DTZ, DTC, DTM, DTM50 |
| **Working Representation** | `AtomicU16`: 3-bit status + 13-bit counter / distance | `uint8_t`: status and ply codes | `int16_t`: signed mate distance and flags | Metric-specific packed entries; 16-bit distance entries |
| **Working Memory** | 2 bytes per index in each live twin table; one array for a self-twin | 1 byte per index per STM array; both arrays resident | 2 bytes per index per STM array, plus resident dependencies | Slice-group pager with a soft `--mem` budget |
| **Loss Resolution** | Decremental move counter in `UNKNOWN` bits | `CHANGED` + forward verification | `MAYBELOSS` + forward verification | Changed flags / scheduled bounds + forward verification |
| **Propagation** | Per-ply scans with block summaries; no frontier queues | Flat table scans | OpenMP flat table scans | Active-group scans with dynamically claimed chunks |
| **Parallelization** | Rayon; atomic CAS updates; sequential pair jobs share a pool/cache | Pthreads / C11 threads; atomic byte updates | OpenMP | Custom thread pool and shared chunk scheduling |
| **Dependencies** | Forward capture/promotion probes during initialization | Reverse marking for non-pawn captures; forward pawn capture/promotion probes | Forward probes, plus a reverse noncapture-promotion pass for six-piece generation | Forward probes and topologically ordered pawn slices |
| **Indexing Approach** | Compact mixed-radix / combinatorial groups; occupied-square compaction; pawn-file/order partitions; same working/file order | Regular king-pair / square-field working layout; pawn-pivot partitions; separate compact combinatorial file encoding | King-pair + fixed-order combinatorial groups; separate en passant region; same working/file order | Pawn slice × king slice × mixed-radix nonpawn classes; combinatorial ranking within classes; permutable file dimensions |
| **Final Compression** | Seekable Zstd with byte transposition | Recursive pair substitution + Huffman coding | Independent Zstd blocks, normally 32K positions | Packed WDL + LZ4-HC; ranked distance streams + LZMA |
| **Compression-Driven Index Permutations** | No | Heuristic search using compression estimates | None found in the reviewed paths | All allowed nonpawn-class permutations, evaluated on samples |

Memory figures describe working entries, not total process RSS. Index ranges differ
between engines, so bytes per index alone do not determine relative memory use.

---

## 2. Deep Dive into the Four Algorithms

### A. Resolution Strategy: Decremental Counters vs. Candidate Verification

**chess-egt** resolves losses without regenerating their forward moves during
propagation:

1. Initialization stores the number of moves not yet known to lose in the 13 spare
   bits of each unresolved entry. Capture/promotion results are incorporated here;
   checkmates, stalemates and immediately resolved conversions seed the solve.
2. At ply `p`, scans find positions resolved at `p - 1`. Losses mark quiet
   predecessors as wins; wins decrement unresolved predecessors' counters. A zero
   counter resolves a loss. Remaining unknowns become draws when propagation stops.
3. `PlySummary` records ply bounds by source kind in 4,096-position blocks, allowing
   irrelevant blocks to be skipped. There are no frontier-index queues.
4. Rayon parallelizes initialization and scans. Cross-index updates use relaxed
   atomic CAS, with barriers between phases. Ordered conversion-type phases make
   distance and conversion-type tie-breaking deterministic.

**Syzygy, Prophet and chesstb** instead mark potential losses and verify them by
checking forward successors. A known loss still gives its predecessors a win;
new wins flag predecessors for reconsideration. Verification can stop at the first
successor that refutes a loss. Previously computed conversion results avoid
repeated dependency probing in the main loop.

The implementations differ: Syzygy combines checking and propagation in its scan
dispatch; Prophet uses separate OpenMP passes; chesstb also checks positions whose
scheduled bounds become due, even without a changed flag. They are not all the
same three-pass algorithm.

**Tradeoff:** counters avoid repeated forward move generation, but each decrement
must be accounted for exactly, including symmetry multiplicities, and concurrent
decrements require atomic read-modify-write operations. Candidate marking is
idempotent and can suppress redundant writes, but verification costs additional
decoding and forward move generation. Syzygy uses atomic flag updates; Prophet's
reviewed propagation uses plain shared assignments, not the same CAS/TTAS scheme.
Idempotence alone is not a guarantee of contention-free or race-free execution.

All four use scans rather than explicit BFS frontiers. The choice of **how to
resolve losses** (counters or verification) is independent of **how to find work**
(queues or scans). Scans avoid frontier memory but can revisit large index ranges
for deep, sparse frontiers; block/group skipping mitigates this.

### B. Memory Allocation & Scaling to 6+ Pieces

- **chess-egt:** the current logical table pair is held in flat `Vec<AtomicU16>`
  arrays. For distinct twins the base cost is `2 * (indices_A + indices_B)` bytes;
  a self-twin needs one array. Block summaries, cached dependency frames and
  finalization buffers are additional. Pawn-file/order partitioning reduces the
  individual jobs, but each job is solved fully in RAM: `--cache-size` budgets
  retained probe frames, not solver arrays or total RSS. Finalization compresses
  frame batches in parallel and retains compressed segments until publication.
- **Syzygy:** both byte arrays remain resident during retrograde solving. `--disk`
  saves intermediate tables so compression can reuse their memory; it is **not an
  out-of-core solver**. Its README specifies at least 16 GB for six-piece
  generation (with `--disk`) and 1 TB for seven-piece generation. WDL and DTZ output
  preparation/compression are separate stages, not two independent full solves.
- **Prophet:** holds both `int16_t` arrays and capture dependencies in memory.
  Its README reports generation through six pieces on a 96 GB machine; this is a
  reported configuration, not a universal per-job RAM requirement. Full-index
  OpenMP loops use static chunks of 2,048 entries.
- **chesstb:** an application-level pager moves slice groups between resident
  entries, compressed RAM and scratch files. `--mem` is a **soft MiB budget**, not
  a strict RSS cap. The active group and required neighboring groups impose a
  material-dependent minimum working set. Paging supports larger jobs than a
  fully resident design, but does not establish that arbitrary seven/eight-piece
  jobs fit a small budget: its README estimates a 2.25 TiB minimum working set for
  the pawnless eight-piece `KQRBKQRN` configuration.

### C. Symmetry & Invariant Handling

All four exploit eightfold board symmetry for ordinary pawnless endgames. This
is not synonymous with using the same index encoding or eliminating every
symmetry duplicate. With pawns, only file reflection remains as a spatial board
symmetry; color/rank normalization is a separate operation. chesstb additionally
supports castling tables, where those spatial reductions are disabled.

- **chess-egt:** pawnless kings use 462 canonical non-attacking pairs. When both
  kings lie on the long diagonal, other pieces can still distinguish two
  reflected indices; both are populated, and one is counted as redundant in
  unique-position statistics. Edges crossing these symmetry cases require
  doubled counter contributions or mirrored predecessor updates, as detailed in
  [`src/retrograde.rs`](src/retrograde.rs). With pawns, reflected asymmetric
  file/order configurations share a canonical table; internally symmetric
  configurations choose an orientation using the king ordering.
- **Syzygy:** pivot-mirror helpers update the reflected predecessor where the
  pivot configuration leaves a diagonal ambiguity. Idempotent marking tolerates
  duplicate updates, but does not remove the need to generate every required
  reflected predecessor.
- **chesstb:** pawn slices are solved in dependency order, starting with lower
  total plies-to-promotion. A post-push slice is solved before the pre-push slice
  that depends on it. This reduces the active working set. Our fixed pawn
  file/order partitioning also survives quiet moves, but **does not currently
  solve separate pawn-rank slices**: pawn pushes propagate within the live pair.

### D. Capture & Promotion Dependencies (Sub-Tables)

Conversions connect a table to different material configurations; promotion
need not reduce the number of pieces. These dependencies must be solved before
the parent can incorporate their results.

- **chess-egt:** initialization decodes parent positions, plays legal captures
  (including en passant) and promotions, and probes existing dependencies through
  the shared reader/cache. Selected prerequisites can be generated earlier, but
  the solver does not recursively generate missing tables. Propagation then uses
  only quiet unmoves within the pair. Reverse capture/promotion initialization is
  not implemented.
- **Syzygy:** `probe_captures_w/b` enumerate child placements with a non-pawn piece
  omitted, probe their results, then reconstruct capture predecessors to mark
  the parent. This is reverse capture marking, not necessarily sequentially
  reading the child's file indices. Pawn captures, capture-promotions and
  noncapturing promotions also have forward parent-probing paths.
- **Prophet:** normally probes resident capture/promotion tables forward. For
  six-piece generation, the driver disables resident noncapture-promotion
  tables; `retrograde_promotion_tbs` reads their compressed blocks and reverse
  promotes to seed parent bounds. This is a fixed policy, not an adaptive memory
  budget. Capture and capture-promotion dependencies remain resident.
- **chesstb:** probes captures/promotions forward during initialization. Quiet
  propagation handles predecessors within the current solve; pawn pushes into
  already-solved slices supply bounds according to the ordering in part C.

### E. Compression and Probing Optimizations

**Final codecs and value transforms:**

- **chess-egt:** each exact canonical table is a seekable Zstd object, with
  independent frames of 262,144 positions. The fixed transform writes low bytes
  first, then high bytes only for decisive outcomes, padding the remaining tail
  with zeros. This is **byte transposition**, not bit-plane transposition or an
  index permutation. Files retain the generation index order and exact DTC and
  conversion-type values.
- **Syzygy:** final WDL/DTZ payloads use recursive pair substitution and Huffman
  coding, with indexed blocks, don't-care handling and DTZ value remapping.
  LZ4/Zstd in the generator compress **temporary files**, not the final probe
  payload. Piece-order search is a separate optimization (part F).
- **Prophet:** compresses contiguous signed-16-bit values directly with Zstd;
  the default is 32,768 positions (64 KiB raw) per independent block.
- **chesstb:** packs WDL into 4-bit codes before LZ4-HC, optionally with a trained
  dictionary. Distance output uses frequency-ranked streams and LZMA. The
  configured block sizes are 64 KiB for WDL and 1 MiB for distances; pager
  compression is a separate concern.

**Dropping one side to move:** this is a storage/probing tradeoff, not an inherent
halving of generation memory or file size.

- chess-egt publishes each distinct twin table and does not reconstruct an
  omitted twin by minimax probing. A self-twin shares one table by symmetry,
  which is different from dropping a side.
- Syzygy stores both sides in asymmetric **WDL** files, but only one side in
  **DTZ** files, selected using compression estimates. DTZ reconstruction requires
  WDL-aware zeroing/distance handling, not a generic minimax formula alone.
- Prophet can reconstruct a missing STM file by probing all legal successors,
  including captures and promotions. This is one local minimax expansion;
  missing dependency-side files can cause further recursive expansions.
- chesstb's `shrink` drops the larger eligible compressed side, with exceptions
  for symmetry and other format constraints. Savings depend on the two payload
  sizes, rather than necessarily approaching 50%.

Both logical STM states participate in solving, but both **entire arrays** need
not be resident in a paged solver such as chesstb. File-side dropping is separate
from that paging mechanism.

**Relaxed bounds:** chesstb can leave certain values implicit when its prober can
recover them from conversions. Relaxed WDL combines a stored lower class with
capture/promotion bounds; relaxed DTZ can omit winning DTZ-1 cells justified by a
class-preserving zeroing move, including a pawn move. These are metric-specific
rules, not permission to replace arbitrary exact DTM/DTC values after a winning
capture. chess-egt does not currently use such probe-time relaxation.

### F. Indexing Approaches & Compression Permutation Search

There are two distinct goals: minimizing the number of indexed cells, and
ordering those cells for efficient generation or compression. A compact index
can cost more to encode/decode; a regular working layout can leave illegal cells
but simplify scans and unmoves. A file layout can differ from both.

| Project | Working Index | File Index / Compression Ordering |
| :--- | :--- | :--- |
| **chess-egt** | Compact mixed-radix groups, with occupied-square compaction and combinatorial ranking of identical pieces; pawn-file/order partitioning | Same index order; fixed byte transform, no compression-driven permutation search |
| **Syzygy** | Regular pawnless king-pair index plus six-bit square fields; pawn-pivot/file partitions for pawnful generation | Separate compact combinatorial encoding; searches piece-type/factor order using estimated compressed size |
| **Prophet** | King-pair index plus fixed-order combinatorial piece groups; extra en passant region | Same order as the working arrays; no permutation search found in the reviewed indexing/compression paths |
| **chesstb** | Pawn slice × king slice × mixed-radix nonpawn classes; identical pieces ranked within each class | Keeps slice geometry but searches permutations of nonpawn-class dimensions, per output/color |

**Our pawn-first encoding:** each table fixes pawn files and the relative color
order on each file. A file containing `k` pawns ranks their occupied ranks among
`C(6, k)` possibilities, irrespective of color, since color order is already
fixed. Kings and nonpawns are encoded after removing occupied-square holes;
identical nonpawns use combination ranks. Pawnless kings instead start with the
462-pair lookup. Symmetric pawn tables pack the kings as a two-square combination
to choose one reflected orientation; other pawn tables encode the kings
individually. Legal en passant states occupy additional internal index ranges.
This excludes overlaps structurally, but does not eliminate every illegal chess
position or the pawnless diagonal redundancy described in part C.

**Other layouts:** Syzygy's compact file encoder removes occupied-square gaps and
ranks identical groups, unlike its more regular generation layout; its pawnless
file pivot cases are not universally a 462-king-pair scheme. Prophet uses 462
pawnless / 1,806 pawnful king-pair entries and combination ranks, while retaining
some invalid/unused cells. chesstb ranks identical pieces over their class's
allowed squares, but overlaps between different classes remain illegal cells.
Its slice geometry is chosen to support paging and dependency locality rather
than global occupied-square compaction.

**Permutation search** rearranges index dimensions to put similar outcome
patterns near one another without changing the answers:

- **Syzygy** heuristically explores piece-type and factor orders using sampled
  compression estimates. It does not guarantee the globally best ordering.
- **chesstb** evaluates all allowed permutations of populated nonpawn classes,
  comparing compressed sizes of evenly spaced sample blocks. This is exhaustive
  over those permutations, but sampled over contents, so not a whole-file optimum
  guarantee.
- **chess-egt** does **not** perform this search. Its group order is fixed, and
  finalization only transforms values within that order. **Prophet** likewise has
  no compression-driven permutation search in the reviewed paths.

### Source Guide

The relevant implementations in the compared repositories are:

- **chess-egt:** [`src/retrograde.rs`](src/retrograde.rs),
  [`src/egt/indexer.rs`](src/egt/indexer.rs),
  [`src/egt_file.rs`](src/egt_file.rs),
  [`src/finalization.rs`](src/finalization.rs).
- **Syzygy:** `src/rtbgen.c`, `src/rtbgenp.c` (resolution and initialization);
  `src/tbgen.c`, `src/tbgenp.c` (allocation/output stages); `src/probe.c`
  (compact encoding); `src/permute.c`, `src/permute_tmpl.c` (ordering search);
  `src/compress.c`, `src/compress_tmpl.c`, `src/huffman.c` (final compression);
  `src/util.c` (temporary-file compression); `README.md` (RAM and file scopes).
- **Prophet:** `src/gen_egtb.cpp`, `src/gen_main.cpp` (generation/dependencies);
  `src/linearize.cpp`, `src/kkx.h`, `src/triangular_indexes.h` (indexing);
  `src/compressed_tb.cpp`, `src/recompress.cpp` (compression);
  `src/prophet.cpp` (missing-side probing).
- **chesstb:** `src/egtb/egtb_gen_dtc.cpp`, `src/egtb/egtb_gen_dtm.cpp`,
  `src/egtb/egtb_gen_dtz.cpp`, `src/egtb/egtb_gen.h` (generation/paging);
  `src/egtb/piece_config_for_gen.cpp`, `src/egtb/piece_group.cpp`,
  `src/egtb/pawn_slice_manager.cpp` (indexing/slices);
  `src/egtb/egtb_compress.cpp`, `src/egtb/index_permutation_plan.h`
  (compression/ordering); `src/shrink/shrink.cpp`, `src/egtb/relax_bound.h`,
  `src/probe/probe.cpp` (dropping/relaxed probing).
