#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Verify FFM downcalls do not hold up opted-in no-roots pauses.

Runs the compiled-worker redefinition/loop-exit probe with the default claim
policy and its explicit `=0` wait control, in both runtime modes and all three
collectors. A default run must finish the redefinition before the native sleep
ends; the kill control must wait almost the full native interval. The Java
post-call marker is not a reliable control signal: the VM can release the
worker after C returns but before its first Java store.
"""

import argparse
import os
from pathlib import Path
import re
import subprocess
import sys
from gate_diagnostics import clean_diagnostics


parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("binary", type=Path)
parser.add_argument("jdk", type=Path)
parser.add_argument("classes", type=Path)
parser.add_argument("agent", type=Path)
parser.add_argument("logs", type=Path)
args = parser.parse_args()
binary, jdk, classes, agent, logs = (
    path.resolve() for path in
    (args.binary, args.jdk, args.classes, args.agent, args.logs)
)
for path in (binary, jdk, classes, agent):
    if not path.exists():
        parser.error(f"missing input: {path}")
logs.mkdir(parents=True, exist_ok=True)
env = dict(os.environ, JAVA_HOME=str(jdk), CRATONVM_JIT_THRESHOLD="1")
env["PATH"] = str(jdk / "bin") + os.pathsep + env["PATH"]
env.pop("CRATONVM_FFM_LAZY_CLAIM_NO_ROOTS_PAUSE", None)

passed = 0
for mode in ("jdk-only", "compatible"):
    for collector in ("ZGC", "G1GC", "GenerationalGC"):
        for enabled in (True, False):
            label = "default" if enabled else "disabled"
            stem = f"{mode}-{collector}-{label}"
            stdout_path, stderr_path = logs / f"{stem}.out", logs / f"{stem}.err"
            command = [
                str(binary), f"--{mode}", f"-XX:+Use{collector}", "-Xmx128m",
                "--enable-native-access=ALL-UNNAMED", f"-javaagent:{agent}",
                "-cp", str(classes), "R20FfmRedefInC",
            ]
            run_env = dict(env)
            if not enabled:
                run_env["CRATONVM_FFM_LAZY_CLAIM_NO_ROOTS_PAUSE"] = "0"
            with stdout_path.open("w") as stdout, stderr_path.open("w") as stderr:
                try:
                    rc = subprocess.run(
                        command, env=run_env, stdout=stdout, stderr=stderr, timeout=20
                    ).returncode
                except subprocess.TimeoutExpired:
                    rc = 124
            output = stdout_path.read_text(errors="replace")
            elapsed = re.search(r"^redefine ms: (\d+)$", output, re.M)
            elapsed_ms = int(elapsed.group(1)) if elapsed else None
            if enabled:
                ok = (rc == 0 and "worker post-call marker incomplete at redefine return: true" in output
                      and "fresh tag: edited-body" in output
                      and "worker stopped: true" in output
                      and "PASS" in output.splitlines() and elapsed_ms is not None
                      and elapsed_ms < 2000)
            else:
                ok = (rc == 0 and "fresh tag: edited-body" in output
                      and "worker stopped: true" in output
                      and "FAIL redefine waited or stale body" in output.splitlines()
                      and elapsed_ms is not None and elapsed_ms >= 2500)
            ok = ok and clean_diagnostics(output, stderr_path.read_text(errors="replace"))
            print(f"{'PASS' if ok else 'FAIL'} {stem} rc={rc} ms={elapsed_ms}", flush=True)
            if not ok:
                sys.exit(1)
            passed += 1

print(f"FFM_NO_ROOTS_GATE_OK cases={passed}", flush=True)
