#!/usr/bin/env python3
"""Audit lazy FFM claims through moving collections in both runtime modes.

Usage: run-claim-moving-gate.py <cratonvm> <jdk-home> <classes-dir> <log-dir>
The binary must contain FfmHeldGcProbe.class compiled from this directory.
"""

from pathlib import Path
import os
import re
import subprocess
import sys
from gate_diagnostics import clean_diagnostics


if len(sys.argv) != 5:
    raise SystemExit(__doc__)

binary, jdk, classes, logs = map(Path, sys.argv[1:])
logs.mkdir(parents=True, exist_ok=True)
env = dict(
    os.environ,
    JAVA_HOME=str(jdk),
    PATH=str(jdk / "bin") + os.pathsep + os.environ.get("PATH", ""),
    CRATONVM_JIT_THRESHOLD="1",
    CRATONVM_DBG_FFM="1",
    CRATONVM_DBG_XT_COVERAGE="1",
    CRATONVM_JIT_BISECT_ONLY="FfmHeldGcProbe",
    CRATONVM_JIT_DENY="FfmHeldGcProbe.main",
)

collection_re = re.compile(
    r"collection claims=(\d+) frozen=(\d+) relocated=(\d+)"
    r"[\s\S]{0,300}?helper_windows:\s*(\d+), unreadable:\s*(\d+), "
    r"pins_complete:\s*(true|false), derived_pointers_resolved:\s*(true|false)"
)

for mode in ("jdk-only", "compatible"):
    for collector in ("ZGC", "G1GC", "GenerationalGC"):
        stem = f"{mode}-{collector}"
        stdout_path, stderr_path = logs / f"{stem}.out", logs / f"{stem}.err"
        command = [
            str(binary),
            f"--{mode}",
            f"-XX:+Use{collector}",
            "-Xmx128m",
            "--enable-native-access=ALL-UNNAMED",
            "-cp",
            str(classes),
            "FfmHeldGcProbe",
        ]
        with stdout_path.open("w") as stdout, stderr_path.open("w") as stderr:
            try:
                result = subprocess.run(
                    command, env=env, stdout=stdout, stderr=stderr, timeout=90
                )
                rc = result.returncode
            except subprocess.TimeoutExpired:
                rc = 124

        output = stdout_path.read_text(errors="replace")
        diagnostics = stderr_path.read_text(errors="replace")
        # Some terminal-backed Linux runs inject cursor positioning sequences
        # and wrap diagnostics at the pseudo-terminal width even though the
        # gate captures stderr to a file. Normalize those artifacts before
        # matching complete verdicts.
        diagnostic_text = re.sub(r"\x1b\[[0-9;]*[A-Za-z]", "", diagnostics)
        diagnostic_text = re.sub(r"\s+", " ", diagnostic_text)
        rows = [tuple(row) for row in collection_re.findall(diagnostic_text)]
        ok = (
            rc == 0
            and clean_diagnostics(output, diagnostics)
            and re.search(r"^PASS$", output, re.M)
            and "gc returned while the worker was in C: true" in output
            and "trace returned while the worker was in C: true" in output
            and "[ffm-lazy] pinned deposit" in diagnostics
            and rows
            and all(
                int(claims) > 0
                and int(frozen) == 0
                and int(helper_windows) > 0
                and int(unreadable) == 0
                and pins_complete == "true"
                and derived_resolved == "true"
                for claims, frozen, relocated, helper_windows, unreadable,
                pins_complete, derived_resolved in rows
            )
            and (collector != "G1GC" or any(int(row[2]) > 0 for row in rows))
        )
        print(f"{'PASS' if ok else 'FAIL'} {mode} {collector} rc={rc}", flush=True)
        if not ok:
            raise SystemExit(1)

print("FFM_CLAIM_MOVING_GATE_OK cases=6", flush=True)
