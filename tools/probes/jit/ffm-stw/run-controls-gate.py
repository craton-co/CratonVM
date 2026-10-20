#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Exercise heap-dump and frame-trace wait controls and disabled liveness paths."""
import argparse
import os
from pathlib import Path
import re
import subprocess
import sys
import time
from gate_diagnostics import clean_diagnostics

parser = argparse.ArgumentParser(description=__doc__)
for name in ("binary", "jdk", "classes", "logs"):
    parser.add_argument(name, type=Path)
args = parser.parse_args()
binary, jdk, classes, logs = (getattr(args, name).resolve() for name in
                              ("binary", "jdk", "classes", "logs"))
for path in (binary, jdk, classes):
    if not path.exists():
        parser.error(f"missing input: {path}")
logs.mkdir(parents=True, exist_ok=True)
env = dict(os.environ, JAVA_HOME=str(jdk))
env["PATH"] = str(jdk / "bin") + os.pathsep + env["PATH"]

def run(mode, collector, label, probe, settings, vm_args=(), probe_args=(), timeout=60,
        heap_mb=8):
    base = logs / f"{mode}-{collector}-{label}"
    # Keep the allocation pressure shorter than the native sleep for all
    # collectors, including GenerationalGC, so its disabled control cannot
    # accidentally reach OOM after C has already returned.
    command = [str(binary), "--" + mode, "-XX:+Use" + collector, f"-Xmx{heap_mb}m",
               "--enable-native-access=ALL-UNNAMED", *vm_args,
               "-cp", str(classes), probe, *probe_args]
    with base.with_suffix(".out").open("w") as out, base.with_suffix(".err").open("w") as err:
        started = time.monotonic()
        try:
            rc = subprocess.run(command, env=dict(env, **settings), stdout=out,
                                stderr=err, timeout=timeout).returncode
        except subprocess.TimeoutExpired:
            rc = 124
        elapsed_ms = int((time.monotonic() - started) * 1000)
    return (rc, base.with_suffix(".out").read_text(errors="replace"),
            base.with_suffix(".err").read_text(errors="replace"), elapsed_ms)

total = 0
with (logs / "summary.txt").open("w") as summary:
    def check(label, ok, rc):
        global total
        row = f"{'PASS' if ok else 'FAIL'} {label} rc={rc}"
        summary.write(row + "\n")
        summary.flush()
        print(row, flush=True)
        if not ok:
            sys.exit(1)
        total += 1

    for mode in ("jdk-only", "compatible"):
        for collector in ("ZGC", "G1GC", "GenerationalGC"):
            for enabled in (True, False):
                label = "heapdump-default" if enabled else "heapdump-disabled"
                dump = logs / f"{mode}-{collector}-{label}.hprof"
                settings = {"CRATONVM_DBG_FFM": "1", "CRATONVM_JIT_THRESHOLD": "1"}
                if not enabled:
                    settings["CRATONVM_FFM_LAZY_CLAIM_NO_ROOTS_PAUSE"] = "0"
                rc, out, err, elapsed_ms = run(
                    mode, collector, label, "FfmHeapDumpProbe", settings,
                    ["-XX:+HeapDumpOnOutOfMemoryError", "-XX:HeapDumpPath=" + str(dump)])
                header = b""
                if dump.exists():
                    with dump.open("rb") as stream:
                        header = stream.read(32)
                dump_time = re.search(r"Heap dump file created .* in ([0-9.]+) secs", out)
                dump_ms = round(float(dump_time.group(1)) * 1000) if dump_time else None
                ok = (rc == 0 and clean_diagnostics(out, err)
                      and "worker returned: true" in out
                      and header.startswith(b"JAVA PROFILE 1.0.")
                      and dump_ms is not None)
                if enabled:
                    ok = (ok and dump_ms < 15000
                          # A Generational allocation retry can already own
                          # the stop-the-world when HPROF asks for its
                          # freeze-only pause. In that case DumpSafepoint
                          # intentionally declines the nested request, and
                          # the active collection's root-gathering claim is
                          # the evidence that the FFM peer is held.
                          and ("no-roots pause (freeze-only)" in err
                               or ("[ffm-lazy] claimed os_tid=" in err
                                   and "[ffm-lazy] collection claims=" in err)))
                else:
                    # The dump itself waits for C to return. Process time also
                    # includes the mutator's allocation loop and is not a useful
                    # discriminator; measure the VM's HPROF duration instead.
                    # GenerationalGC can already have an allocation-triggered
                    # collection in flight when HPROF starts. That independent
                    # root-gathering pause may claim the worker even though the
                    # no-roots flag is off; accept only explicit collection-claim
                    # evidence in that case. Otherwise the disabled freeze-only
                    # path must wait for native return.
                    collection_claim_at_oom = (
                        "[ffm-lazy] collection claims=" in err
                        and "worker still in C at OOM: true" in out)
                    if dump_ms is not None and dump_ms < 15000:
                        ok = (ok and collection_claim_at_oom
                              and "no-roots pause (freeze-only)" not in err)
                    else:
                        ok = (ok and dump_ms >= 15000
                              and "native return synchronized with a stop-the-world pause" in err
                              and "no-roots pause (freeze-only)" not in err)
                print(f"TIMING {mode} {collector} {label} dump_ms={dump_ms} process_ms={elapsed_ms}",
                      flush=True)
                check(f"{mode} {collector} {label}", ok, rc)
        for enabled in (True, False):
            for arm in ("plain", "reflective"):
                label = "enabled" if enabled else "disabled"
                rc, out, err, _ = run(
                    mode, "ZGC", f"trace-{label}-{arm}", "FfmHeldTraceProbe",
                    {"CRATONVM_JIT_THRESHOLD": "100000",
                     "CRATONVM_FFM_LAZY_CLAIM_FRAME_TRACE_PAUSE": "1" if enabled else "0"},
                    probe_args=([arm] if enabled or arm == "plain" else [arm, "wait"]),
                    heap_mb=256)
                trace_time = re.search(r"time trace (\d+) ms", err)
                immediate = enabled or arm == "plain"
                if immediate:
                    trace_ok = ("PASS" in out.splitlines()
                                and "trace returned while the worker was in C: true" in out
                                and trace_time is not None and int(trace_time.group(1)) < 2000)
                else:
                    # Without the lazy claim, reflective metadata cannot be read
                    # safely from a held owner; the legacy path waits for C to return.
                    trace_ok = ("PASS" in out.splitlines()
                                and "trace returned while the worker was in C: false" in out
                                and "worker frames: FfmHeldTraceProbe$Worker.run" in out
                                and trace_time is not None and int(trace_time.group(1)) >= 2500)
                check(f"{mode} trace-{label}-{arm}",
                      rc == 0 and trace_ok and "worker returned: true" in out
                      and clean_diagnostics(out, err), rc)
        for collector in ("ZGC", "G1GC", "GenerationalGC"):
            for key in ("CRATONVM_FFM_DOWNCALL_IN_NATIVE_LAZY", "CRATONVM_XT_JIT_ROOT_SCAN"):
                rc, out, err, _ = run(
                    mode, collector, "deadlock-disabled-" + key,
                    "R16FfmDeadlockShape", {key: "0"}, timeout=10,
                    heap_mb=256)
                check(f"{mode} {collector} {key}=0", rc == 124 and clean_diagnostics(out, err), rc)
print(f"FFM_CONTROLS_GATE_OK cases={total}", flush=True)
