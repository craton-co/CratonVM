#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
# Copyright 2024-2026 Craton Software Company

"""Measure fresh-process application readiness, checking output against HotSpot."""

import argparse
import json
from pathlib import Path
import statistics
import subprocess
import threading
import time


def measure(command, marker, log, timeout):
    lines, stderr_lines, ready = [], [], []
    started = time.perf_counter()
    with subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                          stderr=subprocess.PIPE, text=True, encoding="utf-8", errors="replace") as child:
        def drain_stdout():
            for line in child.stdout:
                line = line.rstrip("\r\n")
                lines.append(line)
                if line == marker:
                    ready.append((time.perf_counter() - started) * 1000)

        def drain_stderr():
            stderr_lines.extend(line.rstrip("\r\n") for line in child.stderr)

        readers = [threading.Thread(target=drain_stdout), threading.Thread(target=drain_stderr)]
        for reader in readers:
            reader.start()
        expired = False
        try:
            child.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            expired = True
            child.kill()
            child.wait()
        finally:
            for reader in readers:
                reader.join()
            log.write_text("\n".join(lines) + "\n", encoding="utf-8")
            log.with_suffix(log.suffix + ".stderr").write_text(
                "\n".join(stderr_lines) + ("\n" if stderr_lines else ""), encoding="utf-8")
        if expired:
            raise TimeoutError(f"Startup timed out: {log}")
        if child.returncode or len(ready) != 1:
            raise RuntimeError(f"exit={child.returncode}, readiness markers={len(ready)}: {log}")
    return ready[0], [line for line in lines if line.startswith(("CHECK ", "READY "))]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("baseline", "candidate", "java-home", "spring-args", "quarkus-args", "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--runs", type=int, default=5)
    parser.add_argument("--timeout", type=float, default=120)
    args = parser.parse_args()
    if args.runs < 3 or args.timeout <= 0:
        parser.error("Use at least three runs and a positive timeout")
    args.output.mkdir(parents=True, exist_ok=False)
    java = args.java_home / "bin" / ("java.exe" if (args.java_home / "bin/java.exe").exists() else "java")
    results = {"commands": {}, "samples": [], "summary": {}}
    for framework, argfile in (("spring-boot", args.spring_args), ("quarkus", args.quarkus_args)):
        marker = "READY " + framework
        reference = [str(java), "@" + str(argfile.resolve())]
        _, expected = measure(reference, marker, args.output / f"{framework}-hotspot.log", args.timeout)
        commands = {
            name: [str(binary.resolve()), "--fast-boot", "--java-home", str(args.java_home.resolve()),
                   "--stack-dump-on-timeout", "0", "@" + str(argfile.resolve())]
            for name, binary in (("baseline", args.baseline), ("candidate", args.candidate))
        }
        results["commands"][framework] = {"hotspot": reference, **commands}
        for round_index in range(args.runs):
            order = ("baseline", "candidate") if round_index % 2 == 0 else ("candidate", "baseline")
            for name in order:
                log = args.output / f"{framework}-{name}-{round_index}.log"
                ready_ms, observed = measure(commands[name], marker, log, args.timeout)
                if observed != expected:
                    raise RuntimeError(f"Output differs from HotSpot: {log}")
                results["samples"].append({"framework": framework, "arm": name,
                                          "round": round_index, "ready_ms": ready_ms})
                print(f"{framework} {name} {round_index}: {ready_ms:.1f} ms", flush=True)
                (args.output / "results.json").write_text(json.dumps(results, indent=2), encoding="utf-8")
        medians = {
            name: statistics.median(row["ready_ms"] for row in results["samples"]
                                    if row["framework"] == framework and row["arm"] == name)
            for name in commands
        }
        speedup = medians["baseline"] / medians["candidate"]
        results["summary"][framework] = {"median_ms": medians, "speedup": speedup,
                                          "target_met": speedup >= 2}
        print(f"{framework}: {speedup:.3f}x; 2x target {'met' if speedup >= 2 else 'not met'}")
    (args.output / "results.json").write_text(json.dumps(results, indent=2), encoding="utf-8")
    return 0 if all(row["target_met"] for row in results["summary"].values()) else 1


if __name__ == "__main__":
    raise SystemExit(main())
