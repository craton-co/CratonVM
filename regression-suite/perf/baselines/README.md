# Perf baselines — and the placeholder problem

A baseline in this directory (or the legacy TSV one level up) is a **regression
threshold**. The perf gate multiplies it by `1 + tolerance` and fails any phase
whose median exceeds the result. That makes an unmeasured baseline actively
dangerous, in two directions:

- **A zero baseline makes the budget zero.** Depending on which way the
  comparison is written, the gate then either fails every run forever (and gets
  disabled by whoever is unblocking CI that week) or — with a `>=` — passes
  everything forever while still printing a verdict. Both outcomes look like a
  working gate from the outside.
- **A copied-from-somewhere-else baseline is worse than none.** It produces
  confident PASS/FAIL lines about a number nobody measured on this host. That is
  the exact failure this repository has already had: `BENCHMARK.md` records a
  **retracted** HashMap regression (22,077 ms, "21.2x", "CONFIRMED and bounded
  to `a36b9d121..e57f0bc7d`") that does not reproduce, and a String/Regex row
  from the same session that has never been re-measured.

So: **a baseline that has not been measured under the protocol must be marked as
a placeholder, and the reliability gate must refuse to use it.** Not "documented
as provisional in a comment" — marked in a field a script reads.

## The marker

Two file dialects are accepted; both are understood by `reliability-gate.sh`,
`reliability-gate.ps1` and `run-cratonbench-gate.sh`.

### JSON (preferred for anything new)

```json
{
  "schema_version": 1,
  "host": "azure-epyc-9v45",
  "phases": {
    "sieve": {
      "baseline_ms": 2567,
      "checksum": "9592",
      "status": "anchored",
      "placeholder": false,
      "evidence": "docs/benchmarking/... (or an evidence doc path)",
      "measured_utc": "2026-07-31T09:00:00Z",
      "run_id": "20260731T090000Z-bench1-9ac1feffe000"
    },
    "stringregex": {
      "baseline_ms": 0,
      "checksum": "",
      "status": "placeholder",
      "placeholder": true,
      "evidence": "NOT MEASURED under docs/benchmarking/methodology.md"
    }
  }
}
```

`"placeholder": true` is the machine-readable marker. Copy
[`TEMPLATE.json`](TEMPLATE.json) to start a new host's baseline: every phase in
it is already marked `"placeholder": true`, so a half-finished baseline refuses
to gate anything instead of silently gating on zeros.

### TSV (the legacy format, still used by
[`../cratonbench-baseline-azure-epyc.tsv`](../cratonbench-baseline-azure-epyc.tsv))

```
# phase	baseline_ms	checksum	status	evidence
sieve	2700	9592	provisional	<evidence>
stringregex	0		placeholder	NOT MEASURED
```

A TSV row is treated as a placeholder when **any** of these hold:

| condition | why |
|---|---|
| `status` is exactly `placeholder` (or contains `placeholder=true`) | explicit marker |
| `baseline_ms` is empty or non-numeric | it is a placeholder in all but name |
| `baseline_ms` is `0` or negative | the budget collapses to zero |
| `checksum` is empty or `-` | without a reference checksum a run can only prove it agreed with itself |

Any of those makes the reliability gate exit **11** with
`RELIABILITY-FAIL[BASELINE-PLACEHOLDER]` or `[BASELINE-CHECKSUM]`, and no perf
verdict is printed.

## Status values

| status | meaning | how it may be changed |
|---|---|---|
| `placeholder` | never measured under the protocol. Refused as a threshold. | measure it (below) |
| `provisional` | measured, but not with a linked evidence document | a normal PR justification |
| `anchored` | measured under the protocol with a linked evidence document | a new evidence document |

`status` is documentation *in addition to* the placeholder marker, not instead
of it: only `placeholder` (and the degenerate-value rules above) stop the gate.

## Recording a real baseline

The full protocol is in
[`docs/benchmarking/methodology.md`](../../../docs/benchmarking/methodology.md);
the short version:

```bash
# Quiet host, single pinned core, >= 7 reps, checksums verified every run.
bash regression-suite/perf/run-cratonbench-gate.sh \
    -Exe /abs/path/to/cratonvm --calibrate --reps 7 --results-dir /tmp/cal
```

`--calibrate` skips the baseline-threshold checks (you are creating the
baseline, so it cannot yet be valid) but keeps every other reliability check —
checksum agreement between runs, load, pin, CPU migration, thermal, variance,
sample count, manifest completeness. **If the calibration run is refused, the
numbers it printed are not a baseline.** It exits non-zero and says so.

Then copy the printed body into the baseline file, and record in the
`evidence` field: the `run_id` of the calibration run, the revision, and the
path to the results directory it wrote. A baseline whose evidence field does
not let a reader find the run that produced it is one retraction away from
being another placeholder.

## Current state of the checked-in baselines

`../cratonbench-baseline-azure-epyc.tsv` (2026-07-31): **no row has a zero or
missing baseline**, so no row needed the in-place placeholder marking. Six rows
are `provisional` and one (`bintrees`) is `anchored`. None of them has yet been
re-measured under the reliability gate introduced with this directory — see the
"unverified" statement in
[`docs/benchmarking/methodology.md`](../../../docs/benchmarking/methodology.md).
