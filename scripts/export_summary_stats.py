#!/usr/bin/env python3
"""Export leaf summaries and rough capacity estimates from --cache-stats metadata.

Refresh --cache-stats for the desired scope before exporting. Do not run while
metadata is being generated or rebuilt. No tablebase contents are read.
"""

import argparse
from pathlib import Path

from stats_csv import (
    OUTCOME_FIELDS, count, decimal, leaf_directories, read_json, table_records,
    validate_root, write_csv,
)

# Reference values from scaling.md.
REFERENCE_PIECES = 5
CORE_US_PER_INDEX = 1.28
COMPUTE_GROWTH_PER_PIECE = 1.4
PARALLEL_EFFICIENCY = 0.70

# Unlike the actual table CSV compression, these rates use INDEX positions.
# Use the measured five-piece rate and the six/seven-piece range midpoints.
COMPRESSION_BITS_PER_INDEX = 0.849
COMPRESSION_BITS_PER_INDEX_6 = 1.05
COMPRESSION_GROWTH_PER_PIECE = 0.20  # 7 pieces: 1.25; extrapolate beyond seven.

# Bound the largest working pair by two largest-table arrays, 2 bytes/index each.
# Overhead is a rough allowance for cache, encoders and other working memory;
# increase it for larger cache budgets or thread counts. Jobs run serially.
# This provisioning heuristic includes headroom, unlike scaling.md's array sizes.
MEMORY_OVERHEAD_GIB = 2
MEMORY_ROUNDING_GIB = 32

FIELDS = (
    "path", "table_count", "available_tables", "index_range",
    "unique_positions", "win", "draw", "loss", "size_gib",
    "estimated_final_size_gib", "estimated_total_core_hours", "estimated_memory_gib",
)


def leaf_row(root, leaf):
    path = leaf.relative_to(root).as_posix() + "/"
    summary_path = leaf / "stats.json"
    if not summary_path.is_file():
        raise ValueError(f"{leaf}: missing stats.json; refresh --cache-stats for the desired scope")
    summary = read_json(summary_path)
    stats = summary.get("stats")
    if summary.get("hierarchy") != path or not isinstance(stats, dict):
        raise ValueError(f"{summary_path}: invalid hierarchy summary")
    selected = count(stats, "selected_tables", summary_path, positive=True)
    available = count(stats, "available_tables", summary_path)
    index_range = count(stats, "index_range", summary_path, positive=True)

    records = list(table_records(leaf))
    complete = [record for record in records if record["bytes"] is not None]
    if (len(records), len(complete), sum(record["index_range"] for record in records)) != (
        selected, available, index_range,
    ):
        raise ValueError(f"{summary_path}: sidecars disagree with summary; refresh --cache-stats")
    all_available = selected == available
    for key in OUTCOME_FIELDS:
        if all_available:
            actual = count(stats, key, summary_path)
            if actual != sum(record[key] for record in complete):
                raise ValueError(f"{summary_path}: {key} disagrees with sidecars; refresh --cache-stats")
        elif key not in stats or stats[key] is not None:
            raise ValueError(f"{summary_path}: incomplete summary must have null {key}")

    row = {
        "path": path,
        "table_count": selected,
        "available_tables": available,
        "index_range": index_range,
    }
    if all_available:
        row.update({key: stats[key] for key in ("unique_positions", "win", "draw", "loss")})
        row["size_gib"] = decimal(stats["bytes"] / 2**30)
    else:
        pieces = int(path.split("/")[0])
        bits_per_index = (COMPRESSION_BITS_PER_INDEX if pieces <= REFERENCE_PIECES else
                          COMPRESSION_BITS_PER_INDEX_6 + COMPRESSION_GROWTH_PER_PIECE * (pieces - 6))
        known_bytes = sum(record["bytes"] for record in complete)
        missing_indexes = sum(record["index_range"] for record in records if record["bytes"] is None)
        core_us = CORE_US_PER_INDEX * COMPUTE_GROWTH_PER_PIECE ** (pieces - REFERENCE_PIECES)
        working_bytes = 4 * max(record["index_range"] for record in records)
        memory_bytes = working_bytes + MEMORY_OVERHEAD_GIB * 2**30
        rounding_bytes = MEMORY_ROUNDING_GIB * 2**30
        row.update({
            "estimated_final_size_gib": decimal((known_bytes + missing_indexes * bits_per_index / 8) / 2**30),
            "estimated_total_core_hours": decimal(index_range * core_us / (3_600_000_000 * PARALLEL_EFFICIENCY)),
            "estimated_memory_gib": ((memory_bytes + rounding_bytes - 1) // rounding_bytes) * MEMORY_ROUNDING_GIB,
        })
    return row


def collect_rows(root):
    leaves = leaf_directories(root)
    if not leaves:
        raise ValueError(f"{root}: no cached leaves; run --cache-stats for the desired scope")
    return [leaf_row(root, leaf) for leaf in leaves]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path, help="Local tablebase root prepared with --cache-stats")
    parser.add_argument("output", type=Path, help="CSV output file")
    args = parser.parse_args()
    try:
        root = validate_root(args.root)
        rows = collect_rows(root)
        write_csv(root, args.output, FIELDS, rows)
    except (OSError, ValueError) as error:
        parser.exit(1, f"error: {error}\n")


if __name__ == "__main__":
    main()
