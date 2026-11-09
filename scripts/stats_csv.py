"""Shared local metadata readers for the CSV exporters (standard library only)."""

import csv
import json

import re


LEAF_PATH = re.compile(r"[2-9][0-9]*/[0-9]+v[0-9]+/op[0-9]+/[0-9]+pawns")
OUTCOME_FIELDS = ("unique_positions", "win", "draw", "loss", "bytes")


def read_json(path):
    try:
        with path.open() as source:
            value = json.load(source)
    except (OSError, ValueError) as error:
        raise ValueError(f"{path}: {error}") from error
    if not isinstance(value, dict):
        raise ValueError(f"{path}: expected a JSON object")
    return value


def count(record, key, path, positive=False):
    value = record.get(key)
    if type(value) is not int or value < int(positive):
        raise ValueError(f"{path}: invalid {key}")
    return value


def validate_root(root):
    root = root.resolve(strict=True)
    if read_json(root / "format.json") != {"format": "ggegt-v0.12"}:
        raise ValueError(f"{root}: unsupported tablebase format")
    return root


def leaf_directories(root):
    return sorted(
        path for path in root.glob("*/*/*/*")
        if path.is_dir() and LEAF_PATH.fullmatch(path.relative_to(root).as_posix())
    )


def table_records(leaf):
    for path in sorted(leaf.glob("*.json")):
        if path.name == "stats.json":
            continue
        record = read_json(path)
        if record.get("tablename") != path.stem:
            raise ValueError(f"{path}: tablename does not match filename")
        index_range = count(record, "index_range", path, positive=True)
        if "bytes" not in record:
            raise ValueError(f"{path}: missing bytes field")
        if record["bytes"] is None:
            if any(record.get(key, 0) is not None for key in OUTCOME_FIELDS):
                raise ValueError(f"{path}: inconsistent placeholder")
        else:
            for key in OUTCOME_FIELDS:
                count(record, key, path)
            if record["unique_positions"] != sum(record[key] for key in ("win", "draw", "loss")):
                raise ValueError(f"{path}: outcomes do not sum to unique_positions")
            if record["unique_positions"] > index_range:
                raise ValueError(f"{path}: unique_positions exceeds index_range")
        yield record


def write_csv(root, output, fields, rows):
    output = output.resolve()
    if output.is_relative_to(root) and output.suffix in (".json", ".ggegt"):
        raise ValueError("CSV output must not overwrite tablebase data or JSON metadata")
    with output.open("w", newline="", encoding="utf-8") as destination:
        writer = csv.DictWriter(destination, fieldnames=fields)
        writer.writeheader()
        writer.writerows(rows)


def decimal(value):
    return f"{value:.6f}"
