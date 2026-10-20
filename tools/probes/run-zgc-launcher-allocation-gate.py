#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
"""Compare launcher allocation/error reporting with fresh HotSpot processes."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time
from gc_process_measure import run_measured


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--cvm", type=Path, required=True)
    ap.add_argument("--jdk", type=Path, required=True)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--repeat", type=int, default=8)
    ap.add_argument("--collectors", nargs="+", choices=["ZGC", "G1", "Generational"], default=["ZGC"])
    args = ap.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    classes = args.out / "classes"
    classes.mkdir(exist_ok=True)
    source = Path(__file__).resolve().parent
    names = ["ZgcLauncherReporting", "ZgcInstanceMainOom"]
    suffix = ".exe" if os.name == "nt" else ""
    subprocess.run([str(args.jdk / "bin" / ("javac" + suffix)), "-d", str(classes),
                    *[str(source / (n + ".java")) for n in names]], check=True)
    selection = subprocess.run([str(args.jdk / "bin" / ("java" + suffix)), "-cp", str(classes),
                                "ZgcLauncherReporting"], capture_output=True, check=True)
    native_encoding, unicode_hex = selection.stdout.decode("ascii").splitlines()
    unicode_argument = bytes.fromhex(unicode_hex).decode("utf-16-be")
    cases = [("ZgcLauncherReporting$" + name, ["one", "one", unicode_argument, unicode_hex] if name == "Arguments" else [])
             for name in ["ConstructorFailure", "MainFailure", "Arguments", "MainFailureAtFullHeap"]]
    cases.append(("ZgcInstanceMainOom", []))
    env = dict(os.environ, JAVA_HOME=str(args.jdk), RUST_LOG="error")
    records = []
    expected = {}
    manifest = dict(binary=str(args.cvm.resolve()),
                    binary_sha256=hashlib.file_digest(open(args.cvm, "rb"), "sha256").hexdigest(),
                    sources={n: hashlib.sha256((source / (n + ".java")).read_bytes()).hexdigest() for n in names},
                    hotspot_native_encoding=native_encoding, unicode_argument_utf16=unicode_hex)
    (args.out / "manifest.json").write_text(json.dumps(manifest, indent=2))

    def run(binary, options, selector, cl, params, label):
        # Serial heaps can reuse the released small block; a full ZGC page
        # cannot. Keep the receiver refusal deterministic in each collector.
        budget = "1" if "Generational" in label else "0"
        # The forced receiver gate needs physical refusal, independent of
        # collector-specific overhead-limit timing.
        hard_limit = ["-XX:-UseGCOverheadLimit"] if cl == "ZgcInstanceMainOom" else []
        cmd = [str(binary), *options, *hard_limit, selector, "-Xmx64m",
               "-Dzr1al.reporting.chunks=" + budget, "-cp", str(classes), cl, *params]
        start = time.monotonic()
        row = dict(label=label, command=cmd)
        row.update(run_measured(cmd, env, 120))
        row["seconds"] = time.monotonic() - start
        records.append(row)
        (args.out / "records.json").write_text(json.dumps(records, indent=2))
        print(label, row.get("rc", "TIMEOUT"), round(row["seconds"], 2), flush=True)
        return row

    for gc in args.collectors:
        selector = {"ZGC": "-XX:+UseZGC", "G1": "-XX:+UseG1GC", "Generational": "-XX:+UseSerialGC"}[gc]
        for cl, params in cases:
            row = run(args.jdk / "bin" / ("java" + suffix), [], selector, cl, params, "hotspot/" + gc + "/" + cl)
            wanted_rc = 0 if cl.endswith("$Arguments") else 1
            if row.get("rc") != wanted_rc or row.get("timeout"):
                raise SystemExit("HotSpot control failed: " + cl)
            if cl.endswith("$Arguments") and (row["stdout"] != "arguments: fresh and preserved\n" or row["stderr"]):
                raise SystemExit("HotSpot argument control failed")
            if cl == "ZgcInstanceMainOom" and (row["stdout"] or row["stderr"] not in [
                    'Exception in thread "main" ',
                    'Exception in thread "main" java.lang.OutOfMemoryError: Java heap space\n']):
                raise SystemExit("HotSpot receiver refusal control failed")
            expected[(gc, cl)] = tuple(row[k] for k in ["rc", "stdout", "stderr"])
    failures = []
    for gc in args.collectors:
        selector = "-XX:+Use" + gc + "GC" if gc != "ZGC" else "-XX:+UseZGC"
        for policy in ["--jdk-only", "--compatible"]:
            for engine in ["jit", "nojit"]:
                options = [policy] + (["--nojit"] if engine == "nojit" else [])
                for cl, params in cases:
                    for n in range(args.repeat):
                        label = f"{gc}/{policy}/{engine}/{cl}/{n}"
                        row = run(args.cvm, options, selector, cl, params, label)
                        if tuple(row.get(k) for k in ["rc", "stdout", "stderr"]) != expected[(gc, cl)]:
                            failures.append(label)
    summary = dict(runs=len(records), failures=failures, accepted=not failures)
    (args.out / "summary.json").write_text(json.dumps(summary, indent=2))
    print(json.dumps(summary), flush=True)
    return int(bool(failures))


if __name__ == "__main__":
    raise SystemExit(main())
