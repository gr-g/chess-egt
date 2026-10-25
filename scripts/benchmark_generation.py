#!/usr/bin/env python3
"""Sequential Linux generation benchmarks, using only the Python standard library.

Use a fresh output directory and existing dependencies in --input-path.
Each selector is benchmarked separately, including its quiet-move twins, with
internal verification disabled. Hashing hierarchical artifacts happens after
timing; no comparison against a reference generation is performed.
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
    r"phase timings: init ([\d.]+)s, propagation ([\d.]+)s, finalize/publish ([\d.]+)s"
)


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
    if not phases:
        raise RuntimeError(f"Expected pair timing records in {log_path}")
    init, propagation, finalize = (
        sum(float(phase[column]) for phase in phases) for column in range(3)
    )
    return {
        "wall_s": elapsed,
        "user_s": usage.ru_utime,
        "system_s": usage.ru_stime,
        "peak_rss_mib": usage.ru_maxrss / 1024,
        "init_s": init,
        "propagation_s": propagation,
        "finalize_publish_s": finalize,
        "pairs": len(phases),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--input-path", type=Path, required=True,
                        help="Existing local dependencies in the current root format")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--selectors", nargs="+", required=True)
    parser.add_argument("--threads", nargs="+", type=int, default=[1])
    parser.add_argument("--repeats", type=int, default=1)
    parser.add_argument("--timeout", type=float, default=600, help="Seconds per generation")
    args = parser.parse_args()
    if platform.system() != "Linux":
        parser.error("Linux is required for wait4 peak RSS units")
    if args.repeats < 1 or min(args.threads) < 1 or args.timeout <= 0:
        parser.error("Repeats, threads, and timeout must be positive")
    binary = args.binary.resolve(strict=True)
    input_path = args.input_path.resolve(strict=True)
    output = args.output.resolve()
    if not input_path.is_dir() or not (input_path / "format.json").is_file():
        parser.error("--input-path must be a current tablebase root with format.json")
    # Delegate selector validation to the pure theoretical listing action.
    for selector in args.selectors:
        subprocess.run([str(binary), "--list", selector], check=True,
                       stdout=subprocess.DEVNULL, timeout=args.timeout)
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
        "input_path": str(input_path),
        "input_format_sha256": sha256(input_path / "format.json"),
        "repeats": args.repeats,
        "threads": args.threads,
        "selectors": args.selectors,
        "timeout_s": args.timeout,
    }
    (output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    results = []
    hashes_by_selector = {}
    for job, selector in enumerate(args.selectors, start=1):
        for repeat in range(1, args.repeats + 1):
            for threads in args.threads:
                run = output / f"job{job}-t{threads}-r{repeat}"
                run.mkdir()
                tables = run / "tables"
                command = [str(binary), "--path", str(tables), "--input-path",
                           str(input_path), "--generate", selector,
                           "--threads", str(threads), "--noverify"]
                print(f"Running {selector}, {threads} thread(s), repeat {repeat}", flush=True)
                timing = measure(command, run / "generation.log", args.timeout)
                artifacts = sorted(p for p in tables.rglob("*") if p.is_file())
                hashes = {p.relative_to(tables).as_posix(): sha256(p) for p in artifacts}
                if "format.json" not in hashes or not any(p.suffix == ".ggegt" for p in artifacts):
                    raise RuntimeError(f"Expected a format descriptor and table artifacts in {tables}")
                stable = hashes == hashes_by_selector.setdefault(selector, hashes)
                record = {
                    "selector": selector, "threads": threads, "repeat": repeat,
                    "command": command, **timing, "sha256": hashes,
                    "matches_first_run": stable,
                }
                (run / "result.json").write_text(json.dumps(record, indent=2) + "\n")
                results.append(record)
                (output / "results.json").write_text(json.dumps(results, indent=2) + "\n")
                print(json.dumps({k: record[k] for k in (
                    "selector", "threads", "repeat", "pairs", "init_s", "propagation_s",
                    "finalize_publish_s", "wall_s", "peak_rss_mib", "matches_first_run",
                )}), flush=True)
                if not stable:
                    raise RuntimeError(f"Output differs between native runs for {selector}")


if __name__ == "__main__":
    main()
