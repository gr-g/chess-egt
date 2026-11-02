# AGENTS.md

- The goal of the project is to produce a chess endgame tablebase (EGT).
- The project is currently functional with an implementation of the table
  indexing (`Egt` and `Indexer`), of the file input/output including
  compression/decompression (`EgtFileReader`, `EgtFileWriter`,
  `SharedFileCache`), and of multi-threaded generation of tables through
  retrograde analysis (`RetrogradeSolver`).
- Tables for all 3-piece, 4-piece and 5-piece endgames were generated and
  verified successfully, they are available in `~/ggegt/`.
- The exact library interface to expose and the command line interface are
  still to be finalized.
- Always run `cargo test --release` for testing, otherwise it takes too much
  time.
- Keeping code simple, short and readable has priority over finding a way to
  implement all desired features. If it cannot be done with simple, short and
  readable code, then perhaps it should not be done.
- Adding one or two focused and compact tests to check functionality is ok.
  Adding hundreds and hundreds of lines of tests is not ok.
- Update the documentation after a change if appropriate, but always leave
  AGENTS.md untouched. Do no use documentation as a diary of every change that
  happened, normally documentation should not mention what the code was doing
  in the past.

See the design specifications in `README.md` for more information.

Current tablebase generation results (with --threads=2, --noverify):
```
== All 3 pieces ==
Generated 16 tables: 367868 unique positions, 0.04 MiB in 00h00m00s.
Average compression: 0.84 bits/pos. Lowest compression (KQ_K): 2.04 bits/pos.
```
```
== All 4 pieces ==
Generated 176 tables: 125132316 unique positions, 13.29 MiB in 00h01m41s.
Average compression: 0.89 bits/pos. Lowest compression (KQ_KR): 3.61 bits/pos.
```
```
== All 5 pieces ==
Generated 1496 tables: 25880730473 unique positions, 3578.66 MiB in 06h36m07s.
Average compression: 1.16 bits/pos. Lowest compression (KBB_KQ): 4.23 bits/pos.
```

## TODO
- Parallelization: see the roadmap in `scaling_and_parallelization.md`. Test on EC2 c6g instance.
- `--list-one-side` and `--stats-one-side`?
- `--generate` does not replace existing table.
- Internal module layout; visibility and public interface.
- Experiment with approach using capture/promotion unmoves for initialization.
  Perhaps reconsider concurrency in `SharedFileCache` if only useful for
  validation (collect all dependencies during init?).
- Frontend, cloning https://syzygy-tables.info/
- Explore possibility of generating tables that store together DTC and DTM for
  the same index, not only DTC.
- General cleanup.
