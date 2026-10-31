## Reproducible native benchmarks

Use [`scripts/benchmark_generation.py`](scripts/benchmark_generation.py) with
Python 3.11+ on Linux. Use a fresh output directory and a current tablebase root
containing `format.json` and all dependencies needed by the requested selectors.
The script benchmarks each selector separately. Start with a small selector,
not a full piece-count set.

For example, run from the repository root:

```sh
python3 scripts/benchmark_generation.py \
  --binary target/release/chess-egt \
  --input-path ~/ggegt \
  --output target/generation-benchmark \
  --selectors KQ_K KPa_K --threads 1 4 --repeats 2 --timeout 600
```

Each run records wall/CPU time, peak RSS, and summed initialization, propagation,
and finalization/publication timings across its pair jobs.

Artifacts are individual tables at `n/maxvmin/opK/Npawns/name.ggegt` with JSON
sidecars. The script hashes relative hierarchical paths after timing,
checks repeat/thread-count determinism, and records build/machine metadata. It
does not compare against an older reference generation. Historical measurements
in the design documents may use earlier storage layouts and are labeled as such.

## Bounded profiling

Build with `cargo build --locked --profile profiling` to retain debug symbols.
Start with a small endgame and fresh output paths; bound runtime with `timeout`:

```sh
timeout 120s valgrind --tool=callgrind --cache-sim=no --branch-sim=no \
  --callgrind-out-file=target/generation-kq-k.callgrind \
  target/profiling/chess-egt \
  --path target/generation-profile-tables --input-path ~/ggegt \
  --generate KQ_K --threads 1 --noverify
callgrind_annotate --auto=no --threshold=100 target/generation-kq-k.callgrind
callgrind_annotate --auto=no --inclusive=yes --tree=caller --threshold=100 \
  target/generation-kq-k.callgrind
```

### Measured example: `KQ_KR` / `KR_KQ`

Measured on 2026-10-31 at revision `4863a6f`, with Rust 1.99.1 and Valgrind
3.27.1, on an x86-64 VM exposing four AMD Ryzen 7 5700G CPUs. The profiling
build uses release optimization (fat LTO, one codegen unit), debug symbols,
and no additional `RUSTFLAGS`. This is a portable build, not CPU-native codegen.

`--generate KQ_KR` generates both sides of the pair: 2,467,122 unique valid
positions, reaching depth 62, with 1.05 MiB of compressed output. It is large
enough to exercise propagation and capture dependency probes, but comfortably
fits a ten-minute Callgrind budget. Dependencies were read from `/home/user/ggegt`.

```sh
cargo build --locked --profile profiling
timeout --signal=TERM --kill-after=15s 600s \
  valgrind --tool=callgrind --cache-sim=no --branch-sim=no \
  --callgrind-out-file=target/profile-kq-kr.callgrind \
  target/profiling/chess-egt \
  --path target/profile-kq-kr-tables --input-path ~/ggegt \
  --generate KQ_KR --threads 1 --noverify
callgrind_annotate --auto=no --threshold=100 target/profile-kq-kr.callgrind
callgrind_annotate --auto=no --inclusive=yes --tree=calling --threshold=100 \
  target/profile-kq-kr.callgrind
callgrind_annotate --auto=no --threshold=100 target/profile-kq-kr.callgrind \
  src/egt/indexer.rs src/retrograde.rs
```

Use fresh output paths when repeating. A separate native run of the **same
profiling binary** used `target/profile-kq-kr-native` and otherwise identical
application arguments. These are single-run measurements, not a benchmark
comparison against an earlier revision.

| Phase | Native | Under Callgrind |
| --- | ---: | ---: |
| Initialization | 0.878 s | 31.158 s |
| Propagation | 3.434 s | 144.483 s |
| Finalization/publication | 2.020 s | 37.391 s |
| Whole-process wall time | 6.339 s | 213.703 s |

Callgrind collected **76,976,672,589 instructions (`Ir`)**. Cache and branch
simulation were disabled: these numbers do not measure cache misses, branch
mispredictions, lock contention, or CPU cycles. Instrumentation changes the
relative phase timings substantially; use native runs to validate speedups.

#### Findings

- **Position encoding dominates: 45.09% inclusive, 31.71% self.**
  `Indexer::position_to_index` ran **40,283,101 times** (about 862 instructions
  per call including callees). Phase L contributed 22,800,742 calls / 25.63% of
  total instructions; phase W contributed 16,507,356 / 18.56%; dependency probes
  contributed 975,003 / 0.90%. Thus 98% of encoding instructions come from
  propagation, not dependency lookup. The pipeline extracts coordinates,
  canonicalizes symmetries, compacts squares, and aggregates the index for every
  accepted predecessor (`src/retrograde.rs:411`, `src/egt/indexer.rs:366`).
- **There is measurable generic overhead even for simple material.**
  `sort_coord_repeated_pieces` costs **3.47% of total instructions** over
  40,627,849 calls, despite neither table nor its capture dependencies having
  repeated-piece groups. The helper traverses groups and tests multiplicity;
  the actual `sort_by_key` branch does not execute. Most of this cost is already
  included in encoding's 45.09%. Precomputing only the groups needing sorting
  is a concrete small experiment. `cpidx_to_index` costs another 5.93% self and
  `reduce_symmetries` 4.05% self, also mostly inside encoding.
- **Material assertions perform software population counts.**
  `position_to_coord` uses ordinary release-enabled assertions for occupied
  count and each piece-group count (`src/egt/indexer.rs:554–575`). Disassembly
  confirms shift/mask/multiply population-count sequences rather than `popcnt`
  in this binary. Integer-macro source attributed to `position_to_index` alone
  accounts for 7.13% of total instructions (not a precise assertion-only cost).
  A CPU-targeted build is worth comparing natively before redesigning indexing.
  A trusted internal encoding path is another hypothesis; retain validation at
  public boundaries rather than simply deleting assertions.
- **Compression is substantial, not statistics or publication.**
  `ZSTD_compressStream2` accounts for **19.45% inclusive**, versus 20.24% for the
  finalization batch traversal including compression and statistics. The main
  compression kernels are `ZSTD_compressBlock_opt2` (11.02% self) and
  `ZSTD_btGetAllMatches_noDict_3` (6.86% self). Finalization uses compression
  level 19. It takes about **32% of native generation time**, making a
  compression-level/time/size comparison worthwhile if output-size tradeoffs
  are acceptable. A different level would change compressed bytes.
- **An avoidable pawnless scan exists, but its isolated cost is not measured.**
  Phase L scans Checkmate, Capture, and Promotion whenever the preceding ply
  contains any wins (`src/retrograde.rs:681–690`). Promotion never occurs for
  this pawnless pair, so that sub-phase scans and matches nothing. Tracking
  wins by conversion type could skip it. The block summary visits 45,566 of
  58,499 potential blocks (77.9%); this measures block pruning, not the fraction
  of useful entries. Do not confuse the large Rayon-named scan functions with
  scheduling overhead: they include inlined unmove generation, board copying,
  attack checks, atomic updates, and scanning.

The first experiments to prioritize are CPU-targeted native benchmarking and
removing no-op group traversal, followed by skipping empty conversion-type
scans. Incremental predecessor encoding or a generation-specific index layout
remains the larger, more invasive opportunity. There is no evidence here to
prioritize dependency-cache redesign or a return to full position validation:
propagation already uses unchecked reconstruction with explicit opposite-check
filtering. This pawnless, single-worker sample does not characterize pawn/EP
handling, repeated-piece sorting, large-table memory behavior, or contention.

#### Reading and validating the artifacts

Rust/LTO splits one function's instruction attribution across project and
library source files. Sum **self** costs by function across those partitions;
do not add overlapping inclusive rows. For example, the indexer-source self row
for `position_to_index` alone reports only 12.54%, whereas its full self cost is
31.71%. Its 45.09% inclusive cost was checked by summing incoming call edges.
Library paths mentioning sorting do not establish that an actual sort ran.

Local artifacts are `target/profile-kq-kr.callgrind`, `.self.txt`, `.inclusive.txt`,
`.callers.txt`, and `.source.txt`; generation logs and timing JSON are
`target/profile-kq-kr.log`, `target/profile-kq-kr.time.json`, and
`target/profile-kq-kr-native.{log,time.json}`. These files are under ignored
`target/`, not versioned fixtures.

Both generated `.ggegt` files were byte-identical between native and Callgrind
runs. After profiling, both tables passed internal consistency verification:

```sh
target/profiling/chess-egt \
  --path target/profile-kq-kr-tables --input-path ~/ggegt \
  --verify KQ_KR KR_KQ --threads 1
```

No solver changes were made. Any behavior-preserving hot-path optimization
should retain byte-identical output and be checked on larger and pawnful
samples as well.
