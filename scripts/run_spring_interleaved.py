#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
# Copyright 2024-2026 Craton Software Company

"""
Spring Framework Suite Runner - Interleaved A/B: Default vs Fast-Boot
Runs the Spring Framework test classes class-by-class, alternating execution order
(A-B / B-A) between:
  Arm 1: Default Mode (--real-jdk)
  Arm 2: Fast-Boot Mode (--fast-boot)

Provides per-class timeouts to ensure no single test hang stalls the suite,
and writes live structured TSV metrics + comprehensive execution logs.
"""

import os
import sys
import time
import subprocess
import argparse

DEFAULT_JDK = "/usr/lib/jvm/java-25-openjdk-amd64"
DEFAULT_BIN = "/root/tc-bin/cratonvm"
DEFAULT_RUNNER_DIR = "/root/CVM/apps/spring-suite-runner"
DEFAULT_CP_DIR = "/root/CVM/apps/spring-suite-runner/.af_spring"  # written by scripts/spring_classpath.py
DEFAULT_CLASSES_FILE = "/root/spring_valid_classes.txt"
DEFAULT_TIMEOUT = 120
# The JVM args Spring's own `test` tasks run with (buildSrc TestConventions.configureTests).
SPRING_JVM_ARGS = [
    "--add-opens=java.base/java.lang=ALL-UNNAMED",
    "--add-opens=java.base/java.util=ALL-UNNAMED",
    "-Djava.awt.headless=true",
    "-XX:+EnableDynamicAgentLoading",
]

def parse_args():
    parser = argparse.ArgumentParser(description="Interleaved Spring Suite Runner (Default vs Fast-Boot)")
    parser.add_argument("--bin", default=os.environ.get("CV_BIN", DEFAULT_BIN), help="Path to cratonvm binary")
    parser.add_argument("--classes", default=DEFAULT_CLASSES_FILE, help="Path to classes list file")
    parser.add_argument("--runner-dir", default=DEFAULT_RUNNER_DIR, help="Working directory for CratonRunner")
    parser.add_argument("--cp-dir", default=DEFAULT_CP_DIR, help="Per-module classpath argfiles + classes.tsv")
    parser.add_argument("--java-home", default=DEFAULT_JDK, help="Path to OpenJDK home")
    parser.add_argument("--timeout", type=int, default=DEFAULT_TIMEOUT, help="Per-class timeout in seconds")
    parser.add_argument("--limit", type=int, default=0, help="Limit number of classes to run (0 = all)")
    parser.add_argument("--agent-jar", default=None, help="Optional -javaagent jar path to preload (e.g. mockito-core.jar)")
    parser.add_argument("--out-dir", default=None, help="Output directory")
    return parser.parse_args()

def load_modules(cp_dir):
    with open(os.path.join(cp_dir, "classes.tsv")) as f:
        return dict(line.rstrip("\n").split("\t") for line in f if line.strip())

def run_arm(bin_path, mode_flags, java_home, cp_file, runner_dir, class_name, timeout_s, agent_jar=None):
    agent_flags = [f"-javaagent:{agent_jar}"] if agent_jar else []
    cmd = [
        bin_path,
    ] + mode_flags + SPRING_JVM_ARGS + agent_flags + [
        "-Xmx4g",
        "--java-home", java_home,
        f"@{cp_file}",
        "CratonRunner",
        class_name
    ]

    t0 = time.time()
    try:
        proc = subprocess.run(
            cmd,
            cwd=runner_dir,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            timeout=timeout_s,
            text=True,
            errors="replace"
        )
        elapsed_ms = int((time.time() - t0) * 1000)
        out = proc.stdout
        rc = proc.returncode

        # Parse @@RESULT
        found = started = ok = failed = aborted = skipped = 0
        reported_ms = elapsed_ms
        status = "PASS" if rc == 0 else "FAIL"

        for line in out.splitlines():
            if line.startswith("@@RESULT "):
                # e.g.: @@RESULT <cls> found=1 started=1 ok=1 failed=0 aborted=0 skipped=0 ms=123
                for token in line.strip().split()[2:]:
                    if "=" in token:
                        k, v = token.split("=", 1)
                        if v.isdigit():
                            val = int(v)
                            if k == "found": found = val
                            elif k == "started": started = val
                            elif k == "ok": ok = val
                            elif k == "failed": failed = val
                            elif k == "aborted": aborted = val
                            elif k == "skipped": skipped = val
                            elif k == "ms": reported_ms = val

        if failed > 0 or rc != 0:
            status = "FAIL"
        elif ok == 0 and found == 0 and rc == 0:
            status = "NO_TESTS"

        return {
            "status": status,
            "rc": rc,
            "found": found,
            "started": started,
            "ok": ok,
            "failed": failed,
            "aborted": aborted,
            "skipped": skipped,
            "ms": reported_ms,
            "wall_ms": elapsed_ms,
            "output": out
        }

    except subprocess.TimeoutExpired as te:
        elapsed_ms = int((time.time() - t0) * 1000)
        raw_out = te.stdout.decode("utf-8", errors="replace") if isinstance(te.stdout, bytes) else (te.stdout or "")
        return {
            "status": "TIMEOUT",
            "rc": -9,
            "found": 0, "started": 0, "ok": 0, "failed": 0, "aborted": 0, "skipped": 0,
            "ms": elapsed_ms,
            "wall_ms": elapsed_ms,
            "output": f"[TIMEOUT after {timeout_s}s]\n" + raw_out
        }
    except Exception as e:
        elapsed_ms = int((time.time() - t0) * 1000)
        return {
            "status": "ERROR",
            "rc": -1,
            "found": 0, "started": 0, "ok": 0, "failed": 0, "aborted": 0, "skipped": 0,
            "ms": elapsed_ms,
            "wall_ms": elapsed_ms,
            "output": f"[EXCEPTION]: {e}\n"
        }

def write_summary(summary_file, stats, total_classes, completed_count):
    with open(summary_file, "w") as sf:
        sf.write("=================================================================\n")
        sf.write(f"Spring Framework Suite: Interleaved A/B (Default vs Fast-Boot)\n")
        sf.write(f"Progress: {completed_count} / {total_classes} classes completed ({completed_count/total_classes*100:.1f}%)\n")
        sf.write("=================================================================\n\n")

        for arm_key in ["default", "fastboot"]:
            s = stats[arm_key]
            sf.write(f"[{arm_key.upper()}]\n")
            sf.write(f"  Classes Run        : {s['classes_run']}\n")
            sf.write(f"  Classes Passed     : {s['classes_pass']}\n")
            sf.write(f"  Classes Failed     : {s['classes_fail']}\n")
            sf.write(f"  Classes Timed Out  : {s['classes_timeout']}\n")
            sf.write(f"  Classes No Tests   : {s['classes_no_tests']}\n")
            sf.write(f"  Tests Found        : {s['tests_found']:,}\n")
            sf.write(f"  Tests Succeeded    : {s['tests_ok']:,}\n")
            sf.write(f"  Tests Failed       : {s['tests_failed']:,}\n")
            sf.write(f"  Cumulative Time    : {s['sum_ms']/1000.0:.1f}s\n\n")

def main():
    args = parse_args()

    if not os.path.exists(args.bin):
        print(f"Error: cratonvm binary not found at {args.bin}", file=sys.stderr)
        sys.exit(1)

    if not os.path.exists(args.classes):
        print(f"Error: classes file not found at {args.classes}", file=sys.stderr)
        sys.exit(1)

    with open(args.classes) as f:
        classes = [l.strip() for l in f if l.strip() and not l.startswith("#")]
    modules = load_modules(args.cp_dir)
    unknown = [c for c in classes if c not in modules]
    if unknown:
        print(f"Error: no module in {args.cp_dir}/classes.tsv for: {unknown[:5]}", file=sys.stderr)
        sys.exit(1)

    if args.limit > 0:
        classes = classes[:args.limit]

    total_classes = len(classes)
    run_id = time.strftime("%Y%m%d_%H%M%S")
    out_dir = args.out_dir or f"/root/spring_interleaved_{run_id}"
    os.makedirs(out_dir, exist_ok=True)

    tsv_path = os.path.join(out_dir, "results_interleaved.tsv")
    summary_path = os.path.join(out_dir, "summary.txt")
    log_default_path = os.path.join(out_dir, "shard_default.log")
    log_fastboot_path = os.path.join(out_dir, "shard_fastboot.log")

    log_def = open(log_default_path, "w", buffering=1)
    log_fb = open(log_fastboot_path, "w", buffering=1)
    tsv = open(tsv_path, "w", buffering=1)

    tsv.write("idx\tclass\tfirst_arm\tdef_status\tdef_ok\tdef_failed\tdef_ms\tfb_status\tfb_ok\tfb_failed\tfb_ms\tspeedup\n")

    arms = {
        "default": ["--real-jdk"],
        "fastboot": ["--fast-boot"]
    }

    stats = {
        "default": {"classes_run": 0, "classes_pass": 0, "classes_fail": 0, "classes_timeout": 0, "classes_no_tests": 0, "tests_found": 0, "tests_ok": 0, "tests_failed": 0, "sum_ms": 0},
        "fastboot": {"classes_run": 0, "classes_pass": 0, "classes_fail": 0, "classes_timeout": 0, "classes_no_tests": 0, "tests_found": 0, "tests_ok": 0, "tests_failed": 0, "sum_ms": 0}
    }

    print("=================================================================", flush=True)
    print(f"Spring Framework Interleaved A/B Suite Run (Total Classes: {total_classes})", flush=True)
    print(f"Binary: {args.bin}", flush=True)
    print(f"Per-Class Timeout: {args.timeout}s", flush=True)
    print(f"Output Directory: {out_dir}", flush=True)
    print("=================================================================\n", flush=True)

    t_start = time.time()

    for idx, cls in enumerate(classes, 1):
        # Alternate order to eliminate warm-up / drift bias
        if idx % 2 == 1:
            order = ["default", "fastboot"]
        else:
            order = ["fastboot", "default"]

        results = {}

        for arm_name in order:
            flags = arms[arm_name]
            res = run_arm(args.bin, flags, args.java_home, f"{args.cp_dir}/{modules[cls]}.txt", args.runner_dir, cls, args.timeout)
            results[arm_name] = res

            log_target = log_def if arm_name == "default" else log_fb
            log_target.write(f"=== [{idx}/{total_classes}] {cls} (status={res['status']}, ok={res['ok']}, fail={res['failed']}, ms={res['ms']}) ===\n")
            log_target.write(res["output"] + "\n\n")

            st = stats[arm_name]
            st["classes_run"] += 1
            if res["status"] == "PASS":
                st["classes_pass"] += 1
            elif res["status"] == "TIMEOUT":
                st["classes_timeout"] += 1
            elif res["status"] == "NO_TESTS":  # helper or abstract class in the list, not a failure
                st["classes_no_tests"] += 1
            else:
                st["classes_fail"] += 1

            st["tests_found"] += res["found"]
            st["tests_ok"] += res["ok"]
            st["tests_failed"] += res["failed"]
            st["sum_ms"] += res["ms"]

        d = results["default"]
        f = results["fastboot"]

        speedup = f"{d['ms'] / f['ms']:.2f}x" if f["ms"] > 0 and d["ms"] > 0 else "N/A"

        tsv.write(f"{idx}\t{cls}\t{order[0]}\t{d['status']}\t{d['ok']}\t{d['failed']}\t{d['ms']}\t{f['status']}\t{f['ok']}\t{f['failed']}\t{f['ms']}\t{speedup}\n")

        print(
            f"[{idx:4d}/{total_classes}] {cls} | "
            f"default: {d['status']} ({d['ok']} ok, {d['failed']} fail, {d['ms']}ms) | "
            f"fastboot: {f['status']} ({f['ok']} ok, {f['failed']} fail, {f['ms']}ms) | "
            f"speedup: {speedup}",
            flush=True
        )

        if idx % 10 == 0 or idx == total_classes:
            write_summary(summary_path, stats, total_classes, idx)

    total_wall_s = int(time.time() - t_start)
    print("\n=================================================================", flush=True)
    print(f"INTERLEAVED RUN COMPLETED in {total_wall_s}s!", flush=True)
    print(f"Summary written to: {summary_path}", flush=True)
    print(f"TSV metrics: {tsv_path}", flush=True)
    print("=================================================================", flush=True)

if __name__ == "__main__":
    main()
