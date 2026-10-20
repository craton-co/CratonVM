#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Compare FFM probe facts against fresh HotSpot 25 references in both modes.

Timing measurements are omitted from the comparison; the cost gate prices them.
Use --hotspot first to generate references with the same JDK and probe classes.
"""
import argparse
import difflib
import os
from pathlib import Path
import re
import subprocess
import sys
from gate_diagnostics import clean_diagnostics

parser = argparse.ArgumentParser(description=__doc__)
for name in ("binary", "jdk", "classes", "agent", "references", "logs"):
    parser.add_argument(name, type=Path)
parser.add_argument("--hotspot", action="store_true")
parser.add_argument("--native-library", type=Path,
                    help="library compiled from ffm-void-diagnostic.c; also checks diagnostic upcalls")
args = parser.parse_args()
binary, jdk, classes, agent, refs, logs = (
    getattr(args, name).resolve() for name in
    ("binary", "jdk", "classes", "agent", "references", "logs"))
for path in (binary, jdk, classes, agent):
    if not path.exists():
        parser.error(f"missing input: {path}")
refs.mkdir(parents=True, exist_ok=True)
logs.mkdir(parents=True, exist_ok=True)
probes = ("R16FfmDeadlockShape R16Ffm3Stw R16FfmInNativeBattery "
          "Gcd5FfmDowncallBlocksGcProbe R16FfmcostPlainMemo R16FfmdoorBind "
          "R16FfmsegBind R16FfmUpcallExactDoor R17Ffm2Direct R17Ffm3Capture "
          "R17FfmsegCost R16FfmDowncallCost R18Eintr19TraceInC FfmHeldRootsProbe "
          "CurrencyCarrierProbe FfmRealMapDispatch FirstVariadicArgProbe").split()
if os.name != "nt":
    probes += ["R17EintrNative", "R17Ffm3EintrRace"]
cases = [(probe, probe, []) for probe in probes]
cases += [("FfmHeldTraceProbe-" + arm, "FfmHeldTraceProbe", [arm])
          for arm in ("plain", "reflective")]
cases += [("R20FfmRedefInC", "R20FfmRedefInC", [])]
if args.native_library:
    native_library = args.native_library.resolve()
    if not native_library.exists():
        parser.error(f"missing native library: {native_library}")
    cases += [("FfmVoidDiagnosticProbe", "FfmVoidDiagnosticProbe", [str(native_library)])]
measurements = re.compile(
    r"^(?:cost-(?:llabs|llabs-critical)-ns \d+|"
    r"(?:pause|alloc)-(?:idle|while-blocked)-ms \d+|\S+ \d+ ns/op|"
    r"(?:direct|bound|inserted|dropped|guarded|filtered) \d+ ms|redefine ms: \d+)$")

def facts(text):
    return [line for line in text.splitlines() if not measurements.fullmatch(line)]

env = dict(os.environ, JAVA_HOME=str(jdk))
env["PATH"] = str(jdk / "bin") + os.pathsep + env["PATH"]
groups = ([("hotspot", "SerialGC")] if args.hotspot else
          [(mode, collector) for mode in ("jdk-only", "compatible")
           for collector in ("ZGC", "G1GC", "GenerationalGC")])
total = 0
with ((refs if args.hotspot else logs) / "summary.txt").open("w") as summary:
    for mode, collector in groups:
        for label, probe, probe_args in cases:
            base = refs / label if args.hotspot else logs / f"{mode}-{collector}-{label}"
            command = ([str(jdk / "bin" / ("java.exe" if os.name == "nt" else "java")),
                        "-XX:+UseSerialGC"] if args.hotspot else
                       [str(binary), "--" + mode, "-XX:+Use" + collector])
            command += ["-Xmx128m", "--enable-native-access=ALL-UNNAMED"]
            if probe == "R20FfmRedefInC":
                command += ["-javaagent:" + str(agent)]
            command += ["-cp", str(classes), probe] + probe_args
            run_env = dict(env)
            if probe in ("FfmHeldTraceProbe", "R20FfmRedefInC"):
                run_env["CRATONVM_JIT_THRESHOLD"] = "1"
            if probe == "FfmVoidDiagnosticProbe":
                run_env.update(CRATONVM_DBG_MH_DISPATCH="1", CRATONVM_FFM_DOWNCALL_GC_SAFE="1")
            with base.with_suffix(".out").open("w") as out, base.with_suffix(".err").open("w") as err:
                try:
                    rc = subprocess.run(command, env=run_env, stdout=out, stderr=err,
                                        timeout=60).returncode
                except subprocess.TimeoutExpired:
                    rc = 124
            output = base.with_suffix(".out").read_text(errors="replace")
            errors = base.with_suffix(".err").read_text(errors="replace")
            diff = [] if args.hotspot else list(difflib.unified_diff(
                facts((refs / (label + ".out")).read_text()), facts(output),
                fromfile="HotSpot25", tofile=mode, lineterm=""))
            ok = (rc == 0 and not diff and not re.search(r"^FAIL", output, re.M)
                  and clean_diagnostics(output, errors))
            row = f"{'PASS' if ok else 'FAIL'} {mode} {collector} {label} rc={rc}"
            summary.write(row + "\n")
            summary.flush()
            print(row, flush=True)
            if not ok:
                base.with_suffix(".diff").write_text("\n".join(diff) + "\n")
                sys.exit(1)
            total += 1
print(f"FFM_{'HOTSPOT' if args.hotspot else 'FOCUSED'}_GATE_OK cases={total}", flush=True)
