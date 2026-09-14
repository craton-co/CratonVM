# CratonVM benchmarking methodology

The protocol every performance number in this repository is supposed to come
from, and the checks that now enforce it.

The one-line version: **a benchmark result is a claim about a measurement, and
this repository has published claims that turned out not to be about any
measurement anyone could repeat.** Everything below exists because of that, not
as ceremony.

- Harness: [`bench/CratonBench.java`](../../bench/CratonBench.java)
- Gate: [`regression-suite/perf/run-cratonbench-gate.sh`](../../regression-suite/perf/run-cratonbench-gate.sh)
- Reliability gate: [`reliability-gate.md`](reliability-gate.md)
- Comparison: [`regression-suite/perf/compare.py`](../../regression-suite/perf/compare.py)
- Baselines and the placeholder marker: [`regression-suite/perf/baselines/README.md`](../../regression-suite/perf/baselines/README.md)

---

## 1. Status of the published numbers

> All seven CPU rows come from ONE
> interleaved series on `dev` @ `ded183df8` — §2's isolation, pinning,
> alternating-arm and no-discard rules, 9 pairs per phase, checksum verified
> against HotSpot on every run, zero mismatches. The window opened only once
> the 1-minute load fell below 2.5 **and** no other `cratonvm` process was
> pinned to the measuring core; §2.3's co-pinned-competitor check is the one a
> load average cannot make for you. Load ran 1.9–3.7 across the series.
>
> This is the first set that is both re-measured under the protocol and taken
> in a quiet window. It is still not `reliability-gate.sh`-certified — that
> gate's ceiling is 2.0 and the series drifted above it — so ratios remain the
> durable content and absolutes are this host on this day.

| Row | Ratio vs JDK 25 C2 | CratonVM CV | earlier | Status |
|---|---|---|---|---|
| Arithmetic (2B ops) | 1.95x | 0.2% | 2.44x | re-measured in a quiet window |
| Fibonacci(44) | 5.89x | 3.5% | 2.79x | re-measured; distance from the July figure is unattributed and is **not** `cov-*` (checked against a control) |
| Sieve (100K x 20,000) | **0.99x** | 2.3% | 2.28x | re-measured — **parity**; an earlier 6.50x reading was a live regression, since fixed |
| Matrix 1280x1280 | **0.99x** | 0.2% | 2.93x | re-measured — parity with C2 |
| HashMap (10M put/get) | 2.07x | 0.8% | 1.75x | re-measured; the July row this replaced had itself replaced a RETRACTED one |
| String/Regex (100K) | 5.37x | 1.1% | 7.7x | re-measured — the first quiet-window number for this row since the session that produced 7.7x was discredited |
| Binary Trees (depth 18) | 9.46x | 0.4% | 8.34x | re-measured; CV fell from 7.6% to 0.4% once the window was quiet |

**One row's HotSpot arm is bimodal and must not be re-derived from a single
run.** On Sieve, HotSpot lands either at ~2,369 ms or ~2,739 ms with nothing
between; a 9-sample median reports whichever mode won. Two consecutive series
on an unchanged binary read 2,386 ms and 2,734 ms. The table's figure is the
median of 18 pooled samples. CratonVM's own samples on that phase are
unimodal. See BENCHMARK.md.

The two rows below are why this section existedThe two rows below are why this section existed, and both are now superseded by
the series above. They are kept because the *failure modes* they record are the
reason every check in the reliability gate exists:

- **The retracted HashMap regression.** `BENCHMARK.md` used to record
  `22,077 ms` / `21.2x`, described as "CONFIRMED and bounded to
  `a36b9d121..e57f0bc7d`". Re-run from scratch it **did not
  reproduce**: the same phase on the same tree measured 3,523 ms / 3.53x at
  n=10M, and 427 ms at n=1M against a recorded 3,084 ms. The bisect range it
  recommended has nothing in it. **Why the original readings were ~5x too slow
  was never established** — the obvious candidate (a CPU-13 pin collision with
  another session) was tested directly and showed no difference. Detail:
  `hashmap-half-gap-20260730.md`.
- **The un-re-measured String/Regex row.** The `7.7x` figure (from a `3.57x`
  predecessor) comes from the *same session* as the retracted
  HashMap number. It has never been re-measured. It survives in the table only
  because it was the smaller of the two claims, which is not a reason to
  believe it.

The lesson those two encode: **the measurement, not the change, was the thing
that went wrong.** Every check in the reliability gate is aimed at a way that
can happen silently.

## 2. The protocol

### 2.1 Correctness first

Every phase prints a checksum. **Compare checksums before comparing times, on
every single run, not once at the start.** A faster wrong answer is a bug, not
a result. A checksum that differs *between runs of the same binary* is worse
than a regression: it means the run is not deterministic and no time from it
means anything.

The reference checksum lives in the baseline file, next to the number it
anchors. A run is only allowed to compare its times against a baseline whose
checksum it matched.

### 2.2 Isolation

- **One phase per process.** Fresh process for every measurement. All-phase
  in-process runs carry JIT and GC state across phases and are only comparable
  to other all-phase runs.
- **Identical flags in both arms**, `-Xmx8g` for the gated set. The flags are
  recorded in the run manifest so an A/B that silently differs can be caught
  instead of argued about.
- **Same host, same day, same binary pair.** Absolute times are not comparable
  across hosts or across re-provisionings; ratios between two binaries measured
  in one alternating series are. `compare.py` refuses by default when the two
  runs differ in host, CPU model, pinned CPU or VM flags.

### 2.3 Pinning

- Pin to **one logical CPU** (`taskset -c N`). The pin is recorded, and the CPU
  the process *actually ran on* is sampled from `/proc/<pid>/stat` while it
  runs — a pin that silently failed and a process that migrated look identical
  in the output otherwise.
- **Check for a co-pinned competitor before measuring.** On the shared bench
  host, concurrent sessions run their own CratonBench pinned to the same
  default CPU. Two benchmarks then timeshare one core while `mpstat` shows
  fourteen idle ones, and the load average never moves. The gate looks for a
  running `CratonBench` and refuses if it finds one.
- Note the standing caveat: pinning to one CPU makes
  `available_parallelism()` return 1, so **the parallel young GC is disabled in
  every gated run**. A change that only helps multi-core GC reads as flat here.
  Measure those unpinned or against a CPU *set*, and say which you did.

### 2.4 Sampling

- **At least 7 runs per phase**, alternating arms when comparing two binaries
  (A B A B ...), never all of A then all of B — that confounds the arm with any
  drift in the host over the measurement window.
- **No sample is discarded.** Not the slowest, not the "warm-up", not the one
  that looked wrong. Every raw sample is written to `samples.tsv` and kept.
- Report the **distribution**, not the median alone: p50, p90, p99, min, max
  and the coefficient of variation. A change that leaves p50 flat and moves p99
  by 40% is a real regression a median cannot see; and a delta smaller than the
  arm's own CV is not a result at all — `compare.py` labels it
  `INDISTINGUISHABLE` rather than reporting a direction.

### 2.5 Environment

Every run records a manifest: revision (and whether the tree was dirty), binary
SHA-256 and mtime, exact command line, VM flags, JDK version, host, kernel, CPU
model, pinned CPU, baseline file and its hash, load average at start and end,
and the per-run checksum. **A run without that manifest is refused**, because
a number whose provenance was not recorded cannot be re-derived later — which
is precisely the position the retracted HashMap result left everyone in.

Build with **fat LTO** for anything baseline-comparable
(`[profile.release]` is `lto = "fat"`, `codegen-units = 1`).
`CARGO_PROFILE_RELEASE_LTO=off` produces a materially slower binary; it is fine
for relative A/B between two binaries built the same way, and never
citable against a baseline.

## 3. Running it

```bash
# The gated regression check. Refuses to measure on a loaded host, refuses to
# report a verdict on an unreliable measurement.
bash regression-suite/perf/run-cratonbench-gate.sh -Exe /abs/path/to/cratonvm

# One phase, more reps, results in a named directory.
bash regression-suite/perf/run-cratonbench-gate.sh \
    -Exe /abs/path/to/cratonvm --phases hashmap --reps 9 \
    --results-dir /tmp/hashmap-after
```

Each run writes, under `regression-suite/perf/results/v2/<run-id>/`:

| File | Contents |
|---|---|
| `manifest.tsv` / `manifest.json` | the environment manifest above, plus one `ir_reach_<phase>` line per phase |
| `samples.tsv` / `samples.json` | **every raw sample**: ms, checksum, observed CPU, load, throttle delta, frequency min/max, peak RSS, C1/C2/OSR compile counts, deopts, GC young pause count/p50/p99/max, minor/major GC counts, exit code, optimizing-tier requests/admitted/bodies |
| `summary.tsv` / `summary.json` | per phase: n, min, p50, p90, p99, max, mean, stddev, CV, checksum, baseline, budget, verdict, plus the per-phase maxima of the secondary metrics |
| `reliability-preflight.*`, `reliability-postflight.*`, `reliability.json` | the reliability gate's decision and every check it ran |

Percentiles are **nearest-rank** (`ceil(p/100 x n)`) everywhere — the runner,
both reliability gates, `compare.py`, and the VM's own G1 pause summary — so a
p99 from one means the same as a p99 from another.

### What a phase's numbers are evidence *about*

Schema 2 added the **optimizing tier's reach**: per phase, how
many compile requests reached the admission chain, how many were admitted to
the optimizing (C2/IR) pipeline, and how many bodies that pipeline actually
produced. It is in `samples.tsv`/`summary.tsv` as `ir_requests` /
`ir_admitted` / `ir_bodies`, and in `manifest.tsv` as one `ir_reach_<phase>`
line per phase.

Read it before quoting a delta as evidence about the JIT. On the seven
CratonBench phases the tier produces **three** bodies in total, so almost every
phase's number is a measurement of the single-pass backend and says nothing
about C2 in either direction — including "the C2 change did no harm". That is
`docs/known-issues/c2/`'s MEAS-02, and the reach record exists so the fact
travels with the numbers instead of having to be rediscovered.

`compiles_c2` is a different column and is not a substitute: it counts
compiles whose requested *tier* was C2, including every one the optimizing
pipeline declined and handed to the single-pass backend. A phase can report
`compiles_c2` above zero with `ir_bodies` at zero.

Comparing two runs:

```bash
python3 regression-suite/perf/compare.py \
    regression-suite/perf/results/v2/<before-run-id> \
    regression-suite/perf/results/v2/<after-run-id>
```

`compare.py` reports p50 **and** p99 deltas with both arms' CV, and **refuses
to render a verdict** if either run failed the reliability gate, was measured
with it skipped, or has no gate report.

## 4. Recording a new baseline legitimately

1. **Quiet host.** 1-minute load below the ceiling (default 2.0), no other
   `CratonBench` running, nothing else pinned to your core. Verify, don't
   assume: `pgrep -f CratonBench` and `taskset -cp <pid>` on anything it finds.
2. **Build fat-LTO** at the exact revision you intend to anchor.
3. **Calibrate with the gate**, which keeps every reliability check except the
   baseline-threshold one (you are creating the threshold, so it cannot yet be
   valid):

   ```bash
   bash regression-suite/perf/run-cratonbench-gate.sh \
       -Exe /abs/path/to/cratonvm --calibrate --reps 7 \
       --results-dir /tmp/calibrate-$(date -u +%Y%m%dT%H%M%SZ)
   ```

4. **If the calibration run is refused, it is not a baseline.** The script exits
   non-zero and says which check failed. Fix the host, or the pin, or the
   variance — do not record the numbers it printed anyway.
5. **Copy the printed body** into the baseline file, and put in the `evidence`
   column: the `run_id`, the revision, the results directory, and (for an
   `anchored` row) the path to the evidence document.
6. **Status rules.** `provisional` may be re-anchored with a normal PR
   justification; `anchored` requires a new evidence document; `placeholder`
   means not measured and is refused by the gate. See
   [`baselines/README.md`](../../regression-suite/perf/baselines/README.md).

A baseline whose evidence column does not let a reader find the run that
produced it is one retraction away from being a placeholder with a number in
it.

## 5. Re-measuring the published table

To move a row in [`BENCHMARK.md`](../../BENCHMARK.md) from "unverified" to
"measured", for that row:

1. Build HotSpot's reference arm and CratonVM's arm on the **same host**, same
   flags, fat LTO.
2. Run **>= 7 alternating fresh pinned processes per arm**, all checksums
   verified, with the reliability gate enabled on the CratonVM arm.
3. Publish the **distribution** for both arms (p50 and p99 at minimum), the
   host load, the CPU model, both revisions, and the `run_id`s.
4. Only then state a ratio — and state it as a ratio of medians, with the
   spread, on that host, on that date.

Until a row has been through that, it stays in the table under the "unverified"
banner in section 1. Prioritising work by those ratios is fine. Quoting them as
results is not.
