# AGENTS.md

- The goal of the project is to produce a chess endgame tablebase (EGT).
- The project is fully functional. Tables for all 3-piece, 4-piece and 5-piece
  endgames were generated and verified successfully, they are available in
  `~/ggegt/`.
- An API server is available (`bin/server.rs`) providing stats and position
  outcomes via HTTP, similarly to the Lichess tablebase API.
- The exact library interface to expose is still to be finalized.
- Keeping code simple, short and elegant has priority over finding a way to
  implement each requirement to the last bit. If it cannot be done with simple,
  short and elegant code, then perhaps it should not be done.
- Always leave AGENTS.md untouched.

See the design specifications in `README.md` for more information.

Current tablebase generation results:
```
== All 3 pieces, 2 threads ==
Generated 16 tables: 367868 unique positions in 00h:00m:01s (4.84 μs/index on 2 threads).
Compressed size: 0.04 MiB (0.84 bits/pos). Lowest compression: KQ_K (2.05 bits/pos).
```
```
== All 4 pieces, 2 threads ==
Generated 176 tables: 125132316 unique positions in 00h:01m:33s (1.20 μs/index on 2 threads).
Compressed size: 12.19 MiB (0.82 bits/pos). Lowest compression: KB_KQ (3.42 bits/pos).
```
```
== All 5 pieces, 2 threads ==
Generated 1496 tables: 25880730473 unique positions in 05h:57m:53s (1.22 μs/index on 2 threads).
Compressed size: 3289.07 MiB (1.07 bits/pos). Lowest compression: KBB_KQ (3.80 bits/pos).
```

## TODO
- Visibility and public interface.
- Freeze file format ("v1").
- `generate` does not replace existing tables.
- Frontend, cloning https://syzygy-tables.info/
- Explore possibility of generating tables that store together DTC and DTM for
  the same index, not only DTC.
- General cleanup.
- Distributed generation by worker swarm with message-passing implementation
  of the update sink. Bounded streaming buffers. Per-phase barriers.
  Checkpoints to object storage.
- Further optimization ideas:
  - Fast-forward initialization if after placing n-2 pieces of the first
    position in a block there is an unblockable check?
  - Pawn-rank slicing: solve slices of pawnful `Egt`s in dependency order.
    Inject successors in already solved slices at the matching ply.
  - A generation-specific index layout.
