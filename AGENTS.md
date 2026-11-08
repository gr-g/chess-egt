# AGENTS.md

- The goal of the project is to produce a chess endgame tablebase (EGT).
- The project is currently functional with an implementation of the table
  indexing (`Egt` and `Indexer`), of the file input/output including
  compression/decompression (`EgtFileReader`, `EgtFileWriter`,
  `SharedFileCache`), and of multi-threaded generation of tables through
  retrograde analysis (`solve_pair`).
- Tables for all 3-piece, 4-piece and 5-piece endgames were generated and
  verified successfully, they are available in `~/ggegt/`.
- An API server is available (`bin/server.rs`) providing stats and position
  outcomes via HTTP, similarly to the Lichess tablebase API.
- The exact library interface to expose and the command line interface are
  still to be finalized.
- Keeping code simple, short and elegant has priority over finding a way to
  implement all desired features. If it cannot be done with simple, short and
  elegant code, then perhaps it should not be done.
- Adding one or two focused and compact tests to check functionality is ok.
  Adding hundreds and hundreds of lines of tests is not ok.
- Update the documentation after a change if appropriate, but always leave
  AGENTS.md untouched. Do no use documentation as a diary of every change that
  happened, normally documentation should not mention what the code was doing
  in the past.

See the design specifications in `README.md` for more information.

Current tablebase generation results (with --noverify):
```
== All 3 pieces, 2 threads ==
Generated 16 tables: 367868 unique positions, 0.04 MiB in 00h00m01s.
Average compression: 0.84 bits/pos. Lowest compression (KQ_K): 2.04 bits/pos.
```
```
== All 4 pieces, 2 threads ==
Generated 176 tables: 125132316 unique positions, 13.29 MiB in 00h01m39s.
Average compression: 0.89 bits/pos. Lowest compression (KQ_KR): 3.61 bits/pos.
```
```
== All 5 pieces, 2 threads ==
Generated 1496 tables: 25880730473 unique positions, 3578.66 MiB in 06h24m08s.
Average compression: 1.16 bits/pos. Lowest compression (KBB_KQ): 4.23 bits/pos.

== All 5 pieces, 32 threads on AMD EPYC-Turin Processor ==
Generated 1496 tables: 25880730473 unique positions, 3578.66 MiB in 01h44m47s.
Average compression: 1.16 bits/pos. Lowest compression (KBB_KQ): 4.23 bits/pos.
```

## TODO
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
- Distributed generation by worker swarm with message-passing implementation
  of the update sink. Bounded streaming buffers. Per-phase barriers.
  Checkpoints to object storage.
- Further optimization ideas:
  - Align frame size to permutation blocks?
    (frame_size = (64-n+1) * (64-n+2) * (64-n+3), or twice that)
    Small experiment between two close sizes (60 * 61 * 62)=226920 vs 229376
    shows 226920 is a bit better (~1%) even if smaller.
  - Align summary block size to permutation blocks
    (block_size = (64-n+1) * (64-n+2)) and fast-forward initialization
    if after placing n-2 pieces of the first position there is an
    unblockable check.
  - Pawn-rank slicing: solve slices of pawnful `Egt`s in dependency order.
    Inject successors in already solved slices at the matching ply.
  - A generation-specific index layout.
