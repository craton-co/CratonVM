# `TestRandomMapOps` — the recorded `ClassCastException` does not reproduce; two other failures of the same class do

## Status

**OPEN 2026-08-24, but not on the signature this page was filed for.** The
`ClassCastException: String cannot be cast to Map$Entry` did not occur once in
**eleven runs** of this class across seven configurations and roughly two and a
half hours of runtime, on the 2026-08-23 `dev` tip. The seed the page records
as the reproducer passes on CratonVM *and* on stock HotSpot 25.

What the same runs did find is two other failures, both reproducible, neither
of which this page describes:

* **`-XX:+UseG1GC`: `NoSuchMethodError: 'java.lang.Object[]
  java.lang.Object.toArray()'` in 41–67 s, 5 of 5 runs.** A receiver whose
  class reads as `java.lang.Object` — the same *family* as the recorded cast
  failure (a reference whose class is wrong), reproducible in a minute instead
  of not at all.
* **`--Xmx 256m`: `NullPointerException: Cannot read the array length because
  "d" is null`** inside MVStore, at 823 s, `seed:-6324998873221791827 op:2093`.

Both are on the **default `dev` binary**; neither is introduced by the
2026-08-24 ZGC/JIT relocation work (the G1 arm was A/B'd against it explicitly
— see below). The page stays open because the class fails; the recorded
signature is retired as a starting point, because chasing it costs runs and
produces nothing.

## The recorded signature, and why it is not a reproducer

```
seed:-418228611310259706 op:1213
  java.lang.ClassCastException: class java.lang.String cannot be cast to
  class java.util.Map$Entry
	at org/h2/test/store/TestRandomMapOps.assertEquals(TestRandomMapOps.java:246)
```

Line 246 is the `for (Map.Entry<K,V> entry : entrySet)` header of the private
`assertEquals(String, Iterable<Map.Entry>, Cursor)` — the implicit
`checkcast Map$Entry` on `Iterator.next()`.

`testMap()` walks 100 iterations whose seeds come from an **unseeded**
`java.util.Random`, so re-running the class cannot re-run a reported seed.
`testOps(fileName, size, seed)` is the deterministic unit underneath, and
`org.h2.test.store.SeededRandomMapOps` (added 2026-08-24, see *Reproducing*)
reaches it directly. On the recorded seed with the recorded size:

| arm | result |
|---|---|
| stock HotSpot 25, `-Xmx1g` | `SEEDED_DONE`, no exception |
| CratonVM `dev`, `--Xmx 1g` | `SEEDED_DONE`, no exception |

So the failure is **not a function of the operation sequence**. It is
timing-dependent — a GC or JIT schedule — and the seed in the message is
therefore not a lever. Worth stating explicitly, because
`TestBase.testFromMain` reruns a failing seed and this page inherited the
assumption that the seed identifies the failure.

## What was run

All on the 2026-08-23 `dev` tip (`3ed73bf89`) or the 2026-08-24 fix branch,
real JDK 25, on the Azure host. `cce` is `grep -c ClassCastException`.

| arm | binary | heap / collector | wall | outcome |
|---|---|---|---|---|
| solo ×3 | `dev` | 1g, ZGC | 900 s cap each | no CCE, no failure of any kind |
| contended (3-way) | `dev` | 1g, ZGC | 2400 s cap | no CCE |
| ×2 | fixed | 1g, ZGC | 900 s cap each | no CCE |
| ×1 | fixed | **256m**, ZGC | 900 s cap | no CCE |
| ×1 | `dev` | **256m**, ZGC | 823 s | **NPE, `"d" is null`** |
| ×1 | fixed | 1g, generational | 900 s cap | no CCE |
| ×1 | fixed | 1g, **G1** | 41 s | **`NoSuchMethodError Object.toArray()`** |
| ×1 | fixed | 1g, `--nojit` | 900 s cap | no CCE |

Two things this table has to say about itself:

* **Every ZGC arm hit its cap rather than completing.** HotSpot runs this class
  to completion in **320 s**; CratonVM does not finish inside 900 s. So these
  are partial runs — but the recorded failure was at **108 s**, well inside
  every window here, so "not far enough" does not explain the zero.
* **A clean arm is worth nothing without a base rate**, and the base rate here
  is one occurrence, found while triaging a 48-class sweep. Eleven runs against
  a rate nobody measured cannot prove the defect is gone. What they do
  establish is that it is not reachable at a rate worth chasing head-on, and
  that the two failures above are.

## The G1 failure is pre-existing — measured, not assumed

The 2026-08-24 relocation work makes `moving_young_osr_fallback` false far more
often, and under G1 that flag also decides whether shadow-stack oops are
published PINNED or MOVABLE. So "G1 fails" needed an A/B, not an observation.
Five interleaved runs, same class, `-XX:+UseG1GC --Xmx 1g`:

| arm | rc | secs | `NoSuchMethodError` |
|---|---|---:|---:|
| `dev` binary | 1 | 48 | 3 |
| fixed binary | 1 | 48 | 3 |
| `dev` binary | 1 | 67 | 3 |
| fixed, `CRATONVM_OSR_COVERAGE_SHADOW=0` | 1 | 64 | 3 |
| fixed binary | 1 | 57 | 3 |

Identical in every column. The G1 failure is not this work's.

## The circumstantial evidence this page carried, resolved

The page's case rested on two collector-guard log lines near the crash, and it
already warned twice that the WARN-level one is uniform background noise. The
2026-08-22 update then found the ERROR-level `in_published_snapshot=…` line
next to a *confirmed-unrelated* crash in `TestClassLoaderLeak` and concluded it
weakened rather than strengthened the hypothesis.

Nothing here changes that, and one thing sharpens it: the recorded seed passes
on both VMs, so whatever the two guard lines were describing, they were not
describing a deterministic consequence of that operation sequence. Treat both
as background until something resolves what they name — which is still
`CRATONVM_DBG_LAYOUT=1` / `CRATONVM_DBG_COERCION=1`, and still unrun.

## Reproducing

The class itself:

```bash
source /data/toolchain/env.sh
cd /data/cratonvm/apps/h2database/h2
CP="target/classes:target/test-classes:$(cat craton-testcp.txt)"

# the reproducible G1 failure -- 41-67 s
<cratonvm-bin> --java-home /data/toolchain/jdk-25 --Xmx 1g -XX:+UseG1GC \
    -c "$CP" org.h2.test.store.TestRandomMapOps
```

A seed, pinned. `org.h2.test.store.SeededRandomMapOps` is a same-package driver
that reflects into the private `testOps(String,int,long)`; `TestRandomMapOps`
offers no seed-pinning knob of its own, which is what made the recorded seed
un-runnable until now:

```bash
javac -cp "$CP" -d <probe-dir> SeededRandomMapOps.java
<cratonvm-bin> --java-home /data/toolchain/jdk-25 --Xmx 1g \
    -c "<probe-dir>:$CP" org.h2.test.store.SeededRandomMapOps <seed> 3000 1
```

It prints `SEEDED_PASS` / `SEEDED_FAIL` per rep and exits non-zero on a
failure, so it is usable as a bisect target. The same command against
`/data/toolchain/jdk-25/bin/java` is the HotSpot control.

## Related

- `docs/known-issues/gc/G30-1-the-silent-reference-slot-coercion-20260817.md`
  — the WARN family this may or may not belong to.
- `docs/known-issues/h2/nonpassed-40-census-20260818.md` §1 — the methodology
  warning about over-reading that WARN shape, which this page's own history
  bears out.
- `docs/known-issues/h2/bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821.md`
  — the ZGC relocation work whose binary the arms above were A/B'd against.
- `docs/known-issues/h2/correctness-issues-consolidated.md` — indexes this
  finding alongside the rest of the 48-class union's correctness results.
