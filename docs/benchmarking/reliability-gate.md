# The benchmark reliability gate

`regression-suite/perf/reliability-gate.sh` (and its Windows twin
`reliability-gate.ps1`) answers a different question from the perf gate.

- The **perf gate** asks: *did the median regress?*
- The **reliability gate** asks: *is this measurement capable of answering that
  question at all?*

It runs twice around every gated measurement — **preflight** before anything is
measured, **postflight** before any verdict is printed — and refuses runs whose
result would not mean anything. A refused run's PASS is worth exactly as much as
its FAIL, so a refusal wins the exit code.

Why it exists: [`BENCHMARK.md`](../../BENCHMARK.md) carries a **retracted**
HashMap regression (`22,077 ms`, `21.2x`, "CONFIRMED and bounded to
`a36b9d121..e57f0bc7d`") that does not reproduce and whose cause was never
found, plus a String/Regex row from the same session that has never been
re-measured. Both passed every check the perf gate had, because the perf gate
had no check that could see them. See
[`methodology.md`](methodology.md) for the protocol this enforces.

---

## Using it

Normally you do not: `run-cratonbench-gate.sh` calls it for you, passing the
same `--max-load`, `--min-samples`, `--max-cv` and `--max-freq-drift` it was
given, and exits with the gate's code if it refuses.

Standalone:

```bash
# Is this baseline file usable as a regression threshold at all?
bash regression-suite/perf/reliability-gate.sh check-baseline \
    --baseline regression-suite/perf/cratonbench-baseline-azure-epyc.tsv

# Judge an existing results directory after the fact.
bash regression-suite/perf/reliability-gate.sh postflight \
    --results regression-suite/perf/results/v2/<run-id> \
    --baseline regression-suite/perf/cratonbench-baseline-azure-epyc.tsv
```

```powershell
# Same checks, same exit codes, same check IDs, on a Windows checkout.
powershell -File regression-suite\perf\reliability-gate.ps1 `
    -Mode postflight -Results results\v1\<run-id> `
    -Baseline regression-suite\perf\cratonbench-baseline-azure-epyc.tsv
```

Both write `reliability-<mode>.tsv`, `reliability-<mode>.json` and
`reliability.json` into the results directory. `compare.py` reads
`reliability.json` and refuses to compare runs that do not say `"status":
"pass"` from a `postflight` report.

Every failure prints one line of the form:

```
RELIABILITY-FAIL[<CHECK-ID>]: <what was wrong, and why it invalidates the run>
```

All failing checks are reported, not just the first — a gate that stops at the
first problem trains people to fix one thing, re-measure for ninety minutes,
and find the next.

## Exit codes

| Code | Meaning | Check IDs |
|---:|---|---|
| 0 | every check passed (warnings may still have printed) | — |
| 2 | usage / setup error | — |
| 3 | host too loaded or contended to measure | `HOST-LOAD`, `HOST-CONTENTION` |
| 10 | checksum drift, checksum != reference, or a run that did not complete | `CHECKSUM-DRIFT`, `CHECKSUM-REFERENCE`, `SAMPLE-EXIT` |
| 11 | placeholder / zero / missing baseline used as a threshold | `BASELINE-MISSING`, `BASELINE-PLACEHOLDER`, `BASELINE-CHECKSUM` |
| 12 | too few samples | `SAMPLE-COUNT` |
| 13 | measurement instability | `CPU-PIN`, `CPU-MIGRATION`, `CPU-THERMAL`, `CPU-FREQ`, `RUN-VARIANCE` |
| 14 | missing or incomplete environment manifest | `MANIFEST-MISSING`, `MANIFEST-FIELD`, `SUMMARY-INTEGRITY` |

`run-cratonbench-gate.sh` passes these through unchanged, so its own exit codes
(`0` pass, `1` perf regression, `2` usage, `3` load) are joined by
`10`–`14`. `compare.py` uses `3` for "refused".

## The checks

### `HOST-LOAD` — exit 3

**Rejects:** a 1-minute load average above the ceiling at the start of the run,
**or in any per-sample reading taken during it**. On Windows the equivalent
source is the `\System\Processor Queue Length` counter — threads waiting for a
core, which is the property that actually matters, since Windows has no load
average.

**Why:** a contended host inflates both arms of a comparison, and it inflates
memory-heavy rows more than others, so it produces garbage *passes* as well as
garbage *failures*. Checking only at the start is not enough: a build that
starts five minutes into a nine-run series moves the tail and leaves the opening
reading looking fine.

### `HOST-CONTENTION` — exit 3

**Rejects:** another `CratonBench` process already running.

**Why:** `BENCHMARK.md`'s own methodology warning — concurrent sessions on the
shared bench host pin their benchmark to the same default CPU. Two benchmarks
then timeshare one core while every other core is idle. The load average does
not move, so `HOST-LOAD` cannot see it. The only reliable signal is the
competitor's existence.

### `BASELINE-PLACEHOLDER`, `BASELINE-MISSING`, `BASELINE-CHECKSUM` — exit 11

**Rejects:** using as a regression threshold a baseline that is missing, has no
row for the phase, is marked `"placeholder": true` (JSON) or `status =
placeholder` (TSV), has an empty / non-numeric / zero / negative `baseline_ms`,
or has no reference checksum.

**Why:** a zero baseline makes the budget zero, so the gate either fails
everything forever (and gets switched off) or passes everything forever — while
still printing a verdict either way. A placeholder that is only *documented* as
provisional in a comment is not a marker; the gate needs a field it can read.
Format and status rules:
[`baselines/README.md`](../../regression-suite/perf/baselines/README.md).

Skipped (with a warning) under `--calibrate`, because a calibration run is
creating the baseline and cannot be gated by it. Every other check still runs.

### `SAMPLE-COUNT` — exit 12

**Rejects:** fewer than `--min-samples` (default 7) samples for any measured
phase. In **preflight** it also rejects a `--reps` below that value, before any
measuring happens.

**Why:** a median of three runs is a draw, not a median, and the tail
percentiles this gate reports need enough samples to exist. Rejecting at
preflight matters as much as rejecting at postflight: the alternative is
discovering after ninety minutes of bench-host time that the run could never
have counted.

### `CHECKSUM-DRIFT`, `CHECKSUM-REFERENCE` — exit 10

**Rejects:** two runs of one phase producing different checksums; or all runs
agreeing with each other but not with the reference checksum recorded in the
baseline.

**Why:** a faster wrong answer is a bug, not a result. Drift *between runs of
the same binary* is worse than a regression — it means the run is
non-deterministic, so no time from it means anything. And agreement without a
reference only proves the binary agreed with itself.

### `SAMPLE-EXIT` — exit 10

**Rejects:** any recorded run whose exit code was not 0, including one killed by
`--run-timeout`.

**Why:** a crashed or hung run must never contribute a time, and it must never
be silently dropped either — the sample row is written first, with its exit
code, precisely so that a series which went wrong cannot be made to look clean
by discarding the runs that went wrong. (A "TIMEOUT" in this repository is very
often a SIGSEGV with no result line; recording it as a distinct outcome keeps
that visible.)

### `CPU-PIN`, `CPU-MIGRATION` — exit 13

**Rejects:** a run with no single pinned CPU recorded; a run where the sampled
`/proc/<pid>/stat` processor field showed the process on a CPU other than the
pinned one, or on more than one CPU. When the observed CPU could not be sampled
at all, the run is rejected *unless* the recorded affinity mask is the single
pinned CPU, which makes migration impossible.

**Why:** cross-core migration changes cache locality and frequency behaviour
mid-measurement, and a pin that silently failed produces output identical to one
that worked. "Which core did it really run on" is exactly the question the
retracted HashMap result could not answer after the fact — the pin-collision
hypothesis had to be re-tested from scratch a week later.

### `CPU-THERMAL` — exit 13

**Rejects:** any increase in the pinned core's
`thermal_throttle/core_throttle_count` during a run.

**Why:** a thermally-limited core is not the core the baseline was measured on.
Throttling is invisible in wall-clock terms except as an unexplained slow run,
which is indistinguishable from a regression.

### `CPU-FREQ` — exit 13

**Rejects:** a within-run spread of the pinned core's `scaling_cur_freq` greater
than `--max-freq-drift` (default 20%).

**Why:** boost and thermal drift of that size is larger than most regressions
this gate is asked to detect, so a "5% regression" measured across a frequency
excursion is not a measurement of the change. When cpufreq sysfs is not readable
(common in containers) this is a **warning**, not a failure, and the run is
recorded as having an unverified frequency — pass `--require-freq-data` to make
it fatal.

### `RUN-VARIANCE` — exit 13

**Rejects:** a coefficient of variation above `--max-cv` (default 5%) for any
phase.

**Why:** high CV is the observable symptom of every instability the individual
checks above can miss — co-tenancy, frequency, thermal, background I/O. A median
drawn from a 10%-CV series cannot resolve a 5% regression, so producing a
verdict from it is guessing with a table attached.

### `MANIFEST-MISSING`, `MANIFEST-FIELD` — exit 14

**Rejects:** no `manifest.tsv`, or any required field missing, empty or
recorded as `-`: schema version, run id, timestamp, host, CPU model, pinned
CPU, revision, binary path, binary SHA-256, VM flags, command line, JDK version,
baseline file, reps, starting load.

**Why:** a number whose provenance was not recorded cannot be re-derived later.
`-` counts as absent on purpose: a field the runner could not read is exactly
the field a later reader would otherwise assume had been checked.

### `SUMMARY-INTEGRITY` — exit 14

**Rejects:** a missing `samples.tsv`; a `summary.tsv` whose per-phase `n` or
`p50` cannot be rederived from the raw samples; a summary row for a phase with
no samples.

**Why:** the raw samples are the record and the summary is a convenience. A
summary that does not follow from its samples means either a recording bug or a
hand-edited result, and in both cases the distribution is fiction. This is also
what catches "we only kept the median" — the failure mode that made the
retracted HashMap series impossible to re-examine.

## Warnings vs failures

A **warning** means a check could not run on this host (no `taskset`, no
`pgrep`, no cpufreq sysfs, a BOM in a hand-edited samples file). Warnings are
printed, counted, and recorded in `reliability.json` so that a reader can see
*which* guarantees a given run actually has. They do not fail the run — except
where a flag (`--require-freq-data`) says they should.

The distinction matters: a gate that silently skips a check it cannot perform
is indistinguishable from a gate that performed it and passed.

## Deliberately weakening the gate

- `--min-samples N` — accept fewer samples. Say so in the write-up.
- `--max-load`, `--max-cv`, `--max-freq-drift` — move a ceiling. Say which, and
  why, in the write-up.
- `--calibrate` — skip only the baseline-threshold checks while recording a new
  baseline.
- `run-cratonbench-gate.sh --skip-reliability` — measure with no gate at all.
  The manifest records `reliability_gate=skipped`, and `compare.py` refuses to
  render a verdict from such a run. Debugging only.

There is deliberately **no** flag that turns off the checksum comparison.
