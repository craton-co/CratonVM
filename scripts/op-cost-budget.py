#!/usr/bin/env python3
"""Measure, calibrate, and enforce OpCostRows instruction-per-operation budgets.

Uses Linux `perf stat -x, -e instructions:u` with `(I(3N) - I(N)) / 2N` across
all 20 kernels in tools/probes/OpCostRows.java, on both HotSpot 25 and CratonVM
(--real-jdk and --compatible) across all three collectors:
  - Serial / Generational (-XX:+UseSerialGC)
  - G1                    (-XX:+UseG1GC)
  - ZGC                   (-XX:+UseZGC)
"""

from __future__ import annotations

import argparse
import csv
import os
import statistics
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PROBE_JAVA = ROOT / "tools" / "probes" / "OpCostRows.java"
DEFAULT_BUDGET = ROOT / "regression-suite" / "perf" / "op-cost-budget.tsv"

ROWS = [
    "arith",
    "getfield",
    "putInt",
    "putRef",
    "aastore",
    "virt2",
    "iface4",
    "lambda",
    "allocObj",
    "allocArr",
    "box",
    "mapGet",
    "listGet",
    "charAt",
    "sbAppend",
    "sync",
    "typeCheck",
    "arraycopy",
    "throwCatch",
    "throwNew",
]

PARITY_ROWS = {"sync", "throwNew", "allocArr"}

MODES: tuple[tuple[str, list[str]], ...] = (
    ("real-jdk", []),
    ("compatible", ["--compatible"]),
)

# Verified Phase 1 origin/dev (670297786b) full-scale (--scale full --reps 3)
# median CratonVM instructions/op across all 18 (collector, mode, row) parity cells.
DEV_PARITY_BASELINE_INSNS: dict[tuple[str, str, str], float] = {
    ("serial", "real-jdk", "sync"): 89.01,
    ("serial", "real-jdk", "throwNew"): 11207.89,
    ("serial", "real-jdk", "allocArr"): 91.43,
    ("serial", "compatible", "sync"): 89.35,
    ("serial", "compatible", "throwNew"): 11371.04,
    ("serial", "compatible", "allocArr"): 91.30,
    ("g1", "real-jdk", "sync"): 86.27,
    ("g1", "real-jdk", "throwNew"): 10193.59,
    ("g1", "real-jdk", "allocArr"): 113.33,
    ("g1", "compatible", "sync"): 86.27,
    ("g1", "compatible", "throwNew"): 10332.66,
    ("g1", "compatible", "allocArr"): 112.90,
    ("zgc", "real-jdk", "sync"): 86.16,
    ("zgc", "real-jdk", "throwNew"): 9056.21,
    ("zgc", "real-jdk", "allocArr"): 274.62,
    ("zgc", "compatible", "sync"): 86.30,
    ("zgc", "compatible", "throwNew"): 9194.27,
    ("zgc", "compatible", "allocArr"): 273.92,
}

COLLECTORS = [
    ("serial", "-XX:+UseSerialGC"),
    ("g1", "-XX:+UseG1GC"),
    ("zgc", "-XX:+UseZGC"),
]

FAST_ROWS = {
    "arith",
    "getfield",
    "putInt",
    "putRef",
    "aastore",
    "virt2",
    "lambda",
    "box",
    "charAt",
    "typeCheck",
}


def row_ops_n(row: str, scale: str) -> int:
    if scale == "quick":
        if row == "throwNew":
            return 50_000
        if row == "throwCatch":
            return 200_000
        return 5_000_000
    if row == "throwNew":
        return 100_000
    if row in FAST_ROWS:
        return 50_000_000
    return 10_000_000


def detect_jdk_home(explicit: str | None) -> str:
    if explicit:
        return explicit
    env_jh = os.environ.get("JAVA_HOME")
    if env_jh and (Path(env_jh) / "bin" / "javac").exists():
        return env_jh
    javac = subprocess.check_output(["which", "javac"], text=True).strip()
    return str(Path(javac).resolve().parent.parent)


def run_perf_instructions(cmd: list[str]) -> tuple[int, str]:
    env = os.environ.copy()
    env["LC_ALL"] = "C"
    proc = subprocess.run(
        ["perf", "stat", "-x,", "-e", "instructions:u", "--", *cmd],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=env,
        check=True,
    )
    done_line = ""
    for out_line in proc.stdout.splitlines():
        if out_line.startswith("done "):
            done_line = out_line.strip()
            break
    if not done_line:
        raise RuntimeError(f"OpCostRows did not emit checksum line: {proc.stdout!r}")
    for line in proc.stderr.splitlines():
        parts = line.split(",")
        if len(parts) >= 3 and parts[2].startswith("instructions"):
            val = parts[0].strip()
            if val in ("<not counted>", "<not supported>", ""):
                raise RuntimeError(f"perf counter invalid ({line!r}) for {cmd}")
            return int(val), done_line
    raise RuntimeError(f"no instructions line in perf output:\n{proc.stderr}")


def measure_row_pair(base_cmd: list[str], classpath: str, row: str, n: int) -> tuple[float, tuple[str, str]]:
    i1, d1 = run_perf_instructions([*base_cmd, "-cp", classpath, "OpCostRows", "count", row, str(n)])
    i3, d3 = run_perf_instructions([*base_cmd, "-cp", classpath, "OpCostRows", "count", row, str(3 * n)])
    return (i3 - i1) / float(2 * n), (d1, d3)


def measure_row_median(base_cmd: list[str], classpath: str, row: str, n: int, reps: int) -> tuple[float, tuple[str, str]]:
    results = [measure_row_pair(base_cmd, classpath, row, n) for _ in range(reps)]
    return statistics.median(v for v, _ in results), results[0][1]


def compile_probe(jdk_home: str, out_dir: Path) -> None:
    javac = str(Path(jdk_home) / "bin" / "javac")
    subprocess.run([javac, "--release", "25", "-d", str(out_dir), str(PROBE_JAVA)], check=True)


def verify_checksum_parity(jdk_home: str, cvm_exe: str, classpath: str) -> None:
    java = str(Path(jdk_home) / "bin" / "java")
    for gc_name, gc_flag in COLLECTORS:
        hs = subprocess.check_output(
            [java, gc_flag, "-Xmx2g", "-cp", classpath, "OpCostRows", "count", "arith", "10000"],
            text=True,
        ).strip().splitlines()[-1]
        for mode_flags in ([], ["--compatible"]):
            out = subprocess.check_output(
                [cvm_exe, *mode_flags, "--java-home", jdk_home, gc_flag, "-Xmx2g", "-cp", classpath, "OpCostRows", "count", "arith", "10000"],
                text=True,
            ).strip().splitlines()[-1]
            if out != hs:
                raise RuntimeError(f"checksum mismatch ({gc_name} {mode_flags}): hs={hs!r} cvm={out!r}")


def cmd_calibrate(args: argparse.Namespace) -> int:
    jdk_home = detect_jdk_home(args.java_home)
    out_path = Path(args.out)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="opcost-") as td:
        cp = Path(td)
        compile_probe(jdk_home, cp)
        verify_checksum_parity(jdk_home, args.exe, str(cp))
        java = str(Path(jdk_home) / "bin" / "java")
        rows_out = []
        for gc_name, gc_flag in COLLECTORS:
            hs_cmd = [java, gc_flag, "-Xmx2g"]
            cvm_real_cmd = [args.exe, "--java-home", jdk_home, gc_flag, "-Xmx2g"]
            cvm_comp_cmd = [args.exe, "--compatible", "--java-home", jdk_home, gc_flag, "-Xmx2g"]
            for row in ROWS:
                n = row_ops_n(row, args.scale)
                hs, hs_sum = measure_row_median(hs_cmd, str(cp), row, n, args.reps)
                cvm_real, real_sum = measure_row_median(cvm_real_cmd, str(cp), row, n, args.reps)
                cvm_comp, comp_sum = measure_row_median(cvm_comp_cmd, str(cp), row, n, args.reps)
                if real_sum != hs_sum or comp_sum != hs_sum:
                    raise RuntimeError(
                        f"checksum mismatch ({gc_name}/{row}): hs={hs_sum} real={real_sum} compat={comp_sum}"
                    )
                ratio_real = cvm_real / hs if hs > 0 else 999.0
                ratio_comp = cvm_comp / hs if hs > 0 else 999.0
                rows_out.append(
                    {
                        "collector": gc_name,
                        "row": row,
                        "n": n,
                        "hotspot_insns": f"{hs:.2f}",
                        "cvm_real_insns": f"{cvm_real:.2f}",
                        "ratio_real": f"{ratio_real:.2f}",
                        "cvm_compat_insns": f"{cvm_comp:.2f}",
                        "ratio_compat": f"{ratio_comp:.2f}",
                        "budget_insns": f"{max(0.50, cvm_real * 1.05):.2f}",
                        "budget_compat_insns": f"{max(0.50, cvm_comp * 1.05):.2f}",
                    }
                )
                print(
                    f"[{gc_name:<6}] {row:<11s} HS={hs:8.2f} CVM_REAL={cvm_real:8.2f} ({ratio_real:5.2f}x) "
                    f"CVM_COMP={cvm_comp:8.2f} ({ratio_comp:5.2f}x)",
                    flush=True,
                )
        with out_path.open("w", newline="") as f:
            writer = csv.DictWriter(
                f,
                fieldnames=[
                    "collector",
                    "row",
                    "n",
                    "hotspot_insns",
                    "cvm_real_insns",
                    "ratio_real",
                    "cvm_compat_insns",
                    "ratio_compat",
                    "budget_insns",
                    "budget_compat_insns",
                ],
                delimiter="\t",
            )
            writer.writeheader()
            writer.writerows(rows_out)
    return 0


def load_budget(path: Path) -> list[dict[str, str]]:
    with path.open("r", newline="") as f:
        return list(csv.DictReader(f, delimiter="\t"))


def cmd_validate_table(args: argparse.Namespace) -> int:
    rows = load_budget(Path(args.budget))
    seen = {(r["collector"], r["row"]) for r in rows}
    expected = {(c, r) for c, _ in COLLECTORS for r in ROWS}
    missing = sorted(expected - seen)
    if missing:
        print(f"ERROR: missing budget entries: {missing}", file=sys.stderr)
        return 1
    failures = []
    passing_2x = 0
    for r in rows:
        hs = float(r["hotspot_insns"])
        cvm = float(r["cvm_real_insns"])
        comp = float(r["cvm_compat_insns"])
        b_real = float(r["budget_insns"])
        b_comp = float(r["budget_compat_insns"])
        if hs <= 0:
            failures.append(f"{r['collector']}/{r['row']}: non-positive HotSpot count {hs}")
            continue
        if b_real < cvm:
            failures.append(f"{r['collector']}/{r['row']}: budget_insns {b_real:.2f} < cvm_real_insns {cvm:.2f}")
        if b_comp < comp:
            failures.append(f"{r['collector']}/{r['row']}: budget_compat_insns {b_comp:.2f} < cvm_compat_insns {comp:.2f}")
        for mode_label, val in (("real-jdk", cvm), ("compatible", comp)):
            if val <= 2.0 * hs:
                passing_2x += 1
            elif args.require_2x:
                failures.append(f"{r['collector']}/{r['row']}: {mode_label} {val:.2f} > 2.0x HotSpot ({hs:.2f})")
    if failures:
        print("Budget table validation failed:\n  " + "\n  ".join(failures), file=sys.stderr)
        return 1
    print(
        f"OK: validated {len(rows)} rows across 3 collectors and both modes "
        f"({passing_2x}/{len(rows) * 2} cells <= 2.0x HotSpot)."
    )
    return 0


def check_parity_cell(
    collector: str,
    mode_name: str,
    row: str,
    craton_insns: float,
    parity_ceiling: float,
) -> str | None:
    baseline = DEV_PARITY_BASELINE_INSNS.get((collector, mode_name, row))
    if baseline is None:
        return f"{collector}/{mode_name}/{row}: missing DEV_PARITY_BASELINE_INSNS entry"
    ratio = craton_insns / baseline
    if ratio > parity_ceiling:
        return (
            f"{collector}/{mode_name}/{row}: {craton_insns:.2f} / {baseline:.2f} "
            f"= {ratio:.4f}x > {parity_ceiling:.2f}x parity ceiling"
        )
    return None


def cmd_verify_parity(args: argparse.Namespace) -> int:
    if args.parity_ceiling <= 0:
        print("ERROR: --parity-ceiling must be > 0 in verify-parity mode", file=sys.stderr)
        return 2
    parity_rows = sorted(PARITY_ROWS)
    failures: list[str] = []
    checked: set[tuple[str, str, str]] = set()

    if args.from_table:
        budget_rows = {(r["collector"], r["row"]): r for r in load_budget(Path(args.budget))}
        mode_columns = (("real-jdk", "cvm_real_insns"), ("compatible", "cvm_compat_insns"))
        for gc_name, _ in COLLECTORS:
            for row in parity_rows:
                b = budget_rows.get((gc_name, row))
                if not b:
                    for mode_name, _ in mode_columns:
                        failures.append(f"{gc_name}/{mode_name}/{row}: missing row in {args.budget}")
                    continue
                for mode_name, col_name in mode_columns:
                    cvm_insns = float(b[col_name])
                    baseline = DEV_PARITY_BASELINE_INSNS[(gc_name, mode_name, row)]
                    ratio = cvm_insns / baseline
                    checked.add((gc_name, mode_name, row))
                    err = check_parity_cell(gc_name, mode_name, row, cvm_insns, args.parity_ceiling)
                    if err:
                        failures.append(err)
                    print(
                        f"[{gc_name:<6}] {mode_name:<10s} {row:<11s} "
                        f"CVM={cvm_insns:8.2f} DEV={baseline:8.2f} ({ratio:5.3f}x <= {args.parity_ceiling:.2f}x)",
                        flush=True,
                    )
    else:
        jdk_home = detect_jdk_home(args.java_home)
        with tempfile.TemporaryDirectory(prefix="opcost-parity-") as td:
            cp = Path(td)
            compile_probe(jdk_home, cp)
            verify_checksum_parity(jdk_home, args.exe, str(cp))
            for gc_name, gc_flag in COLLECTORS:
                for row in parity_rows:
                    n = row_ops_n(row, args.scale)
                    checksums: dict[str, tuple[str, str]] = {}
                    for mode_name, mode_flags in MODES:
                        cvm_cmd = [args.exe, *mode_flags, "--java-home", jdk_home, gc_flag, "-Xmx2g"]
                        cvm_insns, cvm_sum = measure_row_median(cvm_cmd, str(cp), row, n, args.reps)
                        checksums[mode_name] = cvm_sum
                        baseline = DEV_PARITY_BASELINE_INSNS[(gc_name, mode_name, row)]
                        ratio = cvm_insns / baseline
                        checked.add((gc_name, mode_name, row))
                        err = check_parity_cell(gc_name, mode_name, row, cvm_insns, args.parity_ceiling)
                        if err:
                            failures.append(err)
                        print(
                            f"[{gc_name:<6}] {mode_name:<10s} {row:<11s} "
                            f"CVM={cvm_insns:8.2f} DEV={baseline:8.2f} ({ratio:5.3f}x <= {args.parity_ceiling:.2f}x)",
                            flush=True,
                        )
                    if len(set(checksums.values())) > 1:
                        failures.append(f"{gc_name}/{row} checksum mismatch across modes: {checksums}")

    missing_cells = sorted(set(DEV_PARITY_BASELINE_INSNS) - checked)
    for gc_name, mode_name, row in missing_cells:
        failures.append(f"{gc_name}/{mode_name}/{row}: parity cell was not evaluated")

    if failures:
        print("OpCostRows parity verification FAILED:\n  " + "\n  ".join(failures), file=sys.stderr)
        return 1
    print(
        f"OK: all {len(checked)} parity cells (3 collectors x 2 modes x {len(parity_rows)} rows) "
        f"<= {args.parity_ceiling:.2f}x DEV_PARITY_BASELINE_INSNS."
    )
    return 0


def cmd_check(args: argparse.Namespace) -> int:
    jdk_home = detect_jdk_home(args.java_home)
    budget_rows = {(r["collector"], r["row"]): r for r in load_budget(Path(args.budget))}
    failures = []
    with tempfile.TemporaryDirectory(prefix="opcost-check-") as td:
        cp = Path(td)
        compile_probe(jdk_home, cp)
        verify_checksum_parity(jdk_home, args.exe, str(cp))
        java = str(Path(jdk_home) / "bin" / "java")
        for gc_name, gc_flag in COLLECTORS:
            hs_cmd = [java, gc_flag, "-Xmx2g"]
            cvm_real_cmd = [args.exe, "--java-home", jdk_home, gc_flag, "-Xmx2g"]
            cvm_comp_cmd = [args.exe, "--compatible", "--java-home", jdk_home, gc_flag, "-Xmx2g"]
            for row in ROWS:
                b = budget_rows.get((gc_name, row))
                if not b:
                    failures.append(f"missing budget row {(gc_name, row)}")
                    continue
                n = int(b["n"])
                hs, hs_sum = measure_row_median(hs_cmd, str(cp), row, n, args.reps)
                cvm_real, real_sum = measure_row_median(cvm_real_cmd, str(cp), row, n, args.reps)
                cvm_comp, comp_sum = measure_row_median(cvm_comp_cmd, str(cp), row, n, args.reps)
                if real_sum != hs_sum:
                    failures.append(f"{gc_name}/{row} real checksum mismatch: {real_sum} != {hs_sum}")
                if comp_sum != hs_sum:
                    failures.append(f"{gc_name}/{row} compat checksum mismatch: {comp_sum} != {hs_sum}")
                b_real = float(b["budget_insns"])
                b_comp = float(b["budget_compat_insns"])
                ref_real = float(b["cvm_real_insns"])
                ref_comp = float(b["cvm_compat_insns"])
                if cvm_real > b_real:
                    failures.append(f"{gc_name}/{row} real: {cvm_real:.2f} > budget {b_real:.2f}")
                if cvm_comp > b_comp:
                    failures.append(f"{gc_name}/{row} compat: {cvm_comp:.2f} > budget {b_comp:.2f}")
                if hs > 0 and (args.require_2x or ref_real <= 2.0 * hs) and cvm_real > 2.0 * hs:
                    failures.append(f"{gc_name}/{row} real: {cvm_real:.2f} > 2.0x HS {hs:.2f}")
                if hs > 0 and (args.require_2x or ref_comp <= 2.0 * hs) and cvm_comp > 2.0 * hs:
                    failures.append(f"{gc_name}/{row} compat: {cvm_comp:.2f} > 2.0x HS {hs:.2f}")
                if args.parity_ceiling > 0 and row in PARITY_ROWS:
                    for mode_name, cvm_insns in (("real-jdk", cvm_real), ("compatible", cvm_comp)):
                        err = check_parity_cell(gc_name, mode_name, row, cvm_insns, args.parity_ceiling)
                        if err:
                            failures.append(err)
                print(
                    f"[{gc_name:<6}] {row:<11s} HS={hs:8.2f} REAL={cvm_real:8.2f} (<= {b_real:8.2f}) "
                    f"COMP={cvm_comp:8.2f} (<= {b_comp:8.2f})",
                    flush=True,
                )
    if failures:
        print("OpCostRows budget check FAILED:\n  " + "\n  ".join(failures), file=sys.stderr)
        return 1
    if args.parity_ceiling > 0:
        print(
            f"OK: all 20 rows within 5% budget, <= 2.0x HotSpot, and parity rows "
            f"<= {args.parity_ceiling:.2f}x dev baseline across all 3 collectors and both modes."
        )
    else:
        print("OK: all 20 rows within 5% budget and <= 2.0x HotSpot across all 3 collectors and both modes.")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument(
        "--mode",
        choices=["calibrate", "check", "validate-table", "verify-parity"],
        required=True,
    )
    ap.add_argument(
        "--exe",
        "--cratonvm",
        dest="exe",
        default=str(ROOT / "target" / "release" / "cratonvm"),
        help="Path to cratonvm binary (aliases: --exe, --cratonvm)",
    )
    ap.add_argument("--java-home", default=None)
    ap.add_argument("--budget", default=str(DEFAULT_BUDGET))
    ap.add_argument("--out", default=str(DEFAULT_BUDGET))
    ap.add_argument("--reps", type=int, default=3)
    ap.add_argument("--scale", choices=["full", "quick"], default="full")
    ap.add_argument(
        "--parity-ceiling",
        type=float,
        default=1.05,
        help="Max allowed CratonVM/dev ratio on PARITY_ROWS (default: 1.05; 0 disables in check mode)",
    )
    ap.add_argument(
        "--from-table",
        action="store_true",
        help="In verify-parity mode, read CratonVM counts from --budget TSV instead of running live perf",
    )
    ap.add_argument(
        "--require-2x",
        action="store_true",
        help="Fail validate-table / check if any residual cell exceeds 2.0x HotSpot",
    )
    args = ap.parse_args()
    if args.reps < 1:
        ap.error("--reps must be >= 1")
    if args.parity_ceiling < 0:
        ap.error("--parity-ceiling must be >= 0")
    if args.mode == "calibrate":
        return cmd_calibrate(args)
    if args.mode == "validate-table":
        return cmd_validate_table(args)
    if args.mode == "verify-parity":
        return cmd_verify_parity(args)
    return cmd_check(args)


if __name__ == "__main__":
    raise SystemExit(main())
