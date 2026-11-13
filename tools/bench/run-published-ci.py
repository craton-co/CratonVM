#!/usr/bin/env python3
"""Interleaved HotSpot comparison for the CPU and cold-start tables in BENCHMARK.md."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import statistics
import subprocess
import sys
import time


ROOT = Path(__file__).resolve().parents[2]
CPU = {
    "arithmetic": [("100M", 100_000_000), ("1B", 1_000_000_000), ("10B", 10_000_000_000)],
    "fib": [(str(n), n) for n in (40, 44, 48)],
    "sieve": [("50Kx10K", 50_000, 10_000), ("100Kx20K", 100_000, 20_000), ("100Kx60K", 100_000, 60_000)],
    "matrix": [(f"{n}x{n}", n) for n in (720, 1280, 2560)],
    "bintrees": [(str(n), n) for n in (16, 18, 20, 22, 24)],
    "hashmap": [(f"{n // 1_000_000}M", n) for n in (1_000_000, 5_000_000, 10_000_000, 50_000_000, 100_000_000)],
    "stringregex": [(f"{n // 1_000_000}M", n) for n in (1_000_000, 5_000_000, 10_000_000, 50_000_000, 100_000_000)],
    "matrix-tiled": [(f"{n}x{n}", n) for n in (720, 960, 1280, 1920, 2560)],
}
HEAP = {"arithmetic": "8g", "fib": "8g", "sieve": "8g", "matrix": "16g", "bintrees": "16g",
        "hashmap": "24g", "stringregex": "16g", "matrix-tiled": "16g"}
COLD = ("Hello", "P5Zip", "P5Math", "P5Exc", "P5Regex", "P5Nio", "P5Misc")
KERNEL = re.compile(r":\s*(\d+) ms\s*\[(-?\d+)\]")


def run(command, log, timeout, profile=False):
    started = time.monotonic()
    if profile:
        command = ["perf", "stat", "-x,", "-e", "instructions:u,task-clock", "-o", str(log.with_suffix(".perf")),
                   "--", "/usr/bin/time", "-f", "%M", "-o", str(log.with_suffix(".rss")), *command]
    try:
        result = subprocess.run(command, env={**os.environ, "CRATONVM_BOOT_IMAGE": "off"}, capture_output=True, text=True,
                                encoding="utf-8", errors="replace", timeout=timeout)
    except subprocess.TimeoutExpired as exc:
        log.with_suffix(".stdout").write_bytes(exc.stdout or b"")
        log.with_suffix(".stderr").write_bytes(exc.stderr or b"")
        raise RuntimeError(f"Timed out: {log}") from exc
    log.with_suffix(".stdout").write_text(result.stdout, encoding="utf-8")
    log.with_suffix(".stderr").write_text(result.stderr, encoding="utf-8")
    if result.returncode or "panicked" in result.stderr:
        raise RuntimeError(f"exit={result.returncode}: {log}")
    metrics = {}
    if profile:
        values = {fields[2]: fields[0] for line in log.with_suffix(".perf").read_text().splitlines()
                  if len(fields := line.split(",")) > 2}
        try:
            metrics = {"instructions": int(values["instructions:u"]),
                       "task_clock_ms": float(values["task-clock"]),
                       "rss_kib": int(log.with_suffix(".rss").read_text().strip())}
        except (KeyError, ValueError) as exc:
            raise RuntimeError(f"Missing cold-start profile metric: {log}") from exc
    return result.stdout, round((time.monotonic() - started) * 1000), metrics


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--vm", required=True, type=Path)
    parser.add_argument("--jdk", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--suite", choices=("cpu", "cold", "all"), default="all")
    parser.add_argument("--reps", type=int, default=3)
    parser.add_argument("--timeout", type=int, default=1200)
    parser.add_argument("--cpu", type=int)
    parser.add_argument("--case", help="Run one table row (for diagnosis)")
    args = parser.parse_args()
    if args.reps < 1 or args.timeout < 1:
        parser.error("reps and timeout must be positive")
    vm = args.vm.resolve(strict=True)
    java = (args.jdk / "bin/java").resolve(strict=True)
    javac = (args.jdk / "bin/javac").resolve(strict=True)
    args.output.mkdir(parents=True, exist_ok=False)
    classes = args.output / "classes"
    classes.mkdir()
    sources = [ROOT / "tools/bench/CratonBench.java"] if args.suite == "cpu" else \
        list((ROOT / "docs/benchmarking/cold-start").glob("*.java")) if args.suite == "cold" else \
        [ROOT / "tools/bench/CratonBench.java", *(ROOT / "docs/benchmarking/cold-start").glob("*.java")]
    subprocess.run([str(javac), "-encoding", "UTF-8", "-d", str(classes), *map(str, sources)], check=True)
    tile_classes = {64: classes}
    if args.suite in ("cpu", "all"):
        source = (ROOT / "tools/bench/CratonBench.java").read_text(encoding="utf-8")
        marker = "private static final int MATRIX_TILE = 64;"
        if source.count(marker) != 1:
            raise RuntimeError("Matrix tile constant changed; update calibration")
        for tile in (16, 32):
            directory = args.output / f"tile-{tile}"
            directory.mkdir()
            java_source = directory / "CratonBench.java"
            java_source.write_text(source.replace(marker, f"private static final int MATRIX_TILE = {tile};"), encoding="utf-8")
            subprocess.run([str(javac), "-encoding", "UTF-8", "-d", str(directory), str(java_source)], check=True)
            tile_classes[tile] = directory
    launch = ["taskset", "-c", str(args.cpu)] if args.cpu is not None else []
    arms = {"hotspot": [str(java)], "foundational": [str(vm), "--real-jdk", "--java-home", str(args.jdk)],
            "compatible": [str(vm), "--compatible", "--java-home", str(args.jdk)]}
    report = {"revision": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
              "vm_sha256": hashlib.sha256(vm.read_bytes()).hexdigest(),
              "source_sha256": {str(source.relative_to(ROOT)): hashlib.sha256(source.read_bytes()).hexdigest() for source in sources},
              "java_version": subprocess.run([str(java), "-version"], capture_output=True, text=True).stderr.strip(),
              "host": os.uname().nodename, "cpu": args.cpu, "reps": args.reps,
              "samples": [], "summary": {}, "failures": {}}

    def sample(suite, case, arm, rep, command):
        log = args.output / f"{suite}-{case}-{arm}-{rep}"
        stdout, wall_ms, metrics = run([*launch, *arms[arm], *command], log, args.timeout, suite == "cold")
        row = {"suite": suite, "case": case, "arm": arm, "rep": rep, "wall_ms": wall_ms,
               "load1": os.getloadavg()[0], "command": [*launch, *arms[arm], *command], **metrics}
        if suite == "cpu":
            matches = KERNEL.findall(stdout)
            if len(matches) != 1:
                raise RuntimeError(f"Missing or duplicate kernel result: {log}")
            row["kernel_ms"], row["checksum"] = map(int, matches[0])
        else:
            row["stdout"] = stdout
        report["samples"].append(row)
        (args.output / "results.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
        return row

    cases = []
    if args.suite in ("cpu", "all"):
        for phase, sizes in CPU.items():
            for label, *numbers in sizes:
                for gc in ("G1GC", "ZGC"):
                    cases.append(("cpu", f"{phase}-{label}-{gc}", [f"-Xmx{HEAP[phase]}", f"-XX:+Use{gc}", "-cp", str(classes),
                                                             "CratonBench", phase, *map(str, numbers)]))
        for tile in (16, 32, 64):
            cases.append(("cpu", f"calibration-tile-{tile}", ["-Xmx2g", "-XX:+UseG1GC", "-cp", str(tile_classes[tile]),
                                                               "CratonBench", "matrix-tiled", "1280"]))
    if args.suite in ("cold", "all"):
        for name in COLD:
            cases.append(("cold", name, ["-XX:+UseG1GC", "-cp", str(classes), name]))
    if args.case:
        cases = [case for case in cases if case[1] == args.case]
        if not cases:
            parser.error("Unknown case")
    for suite, case, command in cases:
        try:
            reference = None
            for rep in range(args.reps):
                order = tuple(arms) if rep % 2 == 0 else tuple(reversed(arms))
                for arm in order:
                    row = sample(suite, case, arm, rep, command)
                    value = row["checksum"] if suite == "cpu" else row["stdout"]
                    if arm == "hotspot":
                        if reference is not None and value != reference:
                            raise RuntimeError(f"HotSpot output drift: {case}")
                        reference = value
                    elif reference is not None and value != reference:
                        raise RuntimeError(f"HotSpot mismatch: {case} {arm}")
                    print(suite, case, arm, rep, row.get("kernel_ms", row["wall_ms"]), flush=True)
            medians = {arm: statistics.median(row.get("kernel_ms", row["wall_ms"])
                                              for row in report["samples"] if row["case"] == case and row["arm"] == arm)
                       for arm in arms}
            report["summary"][case] = {"median_ms": medians,
                                       "ratio_vs_hotspot": {arm: round(medians[arm] / medians["hotspot"], 3)
                                                            for arm in ("foundational", "compatible")}}
            if suite == "cold":
                report["summary"][case]["cold_profile"] = {
                    arm: {"min_wall_ms": min(row["wall_ms"] for row in report["samples"]
                                              if row["case"] == case and row["arm"] == arm),
                          **{key: statistics.median(row[key] for row in report["samples"]
                                                    if row["case"] == case and row["arm"] == arm)
                             for key in ("instructions", "task_clock_ms", "rss_kib")}}
                    for arm in arms}
        except RuntimeError as exc:
            report["failures"][case] = str(exc)
            print(f"FAIL {case}: {exc}", file=sys.stderr, flush=True)
        (args.output / "results.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
    if report["failures"]:
        raise RuntimeError(f"{len(report['failures'])} cases failed; see {args.output / 'results.json'}")
    print("PUBLISHED_BENCHMARKS_OK", flush=True)


if __name__ == "__main__":
    try:
        main()
    except (OSError, subprocess.CalledProcessError, RuntimeError) as exc:
        print(f"FAIL: {exc}", file=sys.stderr)
        sys.exit(1)
