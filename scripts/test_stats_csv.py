"""Standard-library tests for local metadata CSV exports; no tablebases needed."""

import csv
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

import export_summary_stats as leaf_stats
import export_table_stats as table_stats
import stats_csv


SCRIPTS = Path(__file__).resolve().parent
GIB = 2**30
MIB = 2**20
OUTCOMES = ("unique_positions", "win", "draw", "loss", "bytes")


class StatsCsvTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.base = Path(self.temporary.name)
        self.root = self.base / "tables"
        self.root.mkdir()
        self.put_json(self.root / "format.json", {"format": "ggegt-v0.11"})

    def put_json(self, path, value):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(value), encoding="utf-8")
        return path

    def complete(self, name="KQ_K", index_range=GIB, unique_positions=MIB,
                 size=MIB):
        return {
            "tablename": name,
            "index_range": index_range,
            "unique_positions": unique_positions,
            "win": unique_positions // 2,
            "draw": unique_positions - unique_positions // 2,
            "loss": 0,
            "bytes": size,
        }

    def placeholder(self, name="KR_K", index_range=2 * GIB):
        return {
            "tablename": name,
            "index_range": index_range,
            **dict.fromkeys(OUTCOMES),
        }

    def make_leaf(self, records, relative="5/3v2/op0/0pawns"):
        leaf = self.root / relative
        leaf.mkdir(parents=True, exist_ok=True)
        for record in records:
            self.put_json(leaf / (record["tablename"] + ".json"), record)
        complete = [record for record in records if record["bytes"] is not None]
        stats = {
            "selected_tables": len(records),
            "available_tables": len(complete),
            "index_range": sum(record["index_range"] for record in records),
        }
        for key in OUTCOMES:
            stats[key] = (sum(record[key] for record in complete)
                          if len(complete) == len(records) else None)
        summary = {"hierarchy": relative + "/", "stats": stats}
        self.put_json(leaf / "stats.json", summary)
        return leaf, summary

    def read_csv(self, output):
        with output.open(newline="", encoding="utf-8") as source:
            reader = csv.DictReader(source)
            return reader.fieldnames, list(reader)

    def test_table_rows_skip_placeholders_and_use_actual_unique_positions(self):
        self.make_leaf([self.placeholder(), self.complete()])
        rows = table_stats.collect_rows(self.root)
        self.assertEqual(rows, [{
            "path": "5/3v2/op0/0pawns/",
            "name": "KQ_K", "index_range": GIB, "unique_positions": MIB,
            "win": MIB // 2, "draw": MIB // 2, "loss": 0,
            "size_mib": "1.000000", "compression_bits_per_pos": "8.000000",
        }])
        # Dividing by index_range instead would give 0.007812 bits/position.
        self.assertNotEqual(rows[0]["compression_bits_per_pos"], "0.007812")

    def test_complete_leaf_has_actual_totals_and_no_estimates(self):
        self.make_leaf([
            self.complete("KQ_K", 3 * GIB, 30, GIB),
            self.complete("KR_K", GIB, 10, GIB // 2),
        ])
        self.assertEqual(leaf_stats.collect_rows(self.root), [{
            "path": "5/3v2/op0/0pawns/", "table_count": 2,
            "available_tables": 2, "index_range": 4 * GIB,
            "unique_positions": 40, "win": 20, "draw": 20, "loss": 0,
            "size_gib": "1.500000",
        }])

    def test_mixed_leaf_adds_known_bytes_to_estimated_missing_indexes(self):
        leaf, _ = self.make_leaf([
            self.complete(index_range=GIB, unique_positions=17, size=GIB),
            self.placeholder(index_range=8 * GIB),
        ])
        row = leaf_stats.leaf_row(self.root, leaf)
        self.assertEqual(row["estimated_final_size_gib"], "1.849000")
        self.assertEqual(row["table_count"], 2)
        self.assertEqual(row["available_tables"], 1)
        self.assertEqual(row["index_range"], 9 * GIB)
        for key in ("unique_positions", "win", "draw", "loss", "size_gib"):
            self.assertNotIn(key, row)

    def test_compute_estimates_total_work_not_just_missing_tables(self):
        leaf, _ = self.make_leaf([
            self.complete(index_range=GIB),
            self.placeholder(index_range=GIB),
        ])
        row = leaf_stats.leaf_row(self.root, leaf)
        self.assertEqual(row["estimated_total_core_hours"],
                         f"{2 * GIB * 1.345 / (3_600_000_000 * 0.70):.6f}")
        self.assertNotEqual(row["estimated_total_core_hours"],
                            f"{GIB * 1.345 / (3_600_000_000 * 0.70):.6f}")

    def test_piece_count_scales_index_compression_and_compute_model(self):
        for pieces, bits, growth in ((5, 0.849, 1), (6, 1.05, 2),
                                     (7, 1.25, 2**2)):
            with self.subTest(pieces=pieces):
                leaf, _ = self.make_leaf(
                    [self.placeholder(index_range=8 * GIB)],
                    f"{pieces}/{pieces - 2}v2/op0/0pawns")
                row = leaf_stats.leaf_row(self.root, leaf)
                self.assertEqual(row["estimated_final_size_gib"], f"{bits:.6f}")
                self.assertEqual(row["estimated_total_core_hours"],
                                 f"{8 * GIB * 1.345 * growth / (3_600_000_000 * 0.70):.6f}")

    def test_complete_set_projections_match_scaling_document(self):
        for pieces, indexes, core_hours in ((6, 5_626_522_905_000, 6_000),
                                           (7, 683_014_118_224_656, 1_460_000)):
            with self.subTest(pieces=pieces):
                leaf, _ = self.make_leaf(
                    [self.placeholder(index_range=indexes)],
                    f"{pieces}/{pieces - 2}v2/op0/0pawns")
                row = leaf_stats.leaf_row(self.root, leaf)
                self.assertAlmostEqual(float(row["estimated_total_core_hours"]),
                                       core_hours, delta=core_hours * 0.002)

    def test_memory_rounds_up_at_32_gib_boundary(self):
        # Four bytes/index plus 2 GiB overhead is exactly 32 GiB here.
        boundary = 30 * GIB // 4
        for offset, expected in ((-1, 32), (0, 32), (1, 64)):
            with self.subTest(offset=offset):
                leaf, _ = self.make_leaf([
                    self.placeholder(index_range=boundary + offset),
                ])
                self.assertEqual(leaf_stats.leaf_row(self.root, leaf)
                                 ["estimated_memory_gib"], expected)

    def test_memory_uses_largest_table_including_completed_tables(self):
        leaf, _ = self.make_leaf([
            self.complete(index_range=8 * GIB),
            self.placeholder(index_range=GIB),
        ])
        self.assertEqual(leaf_stats.leaf_row(self.root, leaf)
                         ["estimated_memory_gib"], 64)

    def test_zero_unique_positions_have_blank_csv_compression(self):
        self.make_leaf([self.complete(unique_positions=0, size=123)])
        rows = table_stats.collect_rows(self.root)
        self.assertIsNone(rows[0]["compression_bits_per_pos"])
        output = self.base / "zero.csv"
        stats_csv.write_csv(self.root, output, table_stats.FIELDS, rows)
        fields, written = self.read_csv(output)
        self.assertEqual(fields, list(table_stats.FIELDS))
        self.assertEqual(written[0]["unique_positions"], "0")
        self.assertEqual(written[0]["compression_bits_per_pos"], "")

    def test_csv_unavailable_cells_are_empty_not_na_or_null(self):
        self.make_leaf([self.complete()], "5/3v2/op0/0pawns")
        self.make_leaf([self.complete(), self.placeholder()], "5/3v2/op1/0pawns")
        output = self.base / "leaves.csv"
        stats_csv.write_csv(self.root, output, leaf_stats.FIELDS,
                            leaf_stats.collect_rows(self.root))
        fields, rows = self.read_csv(output)
        self.assertEqual(fields, list(leaf_stats.FIELDS))
        self.assertEqual(len(rows), 2)
        for key in ("estimated_final_size_gib", "estimated_total_core_hours",
                    "estimated_memory_gib"):
            self.assertEqual(rows[0][key], "")
            self.assertNotEqual(rows[1][key], "")
        for key in ("unique_positions", "win", "draw", "loss", "size_gib"):
            self.assertNotEqual(rows[0][key], "")
            self.assertEqual(rows[1][key], "")

    def test_ordering_is_deterministic_across_leaves_and_tables(self):
        later, _ = self.make_leaf([
            self.complete("KC_K"), self.complete("KA_K"),
        ], "5/3v2/op2/0pawns")
        earlier, _ = self.make_leaf([
            self.complete("KZ_K"), self.complete("KB_K"),
        ], "5/3v2/op10/0pawns")
        self.assertEqual(stats_csv.leaf_directories(self.root), [earlier, later])
        self.assertEqual([r["tablename"] for r in stats_csv.table_records(later)],
                         ["KA_K", "KC_K"])
        for _ in range(2):
            self.assertEqual([r["path"] for r in table_stats.collect_rows(self.root)],
                             ["5/3v2/op10/0pawns/"] * 2 + ["5/3v2/op2/0pawns/"] * 2)
            self.assertEqual([r["name"] for r in table_stats.collect_rows(self.root)],
                             ["KB_K", "KZ_K", "KA_K", "KC_K"])
            self.assertEqual([r["path"] for r in leaf_stats.collect_rows(self.root)],
                             ["5/3v2/op10/0pawns/", "5/3v2/op2/0pawns/"])

    def test_only_depth_four_matching_leaf_directories_are_used(self):
        leaf, _ = self.make_leaf([self.complete()])
        for relative in ("5/3v2/op0", "5/3v2/op0/0pawns/nested",
                         "5/3v2/other/0pawns", "5/3v2/op0/other",
                         "other/3v2/op0/0pawns"):
            (self.root / relative).mkdir(parents=True, exist_ok=True)
        self.assertEqual(stats_csv.leaf_directories(self.root), [leaf])

    def test_format_validation_requires_exact_supported_object(self):
        self.assertEqual(stats_csv.validate_root(self.root), self.root.resolve())
        for value in ({"format": "ggegt-v0.10"}, {}, {"format": None},
                      {"format": "ggegt-v0.11", "extra": True}, [], "ggegt-v0.11"):
            with self.subTest(value=value):
                self.put_json(self.root / "format.json", value)
                with self.assertRaises(ValueError):
                    stats_csv.validate_root(self.root)
        (self.root / "format.json").unlink()
        with self.assertRaisesRegex(ValueError, "format.json"):
            stats_csv.validate_root(self.root)
        with self.assertRaises(OSError):
            stats_csv.validate_root(self.root / "nonexistent")

    def test_malformed_json_is_rejected_with_path(self):
        leaf, _ = self.make_leaf([self.complete()])
        for path, action in (
            (self.root / "format.json", lambda: stats_csv.validate_root(self.root)),
            (leaf / "KQ_K.json", lambda: table_stats.collect_rows(self.root)),
            (leaf / "stats.json", lambda: leaf_stats.collect_rows(self.root)),
        ):
            original = path.read_text(encoding="utf-8")
            for malformed in ("{", "[]", "null"):
                with self.subTest(path=path.name, malformed=malformed):
                    path.write_text(malformed, encoding="utf-8")
                    with self.assertRaises(ValueError) as raised:
                        action()
                    self.assertIn(str(path), str(raised.exception))
            path.write_text(original, encoding="utf-8")

    def test_missing_leaf_cache_and_empty_root_are_rejected(self):
        with self.assertRaisesRegex(ValueError, "no cached leaves"):
            leaf_stats.collect_rows(self.root)
        self.assertEqual(table_stats.collect_rows(self.root), [])
        leaf, _ = self.make_leaf([self.complete()])
        (leaf / "stats.json").unlink()
        with self.assertRaisesRegex(ValueError, "missing stats.json"):
            leaf_stats.collect_rows(self.root)

    def test_stale_summary_counts_and_totals_are_rejected(self):
        leaf, summary = self.make_leaf([self.complete()])
        for key in ("selected_tables", "available_tables", "index_range", *OUTCOMES):
            with self.subTest(key=key):
                changed = {**summary, "stats": dict(summary["stats"])}
                changed["stats"][key] += 1
                self.put_json(leaf / "stats.json", changed)
                with self.assertRaisesRegex(ValueError, "refresh --cache-stats"):
                    leaf_stats.leaf_row(self.root, leaf)

    def test_missing_sidecar_is_rejected_as_stale_cache(self):
        leaf, _ = self.make_leaf([self.complete(), self.placeholder()])
        (leaf / "KR_K.json").unlink()
        with self.assertRaisesRegex(ValueError, "sidecars disagree"):
            leaf_stats.leaf_row(self.root, leaf)

    def test_invalid_summary_hierarchy_and_stats_are_rejected(self):
        leaf, summary = self.make_leaf([self.complete()])
        for changes in ({"hierarchy": "5/3v2/op0/0pawns"},
                        {"hierarchy": "5/3v2/op1/0pawns/"},
                        {"stats": None}, {"stats": []}):
            with self.subTest(changes=changes):
                self.put_json(leaf / "stats.json", {**summary, **changes})
                with self.assertRaisesRegex(ValueError, "invalid hierarchy summary"):
                    leaf_stats.leaf_row(self.root, leaf)

    def test_incomplete_summary_requires_explicit_null_outcomes(self):
        leaf, summary = self.make_leaf([self.complete(), self.placeholder()])
        for key in OUTCOMES:
            for missing in (False, True):
                with self.subTest(key=key, missing=missing):
                    changed = {**summary, "stats": dict(summary["stats"])}
                    if missing:
                        del changed["stats"][key]
                    else:
                        changed["stats"][key] = 0
                    self.put_json(leaf / "stats.json", changed)
                    with self.assertRaisesRegex(ValueError, "incomplete summary"):
                        leaf_stats.leaf_row(self.root, leaf)

    def test_invalid_sidecar_fields_are_rejected(self):
        leaf, _ = self.make_leaf([self.complete()])
        record = self.complete()
        mutations = [
            ("tablename", "wrong"), ("index_range", 0), ("index_range", -1),
            ("index_range", True), ("index_range", 1.5), ("index_range", "10"),
            ("unique_positions", -1), ("unique_positions", GIB + 1),
            ("win", True), ("draw", None), ("loss", -1), ("bytes", -1),
            ("unique_positions", MIB + 1),
        ]
        for key, value in mutations:
            with self.subTest(key=key, value=value):
                self.put_json(leaf / "KQ_K.json", {**record, key: value})
                with self.assertRaises(ValueError):
                    table_stats.collect_rows(self.root)
        # Independently exercise the unique_positions <= index_range check.
        oversized = self.complete(index_range=1, unique_positions=2)
        self.put_json(leaf / "KQ_K.json", oversized)
        with self.assertRaisesRegex(ValueError, "exceeds index_range"):
            table_stats.collect_rows(self.root)
        for key in ("bytes", "win", "index_range"):
            with self.subTest(missing=key):
                changed = dict(record)
                del changed[key]
                self.put_json(leaf / "KQ_K.json", changed)
                with self.assertRaises(ValueError):
                    table_stats.collect_rows(self.root)

    def test_placeholder_requires_all_outcomes_explicitly_null(self):
        leaf, _ = self.make_leaf([self.placeholder()])
        for key in OUTCOMES[:-1]:
            for missing in (False, True):
                with self.subTest(key=key, missing=missing):
                    record = self.placeholder()
                    if missing:
                        del record[key]
                    else:
                        record[key] = 0
                    self.put_json(leaf / "KR_K.json", record)
                    with self.assertRaisesRegex(ValueError, "inconsistent placeholder"):
                        table_stats.collect_rows(self.root)

    def test_summary_counts_require_nonnegative_integers(self):
        leaf, summary = self.make_leaf([self.complete()])
        for key in ("selected_tables", "available_tables", "index_range", *OUTCOMES):
            invalid = (None, True, -1, 1.5, "1")
            if key in ("selected_tables", "index_range"):
                invalid += (0,)
            for value in invalid:
                with self.subTest(key=key, value=value):
                    changed = {**summary, "stats": dict(summary["stats"])}
                    changed["stats"][key] = value
                    self.put_json(leaf / "stats.json", changed)
                    with self.assertRaisesRegex(ValueError, "invalid " + key):
                        leaf_stats.leaf_row(self.root, leaf)

    def test_write_csv_refuses_to_overwrite_root_json_metadata(self):
        path = self.root / "format.json"
        original = path.read_bytes()
        with self.assertRaisesRegex(ValueError, "must not overwrite"):
            stats_csv.write_csv(self.root, path, table_stats.FIELDS, [])
        self.assertEqual(path.read_bytes(), original)

    def run_cli(self, script, output):
        return subprocess.run(
            [sys.executable, str(SCRIPTS / script), str(self.root), str(output)],
            cwd=self.base, capture_output=True, text=True, timeout=10,
        )

    def test_actual_cli_exports_with_temp_roots_and_blank_cells(self):
        self.make_leaf([self.complete(unique_positions=0)])
        self.make_leaf([self.placeholder()], "6/4v2/op0/0pawns")
        for script, fields, expected_count in (
            ("export_table_stats.py", table_stats.FIELDS, 1),
            ("export_summary_stats.py", leaf_stats.FIELDS, 2),
        ):
            with self.subTest(script=script):
                output = self.base / (script + ".csv")
                result = self.run_cli(script, output)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stderr, "")
                header, rows = self.read_csv(output)
                self.assertEqual(header, list(fields))
                self.assertEqual(len(rows), expected_count)
                if script == "export_table_stats.py":
                    self.assertEqual(rows[0]["compression_bits_per_pos"], "")
                else:
                    self.assertEqual(rows[0]["estimated_final_size_gib"], "")
                    self.assertEqual(rows[1]["size_gib"], "")
                    self.assertEqual(rows[1]["unique_positions"], "")
                    self.assertEqual(rows[1]["estimated_final_size_gib"], "0.262500")

    def test_cli_reports_invalid_format_without_creating_output(self):
        self.put_json(self.root / "format.json", {"format": "unsupported"})
        for script in ("export_table_stats.py", "export_summary_stats.py"):
            with self.subTest(script=script):
                output = self.base / (script + ".csv")
                result = self.run_cli(script, output)
                self.assertEqual(result.returncode, 1)
                self.assertIn("error:", result.stderr)
                self.assertIn("unsupported tablebase format", result.stderr)
                self.assertNotIn("Traceback", result.stderr)
                self.assertFalse(output.exists())

    def test_cli_reports_missing_cache_without_creating_output(self):
        leaf, _ = self.make_leaf([self.placeholder()])
        (leaf / "stats.json").unlink()
        output = self.base / "missing.csv"
        result = self.run_cli("export_summary_stats.py", output)
        self.assertEqual(result.returncode, 1)
        self.assertIn("missing stats.json", result.stderr)
        self.assertNotIn("Traceback", result.stderr)
        self.assertFalse(output.exists())


if __name__ == "__main__":
    unittest.main()
