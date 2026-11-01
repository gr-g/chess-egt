#!/usr/bin/env python3
"""Export one CSV row per generated table from a local tablebase root."""

import argparse
from pathlib import Path

from stats_csv import decimal, leaf_directories, table_records, validate_root, write_csv


FIELDS = (
    "path", "name", "index_range", "unique_positions", "win", "draw", "loss",
    "size_mib", "compression_bits_per_pos",
)


def collect_rows(root):
    rows = []
    for leaf in leaf_directories(root):
        for record in table_records(leaf):
            if record["bytes"] is None:
                continue
            positions = record["unique_positions"]
            rows.append({
                "path": leaf.relative_to(root).as_posix() + "/",
                                "name": record["tablename"],
                **{key: record[key] for key in ("index_range", "unique_positions", "win", "draw", "loss")},
                "size_mib": decimal(record["bytes"] / 2**20),
                "compression_bits_per_pos": decimal(record["bytes"] * 8 / positions) if positions else None,
            })
    return sorted(rows, key=lambda row: (row["path"], row["name"]))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path, help="Local tablebase root containing format.json")
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
