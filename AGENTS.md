# AGENTS.md

- The goal of the project is to produce chess endgame tablebases (EGTs).
- The current status of the project is: there is an implementation of the file and table indexing (`EgtFile`, `Egt` and `Indexer` classes). There is an implementation of compression/decompression. There is no memory management yet (LRU-eviction of frames from memory) and no parallelization. The generation of tablebase outcomes through retrograde analysis of chess position is implemented (`RetrogradeSolver`) and looks pretty solid. Tablebases for all 3-piece, 4-piece and 5-piece endgames were generated and verified successfully. The exact library interface to expose and the command line interface are still to be defined.
- The core loop of the retrograde analysis (`solve_pair` in `retrograde.rs`) uses decremental move counters driven by per-ply table scans (see the module doc and `scaling_and_parallelization.md` 4.2). Two earlier alternatives (counters + BFS queues, and Syzygy-style candidate flags + forward verification) were removed after producing byte-identical output (see `algorithms_comparison.md` 2.A and the git history). Changes to the core loop that are not meant to change the output must keep the generated files **byte-identical**: compare sha256 against the reference set of tables in `~/tablebases`.
- Always run `cargo test --release` for testing, otherwise it takes too much time.
- Update the documentation after a change if appropriate, but always leave AGENTS.md untouched.

## Performance notes (measured on all 4-piece endgames)

- Profile with `cargo build --profile profiling`, which keeps debug symbols.
  `valgrind --tool=callgrind --cache-sim=no --branch-sim=no` plus
  `callgrind_annotate [--inclusive=yes]` gives deterministic instruction counts,
  at roughly a 30x slowdown, so profile a single endgame rather than a full run.
- The workload is **compute-bound, not memory-bound**: cachegrind reports a D1
  miss rate of 0.2% for `KQ_KP`. Optimizing memory access patterns is therefore
  not worthwhile at this size (this may change for 6+ pieces).
- Remaining hotspots, in order: `shakmaty`'s `Chess::from_setup` (~19% of
  instructions, i.e. re-validating positions that we know are legal because we
  just decoded them from a valid index), `Indexer::position_to_index` (~7%),
  and zstd compression in `save_to_file` (~5-24% depending on the endgame).
- Zstd level 19 vs 9 on pawnless tables: 15% faster generation for 29% larger
  files. Level 19 is kept, since the tables are the artifact.
- Table scans vs the former BFS queues (single-threaded, 5-piece sample of
  `algorithms_comparison.md` 2.A): +0.9% time overall, with peak memory reduced
  to about the tables themselves (2.0 vs 8.4 GiB on `KRP_KQ`). The worst cases
  are deep tables where each ply resolves few positions (`KNN_KP`, +10%), where
  the full-table scans dominate (Step 2 of the roadmap).

Current generation results (single-threaded generation, with --noverify):
=============================================================================================
Generated all 3-pieces endgames, corresponding to 367868 unique positions.
Time: 00h00m01s.
Size on disk: 0.03MiB (0.77 bits/pos on average, lowest compression for KQ_K: 2.07 bits/pos).
=============================================================================================

=============================================================================================
Generated all 4-pieces endgames, corresponding to 125544710 unique positions.
Time: 00h05m39s.
Size on disk: 13.25MiB (0.89 bits/pos on average, lowest compression for KQ_KR: 3.61 bits/pos).
=============================================================================================

=============================================================================================
Generated all 5-pieces endgames, corresponding to 26040612459 unique positions.
Time: 26h01m51s.
Size on disk: 3593.31MiB (1.16 bits/pos on average, lowest compression for KBB_KQ: 4.23 bits/pos).
=============================================================================================

## TODO
- Parallelization: see the roadmap in `scaling_and_parallelization.md` (Step 1 is done).
- Use `object_store` crate to use cloud storage in addition to local filesystem.
- Proper memory management and LRU-eviction. Keep track of number of uncompressed frames in EgtFile.
- Visibility and public interface.
- Add stats without en passant positions to EgtFileStats and implement EgtProber::verify_with_syzygy() to check our stats against the Syzygy stats available on the internet.
- Use compressed frames to generate compressed file (with zeekstd RawEncoder?).
- Experiment with approach using capture/promotion unmoves for initialization.
- Internalize quiet_unmoves() and use stock shakmaty?
- Frontend, cloning https://syzygy-tables.info/
- Avoid the `from_setup` position revalidation when decoding an index (needs an unchecked construction path in shakmaty)?

## High-Level Architecture
The project is built from the following main components:
1. **Outcome Representation (`DtcOutcome`)**: Encodes the game outcome (Win/Loss/Draw), distance-to-conversion (DTC), and conversion type (Checkmate, Promotion, or Capture) into a compact 16-bit value.
2. **Logical Indexing Layer (`Egt` & `Indexer`)**: Maps canonical chess board positions to a contiguous index space `[0, index_range)`.
3. **Storage & Memory Layer (`EgtFile`)**: Manages the physical files on disk, seekable Zstd compression/decompression, and the in-memory frame cache.
4. **Retrograde Analysis** (`RetrogradeSolver`): The recursive algorithm to generate the outcomes, starting from terminal positions (checkmates and known winning/losing positions) and moving backwards to identify all other winning/losing positions.

See the design specifications in `README.md` for more information.
