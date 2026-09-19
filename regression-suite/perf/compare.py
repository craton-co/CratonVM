#!/usr/bin/env python3
"""Compare two CratonBench result directories - medians AND tails.

    compare.py <baseline-run-dir> <candidate-run-dir> [options]

A run directory is what `run-cratonbench-gate.sh` writes under
`regression-suite/perf/results/v2/<run-id>/`: `manifest.tsv`, `samples.tsv`,
`summary.tsv`, their JSON twins, and the reliability-gate report.

Three rules make this tool different from eyeballing two medians:

1. **It refuses to render a verdict when either side failed the reliability
   gate**, or was measured with the gate skipped, or has no gate report at
   all. An unreliable measurement's PASS is worth exactly as much as its
   FAIL, and the failure mode this repository has actually suffered is a
   confidently-reported number that nothing could reproduce (BENCHMARK.md's
   retracted HashMap regression). Refusal exits 3 and prints why. `--force`
   exists, prints the refusal anyway, and stamps every line UNRELIABLE.

2. **It reports the tail, not only the median.** A change that leaves p50
   flat and moves p99 by 40% is a real regression that a median-only
   comparison cannot see, and a p50 delta smaller than either side's
   coefficient of variation is reported as INDISTINGUISHABLE rather than as
   a win or a loss.

3. **It says which phases are not evidence about the optimizing tier.** Since
   schema 2 every run records how far into the C2/IR pipeline each phase got,
   and a phase whose `ir_bodies` is 0 in both arms measured the single-pass
   backend only. That is most of CratonBench (MEAS-02), and quoting one of
   those deltas as a C2 result is the mistake this section exists to stop —
   in both directions, "it got faster" and "it did no harm" alike.

Everything is recomputed from the raw per-run samples, never read out of the
summary: the summary is a convenience, `samples.tsv` is the record.

Exit codes:
    0  compared; no phase regressed beyond --tolerance
    1  compared; at least one phase regressed beyond --tolerance
    2  usage / unreadable input
    3  refused - one or both runs are not usable as evidence
"""

from __future__ import annotations

import argparse
import json
import math
import sys
from pathlib import Path

# Percentiles are nearest-rank, ceil(p/100 * n): the same definition used by
# run-cratonbench-gate.sh, reliability-gate.sh/.ps1 and the VM's own G1 pause
# summary, so a p99 printed here means the same thing as a p99 printed there.
PERCENTILES = (50, 90, 99)

# Manifest fields that must match for an A/B to mean anything. The revision
# and the binary hash are deliberately NOT here - differing is the point.
SAME_HOST_KEYS = ("host", "cpu_model", "cpu_pinned", "vm_flags")


class Refusal(Exception):
    """Raised when a run is not usable as evidence."""


def read_tsv_rows(path: Path) -> tuple[list[str], list[dict[str, str]]]:
    """Read a '#'-header TSV into (header, rows)."""
    header: list[str] = []
    rows: list[dict[str, str]] = []
    for line in path.read_text(encoding="utf-8-sig", errors="replace").splitlines():
        if line.startswith("#"):
            if not header:
                header = line.lstrip("#").split("\t")
            continue
        if not line.strip():
            continue
        fields = line.split("\t")
        if len(fields) < 3:
            continue
        rows.append({key: (fields[i] if i < len(fields) else "") for i, key in enumerate(header)})
    return header, rows


def read_manifest(path: Path) -> dict[str, str]:
    manifest: dict[str, str] = {}
    for line in path.read_text(encoding="utf-8-sig", errors="replace").splitlines():
        if line.startswith("#") or "\t" not in line:
            continue
        key, value = line.split("\t", 1)
        manifest.setdefault(key, value)
    return manifest


def percentile(sorted_values: list[float], pct: int) -> float:
    if not sorted_values:
        return float("nan")
    rank = math.ceil(pct * len(sorted_values) / 100.0)
    return sorted_values[max(1, rank) - 1]


class Run:
    """One result directory, with its samples and its reliability verdict."""

    def __init__(self, directory: Path) -> None:
        self.dir = directory
        if not directory.is_dir():
            raise Refusal(f"{directory}: not a directory")

        manifest_path = directory / "manifest.tsv"
        samples_path = directory / "samples.tsv"
        if not manifest_path.exists():
            raise Refusal(f"{directory}: no manifest.tsv - the run has no recorded provenance")
        if not samples_path.exists():
            raise Refusal(
                f"{directory}: no samples.tsv - only a summary was kept, so nothing here can be re-derived"
            )

        self.manifest = read_manifest(manifest_path)
        _, self.samples = read_tsv_rows(samples_path)
        self.reliability = self._read_reliability()

    def _read_reliability(self) -> dict[str, object]:
        # The postflight report is the one that judged the samples; a
        # preflight-only report says the run was allowed to start, not that
        # what came out of it is usable.
        for name in ("reliability-postflight.json", "reliability.json"):
            path = self.dir / name
            if path.exists():
                try:
                    return json.loads(path.read_text(encoding="utf-8"))
                except json.JSONDecodeError as exc:
                    raise Refusal(f"{self.dir}/{name}: unreadable reliability report ({exc})") from exc
        return {}

    @property
    def label(self) -> str:
        return self.manifest.get("run_id", self.dir.name)

    @property
    def revision(self) -> str:
        return self.manifest.get("revision", "-")

    def check_usable(self) -> None:
        if self.manifest.get("reliability_gate") == "skipped":
            raise Refusal(
                f"{self.label}: measured with --skip-reliability; the run was never checked, "
                f"so it cannot support a verdict"
            )
        if not self.reliability:
            raise Refusal(
                f"{self.label}: no reliability-gate report in {self.dir}; run "
                f"`reliability-gate.sh postflight --results {self.dir} --baseline <file>` first"
            )
        mode = self.reliability.get("mode")
        if mode != "postflight":
            raise Refusal(
                f"{self.label}: the only reliability report is a '{mode}' one. Preflight says the "
                f"run was allowed to start, not that its samples are sound"
            )
        if self.reliability.get("status") != "pass":
            failed = [
                f"{check.get('id')}: {check.get('detail')}"
                for check in self.reliability.get("checks", [])
                if check.get("status") == "FAIL"
            ]
            detail = "; ".join(failed) or "no detail recorded"
            raise Refusal(f"{self.label}: FAILED the reliability gate - {detail}")

    def phases(self) -> list[str]:
        seen: list[str] = []
        for row in self.samples:
            phase = row.get("phase", "")
            if phase and phase not in seen:
                seen.append(phase)
        return seen

    def times(self, phase: str) -> list[float]:
        values: list[float] = []
        for row in self.samples:
            if row.get("phase") != phase:
                continue
            raw = row.get("ms", "-")
            try:
                values.append(float(raw))
            except ValueError:
                # A "-" here means the run produced no time. The reliability
                # gate has already refused the run in that case; if we got
                # this far under --force, skipping it is the honest thing.
                continue
        return sorted(values)

    def stats(self, phase: str) -> dict[str, float]:
        values = self.times(phase)
        n = len(values)
        if n == 0:
            return {"n": 0}
        mean = sum(values) / n
        if n > 1:
            variance = sum((v - mean) ** 2 for v in values) / (n - 1)
        else:
            variance = 0.0
        stddev = math.sqrt(variance)
        out: dict[str, float] = {
            "n": float(n),
            "min": values[0],
            "max": values[-1],
            "mean": mean,
            "stddev": stddev,
            "cv": (100.0 * stddev / mean) if mean else 0.0,
        }
        for pct in PERCENTILES:
            out[f"p{pct}"] = percentile(values, pct)
        return out

    def max_of(self, phase: str, column: str) -> float | None:
        best: float | None = None
        for row in self.samples:
            if row.get("phase") != phase:
                continue
            raw = row.get(column, "-")
            try:
                value = float(raw)
            except (TypeError, ValueError):
                continue
            if best is None or value > best:
                best = value
        return best


def delta_pct(base: float, cand: float) -> float:
    if base == 0:
        return float("nan")
    return 100.0 * (cand - base) / base


def compare_phase(
    phase: str,
    base: Run,
    cand: Run,
    tolerance: float,
) -> dict[str, object]:
    a = base.stats(phase)
    b = cand.stats(phase)
    if not a.get("n") or not b.get("n"):
        return {"phase": phase, "verdict": "NO-DATA", "base": a, "cand": b}

    d50 = delta_pct(a["p50"], b["p50"])
    d99 = delta_pct(a["p99"], b["p99"])
    noise = max(a["cv"], b["cv"])

    # A delta smaller than the worse side's run-to-run spread is not a
    # result. Reporting it as one is how a 1.2x "regression" gets bisected
    # for a week before turning out to be the host.
    if d50 > tolerance:
        verdict = "REGRESSION"
    elif abs(d50) <= noise:
        verdict = "INDISTINGUISHABLE"
    elif d50 < -tolerance:
        verdict = "IMPROVED"
    else:
        verdict = "WITHIN-BUDGET"

    tail_note = ""
    if verdict != "REGRESSION" and d99 > max(tolerance, noise) * 2:
        tail_note = "TAIL-REGRESSION"

    return {
        "phase": phase,
        "verdict": verdict,
        "tail_note": tail_note,
        "delta_p50_pct": d50,
        "delta_p99_pct": d99,
        "noise_pct": noise,
        "base": a,
        "cand": b,
        "base_peak_rss_kb": base.max_of(phase, "peak_rss_kb"),
        "cand_peak_rss_kb": cand.max_of(phase, "peak_rss_kb"),
        "base_compiles_c2": base.max_of(phase, "compiles_c2"),
        "cand_compiles_c2": cand.max_of(phase, "compiles_c2"),
        # MEAS-02. `compiles_c2` counts compiles whose requested TIER was C2 —
        # including every one the optimizing pipeline declined and handed back
        # to the single-pass backend — so it can be non-zero on a phase where
        # the optimizing backend emitted nothing at all. `ir_bodies` is the
        # count that cannot be read that way: it is incremented at the point
        # the optimizing backend produces the body. `None` on a results
        # directory written before schema 2, which is not the same as 0.
        "base_ir_admitted": base.max_of(phase, "ir_admitted"),
        "cand_ir_admitted": cand.max_of(phase, "ir_admitted"),
        "base_ir_bodies": base.max_of(phase, "ir_bodies"),
        "cand_ir_bodies": cand.max_of(phase, "ir_bodies"),
        "base_gc_p99_us": base.max_of(phase, "gc_young_p99_us"),
        "cand_gc_p99_us": cand.max_of(phase, "gc_young_p99_us"),
    }


def format_table(results: list[dict[str, object]], unreliable: bool) -> str:
    head = (
        f"{'phase':<14}{'n':>4}{'p50 A':>10}{'p50 B':>10}{'d p50':>9}"
        f"{'p99 A':>10}{'p99 B':>10}{'d p99':>9}{'CV A':>7}{'CV B':>7}  verdict"
    )
    lines = [head, "-" * len(head)]
    for r in results:
        if r["verdict"] == "NO-DATA":
            lines.append(f"{r['phase']:<14}{'-':>4}{'':>55}  NO-DATA")
            continue
        a = r["base"]
        b = r["cand"]
        verdict = str(r["verdict"])
        if r["tail_note"]:
            verdict = f"{verdict} + {r['tail_note']}"
        if unreliable:
            verdict = f"UNRELIABLE({verdict})"
        lines.append(
            f"{r['phase']:<14}"
            f"{int(min(a['n'], b['n'])):>4}"
            f"{a['p50']:>10.0f}{b['p50']:>10.0f}{r['delta_p50_pct']:>+8.1f}%"
            f"{a['p99']:>10.0f}{b['p99']:>10.0f}{r['delta_p99_pct']:>+8.1f}%"
            f"{a['cv']:>6.1f}%{b['cv']:>6.1f}%  {verdict}"
        )
    return "\n".join(lines)


def _num(value: float | None) -> str:
    if value is None:
        return "-"
    if float(value).is_integer():
        return str(int(value))
    return f"{value:.2f}"


def format_secondary(results: list[dict[str, object]]) -> str:
    rows = []
    for r in results:
        if r["verdict"] == "NO-DATA":
            continue
        parts = []
        for label, key_a, key_b, unit in (
            ("peak RSS", "base_peak_rss_kb", "cand_peak_rss_kb", "kB"),
            ("C2-tier compiles", "base_compiles_c2", "cand_compiles_c2", ""),
            ("C2 admitted", "base_ir_admitted", "cand_ir_admitted", ""),
            ("C2 bodies", "base_ir_bodies", "cand_ir_bodies", ""),
            ("GC young p99", "base_gc_p99_us", "cand_gc_p99_us", "us"),
        ):
            a, b = r[key_a], r[key_b]
            if a is None and b is None:
                continue
            parts.append(f"{label} {_num(a)} -> {_num(b)}{unit}")
        if parts:
            rows.append(f"  {r['phase']:<12} " + "; ".join(parts))
    if not rows:
        return ""
    return "Secondary metrics (per-phase maxima across runs; '-' = the VM reported none):\n" + "\n".join(rows)


def format_reach_caveat(results: list[dict[str, object]]) -> str:
    """MEAS-02: name the phases whose delta says nothing about the C2 tier.

    A CratonBench delta has repeatedly been quoted as evidence about the
    optimizing tier on phases where that tier produced no code, because
    nothing in the output said which phases those were. This says it.

    Absent (schema < 2) is reported separately from a measured zero: "the run
    did not record reach" and "the tier produced nothing" license completely
    different follow-ups, and collapsing them is how the first one gets read
    as the second.
    """
    unreached, unmeasured = [], []
    for r in results:
        if r["verdict"] == "NO-DATA":
            continue
        a, b = r["base_ir_bodies"], r["cand_ir_bodies"]
        if a is None or b is None:
            unmeasured.append(str(r["phase"]))
        elif a == 0 and b == 0:
            unreached.append(str(r["phase"]))
    out = []
    if unreached:
        out.append(
            "C2 reach (MEAS-02): the optimizing backend produced NO body in either arm for:\n"
            f"  {', '.join(unreached)}\n"
            "  Those deltas measure the single-pass backend. They are not evidence about the\n"
            "  optimizing tier in either direction — including 'the C2 change did no harm'."
        )
    if unmeasured:
        out.append(
            "C2 reach (MEAS-02): NOT RECORDED for "
            f"{', '.join(unmeasured)} — a pre-schema-2 results directory, or --no-vm-stats.\n"
            "  Unrecorded is not zero. Re-measure before quoting any of it about C2."
        )
    return "\n\n".join(out)


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Compare two CratonBench result directories (median and tail).",
        epilog="A verdict is refused unless BOTH runs passed the reliability gate.",
    )
    parser.add_argument("baseline", type=Path, help="result directory measured first (the A arm)")
    parser.add_argument("candidate", type=Path, help="result directory to judge (the B arm)")
    parser.add_argument("--phases", default="", help="comma-separated subset (default: phases present in both)")
    parser.add_argument(
        "--tolerance",
        type=float,
        default=5.0,
        help="percent over the A arm's median that counts as a regression (default 5)",
    )
    parser.add_argument("--json", action="store_true", help="emit machine-readable JSON instead of a table")
    parser.add_argument(
        "--force",
        action="store_true",
        help="compare anyway when a run failed the reliability gate; every verdict is stamped UNRELIABLE",
    )
    parser.add_argument(
        "--allow-different-host",
        action="store_true",
        help="do not refuse when the two runs were measured on different hosts/CPUs/flags",
    )
    args = parser.parse_args()

    try:
        base = Run(args.baseline)
        cand = Run(args.candidate)
    except Refusal as exc:
        print(f"REFUSED: {exc}", file=sys.stderr)
        return 3

    refusals: list[str] = []
    for run in (base, cand):
        try:
            run.check_usable()
        except Refusal as exc:
            refusals.append(str(exc))

    if not args.allow_different_host:
        for key in SAME_HOST_KEYS:
            a = base.manifest.get(key, "-")
            b = cand.manifest.get(key, "-")
            if a != b:
                refusals.append(
                    f"{key} differs between the two runs ({a!r} vs {b!r}). The protocol is same-host, "
                    f"same-flags A/B; absolute times are not comparable across hosts or dates. "
                    f"Pass --allow-different-host if you really mean to"
                )

    if refusals:
        for line in refusals:
            print(f"REFUSED: {line}", file=sys.stderr)
        if not args.force:
            print(
                "No verdict. See docs/benchmarking/reliability-gate.md; re-measure, do not re-interpret.",
                file=sys.stderr,
            )
            return 3
        print("--force given: comparing anyway. Nothing below is evidence.", file=sys.stderr)

    if args.phases:
        wanted = [p for p in args.phases.split(",") if p]
    else:
        wanted = [p for p in base.phases() if p in cand.phases()]
    if not wanted:
        print("REFUSED: the two runs have no phase in common", file=sys.stderr)
        return 3

    results = [compare_phase(p, base, cand, args.tolerance) for p in wanted]
    regressed = [r for r in results if r["verdict"] == "REGRESSION" or r["tail_note"]]

    if args.json:
        print(
            json.dumps(
                {
                    "schema_version": 1,
                    "baseline": {"run_id": base.label, "revision": base.revision, "dir": str(base.dir)},
                    "candidate": {"run_id": cand.label, "revision": cand.revision, "dir": str(cand.dir)},
                    "tolerance_pct": args.tolerance,
                    "reliable": not refusals,
                    "phases": results,
                },
                indent=2,
                default=lambda o: None,
            )
        )
    else:
        print(f"A (baseline):  {base.label}  rev {base.revision[:12]}  {base.dir}")
        print(f"B (candidate): {cand.label}  rev {cand.revision[:12]}  {cand.dir}")
        print(f"host {base.manifest.get('host', '-')}  cpu {base.manifest.get('cpu_pinned', '-')}"
              f"  flags {base.manifest.get('vm_flags', '-')}  tolerance {args.tolerance}%")
        print()
        print(format_table(results, unreliable=bool(refusals)))
        print()
        print("d p50 / d p99 are B relative to A; positive is slower. INDISTINGUISHABLE means the")
        print("delta is inside the worse arm's own run-to-run spread (CV), so it is not a result.")
        secondary = format_secondary(results)
        if secondary:
            print()
            print(secondary)
        caveat = format_reach_caveat(results)
        if caveat:
            print()
            print(caveat)

    if refusals:
        return 3
    return 1 if regressed else 0


if __name__ == "__main__":
    sys.exit(main())
