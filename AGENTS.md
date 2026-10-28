# AGENTS.md

- The goal of the project is to produce chess endgame tablebases (EGTs).
- The current status of the project is: there is an implementation of the file and table indexing (`Egt` and `Indexer` classes). There is an implementation of file management with compression/decompression, supporting a reader cache with LRU frame eviction (`EgtFileReader`, `EgtFileWriter`, `SharedFileCache`). Multi-threaded generation of tablebase outcomes through retrograde analysis of chess position is implemented (`RetrogradeSolver`). Tablebases for all 3-piece, 4-piece and 5-piece endgames were generated and verified successfully, they are available in `~/ggegt/`.
- The exact library interface to expose and the command line interface are still to be defined.
- The core loop of the retrograde analysis (`solve_pair` in `retrograde.rs`) uses decremental move counters driven by per-ply table scans (see the module doc and `scaling_and_parallelization.md` 4.2). Changes to the core loop that are not meant to change the output must keep the generated files byte-identical.
- Always run `cargo test --release` for testing, otherwise it takes too much time.
- Update the documentation after a change if appropriate, but always leave AGENTS.md untouched. Remember that the documentation is intended for humans, to quickly grasp what the **current version** of the code is doing and why. It should be simple and to the point. It should **not** be a diary of every change that happened.

Current generation results (with --threads=2, --noverify):
== All 3 pieces ==
Generated 17 tables: 368330 unique positions, 0.04 MiB in 00h00m01s.
Average compression: 0.84 bits/pos. Lowest compression (KQ_K): 2.04 bits/pos.
== All 4 pieces ==
Generated 176 tables: 125544710 unique positions, 13.31 MiB in 00h02m18s.
Average compression: 0.89 bits/pos. Lowest compression (KQ_KR): 3.61 bits/pos.
== All 5 pieces ==
Generated 1496 tables: 26040612459 unique positions, 3592.45 MiB in 09h38m05s.
Average compression: 1.16 bits/pos. Lowest compression (KBB_KQ): 4.23 bits/pos.

## TODO
- Parallelization: see the roadmap in `scaling_and_parallelization.md`.
- Internal module layout; visibility and public interface.
- Add stats without en passant positions to EgtFileStats and implement a verify_with_syzygy() function.script to check our stats against the Syzygy stats available on the internet.
- Experiment with approach using capture/promotion unmoves for initialization. Perhaps reconsider concurrency in `SharedFileCache` if only useful for validation.
- Frontend, cloning https://syzygy-tables.info/

## High-Level Architecture
The project is built from the following main components:
1. **Outcome Representation (`DtcOutcome`)**: Encodes the game outcome (Win/Loss/Draw), distance-to-conversion (DTC), and conversion type (Checkmate, Promotion, or Capture) into a compact 16-bit value.
2. **Logical Indexing Layer (`Egt`, `Indexer`)**: Maps canonical chess board positions to a contiguous index space `[0, index_range)`.
3. **Storage & Memory Layer (`EgtFileReader`, `EgtFileWriter`, `SharedFileCache`)**: Manages the physical access to files through `object_store`, seekable Zstd compression/decompression, and in-memory frame cache.
4. **Retrograde Analysis** (`RetrogradeSolver`): The backward-propagation algorithm to generate the outcomes, starting from terminal positions (checkmates and known winning/losing positions) and moving backwards to identify all other winning/losing positions.

See the design specifications in `README.md` for more information.
