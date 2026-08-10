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
