# AGENTS.md

- The goal of the project is to produce chess endgame tablebases (EGTs).
- A high-level overview is in README.md, read it to get information.
- The current status of the project is: there is an implementation of the file and table indexing (`EgtFile` class, `Egt` and `Indexer` classes). There is an implementation of compression/decompression. There is the start of memory management (LRU-eviction of frames from memory). The actual generation of tablebase outcomes through retrograde analysis of chess position is specificed in README.md but is still missing.
- Always run `cargo test --release` for testing, otherwise it takes too much time.
