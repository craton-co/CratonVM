# `TestRandomMapOps` — the recorded `ClassCastException` does not reproduce; two other failures of the same class do

## Status

**OPEN 2026-08-24 — one of the two live failures is now FIXED, the other is
not, and neither is the signature this page was filed for.** The
`ClassCastException: String cannot be cast to Map$Entry` did not occur once in
**eleven runs** of this class across seven configurations and roughly two and a
half hours of runtime, on the 2026-08-23 `dev` tip. The seed the page records
as the reproducer passes on CratonVM *and* on stock HotSpot 25.

| failure | state |
|---|---|
| `-XX:+UseG1GC`: `NoSuchMethodError: 'java.lang.Object[] java.lang.Object.toArray()'` | **FIXED** — an unpinned receiver across a GC point; see below |
| `--Xmx 256m`: a stale receiver inside `assertEquals`, ~823–845 s | **OPEN**, and it has now shown TWO different faces |
| the recorded `ClassCastException` | **not reproduced**, retired as a starting point |

The page stays open on the second row.

## The G1 failure: an unpinned receiver across a GC point — FIXED

`new ArrayList<>(map.keySet())` reaches `native_al_init_from_collection` →
`collect_collection_elements_or_real`, which asks the receiver's own `size()`
and then its `toArray()`:

```rust
let real_size = ctx.invoke_virtual(coll, "size", "()I", &[]);   // GC point
let arr       = ctx.invoke_virtual(coll, "toArray", ...);       // stale `coll`
```

`invoke_virtual` re-enters Java, so it is a GC point, and `coll` was a bare
Rust local that nothing rooted. G1 evacuated `coll`'s region during `size()`
and **recycled** it; `toArray()` then dispatched against the freed region's base
address. An all-zero header reads as `ClassId(0)`, which **is**
`java.lang.Object` — the first class this VM loads — so a use-after-move
surfaced as a missing method on a class nobody called.

**`--nojit` is the control that matters.** This page filed the failure as "a
receiver whose class reads as `java.lang.Object` — the same *family* as the
recorded cast failure", which points at a lost JIT root. It is not one:

| arm | result |
|---|---|
| G1, `--Xmx 1g` | 3 of 3 FAIL, 155–178 s |
| G1, `--Xmx 1g`, **`--nojit`** | FAILS — so not a JIT root defect |
| G1, **`--Xmx 8g`** | clean past 500 s — so it needs a collection |
| ZGC / generational, `--Xmx 1g` | clean past 420 s — so it needs an EVACUATING collector |
| G1, `--Xmx 1g`, **fixed binary** | **3 of 3 clean past 500 s** |

**`report_reclaimed_receiver` stayed silent the whole time**, which is worth
recording because that guard exists to answer exactly this question: it asks the
FREE LIST, and a whole evacuated G1 region is not a free-list block. A quiet
reclaim guard is not a clean one.

**The fix is measured, not inferred.** `CRATONVM_DBG_COLL_REFRESH=1` counts
every time the re-read finds the receiver has actually moved — 46 and 42 per
run, **all** of them at `between size() and toArray()`, with stale addresses
that are region bases:

```text
[COLL-REFRESH] between size() and toArray(): 0x251689f0060 -> 0x25167df0060
[COLL-REFRESH] between size() and toArray(): 0x251689f0000 -> 0x251683c4840
```

So the old code used a stale pointer dozens of times per run and raised the
error only on the occasions when the region had also been recycled. That
counter is the difference between "the fix works" and "the symptom did not
happen this time", which a timing-dependent defect can fake on any single run.

### The same shape elsewhere, audited rather than assumed

Scanning `native-collections/src/lib.rs` for two or more `invoke_virtual` calls
on one unpinned receiver finds **13 candidate sites**. The four on this call
path are pinned by the same change, each with its own counter label: the
Hibernate `size()`→`toArray()`, the two Jetty `size()`→`get(i)` loops and the
`Path` `getNameCount()`→`getName(i)` loop in `collect_collection_elements`.
**On this workload only the proven site ever fires** — the per-site counter says
so. The rest are in unrelated natives (`Optional.orElseGet`, the stream
mappers, `COWAL.addAll`); they are listed here rather than patched blind,
because nothing measured reaches them and an unmeasured fix to nine sites is
nine chances to break something.

## The `--Xmx 256m` failure is still open, and has two faces

On the **fixed** binary, `--Xmx 256m` (default collector) still fails at
**845 s** — close to the 823 s this page recorded, but with a different
message:

```
NoSuchMethodError: 'boolean <unknown class 2460030832>.equals(java.lang.Object)'
  at org/h2/test/store/TestRandomMapOps.assertEquals(TestRandomMapOps.java)
  at org/h2/test/store/TestRandomMapOps.testOps(TestRandomMapOps.java:162)
```

Same family as the G1 one — a receiver read at an address that no longer holds
the object it was — but a **different site and a different face**: a garbage
class id rather than `ClassId(0)`, which is what a recycled address looks like
once something else has been allocated over it. `CRATONVM_DBG_COLL_REFRESH`
reports **zero** engagements on that run, so the path fixed above is not
involved at all.

Two faces (`NullPointerException: "d" is null` at 823 s, this one at 845 s) at
nearly the same point is itself the finding: the recorded message is not the
lever, the same way the recorded seed was not.

**And it is not a reliable reproducer either.** A third 256 m run, with the
receiver dump armed, reached its **1500 s** cap without failing at all. So the
base rate on this arm is roughly one occurrence in two or three ~850 s runs,
and no clean arm shorter than that says anything about it — the same trap this
page already documents for the `ClassCastException`. Anyone taking this row on
should budget for the base rate first, or find a smaller reproducer, rather
than reading a single quiet run as progress.

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

**It reproduces locally on Windows, in minutes, with no Azure host.** The H2
tree under `apps/h2database/h2` is already compiled into `temp/` and
`cp_abs.txt` is the classpath; the G1 arm failed at 155-178 s on this laptop.
The host commands below still work, but nothing here needs them:

```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home "<jdk-25>" --Xmx 1g -XX:+UseG1GC     -c "$(cat cp_abs.txt)" org.h2.test.store.TestRandomMapOps
```

`CRATONVM_DBG_COLL_REFRESH=1` prints every receiver move the pinning absorbs,
and `CRATONVM_DBG_CCE_BT=1` dumps the offending receiver's shape, frame stack
and move history at the dispatch miss -- the two instruments that settled this.

The class itself:

```bash
source /data/toolchain/env.sh
cd /data/cratonvm/apps/h2database/h2
CP="target/classes:target/test-classes:$(cat craton-testcp.txt)"

# the G1 failure, FIXED 2026-08-24 -- this arm used to die in 41-155 s
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
