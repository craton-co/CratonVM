---
name: zipcontenttests-bytebuffer-accessor-call-cost-RETIRED-20260810
description: RETIRED 2026-08-10. ZipContentTests' 10-14x-HotSpot cost WAS correctly attributed to java.nio.ByteBuffer scalar accessors (287ns vs HotSpot's 2.19ns), but the intrinsic that page proposed is capped at ~10% by arithmetic and a powered 8-pair A/B cannot separate it from zero (+7.83%, 95% CI [-0.51%, +16.16%]) - so it ships default-OFF on evidence. The page's secondary claim that "--nojit is faster than JIT" is TRUE and far larger than the accessor question (+118.95s CPU, +83.1%, 5/5 pairs); it is not compilation, not slow compiled code, and not the JIT root scan, and is tracked separately.
metadata:
  type: retired-known-issue
  area: jit, nio, throughput, springboot
---

# `ZipContentTests` — the ByteBuffer accessor question, answered and closed

**RETIRED 2026-08-10.** Everything this page asked has an answer. The one thing
it turned up on the way — that the JIT itself is a net negative on this class —
is bigger than the page's own subject and moved to
`docs/known-issues/vm/jit-net-negative-on-call-dense-classes-20260810.md`.

Superseded pages, both already retired:
`zipcontenttests-gc-pressure-theory-REFUTED-20260810.md` (both of its hypotheses
refuted by measurement) and the 2026-08-07 GC-pressure framing.

## What was right

The attribution was right. `ZipContentTests` passes 29/29 on every arm — it was
never a hang, only a class that clears the 300 s suite budget by 5% — and the
time really is concentrated in `java.nio.ByteBuffer` scalar reads, which Spring
Boot's zip header reader calls eleven `getShort()` and six `getInt()` times per
central-directory record. On Azure, `--stack-sample-ms=200` over the `--nojit`
arm puts `HeapByteBuffer.getShort` 7.24% + `Buffer.nextGetIndex` 5.84% +
`getInt` 3.30% + `byteOffset` 1.91% = **18.3%** of leaf samples, independently
reproducing the 18% measured on Windows.

The per-operation gap against HotSpot is real and large (min-of-3, ns/op,
`probes/ByteBufferScalarSplitProbe.java`):

| arm | HotSpot | CratonVM JIT | ratio |
|---|---:|---:|---:|
| `buffer.get(int)` | 2.19 | 286.99 | 131x |
| `buffer.getShort(int)` | 2.51 | 1010.46 | 403x |
| `buffer.getInt(int)` | 2.08 | 1076.35 | 517x |
| `buffer.getShort()` | 2.40 | 804.26 | 335x |
| `buffer.getInt()` | 2.51 | 844.54 | 336x |
| raw `byte[]` read | 0.47 | 1.66 | 3.5x |

Array access itself is fine at 3.5x. Everything above it is call cost.

## What was wrong, and what the correction cost

**The intrinsic cannot pay off, and the arithmetic said so before any run.**
The accessors are ~18.3% of leaf samples and the intrinsic makes them ~2.4x
faster, so it can remove at most `18.3% x (1 - 1/2.4)` = **~10.7%** of total
time. The page's evidence against it was three single runs (5.9%, 6.2%, 12.2%
"slower") drawn from a configuration whose own spread across one session was
315.5 / 353.8 / 413.7 / 450.7 s — **+-20%**. An experiment whose noise is twice
its ceiling cannot produce a sign, and the sign it produced was noise.

Re-run properly on Azure — 8 interleaved ON/OFF pairs, per-process CPU time,
same binary:

    deltas +5.01 +7.64 +5.12 +17.84 +2.26 +27.28 -2.85 +0.32 %
    mean +7.83%  sd 9.97  sem 3.52  95% CI [-0.51%, +16.16%]  7/8 positive

Every run 29 tests / 0 failed. The interval contains zero and also contains the
ceiling: this class cannot distinguish "the intrinsic works exactly as
predicted" from "the intrinsic does nothing". **So it stays default-OFF —
now because it was measured, not because a noisy triple said "slower".**

The microbenchmark, which IS powered, stands: `getShort()` 1.42x, `getInt()`
1.5x, `getShort(int)` 2.09x, `getInt(int)` 2.14x, with the two deliberately
excluded arms measuring 1.00x and 0.97x — the lever is scoped to exactly the
accessors it claims. `probes/ByteBufferAccessorMatrixProbe.java` (119 lines)
pins the contract against HotSpot: every buffer shape, both byte orders, bounds
against `limit`, exception class and message per width, NaN / `-0.0` bit
patterns.

**The retracted mechanism.** An earlier revision claimed the intrinsic lost
because registering a native took the accessors away from the JIT's inliner.
That does not hold: the single-pass emitter "bails on any callee invoke that is
not a resolver-proven elidable super-`<init>`" (`jit/src/lib.rs:5239`), and
`HeapByteBuffer.getShort()`'s body is four invokes. It was never inlined into
its callers, so there was no inlining to lose.

**Two smaller findings worth keeping.**

* `get(int)` and `get()` are deliberately NOT intrinsified. They are the only
  accessors whose Java body makes no native call, so a crossing makes them
  *slower* — 0.76x measured. Intrinsifying everything that looked alike would
  have cost throughput on the commonest accessor of the set.
* The first version covered only the ABSOLUTE forms and moved this class not at
  all: the zip header reader calls the RELATIVE forms and the absolute ones
  never. Reach before speed — a shape the hot code never executes cannot show a
  win however fast it is.

## The claim that outgrew this page

The page also said, correctly, that `--nojit` beats the JIT here. That is not a
footnote. Five interleaved pairs, per-process CPU:

| pair | JIT | `--nojit` | delta |
|---|---:|---:|---:|
| 1 | 268.24 | 145.17 | +123.07 |
| 2 | 259.38 | 142.37 | +117.01 |
| 3 | 262.03 | 143.83 | +118.20 |
| 4 | 259.25 | 141.76 | +117.49 |
| 5 | 261.13 | 142.17 | +118.96 |

**mean +118.95 s, sd 2.40, sem 1.08 — +83.1% CPU, 95% CI [+116.0, +121.9] s,
5/5.** CPU time held to sd 2.40 while the host's `load1` swung 11.4 -> 27.7,
which is why the metric is per-process CPU and not wall clock.

That dwarfs the ~10% the accessor intrinsic could ever have been worth, and it
is a different problem. It is **not** compilation (231 compiles,
`total_compile_time_ms=3`), **not** compiled code being slower than the
interpreter (the JIT beats the interpreter 4.6x-98.8x on every arm of the split
probe), and **not** the JIT conservative root scan (`band_scans=0` over the
whole run). It has its own page.

## Carried over from dev: the collector-agnostic rerun

Added to the live page on 2026-08-10 by a concurrent session and preserved
here, because it is independent corroboration and it survives the retirement.
Its conclusion is strengthened by what this page now knows: the class is
collector-agnostic because the collector was never the variable — the dominant
term is the JIT, and every one of those three arms ran with the JIT on.

One correction to it, from the measurements above: it reads the ~300 s wall
times as "a hair over the 300 s ceiling". On an uncapped Azure run the JIT arm
is 260-290 s of CPU against `--nojit`'s 143 s, so the class is not marginal
against that budget for the reason assumed — it is carrying an 83% JIT tax, and
removing it clears the budget by a factor of two rather than by a hair.

## 2026-08-10 reconciliation — the 139-class rerun shows TIMEOUT on all three collectors, no OOM this time

Reconciling the 139-class non-passed union from the same-day `default`/`g1`/`zgc`
suite rerun (binaries `cratonvm-{default,g1,zgc}-20260808f.exe`, `dev@6365de194`,
`-Xmx 2g` — the suite's default `MaxHeap`). `ZipContentTests` is TIMEOUT/HANG at
~300s under **all three** collectors: default 300.195s, G1 300.144s, ZGC 300.102s.
Logs:
`apps/spring-boot-suite-runner/.suite/results/craton-nonpassed-{default,g1,zgc}-20260808f-s1/all-jit/logs/loader_spring-boot-loader.org.springframework.boot.loader.zip.ZipContentTests.{out,err}.log`.

Every one of the three matches this page's own finding, not the retired
GC-pressure framing:

- **All three `.out.log`s are 0 bytes.** Exactly the "did not finish" signature
  this page already established (`SbRunner` prints nothing until
  `launcher.execute(req)` returns) — not evidence either way about *where* the
  time went, but consistent with the class simply not reaching its own summary
  print in the 300s window.
- **No `OutOfMemoryError` anywhere in any of the three `.err.log`s.** At `-Xmx 2g`
  this page's own ZGC heap-lever table recorded an OOM at 262s in one run and a
  clean 301s pass in another (both under 2g) — i.e. run-to-run variance was
  already on file for this exact heap size. This rerun's ZGC arm landed on the
  "just runs out of the 300s budget first" side of that variance rather than the
  "OOMs at 262s" side; both are downstream of the same finding (this class is
  ~14x HotSpot and clears an *uncapped* run in 301-315s — a hair over the 300s
  ceiling either way it resolves).
- **default's `.err.log`** is active, not silent: `[moving-young] fallback` climbs
  to #16 (`reason=unregistered-jit-frame-on-stack` after starting with
  `innermost-rbp-belongs-to-unguarded-callee`), one `gc::guard` LIVE-object
  retention, and repeated `old-gen mark: conservative root ... is an INTERIOR
  word of the live object` warnings continuing every 20-40s up to the kill —
  more fallback churn than this page's own 9-line/315s reference run logged, but
  the same qualitative shape ("present, but not a churn story" per the section
  above) and consistent with sitting closer to the ceiling this time round.
- **G1's and ZGC's `.err.log`s are silent** (5 lines each, routine
  post-clinit-fixup boilerplate only) — no GC/JIT diagnostic activity logged at
  all in either, for the whole 300s run.

**Collector-agnostic (reproduces under Generational, G1, and ZGC)** — all three
land within 5% of the same ~300-315s wall time this page already priced to the
`ByteBuffer` accessor cost, not to a hang or a new GC-pressure mechanism. No
symptom drift from what this page describes.

## Reproducers

## Reproducers

* `probes/ByteBufferScalarSplitProbe.java` — accessor cost by layer, absolute
  and relative forms. Run it under `--nojit` too: that control was missing from
  the original page and is what refuted its dispatch-floor reading.
* `probes/ByteBufferAccessorMatrixProbe.java` — the HotSpot-diffed contract.
* `probes/ZipContentTermsProbe.java` — prices every term the sampler named.
  Note `FileOutputStream.write` 0.92x and `ZipOutputStream` 0.94x: CratonVM is
  *faster* than HotSpot at the file and zip-framing work, so neither disk nor
  zlib was ever the story.

## Affected classes

`loader/spring-boot-loader` — `org.springframework.boot.loader.zip.ZipContentTests`.
Nothing here is special to it: any class parsing binary headers through
`ByteBuffer` pays the same per-field cost.
