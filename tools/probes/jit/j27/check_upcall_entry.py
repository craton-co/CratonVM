#!/usr/bin/env python3
"""HotSpot parity and interleaved timing of the native-to-Java entry record.

Compile the Java probes into --classes first. Pass --agent pointing to a jar
with J27UpcallEntryRedefine*.class and its documented agent manifest.
All child output, return codes and timings are retained in --output.
"""
import argparse
import json
import os
from pathlib import Path
import re
import statistics
import subprocess
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--vm", required=True)
    parser.add_argument("--java", default="java")
    parser.add_argument("--classes", required=True)
    parser.add_argument("--agent", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--timing-reps", type=int, default=0)
    parser.add_argument("--cpus", help="Linux CPU list, e.g. 4-7")
    parser.add_argument("--timing-only", action="store_true")
    parser.add_argument("--gc-only", action="store_true",
                        help="Run collector/fallback parity and filename reachability")
    parser.add_argument("--timing-arms", nargs="+", choices=["base", "entry", "default", "key", "mint"],
                        default=["base", "entry", "default"])
    args = parser.parse_args()
    output = Path(args.output)
    output.mkdir(parents=True, exist_ok=True)
    results = []

    def run(label, executable, flags, probe, probe_args=(), env_flags=None, gc_group=None):
        cmd = [executable, *flags, "--enable-native-access=ALL-UNNAMED",
               "-cp", args.classes, probe, *probe_args]
        if args.cpus:
            cmd = ["taskset", "-c", args.cpus, *cmd]
        # Use the supported surface so flag migration notices do not become
        # extra stderr in the byte-for-byte uncaught-exception comparison.
        env = {key: value for key, value in os.environ.items() if not key.startswith("CRATONVM_")}
        names = {
            "CRATONVM_EXECUTE_CACHED_ENTRY": "execute-cached-entry",
            "CRATONVM_FFM_UPCALL_KNOWN_FIRST_CLASS": "ffm-upcall-known-first-class",
            "CRATONVM_NATIVE_CALLBACK_MEMO_STATIC_KEY_VALIDATE_ONCE": "native-callback-memo-static-key-validate-once",
            "CRATONVM_BG_COMPILE": "bg-compile",
            "CRATONVM_FFM_UPCALL_MINT_BATCH": "ffm-upcall-mint-batch",
            "CRATONVM_FFM_UPCALL_DIRECT_STATIC": "ffm-upcall-direct-static",
        }
        tokens = [("" if value in ("1", "true") else "-") + names[key]
                  for key, value in (env_flags or {}).items()]
        grouped = {"CRATONVM_JIT": ",".join(tokens)} if tokens else {}
        if gc_group:
            grouped["CRATONVM_GC"] = gc_group
        env.update(grouped)
        started = time.monotonic()
        completed = subprocess.run(cmd, env=env, capture_output=True, text=True, timeout=240)
        (output / (label + ".stdout")).write_text(completed.stdout)
        (output / (label + ".stderr")).write_text(completed.stderr)
        row = dict(label=label, command=cmd, env=grouped, rc=completed.returncode,
                   seconds=time.monotonic() - started)
        results.append(row)
        (output / "results.json").write_text(json.dumps(results, indent=2))
        return completed

    probes = ["J27EntryFileUnload", "J27EntryFileReach", "J27UpcallEntryRows", "J27UpcallEntryRedefine", "J27UpcallEntryRedefineCold", "J27UpcallEntryRedefineRemoveSource", "J27W8UpcKeyRows",
              "J27FfmUpcallRows", "J27FfmUpcallThrows", "R22FfmUpcallCost"]
    if not args.timing_only and not args.gc_only:
        reference = {}
        for probe in probes:
            flags = ["-javaagent:" + args.agent] if "Redefine" in probe else []
            extra = ["4000", "target"] if probe == "R22FfmUpcallCost" else []
            ref = run("hotspot-" + probe, args.java, flags, probe, extra)
            reference[probe] = ref
            assert ref.returncode == (1 if probe.endswith("Throws") else 0), (probe, ref.stderr)
        for mode in ["jdk-only", "compatible"]:
            for engine in ["jit", "nojit", "eager"]:
                for cached in ["0", "1"]:
                    for minted in ["0", "1"]:
                        for probe in probes:
                            flags = ["--" + mode]
                            if engine == "nojit": flags += ["--nojit"]
                            if "Redefine" in probe: flags += ["-javaagent:" + args.agent]
                            env = {"CRATONVM_EXECUTE_CACHED_ENTRY": cached,
                                   "CRATONVM_FFM_UPCALL_KNOWN_FIRST_CLASS": minted,
                                   "CRATONVM_NATIVE_CALLBACK_MEMO_STATIC_KEY_VALIDATE_ONCE": "1"}
                            if engine == "eager": env["CRATONVM_BG_COMPILE"] = "0"
                            extra = ["4000", "target"] if probe == "R22FfmUpcallCost" else []
                            label = f"{mode}-{engine}-entry{cached}-mint{minted}-{probe}"
                            got = run(label, args.vm, flags, probe, extra, env)
                            ref = reference[probe]
                            facts = lambda text: "\n".join(line for line in text.splitlines()
                                                           if "-ns-per-upcall(info)" not in line)
                            assert got.returncode == ref.returncode, (label, got.returncode, got.stderr)
                            assert facts(got.stdout) == facts(ref.stdout), (label, got.stdout, ref.stdout)
                            if probe.endswith("Throws"):
                                assert got.stderr == ref.stderr, (label, got.stderr, ref.stderr)
                            print("PASS", label, flush=True)

    if args.gc_only:
        for collector in ["Serial", "G1", "Z"]:
            collector_flag = "-XX:+Use" + collector + "GC"
            for probe in ["J27EntryFileReach", "J27EntryFileUnload", "J27FfmUpcallRows",
                          "J27UpcallEntryRows", "J27FfmUpcallThrows"]:
                ref = run(f"hotspot-{collector}-{probe}", args.java, [collector_flag], probe)
                assert ref.returncode == (1 if probe.endswith("Throws") else 0), (probe, ref.stderr)
                filename_probe = probe.startswith("J27EntryFile")
                if filename_probe:
                    assert "false" not in ref.stdout, (collector, probe, ref.stdout)
                # G1's strong pool is a separate default policy: use its supported
                # weak mode when checking whether a filename can die with a class.
                gc_group = "g1-weak-string-table" if collector == "G1" and filename_probe else None
                for mode in ["jdk-only", "compatible"]:
                    variants = ["default", "uncached"] if filename_probe else ["default", "uncached", "unbatched", "indirect"]
                    for variant in variants:
                        env = {"CRATONVM_EXECUTE_CACHED_ENTRY": "0" if variant == "uncached" else "1",
                               "CRATONVM_FFM_UPCALL_KNOWN_FIRST_CLASS": "0" if variant == "uncached" else "1"}
                        if variant == "unbatched": env["CRATONVM_FFM_UPCALL_MINT_BATCH"] = "0"
                        if variant == "indirect": env["CRATONVM_FFM_UPCALL_DIRECT_STATIC"] = "0"
                        label = f"{mode}-{collector}-{variant}-{probe}"
                        got = run(label, args.vm, ["--" + mode, collector_flag, "-Xmx128m"],
                                  probe, env_flags=env, gc_group=gc_group)
                        assert got.returncode == ref.returncode, (label, got.stderr)
                        assert got.stdout == ref.stdout, (label, got.stdout, ref.stdout)
                        if probe.endswith("Throws"):
                            assert got.stderr == ref.stderr, (label, got.stderr, ref.stderr)
                        print("PASS", label, flush=True)

    timings = []
    for mode in ["jdk-only", "compatible"]:
        for engine in ["jit", "nojit"]:
            for rep in range(args.timing_reps):
                # Reverse the sequence on alternate repetitions to expose drift.
                for arm in (args.timing_arms if rep % 2 == 0 else list(reversed(args.timing_arms))):
                    env = {"CRATONVM_EXECUTE_CACHED_ENTRY": "0" if arm == "base" else "1",
                           "CRATONVM_NATIVE_CALLBACK_MEMO_STATIC_KEY_VALIDATE_ONCE": "1" if arm in ("key", "mint") else "0",
                           "CRATONVM_FFM_UPCALL_KNOWN_FIRST_CLASS": "1" if arm in ("default", "mint") else "0"}
                    flags = ["--" + mode] + (["--nojit"] if engine == "nojit" else [])
                    label = f"time-{mode}-{engine}-{rep}-{arm}"
                    got = run(label, args.vm, flags, "R22FfmUpcallCost", ["20000", "target"], env)
                    assert got.returncode == 0, (label, got.stderr)
                    assert "target -8203967840518115083" in got.stdout, (label, got.stdout)
                    ns = int(re.search(r"target-ns-per-upcall\(info\) (\d+)", got.stdout)[1])
                    timings.append(dict(mode=mode, engine=engine, rep=rep, arm=arm, ns=ns))
                    (output / "timings.json").write_text(json.dumps(timings, indent=2))
                    print(label, ns, flush=True)
    if timings:
        for mode in ["jdk-only", "compatible"]:
            for engine in ["jit", "nojit"]:
                for arm in args.timing_arms:
                    values = [r["ns"] for r in timings if (r["mode"], r["engine"], r["arm"]) == (mode, engine, arm)]
                    print(mode, engine, arm, "median", statistics.median(values), "range", min(values), max(values))


if __name__ == "__main__":
    main()
