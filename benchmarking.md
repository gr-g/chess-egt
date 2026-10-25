## Reproducible native benchmarks

Use [`scripts/benchmark_generation.py`](scripts/benchmark_generation.py) with
Python 3.11+ on Linux. Use a fresh output directory and a current tablebase root
containing `format.json` and all dependencies needed by the requested selectors.
The script benchmarks each selector separately; generation adds its quiet-move
twins but does not recursively generate dependencies. Start with a small selector,
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
and finalization/publication timings across its pair jobs. Finalization includes
compression and upload; there is no separate material-file assembly phase. One
Rayon pool is reused across a generation call; dependency frames are retained
across jobs within each material family, not across benchmark subprocesses.
Dependencies have no LRU eviction, so a warm multi-job cache can affect memory
and timing.

Artifacts are individual tables at `n/maxvmin/opK/Npawns/name.ggegt` with JSON
sidecars. Root `format.json` declares format/indexing/layout versions 1 and
frame size 262144. The script hashes relative hierarchical paths after timing,
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
