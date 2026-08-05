# MEAS-02 — the CPU benchmark suite issues seven C2 requests and gets two bodies

**Status:** not started. **Blocks nothing; invalidates a lot.**
**Owns:** `../../../regression-suite/perf` (the gate, its baseline TSV and its phase
list) and `../../../bench`. Not `../../../jit`.

## The measurement

`CRATONVM_DBG=ir-compiles`, all seven CratonBench phases, one isolated process
each, default configuration:

| workload | compile requests reaching the admission chain | admitted to the optimizing pipeline | bodies |
|---|---:|---:|---:|
| arithmetic | 0 | 0 | 0 |
| fib | 1 | 1 | 1 |
| sieve | 2 | 1 | 0 |
| matrix | 1 | 0 | 0 |
| hashmap | 0 | 0 | 0 |
| stringregex | 0 | 0 | 0 |
| bintrees | 3 | 1 | 1 |
| **total** | **7** | **3** | **2** |

Against one Spring Boot test class in the same survey: 1,372 requests, 694
admitted, 409 bodies.

## What that means

**The perf gate measures the single-pass backend.** `run-cratonbench-gate.sh`
is the mandatory no-perf-regression check, its baselines are anchored per host,
and the README's published table comes from it — and across all seven phases the
optimizing tier compiles two methods. A change that made C2 twice as fast, or
switched it off entirely, would move these numbers by approximately nothing.

That is not a criticism of the gate, which does its job well: it is pinned,
it verifies a checksum on every run, it records full distributions, and it
**refuses to measure on a loaded host** rather than emit a noisy verdict. The
problem is coverage, not rigour.

It also explains `cov-02`. `IrBuilder::build` has arms for `faload`, `daload`,
`fastore` and `dastore` and for **no** integral or reference array access. Those
four are what an FP kernel needs. The arms that exist are the arms the fixtures
demanded, and the suite that would have shown the gap does not reach the tier.
This directory's own rule — *"Do not size a lane from a fixture's node mix"* —
was written after the HIR lane's synthetic corpus claimed 38.2% isel coverage
where real code gives 15.7–19.0%. Same failure, one level down.

## The first increment

**Do not add a phase to the gate yet.** Adding a workload to a gate with
anchored baselines and a 5% budget is a commitment; adding one whose numbers
nobody understands yet is how a gate starts getting waived.

1. **A C2-reach column in the survey.** Extend
   `ir-coverage-survey-20260803.md`'s per-workload table to whatever candidate
   workloads are proposed, so "does this reach the optimizing tier" is answered
   before anything is anchored. The command is one env var.
2. **Pick a candidate and characterise it.** Something framework-shaped and
   deterministic, with a checksum, that runs in seconds — the gate's existing
   requirements. A Spring Boot test class is not a benchmark (no checksum,
   external fixture, minutes long); the point is to find something with a
   *similar node mix* and benchmark discipline.
3. **Only then** anchor it, with the evidence document the README's re-anchoring
   policy already requires.

## The cheaper half, worth doing first

The gate already asks the VM for its shutdown JIT summaries (`--no-vm-stats`
turns it off, so it is on by default). Record **`admitted` and `bodies` per
phase in the results manifest**, alongside the compile counts already there.
That costs one env var and no baseline, and it makes "this phase does not
exercise the optimizing tier" a fact in the results directory rather than
something a reader has to go and discover. Every future C2 measurement then
carries its own reach evidence.

## How to verify

The manifest for a gate run contains a per-phase optimizing-tier reach, and it
reads 0–3 for the current seven phases. If it reads anything else, this
lane's premise has changed and the numbers above need re-taking before any
candidate workload is chosen.

## What to refuse

Anchoring a baseline for a workload whose C2 reach has not been measured, and
quoting a CratonBench delta as evidence about the optimizing tier. The second
one is the reason this doc exists.
