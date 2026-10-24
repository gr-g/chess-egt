# AGENTS.md

- The goal of the project is to produce chess endgame tablebases (EGTs).
- The current status of the project is: there is an implementation of the file and table indexing (`EgtFile`, `Egt` and `Indexer` classes). There is an implementation of compression/decompression (`EgtFile`, `EgtFileWriter`). There is no memory management yet (LRU-eviction of frames from memory). Multi-threaded generation of tablebase outcomes through retrograde analysis of chess position is implemented (`RetrogradeSolver`). Tablebases for all 3-piece, 4-piece and 5-piece endgames were generated and verified successfully. The exact library interface to expose and the command line interface are still to be defined.
- The core loop of the retrograde analysis (`solve_pair` in `retrograde.rs`) uses decremental move counters driven by per-ply table scans (see the module doc and `scaling_and_parallelization.md` 4.2). Changes to the core loop that are not meant to change the output must keep the generated files **byte-identical**: compare sha256 against the reference set of tables in `~/tablebases`.
- Always run `cargo test --release` for testing, otherwise it takes too much time.
- Update the documentation after a change if appropriate, but always leave AGENTS.md untouched.
- Remember that the documentation is intended for humans to quickly grasp what the **current version** of the code is doing and why. It should be simple and to the point. It should **not** be a diary of every change that happened.

Current generation results (with --threads=2, --noverify):
=============================================================================================
Generated all 3-pieces endgames, corresponding to 367868 unique positions.
Time: 00h00m01s.
Size on disk: 0.04MiB (0.84 bits/pos on average, lowest compression for KQ_K: 2.04 bits/pos).
=============================================================================================

=============================================================================================
Generated all 4-pieces endgames, corresponding to 125544710 unique positions.
Time: 00h02m41s.
Size on disk: 13.31MiB (0.89 bits/pos on average, lowest compression for KQ_KR: 3.61 bits/pos).
=============================================================================================

=============================================================================================
Generated all 5-pieces endgames, corresponding to 26040612459 unique positions.
Time: 11h05m04s.
Size on disk: 3592.53MiB (1.16 bits/pos on average, lowest compression for KBB_KQ: 4.23 bits/pos).
=============================================================================================

## TODO
- Parallelization: see the roadmap in `scaling_and_parallelization.md`.
- Proper memory management and LRU-eviction. Keep track of number of uncompressed frames in EgtFile.
- Visibility and public interface.
- Add stats without en passant positions to EgtFileStats and implement EgtProber::verify_with_syzygy() to check our stats against the Syzygy stats available on the internet.
- Experiment with approach using capture/promotion unmoves for initialization.
- Frontend, cloning https://syzygy-tables.info/

## High-Level Architecture
The project is built from the following main components:
1. **Outcome Representation (`DtcOutcome`)**: Encodes the game outcome (Win/Loss/Draw), distance-to-conversion (DTC), and conversion type (Checkmate, Promotion, or Capture) into a compact 16-bit value.
2. **Logical Indexing Layer (`Egt`, `Indexer`)**: Maps canonical chess board positions to a contiguous index space `[0, index_range)`.
3. **Storage & Memory Layer (`EgtFile`, `EgtFileWriter`)**: Manages the physical files on disk, seekable Zstd compression/decompression, and the in-memory frame cache.
4. **Retrograde Analysis** (`RetrogradeSolver`): The recursive algorithm to generate the outcomes, starting from terminal positions (checkmates and known winning/losing positions) and moving backwards to identify all other winning/losing positions.

See the design specifications in `README.md` for more information.
