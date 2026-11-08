# Tablebase scaling and cloud costs

Resource estimates for generating and storing `chess-egt` depth-to-conversion
(DTC) tables. Three-, four-, and five-piece results are measured; larger sets
are projections.

## 1. Problem size

A table file represents one exact canonical table, including pawn files and
side-to-move orientation. **Index positions** are the sum of `index_range`
over all files, including invalid and redundant slots. **Unique positions**
count valid, nonredundant positions after symmetry reduction. Generation work
scales with index positions; reported compression in bits per position uses
unique positions unless explicitly stated otherwise.

| Pieces | Table files | Twin-pair jobs | Index positions | Largest working job |
|---|---:|---:|---:|---:|
| 3 | 16 | 8 | 416,640 | 0.11 MB |
| 4 | 176 | 96 | 155,965,898 | 6.99 MB |
| 5 | 1,496 | 748 | 35,268,305,340 | 0.42 GB |
| 6 | 10,526 | 5,338 | 5,606,162,736,600 | 24.7 GB |
| 7 | 63,784 | 31,892 | 679,345,381,690,216 | 1.44 TB |

### Working memory

Generation stores a 16-bit value per index slot in each active table. For a
pair with `N` slots per side, the working arrays therefore occupy `4N` bytes.
These figures exclude eagerly loaded dependency tables, block summaries,
compression buffers, and other process overhead. A self-twin has only one
working array.

Worst-case pairs, with all non-king pieces distinct:

| Pieces | Pawnless pair | One-pawn pair, fixed file | One pawn-rank slice |
|---|---:|---:|---:|
| 5 | 0.42 GB | 0.34 GB | 57 MB |
| 6 | 24.7 GB | 20.2 GB | 3.37 GB |
| 7 | 1.44 TB | 1.17 TB | 196 GB |
| 8 | 81.8 TB | 66.9 TB | 11.2 TB |

Seven-piece jobs have a heavy-tailed size distribution: the median job needs
about 1.06 GB of working arrays, but the position-weighted median needs
587 GB. A job such as `KRRPb_KQB` has 146.76 billion slots per side and
587 GB of arrays. By index-position weight, about 35% of seven-piece work is
in jobs up to 300 GB, 31% in jobs from 300 to 700 GB, and 34% above 700 GB.
All six-piece jobs fit below 25 GB of working arrays.

## 2. Measured generation performance

Historical complete generation-only runs on an AMD Ryzen 7 5700G, with two
threads and verification disabled (`--threads=2 --noverify`):

| Pieces | Index positions | Wall time | Allocated core-hours | Core time / index position |
|---|---:|---:|---:|---:|
| 3 | 416,640 | < 1 s | — | — |
| 4 | 155,965,898 | 1 m 37 s | 0.0539 | 1.24 µs |
| 5 | 35,268,305,340 | 6 h 17 m 06 s | 12.57 | 1.28 µs |

Allocated core-hours are wall time multiplied by the two worker threads, not
measured CPU utilization.

From four to five pieces, index positions grow 226× and generation time grows
233×: the cost per index position rises only 1.03×.

Measured compressed sizes are listed in Section 4. The five-piece set occupies
3,578.66 MiB, averaging 1.16 bits per unique position, or 0.849 bits per
index position.

## 3. Compute projections and EC2 costs

### Model

Use the five-piece measurement as the reference: `c = 1.28 µs` of allocated
core time per index position. For six and seven pieces, assume:

- **Per-position growth:** a factor `g = 1.0–1.8` per additional piece,
  with central value `1.4`. More moves and deeper, low-yield scans can increase
  the cost. The measured four-to-five-piece growth is much smaller; this
  wider range is a conservative assumption, not a measured scaling law.
- **Parallel efficiency:** `η = 0.70` relative to the two-thread reference.
  This is an additional allowance for scaling to many cores.
- **Relative per-core speed:** Zen 4 = `1.0`, Zen 5 = `1.15`, and
  Graviton4 = `0.75`, relative to the reference core. These are assumptions,
  not cross-platform benchmarks. The listed AMD and Graviton instances have
  one physical core per vCPU.

For `N` index positions and `n` pieces:

```text
allocated reference-equivalent core-hours H = N × c × g^(n − 5) / (3,600 × η)
wall hours on P cores of relative speed s  = H / (P × s)
EC2 cost                                  = wall hours × instance price/hour
```

Here `c` is in seconds. Applying the 70% efficiency allowance gives:

| Set | Reference-equivalent core-hours | Central scenario |
|---|---:|---:|
| 6-piece | 2,800–5,200 | 4,000 |
| 7-piece | 350,000–1,100,000 | 725,000 |

The measured five-piece total is 12.57 core-hours before this additional
large-machine efficiency allowance.

### Instance prices and memory tiers

Price snapshot: **2026-10-17, AWS `us-east-1`, Linux**. S3 rates are from the
AWS Price List API; EC2 on-demand and spot rates are from
[instances.vantage.sh](https://instances.vantage.sh/). These are dated planning
inputs, not live quotes. Spot prices and capacity availability fluctuate.

| Instance | vCPUs | Memory | On-demand / hour | Spot / hour |
|---|---:|---:|---:|---:|
| `c8a.48xlarge` (Zen 5) | 192 | 384 GiB | $10.35 | $4.88 |
| `c7a.48xlarge` (Zen 4) | 192 | 384 GiB | $9.85 | $3.56 |
| `hpc7a.96xlarge` (Zen 4) | 192 | 768 GiB | $9.08 | — |
| `c8g.48xlarge` (Graviton4) | 192 | 384 GiB | $7.66 | $2.61 |
| `m8g.48xlarge` (Graviton4) | 192 | 768 GiB | $8.62 | $3.84 |
| `r8g.48xlarge` (Graviton4) | 192 | 1.5 TiB | $11.31 | $3.23 |
| `x8g.48xlarge` (Graviton4) | 192 | 3 TiB | $18.76 | $4.59 |

`hpc7a` offers a lower on-demand core-hour price and more RAM per core, but
has limited regional availability. Concurrent jobs must be sized using working
arrays plus all loaded dependencies and overhead, not the array table alone.

### Generation time and compute bill

Except for the explicitly measured five-piece row, these are conditional
scenarios using the model above, not measured cloud runs. Individual
seven-piece rows cover both files in the pair. Fleet rows assume
balanced scheduling of independent, dependency-ready jobs; the EC2 estimates
exclude verification, idle dependency waits, storage, and transfer charges.

| Target | Fleet | Modeled wall time | On-demand | Spot |
|---|---|---:|---:|---:|
| All 5-piece (measured) | Genoa guest, 32 SMT vCPUs | 32 m 47 s | — | — |
| All 6-piece | 2 × `hpc7a.96xlarge`; spot alternative: 2 × `c7a.48xlarge` | 12–19 h | $210–340 | $85–135 |
| Representative 587 GB 7-piece pair (`KRRPb_KQB`) | 192 Graviton4-equivalent workers; RAM sizing unresolved | 2.4–6.3 h | — | — |
| Heavy one-pawn 7-piece pair (`KRPa_KQBN`, 1.17 TB arrays) | >3.35 TiB RAM required; no priced configuration | — | — | — |
| Largest pawnless 7-piece pair (`KN_KQRBN`, 1.44 TB) | 1 × `x8g.48xlarge` | 6.0–15.3 h | $112–287 | $27–70 |
| All 7-piece | 9,600 Graviton4-equivalent workers; RAM sizing unresolved | 4.7–12.2 days | — | — |

The all-seven-piece time includes the assumed `0.75` Graviton4 speed factor.
It is a throughput estimate, not a calendar-time guarantee: capture and
promotion dependencies constrain job order, and a long tail of heavy jobs
can leave much of a fleet idle. Instances must be released or reassigned when
there is insufficient work; keeping idle capacity increases the bill.
Likewise, scaling one job to 192 threads is an assumption, not established by
the two-thread measurements. Dependency-cache and compression memory must fit
alongside every concurrent job.

Spot figures exclude interruption losses. An interrupted job must either be
restarted or resumed from a consistent checkpoint; replaying counter updates
can corrupt results because decrements are not idempotent. Without such
recovery, frequent interruptions can make the quoted spot savings unrealistic.

## 4. Compressed storage and S3 costs

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

Monthly storage-only costs, using the same dated AWS price snapshot:

| Set | Compressed size | S3 Standard | Standard-IA | Glacier Instant Retrieval | Deep Archive |
|---|---:|---:|---:|---:|---:|
| 5-piece (measured size) | 3.495 GiB | $0.08 | $0.04 | $0.014 | $0.0035 |
| 6-piece (projected) | 0.58–0.77 TiB | $14–18 | $7–10 | $2–3 | < $1 |
| 7-piece (projected) | 78–116 TiB | $1,800–2,700 | $990–1,490 | $320–480 | $79–118 |

Rates per GiB-month: Standard $0.023 for the first 50 TiB and $0.022 for the
next tier; Standard-IA $0.0125; Glacier Instant Retrieval $0.004; Deep Archive
$0.00099. Sizes cover compressed table files, not JSON statistics, checkpoints,
scratch space, or extra copies. Lower-piece dependencies are additional,
although small relative to a complete larger set.

Storage class matters beyond the quoted capacity price:

- Standard-IA and Glacier classes add retrieval charges, minimum storage
  durations, and minimum billable sizes or metadata overhead. Frequent random
  probing can make retrieval and request costs significant.
- Deep Archive requires restoration and is unsuitable for interactive serving.
- EC2–S3 transfers in the same region have no data-transfer charge, but S3
  requests, retrievals, and some network paths (such as NAT gateways) can cost
  extra.
- At roughly $0.09 per GB of internet egress, a full seven-piece download is
  on the order of **$7k–11k**, before volume discounts and free allowances.
  Public distribution can therefore cost much more than storage. Requester
  Pays or AWS Open Data sponsorship may be appropriate.
