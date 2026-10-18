## Reproducible native benchmarks

Use [`scripts/benchmark_generation.py`](scripts/benchmark_generation.py) with
Python 3.11+ on Linux. It requires a fresh output directory and existing reference
`.ggegt`/`.json` files for each requested endgame and its reverse orientation.

For example, run from the repository root:

```sh
python3 scripts/benchmark_generation.py \
  --binary target/release/chess-egt \
  --reference ~/tablebases \
  --output target/generation-benchmark \
  --endgames KQ_K KP_KP --threads 1 4 --repeats 2 --timeout 600
```

## Bounded profiling

Build with `cargo build --locked --profile profiling` to retain debug symbols.
Start with a small endgame and fresh output paths; bound runtime with `timeout`:

```sh
timeout 120s valgrind --tool=callgrind --cache-sim=no --branch-sim=no \
  --callgrind-out-file=target/generation-kq-k.callgrind \
  target/profiling/chess-egt \
  --path target/generation-profile-tables --input-path ~/tablebases \
  --generate KQ_K --threads 1 --noverify
callgrind_annotate --auto=no --threshold=100 target/generation-kq-k.callgrind
callgrind_annotate --auto=no --inclusive=yes --tree=caller --threshold=100 \
  target/generation-kq-k.callgrind
```
