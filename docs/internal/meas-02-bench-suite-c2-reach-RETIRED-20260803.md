# MEAS-02 — the bench suite does not reach C2 — RETIRED 2026-08-03

The brief is at
[`docs/known-issues/c2/archive/meas-02-the-bench-suite-does-not-reach-c2.md`](../known-issues/c2/archive/meas-02-the-bench-suite-does-not-reach-c2.md).
It owned `regression-suite/perf/` and `bench/`, and it asked for three things:
record the optimizing tier's reach in the gate's own results, add a C2-reach
column to the coverage survey, and characterise one candidate workload —
while explicitly refusing to anchor anything.

All three landed. Anchoring did not, on purpose; §5 says why and what would
change that.

## 1. The premise, re-taken

The brief's table was re-measured on the Azure bench host at `7c08e9abe`, one
run per phase, default configuration. Counts, not timings, so the host's load
(1-min 13–50 throughout) does not touch them.

| phase | requests | admitted | bodies | brief said |
|---|---:|---:|---:|---|
| arithmetic | 0 | 0 | 0 | 0 / 0 / 0 ✓ |
| fib | 1 | 1 | 1 | 1 / 1 / 1 ✓ |
| sieve | 2 | 1 | 0 | 2 / 1 / 0 ✓ |
| matrix | 1 | 0 | 0 | 1 / 0 / 0 ✓ |
| hashmap | 0 | 0 | 0 | 0 / 0 / 0 ✓ |
| stringregex | **1** | 0 | 0 | **0** / 0 / 0 ✗ |
| bintrees | 3 | 1 | 1 | 3 / 1 / 1 ✓ |
| **total** | **8** | **3** | **2** | 7 / 3 / 2 |

**The premise holds.** One extra request on `stringregex` moves the total from
seven to eight and moves nothing else: `admitted` is 3 and `bodies` is 2, as
the brief measured. The brief's verification clause — "it reads 0–3 for the
current seven phases; if it reads anything else, this lane's premise has
changed" — is satisfied.

Reproducibility, for whoever re-takes this: the reach was measured twice
across all ten phases. `bodies` was identical both times for every phase.
`requests` moved once, on the candidate's `bind` phase (9 then 10), because
tier promotion is invocation-count driven against a background compiler.
Treat single-digit `requests` differences as noise; do not treat a `bodies`
difference that way.

## 2. What a results directory now records

Results schema **1 → 2**. Every existing column kept its name and every
consumer resolves columns by name, so a v1 reader reads a v2 directory
correctly and simply sees no reach. The version moved because "this directory
records C2 reach" must not be something a reader infers from a column's
presence.

* `samples.tsv` / `summary.tsv`: `ir_requests`, `ir_admitted`, `ir_bodies`.
* `manifest.tsv`: one `ir_reach_<phase>` line per phase, plus `ir_reach_total`,
  `ir_reach_recorded` and `ir_reach_scrape_broken`. The manifest is what a
  reader opens to find out what a results directory *is*, so the caveat lives
  next to the CPU model and the binary hash.
* the console, at the end of every run — and when `bodies` is 0 it says what
  that licenses, not just the number.
* `compare.py` names the phases whose delta is not evidence about the tier, and
  reports "not recorded" separately from a measured zero.

### `compiles_c2` is not this number

It was the closest thing available before, and it is wrong in two directions.
`TieredCompiler` counts a compile under the TIER it was requested at, whichever
backend produced the body — so a method the optimizing pipeline admitted and
then declined, and which the single-pass backend compiled, is counted as a C2
compile. And OSR compiles are counted there too. `arithmetic` and `hashmap`
each report `c2=1 osr=1` while issuing **zero** compile requests: that one C2
compile is the OSR one, which enters through `compile_osr_artifact` — a second
compile door that calls the backend directly and never passes the admission
chain.

`compare.py` now prints `C2-tier compiles`, `C2 admitted` and `C2 bodies` as
three separate rows for exactly this reason.

## 3. The two defects found on the way

### 3.1 The gate could not compile its own benchmark

`run-cratonbench-gate.sh` does `export LC_ALL=C`, correctly, so that the awk
distribution arithmetic cannot be broken by a comma-decimal locale. On a JDK
whose `javac` derives its default **source** encoding from the platform charset
— 17 on this host; JEP 400 moved `file.encoding`, not this — `C` means
US-ASCII, and `bench/CratonBench.java` has em-dashes in its header comment. The
gate died at setup with 30 `unmappable character (0xE2)` errors and
`FATAL: bench compile failed`, before measuring anything.

Reproduced on unmodified `origin/dev`. The bench host's ambient locale is
`C.UTF-8`, so the same `javac` run by hand in the same shell succeeds and the
failure appears only *inside* the gate — which is how it survived. Fixed by
pinning `-encoding UTF-8`, which is the same argument that pinned the locale:
one fewer thing that differs between two runs being compared.

This is worth more than its diff. The mandatory perf gate has been unrunnable
end-to-end on the bench host, which is a plausible reason nobody noticed the
suite does not reach C2.

### 3.2 The fail-closed check was itself wrong, and said so on its first run

The reach is scraped from two `[ir] …` stderr lines. A scrape fails **open**:
reword either line and every phase records a confident zero, which is
character-for-character what the expected finding looks like. Two independent
checks close that — reach must be monotone, and the tier manager's own compile
counts (a different line, a different module) must not report more non-OSR
compiles than the admission chain saw requests.

The second check was first written as "a C1 or C2 compile cannot happen
without passing the chain", and its first run REFUSED `arithmetic` and
`hashmap` — both of which have a genuine reach of zero. The premise was false
for the reason in §2: a C2-tier OSR compile lands in `c2=`. The comparison is
now `c1 + c2 - osr > requests`, checked against all ten measured phases and
against a stub that reproduces a reworded `[ir] admission` line, which it still
catches.

Worth keeping in mind that the check failing *closed* is why this was a
five-minute correction rather than a wrong number in a document.

## 4. The candidate: `bench/CratonBenchC2.java`

Three phases — `dispatch`, `bind`, `pipeline` — built to the gate's existing
requirements (deterministic, checksummed, isolated process per phase, seconds
not minutes) with a framework-shaped node mix instead of a kernel-shaped one.
The thing that makes it reach the tier at all is not the opcodes: it is **many
small methods each invoked past `c2_threshold` (20,000)**, which is what a
dispatch loop has and a single enormous kernel loop does not.

| | requests | admitted | bodies |
|---|---:|---:|---:|
| CratonBench, seven phases | 8 | 3 | 2 |
| CratonBenchC2, three phases | 38 | 19 | 12 |

Per phase, identical on two consecutive runs: `dispatch` 17/9/6, `bind` 9/5/2,
`pipeline` 12/5/4.

Reaching the tier is the cheap part. The part that matters is *where* it
fails, because a candidate that reaches C2 and then fails in places real code
never touches would be a second fixture whose node mix nobody should size a
lane from:

| refusal | lane | CratonBench | CratonBenchC2 | rank in the Spring survey |
|---|---|---:|---:|---|
| `checkcast` / `instanceof` | `cov-05` | 0 | 5 | 1st (306) |
| `anewarray` | `cov-06` | 0 | 4 | 2nd (138) |
| `invokespecial` (`ir.rs:5204`) | `cov-04` | 0 | 4 | 53 |
| reference `putfield` (`ir.rs:5085`) | `cov-03` | 0 | 1 | 37 |
| `ldc` | `cov-01` | 0 | 1 | joint 1st opcode gap (90) |
| `arraylength` | `cov-02` | 0 | 1 | 43 |
| `athrow` | `cov-07` | 2 | 0 | 89 |
| `multianewarray` | `cov-06` | 2 | 0 | 2 |
| `bastore` | `cov-02` | 1 | 0 | 2 |

The candidate's top three refusals are the survey's top three. CratonBench
never once touches `checkcast`, `anewarray`, `invokespecial` or a reference
field store. The two suites do not merely differ in how far they get — they
disagree about what the optimizing tier's problems are.

### Sizes, and what sizing them taught

`DISPATCH_REQS` = 400,000, `BIND_REQS` = 2,000,000, `PIPELINE_BATCHES` =
40,000 x 16. Two constraints, and the second is the one that is easy to break
by accident:

* **Seconds, not minutes** — the gate's requirement. Measured: 2.9–5.3 s,
  3.2–3.3 s, 4.1–6.4 s on CratonVM; 37 ms, 72 ms, 32 ms on HotSpot, which is
  a 45–200x gap and a separate subject.
* **Every method whose compilation is the point must stay an order of
  magnitude past `c2_threshold` = 20,000 INVOCATIONS.** `pipeline` is
  40,000 x 16 rather than the more natural 1,000 x 256 for this reason alone:
  the same 640,000 handler calls either way, but at 1,000 batches `runBatch`
  itself is invoked a thousand times and never becomes a C2 candidate. Batch
  *count* is what makes the per-batch frame hot; batch *size* only makes each
  call do more.

An earlier draft, sized closer to the threshold, measured `bind` at 9/5/2 then
10/6/2 on consecutive runs. At the sizes above, all three phases reproduced
cell for cell across two runs. The checksums also agree exactly between
CratonVM and HotSpot, on all three phases.

## 5. What was deliberately NOT done

**The candidate is not a gate phase and has no baseline.** There are now three
reasons, and only the first was in the brief.

1. The brief's own instruction: "do not add a phase to the gate yet".
2. **The host has not been quiet.** The 1-min load ran 13–50 for the whole
   session and the gate refuses to measure above 2.0, for good reasons. A
   baseline anchored under that load would be a number nobody could reproduce
   — the failure mode `BENCHMARK.md`'s retracted HashMap row already cost this
   project once.
3. **`dispatch` is bimodal, and that is not a load artefact alone.** The same
   binary on the same classes measured 0.42 s and 0.65 s, then 8.8 s, then
   2.1 s, then 19 s, then 30 s — with **identical** compile counts
   (`c1=6 c2=6 osr=1`) in the fast and slow modes. It is not driven by the
   iteration count: 2,000,000 reps measured 2.1 s and 19 s on different runs.
   Pinning to two CPUs instead of one made some runs fast (3.7 s, 4.7 s) and
   left others slow (22 s, 30 s), so contention between the mutator and the
   background compiler on a single pinned core is at most part of it. Cause
   not isolated; the host load is a confound that could not be removed.

Reason 3 is the interesting one and it is not only about anchoring: a 17x
swing on identical work with identical compile counts is a lead, not just a
nuisance. Note it is invisible to the current gate, whose phases compile
almost nothing — which is the same blind spot MEAS-02 is about, showing up in
timing rather than in coverage.

What anchoring needs, in order:

1. A quiet host (1-min load < 2.0) — the gate enforces this itself.
2. An explanation for reason 3, or a demonstration that it does not occur on a
   quiet host. A 5% budget against a phase that swings 17x is not a gate.
3. `--calibrate` on that host, with a per-phase CV inside the gate's 5%
   ceiling.
4. An evidence document, as the re-anchoring policy in
   `regression-suite/README.md` requires — and it should quote the phase's C2
   reach, which is now recorded for it automatically.

Until then the candidate is a workload whose *reach* and *node mix* are
characterised and reproducible, and whose *timing* is not; and
`regression-suite/perf/c2-reach.sh` is how the next candidate gets the same
treatment before anybody proposes anchoring it.

## 6. Does recording the reach perturb the measurement?

`--no-vm-stats`'s existing note said both knobs it controls are shutdown-only.
`ir-compiles` is not: it prints on the compile path, once per compile request.
That path is entered a single-digit number of times per phase — which is the
finding — but "should be free" is not a measurement.

**The direct cost, counted.** Every `ir_stage_reporting()` call site is inside
the compile path — `try_compile_inner`, `ir_verify_reject` (which it calls),
and `ir_lower` — and none is on an interpreter or execution path, so the flag
costs nothing at all in the code a phase actually spends its time in. The
whole extra output per phase, from the same runs as §1:

| phase | extra lines | extra bytes | phase baseline |
|---|---:|---:|---:|
| arithmetic | 0 | 0 | 3,976 ms |
| fib | 2 | 136 | 4,450 ms |
| sieve | 4 | 325 | 2,700 ms |
| matrix | 3 | 220 | 5,700 ms |
| hashmap | 0 | 0 | 1,800 ms |
| stringregex | 3 | 232 | 165 ms |
| bintrees | 4 | 419 | 1,550 ms |

Four lines and 419 bytes is the worst case, against the shortest phase of
165 ms. There is no plausible cost model in which that is measurable, and the
flag is off on any path the phase actually spends its time in.

**The A/B, and what it is worth.** Interleaved, ABBA order per pair so host
drift cannot masquerade as an effect, 8 pairs per phase, on the three phases
where an effect would show first — `stringregex` (shortest, so most sensitive
to a fixed cost), `bintrees` (most compile requests), `sieve`.

| phase | OFF median | ON median | delta | within-arm spread (min→max) |
|---|---:|---:|---:|---|
| stringregex | 256.5 ms | 258.0 ms | +0.6% | 55% |
| bintrees | 2,187.5 ms | 2,104.0 ms | −3.8% | 39–74% |
| sieve | 4,239.0 ms | 4,682.5 ms | +10.5% | 38–84% |

Read this honestly: the sign flips across phases and every delta is far inside
a within-arm spread of 38–84%, which is what a 1-min load of 13–50 does to a
pinned measurement. **This corroborates the line count; it does not
independently establish anything.** The claim that stands on its own is the
table above it — four lines per phase. Re-take the A/B on a quiet host if the
answer ever needs to be tighter than that.

## 7. Residuals

Every one of these is unowned.

* **`dispatch`'s 17x bimodality** (§5 reason 3): identical work, identical
  compile counts, 0.42 s to 30 s. The most interesting thing this lane found
  and the one it did not explain. Repro: `bench/CratonBenchC2.java`, phase
  `dispatch`, run it ten times.
* **Anchoring the candidate**, per §5 — blocked on the two above it, not on a
  decision.
* **The A/B in §6 was taken on a loaded host.** Interleaving bounds the effect
  rather than eliminating the confound; re-take it on a quiet host if the
  answer ever needs to be tighter than "inside the noise floor".
* **The reach scrape reads two stderr strings.** The two consistency checks
  make a reworded line loud instead of silent, but the robust version of this
  is a counter in the VM's own shutdown summary, which would need a change in
  `jit/` — a different lane's file, and not worth taking while four `cov-*`
  lanes are editing it.
* **`javac` is 17 and `java` is 21 on the bench host.** Noticed while fixing
  §3.1, not investigated. The gate records both in its manifest.
