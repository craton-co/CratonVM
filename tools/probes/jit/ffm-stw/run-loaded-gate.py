#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Four workers x 20 rounds x three probes, both modes and all collectors.

Windows uses the event-based deadlock probe in place of the Linux EINTR probe.
Each process must finish, print PASS, and have a clean root-scan audit.

`--workers` reduces peak memory on shared hosts while preserving the total
worker-round count for each mode/collector pair.

The retention suite uses two workers over 15 rounds each. Its longer native
windows make four simultaneous VM processes exceed the runner's bounded
per-probe timeout under G1, without improving the retained-root repetitions.

Bounding one invocation (JIT round 20 wave 14, lane ffm3c). The retention
suite is sleep-bound: every `R16Ffm3Stw` run sits in C for its GC window (10 s;
60 s for compatible GenerationalGC) plus its allocation window (4 s; 20 s under
G1), and every `FfmHeldRootsProbe` run for 12 s. With two workers that is about
450 s per ZGC or jdk-only GenerationalGC group, 690 s per G1 group and 1200 s
for compatible GenerationalGC: ~3900 s for the six groups in one sequential
invocation, which a 3600 s outer timeout kills before the last group (and,
before this change, with no per-run line in the log). So:

* `--collectors` / `--modes` split the matrix across invocations (each with its
  own log directory and timeout); the acceptance count is per group (60/60 for
  retention), so the split loses nothing;
* `--rounds N` sets the per-group round budget (default 80 loaded / 30
  retention; it must be a multiple of the worker count) for a bounded smoke;
* `--jobs K` runs up to K mode/collector groups at once (default 1);
* `--deadline S` starts no new run after S seconds and ends with a partial
  verdict (exit 2) instead of being killed mid-group by an outer timeout;
* every finished run prints one `PASS|FAIL <mode> <collector> worker=.. round=..
  probe=.. rc=.. secs=..` line at once, so a killed invocation still shows
  what ran.
"""
import argparse
import concurrent.futures
import os
from pathlib import Path
import re
import subprocess
import sys
import threading
import time
from gate_diagnostics import clean_diagnostics

parser = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
parser.add_argument("binary", type=Path)
parser.add_argument("jdk", type=Path)
parser.add_argument("classes", type=Path)
parser.add_argument("logs", type=Path)
parser.add_argument("--suite", choices=["loaded", "retention"], default="loaded",
                    help="retention repeats STW and held-root probes 30 times per mode/collector")
parser.add_argument("--modes", nargs="+", choices=["jdk-only", "compatible"],
                    default=["jdk-only", "compatible"])
parser.add_argument("--collectors", nargs="+",
                    choices=["ZGC", "G1GC", "GenerationalGC"],
                    default=["ZGC", "G1GC", "GenerationalGC"])
parser.add_argument("--workers", type=int, default=None,
                    help="parallel VM processes per mode/collector pair; total runs stay fixed")
parser.add_argument("--rounds", type=int, default=None,
                    help="rounds per mode/collector pair (default 80 loaded, 30 retention)")
parser.add_argument("--jobs", type=int, default=1,
                    help="mode/collector pairs run at once (default 1)")
parser.add_argument("--deadline", type=float, default=None,
                    help="seconds after which no new run starts (partial verdict, exit 2)")
args = parser.parse_args()
binary, jdk, classes, logs = (p.resolve() for p in
                             (args.binary, args.jdk, args.classes, args.logs))
for path in (binary, jdk, classes):
    if not path.exists():
        parser.error(f"missing input: {path}")
logs.mkdir(parents=True, exist_ok=True)
probes = ["R16FfmInNativeBattery", "R16Ffm3Stw",
          "R16FfmDeadlockShape" if os.name == "nt" else "R17EintrNative"]
if args.suite == "retention":
    probes = ["R16Ffm3Stw", "FfmHeldRootsProbe"]
worker_count = args.workers or (4 if args.suite == "loaded" else 2)
round_budget = args.rounds or (80 if args.suite == "loaded" else 30)
if worker_count < 1 or args.jobs < 1 or round_budget < 1:
    parser.error("--workers, --jobs and --rounds must be positive")
if round_budget % worker_count:
    parser.error(f"--workers {worker_count} must divide the {round_budget}-round suite budget")
rounds = round_budget // worker_count
expected_runs = round_budget * len(probes)
env = dict(os.environ, JAVA_HOME=str(jdk), CRATONVM_JIT="xt-root-scan-audit")
env["PATH"] = str(jdk / "bin") + os.pathsep + env["PATH"]
started = time.monotonic()
print_lock = threading.Lock()
# A failed group stops every group from starting new runs, as the sequential
# gate always did (it exited at the first failed group).
abort = threading.Event()


def out_of_time():
    return abort.is_set() or (args.deadline is not None
                              and time.monotonic() - started >= args.deadline)


def say(line):
    with print_lock:
        print(line, flush=True)


def worker(mode, collector, group, number):
    rows = []
    for round_number in range(1, rounds + 1):
        for probe in probes:
            if out_of_time():
                (group / f"worker{number}-summary.txt").write_text("\n".join(rows) + "\n")
                return rows
            output = group / f"w{number}-r{round_number}-{probe}.log"
            command = [str(binary), "--" + mode, "-XX:+Use" + collector,
                       # The probe loads the JDK image as a byte array
                       # (~145 MiB); keep the cap above that fixture size on
                       # both Windows and Linux.
                       "-Xmx256m", "--enable-native-access=ALL-UNNAMED",
                       "-cp", str(classes), probe]
            if args.suite == "retention" and probe == "R16Ffm3Stw":
                gc_sleep_ms = 60_000 if (mode == "compatible"
                                          and collector == "GenerationalGC") else 10_000
                command.insert(4, f"-Dcratonvm.ffm.stw.gc-sleep-ms={gc_sleep_ms}")
                if collector == "G1GC":
                    command.insert(5, "-Dcratonvm.ffm.stw.alloc-sleep-ms=20000")
            t0 = time.monotonic()
            with output.open("w") as stream:
                try:
                    rc = subprocess.run(command, env=env, stdout=stream,
                                        stderr=subprocess.STDOUT, timeout=180).returncode
                except subprocess.TimeoutExpired:
                    rc = 124
            text = output.read_text(errors="replace")
            passed = (rc == 0 and "PASS" in text.splitlines()
                      and "FAIL" not in text and "Session is acquired" not in text
                      and clean_diagnostics(text))
            row = (f"{'PASS' if passed else 'FAIL'} worker={number} "
                   f"round={round_number} probe={probe} rc={rc}")
            rows.append(row)
            say(f"{'PASS' if passed else 'FAIL'} {mode} {collector} worker={number} "
                f"round={round_number} probe={probe} rc={rc} "
                f"secs={time.monotonic() - t0:.0f}")
            if not passed:
                break
        else:
            continue
        break
    (group / f"worker{number}-summary.txt").write_text("\n".join(rows) + "\n")
    return rows


def run_group(mode, collector):
    group = logs / f"{mode}-{collector}"
    group.mkdir(parents=True, exist_ok=True)
    with concurrent.futures.ThreadPoolExecutor(max_workers=worker_count) as pool:
        futures = [pool.submit(worker, mode, collector, group, number)
                   for number in range(1, worker_count + 1)]
        rows = [row for future in futures for row in future.result()]
    (group / "summary.txt").write_text("\n".join(rows) + "\n")
    failed = any(row.startswith("FAIL") for row in rows)
    if failed:
        verdict = "FAIL"
        abort.set()
    elif len(rows) != expected_runs:
        verdict = "PARTIAL"
    else:
        verdict = "PASS"
    say(f"{mode} {collector} {verdict} runs={len(rows)}/{expected_runs}")
    return verdict, len(rows)


groups = [(mode, collector) for mode in args.modes for collector in args.collectors]
total_runs = 0
verdicts = []
with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
    for verdict, count in pool.map(lambda g: run_group(*g), groups):
        verdicts.append(verdict)
        total_runs += count
if "FAIL" in verdicts:
    print(f"FFM_{args.suite.upper()}_GATE_FAIL runs={total_runs}", flush=True)
    sys.exit(1)
if "PARTIAL" in verdicts:
    print(f"FFM_{args.suite.upper()}_GATE_PARTIAL runs={total_runs}/"
          f"{expected_runs * len(groups)} (deadline {args.deadline} s)", flush=True)
    sys.exit(2)
print(f"FFM_{args.suite.upper()}_GATE_OK runs={total_runs}", flush=True)
