#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
"""Fresh-process HotSpot controls and heap-exhaustion recovery acceptance.

No stderr is discarded. Full records and binary/source hashes are retained in
--out. --allow-failures is for measuring the unpatched baseline, never acceptance.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time
from gc_process_measure import run_measured


def sha(path):
    return hashlib.file_digest(open(path, "rb"), "sha256").hexdigest()


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--cvm", type=Path, required=True)
    ap.add_argument("--jdk", type=Path, required=True)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--repeat", type=int, default=8)
    ap.add_argument("--engines", nargs="+", choices=["jit", "nojit"], default=["jit", "nojit"])
    ap.add_argument("--collectors", nargs="+", choices=["ZGC", "G1", "Generational"], default=["ZGC"])
    ap.add_argument("--direct-only", action="store_true")
    ap.add_argument("--preseed-only", action="store_true")
    ap.add_argument("--ffm-lean", action="store_true",
                    help="exercise primitive lean calls and the allocating-return fallback")
    ap.add_argument("--allow-failures", action="store_true")
    ap.add_argument("--timeout", type=int, default=240)
    args = ap.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    classes = args.out / "classes"
    classes.mkdir(exist_ok=True)
    source_dir = Path(__file__).resolve().parent
    names = ["ZgcInfallibleAllocation", "Zr3LtNoAbort", "Zr4AbNoAbort", "ZgcFfmLeanBoundary"]
    suffix = ".exe" if os.name == "nt" else ""
    subprocess.run([str(args.jdk / "bin" / ("javac" + suffix)), "-d", str(classes),
                    *[str(source_dir / (n + ".java")) for n in names]], check=True)
    cases = [("ZgcInfallibleAllocation", [m]) for m in ["lower", "mirror", "arraymirror", "ffm"]]
    if not args.direct_only:
        cases += [("Zr3LtNoAbort", [m, "3"]) for m in ["int", "long", "mixed"]]
        cases += [("Zr4AbNoAbort", ["all", str(n)]) for n in [0, 4, 64]]
    if args.ffm_lean:
        cases = [("ZgcFfmLeanBoundary", [])]
    if args.preseed_only:
        cases = [("Zr4AbNoAbort", ["all", str(n)]) for n in [0, 4, 64]]
    env = dict(os.environ, JAVA_HOME=str(args.jdk))
    if args.ffm_lean:
        env.update(CRATONVM_JIT_FFM_DOWNCALL_DIRECT="0", CRATONVM_JIT_FFM_DOWNCALL_LEAN="1")
    records = []
    manifest = dict(binary=str(args.cvm.resolve()), binary_sha256=sha(args.cvm),
                    sources={n: sha(source_dir / (n + ".java")) for n in names},
                    repeat=args.repeat, engines=args.engines, collectors=args.collectors,
                    ffm_lean=args.ffm_lean, preseed_only=args.preseed_only)
    (args.out / "manifest.json").write_text(json.dumps(manifest, indent=2))

    def run(binary, options, collector, probe, params, label):
        cmd = [str(binary), *options, "--enable-native-access=ALL-UNNAMED", collector, "-Xmx64m",
               "-cp", str(classes), probe, *params]
        start = time.monotonic()
        row = dict(label=label, command=cmd)
        row.update(run_measured(cmd, env, args.timeout))
        row["seconds"] = time.monotonic() - start
        records.append(row)
        (args.out / "records.json").write_text(json.dumps(records, indent=2))
        print(label, row.get("rc", "TIMEOUT"), round(row["seconds"], 2), flush=True)
        return row

    def parameters(probe, params, engine):
        # Compilation needs a hot call site. Interpreter acceptance exercises the
        # same full-heap operations and recovery without redundant warmup work.
        return params + ["2000"] if engine == "nojit" and probe == "ZgcInfallibleAllocation" else params

    expected = {}
    for gc in args.collectors:
        oracle_gc = {"ZGC": "-XX:+UseZGC", "G1": "-XX:+UseG1GC", "Generational": "-XX:+UseSerialGC"}[gc]
        for probe, params in cases:
            for engine in args.engines:
                actual = parameters(probe, params, engine)
                label = f"hotspot/{gc}/{engine}/{probe}/{'-'.join(actual)}"
                row = run(args.jdk / "bin" / ("java" + suffix), [], oracle_gc, probe, actual, label)
                if row.get("rc") != 0:
                    raise SystemExit("HotSpot control failed: " + label)
                expected[(gc, engine, probe, tuple(params))] = row["stdout"]
    failures = []
    for gc in args.collectors:
        selector = {"ZGC": "-XX:+UseZGC", "G1": "-XX:+UseG1GC", "Generational": "-XX:+UseGenerationalGC"}[gc]
        for policy in ["--jdk-only", "--compatible"]:
            for engine in args.engines:
                for probe, params in cases:
                    for n in range(args.repeat):
                        label = f"{gc}/{policy}/{engine}/{probe}/{'-'.join(params)}/{n}"
                        opts = [policy] + (["--nojit"] if engine == "nojit" else [])
                        row = run(args.cvm, opts, selector, probe, parameters(probe, params, engine), label)
                        crash = any(s in row.get("stderr", "") for s in ["FATAL:", "[jit-helper-panic]", "unknown panic"])
                        if row.get("rc") != 0 or row.get("timeout") or crash or row.get("stdout") != expected[(gc, engine, probe, tuple(params))]:
                            failures.append(label)
    summary = dict(runs=len(records), failures=failures, accepted=not failures)
    (args.out / "summary.json").write_text(json.dumps(summary, indent=2))
    print(json.dumps(summary), flush=True)
    return int(bool(failures) and not args.allow_failures)


if __name__ == "__main__":
    raise SystemExit(main())
