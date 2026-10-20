#!/usr/bin/env python3
"""Fresh-process GC regression matrix; all generated files live in --output.

Compile the four Java probes into --classes and build the JNI source into
--jni first. Generational uses HotSpot Serial's matching reference/bean policy.
Each row keeps complete stdout/stderr and must match its fresh HotSpot oracle.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import time


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--vm")
    p.add_argument("--java", default="java")
    p.add_argument("--classes", required=True)
    p.add_argument("--jni", required=True)
    p.add_argument("--output", required=True)
    p.add_argument("--repeats", type=int, default=3)
    p.add_argument("--rounds", type=int, default=200)
    p.add_argument("--referent-heap", default="128m",
                   help="matching VM/HotSpot heap limit for referent stress rows")
    p.add_argument("--zgc-referent-heap",
                   help="override the referent heap limit for ZGC stress rows")
    p.add_argument("--unsafe-repeats", type=int, default=20)
    p.add_argument("--timeout", type=int, default=600)
    p.add_argument("--only", choices=["threshold", "referent", "unsafe", "notification"])
    p.add_argument("--doors", nargs="+",
                   choices=["plain", "volatile", "reflect", "mh", "vh",
                            "vhvolatile", "vhcasfail", "jni"],
                   default=["plain", "volatile", "reflect", "mh", "vh",
                            "vhvolatile", "vhcasfail", "jni"],
                   help="restrict referent rows to selected read doors")
    p.add_argument("--collectors", nargs="+", choices=["G1", "Generational", "Z"],
                   default=["G1", "Generational", "Z"])
    p.add_argument("--no-jit", action="store_true")
    p.add_argument("--force-marking", action="store_true",
                   help="request early G1 marking; set CRATONVM cycle flags explicitly for other collectors")
    p.add_argument("--gc-log", action="store_true")
    p.add_argument("--all-pools", action="store_true", help="also check class-space and code-heap setters")
    a = p.parse_args()
    out = Path(a.output).resolve()
    out.mkdir(parents=True, exist_ok=True)
    env = os.environ.copy()
    # A caller can tune cycle-start flags explicitly. Record the complete
    # CRATONVM environment so a run is reproducible, never silently sanitize it.
    (out / "environment.json").write_text(json.dumps({k: v for k, v in env.items()
        if k.startswith(("CRATONVM_", "JAVA_", "RUST_"))}, indent=2))
    records = []

    def run(label, executable, flags, probe, args):
        command = [executable, *flags, "-cp", str(Path(a.classes).resolve()), probe, *args]
        start = time.monotonic()
        try:
            r = subprocess.run(command, capture_output=True, timeout=a.timeout, env=env)
            code, stdout, stderr = r.returncode, r.stdout, r.stderr
        except subprocess.TimeoutExpired as e:
            code, stdout, stderr = -999, e.stdout or b"", e.stderr or b""
        (out / (label + ".stdout")).write_bytes(stdout)
        (out / (label + ".stderr")).write_bytes(stderr)
        record = dict(label=label, command=command, exit=code,
                      seconds=round(time.monotonic() - start, 3))
        records.append(record)
        (out / "results.json").write_text(json.dumps(records, indent=2))
        if code:
            raise RuntimeError(f"{label}: exit={code}; see {out / (label + '.stderr')}")
        return stdout

    for collector, oracle_gc in [("G1", "G1"), ("Generational", "Serial"), ("Z", "Z")]:
        if collector not in a.collectors:
            continue
        oracle = [f"-XX:+Use{oracle_gc}GC"]
        if a.no_jit:
            oracle.append("-Xint")
        vmflags = [f"-XX:+Use{collector}GC"]
        rows = []
        if a.only == "notification":
            rows.append(("notification-locked", "GcNotificationThreadProbe", [], a.repeats, False))
            rows.append(("notification-lockfree", "GcNotificationThreadProbe", ["lockfree"], a.repeats, False))
        if a.only in (None, "threshold"):
            rows.append(("asleep", "Zs9Ob6ThresholdWhileAsleep", [], a.repeats, False))
            pools = ["heap", "nonheap", "classspace", "code"] if a.all_pools else ["heap", "nonheap"]
            if a.no_jit and "code" in pools:
                # No compiled nmethods exist in an interpreter-only VM, so
                # this code pool cannot have a set-time usage crossing.
                pools.remove("code")
            for pool in pools:
                for wait in ("sleep", "busy"):
                    rows.append((f"threshold-{pool}-{wait}", "GcSetThresholdDeliveryProbe",
                                 [pool, wait], a.repeats, False))
        if a.only in (None, "referent"):
            for door in a.doors:
                args = [door, str(a.rounds), "400"]
                if door == "jni":
                    args.append(str(Path(a.jni).resolve()))
                rows.append((f"referent-{door}", "GcReferentReadKeepAliveProbe", args, a.repeats, True))
        if a.only in (None, "unsafe"):
            rows.append(("original-unsafe", "Zt1MkUnsafeReferent", ["200", "400", "4194304"],
                         a.unsafe_repeats, True))
        for name, probe, args, repeats, refs in rows:
            extra = ["--add-opens", "java.base/java.lang.ref=ALL-UNNAMED"] if refs else []
            heap_limit = a.referent_heap
            if refs and collector == "Z" and a.zgc_referent_heap:
                heap_limit = a.zgc_referent_heap
            heap = ["-Xmx" + heap_limit if refs else "-Xmx256m"]
            if refs and a.force_marking and collector == "G1":
                extra += ["-XX:InitiatingHeapOccupancyPercent=1"]
            for repeat in range(repeats):
                prefix = f"{collector}-{name}-{repeat}"
                expected = run(prefix + "-hotspot", a.java, oracle + heap + extra, probe, args)
                if a.vm:
                    for mode in ("jdk-only", "compatible"):
                        flags = ["--" + mode, *vmflags, *heap, *extra]
                        if a.no_jit:
                            flags.append("--nojit")
                        if a.gc_log:
                            flags.append("--verbose:gc")
                        label = prefix + "-" + mode
                        actual = run(label, a.vm, flags, probe, args)
                        records[-1]["same"] = actual == expected
                        (out / "results.json").write_text(json.dumps(records, indent=2))
                        if actual != expected:
                            raise AssertionError(f"{label}: stdout differs from HotSpot")
                print(prefix + " SAME", flush=True)
    print(f"GC_REFERENT_THRESHOLD_OK processes={len(records)}", flush=True)


if __name__ == "__main__":
    main()
