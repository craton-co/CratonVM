# PERF-01 — the optimizing tier's `sieve` body is 6.4x slower than the C1 body it replaced

**Status: FIXED 2026-08-04**, same day it was found. Closeout, with the
verification and the limitation the fix leaves behind:
`perf-01-sieve-ir-body-slower-than-c1-FIXED-20260804.md`.
The brief is kept because its reasoning — and the policy question in its "first
increment", which is **still open** — is what a reader needs, not because
anything in the measurement below is still current.

**Found 2026-08-04. Landed 2026-08-03.**
**Owns:** whatever lowers `baload`/`bastore` in `IrBuilder::build`, and the
decision of whether an IR body may replace a C1 body without evidence.
Not `regression-suite/perf/`.

## The measurement

`bench/CratonBench.java` phase `sieve`, isolated fresh process, pinned cpu 13,
`-Xmx8g`, arms interleaved with the order flipped on alternate pairs, 5 pairs.
Both builds are from the same tree and differ only by `cov-02`:

| build | samples (ms) | median |
|---|---|---:|
| before (`cratonvm-cov02-base-20260803`) | 2,462 / 2,487 / 2,324 / 2,461 / 2,474 | **2,462** |
| after (`cratonvm-cov02-arrays-20260803`) | 15,949 / 15,805 / 15,662 / 15,922 / 15,823 | **15,823** |

**6.4x.** The same gap reproduces against the merged `dev` tree
(`12b8cbdea`, 15,717–16,052 ms) and against a pre-`cov-02` control
(2,314–2,411 ms), so it is not a property of one binary.

For scale: HotSpot JDK 25 C2 runs this phase in 2,412 ms. **CratonVM was
faster than HotSpot on `sieve` before this change and is 6.5x slower after
it.**

## The cause, exactly

One method. `CRATONVM_DBG=ir-compiles` on both builds:

```
before:  [ir] admission CratonBench.sieve([ZI)I: optimize=false — the C1/fast tier was requested, not C2
         [ir] admission CratonBench.sieve([ZI)I: admitted to the optimizing pipeline
after:   [ir] admission CratonBench.sieve([ZI)I: optimize=false — the C1/fast tier was requested, not C2
         [ir] admission CratonBench.sieve([ZI)I: admitted to the optimizing pipeline
         [ir] optimizing backend produced a body for CratonBench.sieve([ZI)I
```

Both builds **admit** the method. Only the newer one **lowers** it — before,
`IrBuilder::build` refused at `0x54 bastore` and the method fell through to the
single-pass backend. `cov-02` added the arm, the refusal went away, and the
body the optimizing tier now emits is 6.4x slower than the one it displaced.

Per-phase C2 reach, from the same runs (`regression-suite/perf/c2-reach.sh`):

| build | requests | admitted | bodies |
|---|---:|---:|---:|
| before | 2 | 1 | **0** |
| after | 2 | 1 | **1** |

`sieve` is the *only* CratonBench phase whose reach changed. It is also the
only one whose time changed. That is the whole finding.

## What this is not

**Not a correctness bug.** The checksum is `9592` on both builds and matches
HotSpot on every run. Nothing here is wrong, only slow.

**Not an argument against `cov-02`.** Lowering integral array access is right,
and `cov-02`'s own closeout measured what it set out to measure — refusals for
its seven opcodes 79 → 0, optimizing-backend bodies 591 → 652 on the Spring
workloads. This is the *other* half of that trade, which
`ir-coverage-survey-20260803.md` had already written down in as many words:

> It does not say lowering these opcodes makes anything **faster**. It says the
> optimizing tier declines to compile 41% of what it admits. Whether an
> optimized body beats the single-pass one for a given method is a separate
> measurement.

This is that separate measurement, and for this method the answer is no.

## Why nobody caught it

Two reasons, both now closed:

1. **The perf gate could not run.** Its own `export LC_ALL=C` made `javac`
   default to US-ASCII and it died compiling `bench/CratonBench.java` before
   measuring anything (fixed 2026-08-03, `MEAS-02`). A gate that cannot start
   catches nothing.
2. **Nothing recorded which phases reach the optimizing tier**, so the one
   phase whose reach changed was not a thing anybody could look at. The
   per-phase reach record (`ir_reach_<phase>` in every gate run's
   `manifest.tsv`) is what turned this into a one-step diagnosis.

The gate would now fail this outright: `sieve`'s baseline is 2,700 ms with a 5%
budget, and it measures 15,680 ms.

## First increment

**Do not revert `cov-02`.** The arm is wanted; the body it produces is not.

1. **Find out what the IR body does that the C1 body does not.** The inner loop
   is `composite[j] = true` over a `boolean[]` — a `bastore` in a tight
   stride loop, plus the `for (i = 0; i <= limit; i++) composite[i] = false`
   clear. Compare the two bodies' emitted code; a 6.4x gap on a store-only loop
   is a bounds check, a write barrier, or a helper call per element, not a
   register-allocation difference. `performance/` already records
   one case of exactly this shape (`reference_c2_tier_slower_because_fields_take_the_helper`).
2. **Then decide the policy question this raises**, which is bigger than the
   defect: the optimizing tier currently replaces a C1 body with an IR body
   whenever it *can*, with no evidence that the replacement is faster. Every
   `cov-*` lane widens the set of methods that happens to. A cheap guard —
   compare against the C1 body once, keep the faster one — would make the whole
   `cov-*` programme monotone. Nothing like it exists today.

## How to verify a fix

`sieve` back under 2,700 ms **with `ir_bodies = 1`** for the phase. Getting
there with `ir_bodies = 0` is a revert, not a fix, and the reach record now
makes the difference visible in the results directory:

```bash
bash regression-suite/perf/run-cratonbench-gate.sh -Exe /abs/path/to/cratonvm --phases sieve
grep ir_reach_sieve <results>/manifest.tsv
```
