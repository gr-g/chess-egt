# Tablebase scaling and cloud costs

Resource estimates for generating, verifying, and storing `chess-egt`
depth-to-conversion (DTC) tables. Three-, four-, and five-piece results are
measured; larger sets are projections.

## 1. Problem size

A table file represents one exact canonical table, including pawn files and
side-to-move orientation. **Index positions** are the sum of `index_range`
over all files, including invalid and redundant slots. **Unique positions**
count valid, nonredundant positions after symmetry reduction. Generation work
scales with index positions; reported compression in bits per position uses
unique positions unless explicitly stated otherwise.

| Pieces | Table files | Twin-pair jobs | Index positions | Max generation memory | Max verification memory |
|---|---:|---:|---:|---:|---:|
| 3 | 16 | 8 | 416,640 | 0.11 MB | 0.28 MB |
| 4 | 176 | 96 | 155,965,898 | 6.99 MB | 23.1 MB |
| 5 | 1,496 | 748 | 35,268,305,340 | 0.42 GB | 1.41 GB |
| 6 | 10,526 | 5,338 | 5,606,162,736,600 | 24.7 GB | 84.8 GB |
| 7 | 63,784 | 31,892 | 679,345,381,690,216 | 1.44 TB | 5.01 TB |

### Working memory

Target streaming model (not yet implemented): generation allocates the working
arrays and streams the required dependencies; verification allocates the
dependencies and streams the table to verify. With `mem(X) = 2 × index_range(X)`
bytes:

```text
generation(X)   = sum(mem(T) for T in {X, twin(X)})
verification(X) = sum(mem(T) for T in {twin(X)} ∪ dependencies(X))
```
Worst-case generation pairs, with all non-king pieces distinct:

| Pieces | Pawnless pair | One-pawn pair, fixed file | One pawn-rank slice |
|---|---:|---:|---:|
| 5 | 0.42 GB | 0.34 GB | 57 MB |
| 6 | 24.7 GB | 20.2 GB | 3.37 GB |
| 7 | 1.44 TB | 1.17 TB | 196 GB |
| 8 | 81.8 TB | 66.9 TB | 11.2 TB |

Seven-piece jobs have a heavy-tailed size distribution: the median job needs
about 1.06 GB of working arrays, but a job such as `KRRPb_KQB` has 146.76
billion slots per side and 587 GB of arrays. All six-piece jobs fit below 25
GB of working arrays.

## 2. Measured generation performance

Historical complete generation-only runs on an AMD Ryzen 7 5700G, with two
threads and verification disabled (`--threads=2 --noverify`):

| Pieces | Index positions | Wall time | Allocated core-hours | Allocated core-µs/index |
|---|---:|---:|---:|---:|
| 3 | 416,640 | < 1 s | — | — |
| 4 | 155,965,898 | 1 m 37 s | 0.0539 | 1.24 µs |
| 5 | 35,268,305,340 | 6 h 17 m 06 s | 12.57 | 1.28 µs |

Allocated core-hours are wall time multiplied by worker threads. Performance
is allocated core-µs/index: `wall_seconds × worker_threads × 1e6 / sum(index_range)`.

From four to five pieces, index positions grow 226× and generation time grows
233×: the cost per index position rises only 1.03×.

Measured compressed sizes are listed in Section 4. The five-piece set occupies
3,578.66 MiB, averaging 1.16 bits per unique position, or 0.849 bits per
index position.

## 3. Compute projections and costs

Use `c = 1.28 µs/index` from the five-piece measurement, per-position growth
`g = 1.0–1.8` per additional piece (central `1.4`), and parallel efficiency
`η = 0.70`. Treat each vCPU as one reference-equivalent worker; actual CPU
and SMT performance may differ. These are planning assumptions, not benchmarks.

For `N` index positions and `n` pieces, with `c` in seconds:

```text
allocated core-hours H = N × c × g^(n − 5) / (3,600 × η)
wall hours            = H / total_vCPUs
compute cost          = H × instance_price_per_hour / instance_vCPUs
```

Assume **$1–2/hour per 32-vCPU instance** or **$6–18/hour per 192-vCPU
instance**. Combining runtime and price ranges gives:

| Set | Allocated core-hours (central) | Cost using 32-vCPU instances | Cost using 192-vCPU instances |
|---|---:|---:|---:|
| 6-piece | 2,800–5,200 (4,000) | $90–320 | $90–480 |
| 7-piece | 350,000–1,100,000 (675,000) | $11k–70k | $11k–105k |

Costs cover generation only, excluding verification, idle time, interruption
losses, storage, and transfers. Prices do not guarantee sufficient RAM: use
Section 1's separate generation/verification estimates plus overhead, under
the target streaming model.

## 4. Compressed storage and costs

Measured compression for complete three-, four-, and five-piece sets:

| Pieces | Compressed size | Bits / unique position | Least-compressible table |
|---|---:|---:|---|
| 3 | 0.04 MiB | 0.84 | `KQ_K`: 2.04 bits/position |
| 4 | 13.29 MiB | 0.89 | `KQ_KR`: 3.61 bits/position |
| 5 | 3,578.66 MiB (3.495 GiB) | 1.16 | `KBB_KQ`: 4.23 bits/position |

For larger sets, assume **0.9–1.2 bits per index position** at six pieces and
**1.0–1.5 bits per index position** at seven pieces. The comparable five-piece
measurement is 0.849 bits per index position. Projected sizes use
`index_positions × bits_per_index_position / 8`; unique-position counts for
these larger sets are not needed.

Assume **$0.01–0.02 per GiB-month** for storage:

| Set | Compressed size | Monthly storage cost |
|---|---:|---:|
| 5-piece (measured size) | 3.495 GiB | $0.03–0.07 |
| 6-piece (projected) | 0.57–0.77 TiB | $6–16 |
| 7-piece (projected) | 77–116 TiB | $790–2,400 |

Costs cover compressed table files only. Metadata, lower-piece dependencies,
checkpoints, scratch space, extra copies, requests, retrievals, and data transfer
are additional; public distribution can cost more than storage.
