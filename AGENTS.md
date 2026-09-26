# AGENTS.md

- The goal of the project is to produce chess endgame tablebases (EGTs).
- The current status of the project is: there is an implementation of the file and table indexing (`EgtFile`, `Egt` and `Indexer` classes). There is an implementation of compression/decompression. There is no memory management yet (LRU-eviction of frames from memory) and no parallelization. The generation of tablebase outcomes through retrograde analysis of chess position is implemented (`RetrogradeSolver`) and looks pretty solid. Tablebases for all 3-piece, 4-piece and 5-piece endgames were generated and verified successfully. The exact library interface to expose and the command line interface are still to be defined.
- The details of the retrograde analysis implementation differ from the design specifications, as this part is in flux. Currently two interchangeable core loops exist, selected with `--algorithm` (`Algorithm` enum, `EgtGenerator::with_algorithm`): `counters` (default, decremental move counters + BFS queues, `solve_pair_counters` in `retrograde.rs`) and `sweep` (Syzygy-style `CHANGED` candidate flags + forward verification + full table sweeps per ply, `retrograde_sweep.rs`). Both must produce **byte-identical** files; any change to either must preserve this (compare sha256 against a reference set of tables). The unit tests run both.
- Always run `cargo test --release` for testing, otherwise it takes too much time.

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
- Counters vs sweep (single-threaded, see `algorithms_comparison.md` 2.A for
  details): sweep is 13% slower on all 4-piece tables and 23% slower on a
  5-piece sample (up to +53% on deep, thin-frontier tables like `KNN_KP`), but
  peak memory is ~4x lower (2.0 vs 8.4 GiB on `KRP_KQ`), because the counters'
  `usize` queues peak at several times the size of the tables on shallow
  endgames. The choice will depend on parallel scaling and 6-piece memory.

Current generation results (single-threaded generation, with --noverify):
=============================================================================================
Generated all 3-pieces endgames, corresponding to 367868 unique positions.
Time: 00h00m01s.
Size on disk: 0.03MiB (0.77 bits/pos on average, lowest compression for KQ_K: 2.07 bits/pos).
=============================================================================================

=============================================================================================
Generated all 4-pieces endgames, corresponding to 125544710 unique positions.
Time: 00h03m14s.
Size on disk: 13.25MiB (0.89 bits/pos on average, lowest compression for KQ_KR: 3.61 bits/pos).
=============================================================================================

=============================================================================================
Generated all 5-pieces endgames, corresponding to 26040612459 unique positions.
Time: 26h01m51s.
Size on disk: 3593.31MiB (1.16 bits/pos on average, lowest compression for KBB_KQ: 4.23 bits/pos).
=============================================================================================

## TODO
- Counters vs sweep follow-up, parallelization: see `scaling_and_parallelization.md`.
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
