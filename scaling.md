# Tablebase scaling and cloud costs

Resource estimates for generating, verifying, and storing the gg-EGT tablebase.
Results up to 5 pieces are available. Results for larger sets are estimated.

## 1. Problem size

As described in [Table scopes and naming](https://github.com/gr-g/chess-egt#table-scopes-and-naming),
endgame tables are generated and stored as individual files, each representing
positions for a specific set of material, side to move, and pawn arrangement
on the files.

**Index positions** are the sum of indices stored over all files, including
invalid and redundant slots. All resources scale with the index positions, so
the table below shows how the work increases as the number of pieces grows.

| Pieces | Tables | Twin-pair jobs | Index positions |
|---|---:|---:|---:|
| 3 | 16 | 8 | 416,640 |
| 4 | 176 | 96 | 155,965,898 |
| 5 | 1,496 | 748 | 35,268,305,340 |
| 6 | 10,526 | 5,338 | 5,606,162,736,600 |
| 7 | 63,784 | 31,892 | 679,345,381,690,216 |

Tables are generated in pairs (e.g. `KQ_KR` is generated together with the
"twin" table `KR_KQ`), except for symmetric tables like `KQ_KQ` which are
their own twins. Therefore the number of generating jobs can be slightly more
than half the number of tables.

## 2. Memory requirements

For **table generation**, a theoretical efficient model foresees that only the
table pair being generated is allocated in memory during the entire process,
while the required dependencies are read only once sequentially during
initialization, without adding significant memory allocation cost. In this
model, memory requirements for generation can be approximated by the size in
memory of the table-pair (with our encoding: 2 bytes per positions times the
number of positions).

Note that this theoretical model can be implemented by scanning the
dependencies sequentially and playing reverse capture/promotion moves from the
dependecies into the tables being generated. This approach is more complex and
more computationally intensive compared to the alternative: simply playing the
forward capture/promotion moves during initialization and looking up their
outcomes in the dependency tables.

This simpler approach is the one currently applied in this project, at the cost
of increased memory requirements. The worst case occurs when large 1-pawn
tables are being generated, which have large promotion dependencies. With our
encoding (2 bytes per position for the tables being generated, but ~1 byte per
position for the tables already generated), this results in approximately
double requirements compared to the size in memory of the table-pair. Complex
schemes could be devised to re-encode and keep in memory only the relevant
parts of the promotion dependencies, since only a small percentage of the
positions stored in those dependencies are actually reachable by a promotion
move, but this is currently not planned.

For **table verification**, the theoretical model is reversed: the required
dependencies are allocated in memory during the entire process, while the table
to verify could be read sequentially. As a result, our memory requirements for
generation (i.e. "double the size in memory of the table-pair") are generally
suitable for verification, too (and are in fact more difficult to reduce
compared to generation).

The table below shows the worst-case requirements for table-pair generation.

| Pieces | Index positions | Largest table-pair | Worst-case memory estimate |
|---|---:|---:|---:|
| 3 | 416,640 | 0.11 MB | <1 GB |
| 4 | 155,965,898 | 6.99 MB | <1 GB |
| 5 | 35,268,305,340 | 0.42 GB | ~1 GB |
| 6 | 5,606,162,736,600 | 24.7 GB | ~50 GB |
| 7 | 679,345,381,690,216 | 1.44 TB | ~3 TB |

All 6-piece jobs fit confortably on a server with 64 GB of memory. Seven-piece
jobs have a heavy-tailed size distribution: the median job needs about 2 GB of
working memory, but jobs for large tables can require 1+ TB of memory for the
table-pair plus a comparable amount of memory to store dependencies.

## 3. Compute requirements

The following measurements are based on runs executed on an AMD Ryzen 7 5700G,
with 2 threads and without including verification.

| Pieces | Index positions | Elapsed time (on 2 threads) | Allocated core-hours | Allocated core-hours per billion indices |
|---|---:|---:|---:|---:|
| 3 | 416,640 | < 1 s | — | — |
| 4 | 155,965,898 | 1 m 37 s | 0.0539 | 0.346 |
| 5 | 35,268,305,340 | 6 h 17 m 06 s | 12.57 | 0.356 |

From four to five pieces, index positions grow 226× and generation time grows
233×: the cost per index position rises only 1.03×.

Assuming a value of `0.4` for required core-hours per billion indices, and
assuming a cost for cloud compute resources in the range $0.03-$0.05 per
core-hour (as of end-2026), we have the following estimates.

| Pieces | Core-hours per billion indices | Allocated core-hours | Cost |
|---|---:|---:|---:|
| 6 (estimated) | 0.4 | 2,242 | $67-$112 |
| 7 (estimated) | 0.4 | 271,738 | $8,150-$13,600 |

In terms of **time**, these estimates imply generation of all 6-piece tables in
approximately 3 days on a single server with 32 vCPUs, and generation of all
7-piece tables in approximately 2 months on a single server with 192 vCPUs.

## 4. Storage requirements

We consider the requirements for storing the full set of tables in compressed
format, including both sides of each table pair. We start from the measured
compression for tables up to 5 pieces.

| Pieces | Compressed size | Bits / index position | Bits / unique position |
|---|---:|---:|---|
| 3 | 0.04 MiB | 0.68 | 0.84 |
| 4 | 13.29 MiB | 0.71 | 0.89 |
| 5 | 3.495 GiB | 0.85 | 1.16 |

Let's assume `1.00` bits per index position at 6 pieces and `1.20` bits per
index position at 7 pieces. Let's also assume a cost for cloud storage in the
range $0.01–$0.02 per GiB-month (as of end-2026). We have these estimates.

| Pieces | Bits / index position | Compressed size | Monthly storage cost |
|---|---:|---:|---:|
| 5 (measured) | 0.85 | 3.495 GiB | $0.03–$0.07 |
| 6 (estimated) | 1.00 | 0.64 TiB | $6–$13 |
| 7 (estimated) | 1.20 | 93 TiB | $950–$1,900 |
