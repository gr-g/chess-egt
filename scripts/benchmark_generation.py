#!/usr/bin/env python3
"""Sequential Linux generation benchmarks, using only the Python standard library.

Use a fresh output directory. Dependencies are read from --reference; recursive
creation and internal verification are disabled. Hashing happens after timing.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import subprocess
import time


PHASES = re.compile(
    r"phase timings: init ([\d.]+)s, propagation ([\d.]+)s, finalize ([\d.]+)s"
)
ASSEMBLY = re.compile(r"output assembly ([\d.]+)s")


def sha256(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def measure(command, log_path, timeout):
    start = time.monotonic()
    with log_path.open("w") as log:
        child = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT)
        try:
            while True:
                pid, status, usage = os.wait4(child.pid, os.WNOHANG)
                if pid:
                    child.returncode = os.waitstatus_to_exitcode(status)
                    break
                if time.monotonic() - start > timeout:
                    raise TimeoutError(f"Generation exceeded {timeout}s; see {log_path}")
                time.sleep(0.05)
        except BaseException:
            child.kill()
            _, status, _ = os.wait4(child.pid, 0)
            child.returncode = os.waitstatus_to_exitcode(status)
            raise
    elapsed = time.monotonic() - start
    if child.returncode:
        raise RuntimeError(f"Generation exited {child.returncode}; see {log_path}")
    text = log_path.read_text()
    phases = PHASES.findall(text)
    assembly = ASSEMBLY.findall(text)
    if len(phases) != 1 or len(assembly) != 1:
        raise RuntimeError(f"Expected one complete endgame timing record in {log_path}")
    init, propagation, finalize = map(float, phases[0])
    return {
        "wall_s": elapsed,
        "user_s": usage.ru_utime,
        "system_s": usage.ru_stime,
        "peak_rss_mib": usage.ru_maxrss / 1024,
        "init_s": init,
        "propagation_s": propagation,
        "finalize_s": finalize,
        "assembly_s": float(assembly[0]),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--reference", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--endgames", nargs="+", required=True)
    parser.add_argument("--threads", nargs="+", type=int, default=[1])
    parser.add_argument("--repeats", type=int, default=1)
    parser.add_argument("--timeout", type=float, default=600, help="Seconds per generation")
    args = parser.parse_args()
    if platform.system() != "Linux":
        parser.error("Linux is required for wait4 peak RSS units")
    if args.repeats < 1 or min(args.threads) < 1 or args.timeout <= 0:
        parser.error("Repeats, threads, and timeout must be positive")
    binary = args.binary.resolve(strict=True)
    reference = args.reference.resolve(strict=True)
    output = args.output.resolve()
    if not reference.is_dir():
        parser.error("--reference must be a directory")
    expected = {}
    for endgame in args.endgames:
        parts = endgame.split("_")
        if len(parts) != 2 or not all(re.fullmatch(r"K[QRBNP]*", p) for p in parts):
            parser.error(f"Invalid endgame: {endgame}")
        for name in {endgame, "_".join(reversed(parts))}:
            for extension in ("ggegt", "json"):
                path = reference / f"{name}.{extension}"
                expected[path.name] = sha256(path)
    output.mkdir(parents=True, exist_ok=False)
    root = Path(__file__).resolve().parent.parent
    manifest = {
        "binary": str(binary),
        "binary_sha256": sha256(binary),
        "cargo_lock_sha256": sha256(root / "Cargo.lock"),
        "git_commit": subprocess.check_output(
            ["git", "--no-pager", "rev-parse", "HEAD"], cwd=root, text=True
        ).strip(),
        "rustc": subprocess.check_output(["rustc", "-Vv"], text=True),
        "platform": platform.platform(),
        "cpuinfo": Path("/proc/cpuinfo").read_text(),
        "meminfo": Path("/proc/meminfo").read_text(),
        "environment": {k: os.environ[k] for k in (
            "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC_WRAPPER",
            "CARGO_BUILD_TARGET", "CARGO_BUILD_RUSTFLAGS", "RAYON_NUM_THREADS",
        ) if k in os.environ},
        "reference": str(reference),
        "reference_sha256": expected,
        "repeats": args.repeats,
        "threads": args.threads,
        "endgames": args.endgames,
        "timeout_s": args.timeout,
    }
    (output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    results = []
    hashes_by_endgame = {}
    for endgame in args.endgames:
        for repeat in range(1, args.repeats + 1):
            for threads in args.threads:
                run = output / f"{endgame}-t{threads}-r{repeat}"
                run.mkdir()
                tables = run / "tables"
                command = [str(binary), "--path", str(tables), "--input-path",
                           str(reference), "--generate", endgame,
                           "--threads", str(threads), "--noverify"]
                print(f"Running {endgame}, {threads} thread(s), repeat {repeat}", flush=True)
                timing = measure(command, run / "generation.log", args.timeout)
                hashes = {p.name: sha256(p) for p in sorted(tables.iterdir())}
                names = {endgame, "_".join(reversed(endgame.split("_")))}
                expected_names = {f"{name}.{ext}" for name in names for ext in ("ggegt", "json")}
                if hashes.keys() != expected_names:
                    raise RuntimeError(f"Unexpected output file set in {tables}")
                stable = hashes == hashes_by_endgame.setdefault(endgame, hashes)
                record = {
                    "endgame": endgame, "threads": threads, "repeat": repeat,
                    "command": command, **timing, "sha256": hashes,
                    "matches_reference": {name: digest == expected[name]
                                          for name, digest in hashes.items()},
                    "matches_first_run": stable,
                }
                (run / "result.json").write_text(json.dumps(record, indent=2) + "\n")
                results.append(record)
                (output / "results.json").write_text(json.dumps(results, indent=2) + "\n")
                print(json.dumps({k: record[k] for k in (
                    "endgame", "threads", "repeat", "init_s", "propagation_s",
                    "finalize_s", "assembly_s", "wall_s", "peak_rss_mib",
                    "matches_reference", "matches_first_run",
                )}), flush=True)
                if not stable:
                    raise RuntimeError(f"Output differs between native runs for {endgame}")
    if not all(all(r["matches_reference"].values()) for r in results):
        raise SystemExit("Reference hashes differ; inspect file layout/statistics before comparing performance")


if __name__ == "__main__":
    main()
