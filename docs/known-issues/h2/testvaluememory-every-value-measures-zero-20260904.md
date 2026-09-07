# H2 `TestValueMemory`: `System.gc()` under `-XX:+UseGenerationalGC` leaves 7 MB where HotSpot leaves 0.5

*2026-09-04, re-diagnosed 2026-09-06. The original title was "every value
measures zero"; that framing is refuted below and kept only as history.*

## Status

**OPEN, and collector-specific.** On dev `8d83c7585`:

| arm | result |
|---|---|
| CratonVM, default collector | **PASS 4/4**, 11 s, all 40 types |
| CratonVM, `-XX:+UseGenerationalGC -Xmx2g` | **FAIL 3/3**, at Type 0 |
| HotSpot JDK 25 | PASS, all 40 types |

The failing assertion is `TestValueMemory.testType`:

```java
long first = Utils.getMemoryUsed();
... build ~1 MB of values, then drop the list and the map ...
System.gc(); System.gc();
long used = Utils.getMemoryUsed() - first;   // KB
memory /= 1024;                              // KB, "calculated"
if (used > memory * 3) fail(msg);
```

and the numbers for Type 0, threshold `3 x 976 = 2928`:

| arm | `Used memory` (KB) | `calculated` (KB) | ratio | verdict |
|---|---:|---:|---:|---|
| HotSpot | 488 | 976 | 0.5x | pass |
| CratonVM default | 2228 | 976 | 2.3x | pass, with little room |
| CratonVM generational | **7018** | 976 | **7.2x** | **FAIL** |

Type 0 is `Value.NULL`, so the 125,000 values are 125,000 references to the
`ValueNull.INSTANCE` singleton. What is actually dropped before the two
`System.gc()` calls is an `ArrayList` of 125,000 pointers and an
`IdentityHashMap` of the same, roughly 6 MB of garbage. **HotSpot reclaims it;
the generational collector retains it across two explicit full collections.**
The default collector reclaims most of it but still holds 4.5x what HotSpot
does, which is why it passes at 2.3x against a 3x threshold — a thin margin, and
worth its own look.

## What the original page claimed, and why each part is wrong

The page was built on the `real: 0` column and none of it survives contact with
the source.

**"Every value measures `real: 0`" is not a CratonVM behaviour.** HotSpot prints
byte-for-byte the same values:

```
type: 39 calculated: 32   real: 0     <- HotSpot AND CratonVM
type: 40 calculated: 1972 real: 0     <- HotSpot AND CratonVM
type: 41 calculated: 1706 real: 0     <- HotSpot AND CratonVM
```

**That line asserts nothing.** It is built and handed to `trace(s)` in the first
loop of `test()`. Only `testType()` can fail the test, and it never looks at
`real:`.

**`real:` comes from `MemoryFootprint.getObjectSize(v)`**, and the page's claim
that "the string is not in `TestValueMemory`'s own bytecode -- it comes from
elsewhere in H2" is false: it is at
`src/test/org/h2/test/unit/TestValueMemory.java:104`. The file's own header
comment says `// run using -javaagent:ext/h2-1.2.139.jar`, which is what an
`Instrumentation.getObjectSize` path needs; without the agent it answers 0 on
every JVM.

**`Utils.getMemoryUsed()` was ruled out on the wrong grounds.** The page's
`HeapUsedDelta` probe showed `totalMemory()-freeMemory()` tracks retention on
CratonVM, concluded the pair was innocent, and moved on. But that pair *is* the
measurement in the failing assertion — `Utils.getMemoryUsed()` is
`collectGarbage(); (totalMemory()-freeMemory()) >> 10`. The probe was right that
the pair reports growth faithfully; it does not follow that the number it
reports is small enough to pass, and here it is not. A function that reports
correctly can still report a failing value.

**Identity is NOT involved.** `length: 125000 size: 1` looks alarming — 125,000
objects into an `IdentityHashMap` yielding one entry — and it is identical on
HotSpot, because they are 125,000 references to one singleton. Every
`length`/`size` pair matches across all three arms (`4996/4971`, `4977/4888`,
`6821/6821`, ...). Nothing is collapsing.

## Reproducer

The original reproducer is correct and is what re-opened this; it must be run
**as written**, with the generational collector. Under the default collector the
test passes and the page looks closed.

```bash
H2=<h2 checkout>
CP="$H2/target/classes:$H2/target/test-classes:$(cat $H2/craton-testcp.txt)"
cratonvm --java-home "$JDK25" -XX:+UseGenerationalGC -Xmx2g -cp "$CP" \
    org.h2.test.unit.TestValueMemory
```

`rc=1` in about a second, with

```
AssertionError: Type: 0 Used memory: 7018 calculated: 976 length: 125000 size: 1
```

Both `target/classes` and `target/test-classes` are required and are not in
`craton-testcp.txt`, which is the Maven dependency path only; omitting them
exits in under a second with `class not found: org.h2`, which reads exactly like
a fast failure of the test.

## H2 is not needed to see it

The minimal probe below removes H2, the value types, and `MemoryFootprint`
entirely. Allocate 125,000 objects into an `ArrayList` and an
`IdentityHashMap`, drop both, collect, and read the same metric H2 asserts on.

```java
import java.util.ArrayList;
import java.util.IdentityHashMap;

public class GenRetentionProbe {
    static long usedKb() {
        System.gc();
        Runtime r = Runtime.getRuntime();
        return (r.totalMemory() - r.freeMemory()) >> 10;
    }
    public static void main(String[] args) {
        int n = Integer.parseInt(args[0]);
        long before = usedKb();
        ArrayList<Object> list = new ArrayList<>();
        for (int i = 0; i < n; i++) { list.add(new Object()); }
        IdentityHashMap<Object, Object> m = new IdentityHashMap<>();
        for (Object o : list) { m.put(o, o); }
        long peak = usedKb();
        m.clear(); m = null; list = null;
        System.gc(); System.gc();
        long after = usedKb();
        System.out.println("before=" + before + " peak=" + peak
            + " after=" + after + " retained=" + (after - before));
    }
}
```

`GenRetentionProbe 125000`, `-Xmx2g`, same host and binary (`8d83c7585`). Note
that `usedKb()` itself collects, so `after` is a post-GC reading taken after two
further explicit `System.gc()` calls:

| arm | before | peak | after | retained (KB) |
|---|---:|---:|---:|---:|
| HotSpot | 1757 | 6309 | 1246 | **-511** |
| CratonVM default | 1435 | 8735 | **8735** | **+7300** |
| CratonVM `-XX:+UseGenerationalGC` | 1382 | 16067 | **23341** | **+21959** |

## Minimised 2026-09-06: these are TWO defects, and only one is a leak

The single-shot table above cannot tell a collector that reclaims nothing from a
metric that never falls. Looping the workload separates them. Six shapes at
`n=125000` (`full` = list+map, `list`, `map`, `array`, `churn` = allocate and
never store, `bytes`), then the same shapes repeated round after round.

**HotSpot returns to baseline in every shape** — `retained=-512` for all six.

### The default collector does NOT leak. Its `usedKb` simply never falls.

| loop | rounds | `usedKb` |
|---|---|---|
| pure churn, `-Xmx256m` | 40 | **1435, flat from round 0** |
| build-a-list-and-drop-it, `-Xmx256m` | 16 | rises once to **7843, then flat** |

A collector reclaiming nothing would climb ~3 MB per round and die inside a
256 MB heap. It plateaus instead, so it IS reclaiming. **This corrects the
sentence this section previously carried** — "on this workload the collection
reclaimed nothing at all" was wrong, and it was wrong because a single
before/peak/after triple cannot distinguish a plateau from a leak.

What is true is narrower and still worth fixing: `(totalMemory - freeMemory)`
does not come back down after a collection. It reports something closer to the
heap's high-water mark than to the live set, which is why `after == peak`
exactly, why the H2 test reads 2228 KB where HotSpot reads 488, and why it
passes only at 2.3x of a 3x threshold. H2's `Utils.getMemoryUsed()` is a
faithful caller of a JDK contract this VM answers loosely.

### The generational collector DOES retain, and it is the whole allocation

`ChurnLoop` allocates 125,000 immediately-dead `Object`s per round — nothing is
stored, there is no collection, no map, no array — and calls `System.gc()`:

| round | 0 | 5 | 7 |
|---|---|---|---|
| `usedKb` | 3431 | 13415 | 18279 |

**~2.1 MB per round, monotonic.** 125,000 x 16 bytes is ~2 MB, so essentially
100% of each round's garbage survives. It then degrades: 8 rounds finish
instantly, 40 rounds exceed a 300 s cap, 200 rounds exceed 900 s — the cost of
each collection growing with the set it fails to free. It was still thrashing
rather than throwing when the cap hit, so whether it ends in `OutOfMemoryError`
is untested.

`churn` is the minimal reproducer and it is much smaller than this page's
original: no H2, no value types, no collections, no dropped references. Just
allocation and `System.gc()`.

```java
public class ChurnLoop {
    static long usedKb() {
        System.gc();
        Runtime r = Runtime.getRuntime();
        return (r.totalMemory() - r.freeMemory()) >> 10;
    }
    public static void main(String[] args) {
        int rounds = Integer.parseInt(args[0]);
        int n = Integer.parseInt(args[1]);
        for (int k = 0; k < rounds; k++) {
            for (int i = 0; i < n; i++) {
                Object o = new Object();
                if (o == null) { System.out.print(""); }   // defeat elision
            }
            if (k % 5 == 0 || k == rounds - 1) {
                System.out.println("round=" + k + " usedKb=" + usedKb()
                    + " totalKb=" + (Runtime.getRuntime().totalMemory() >> 10));
            }
        }
        System.out.println("SURVIVED all " + rounds + " rounds");
    }
}
```

```bash
cratonvm --java-home "$JDK25" -XX:+UseGenerationalGC -Xmx256m -cp . ChurnLoop 8 125000
```

Run it without `-XX:+UseGenerationalGC` for the control: `usedKb` is flat at
1435 for 40 rounds. The `if (o == null)` is load-bearing — without a use, the
allocation is a candidate for elimination and the probe measures nothing (which
is how HotSpot's `list`/`map`/`array` rows come back at `peak` BELOW `before`:
it elides workloads whose result is never read, so those three HotSpot cells are
not a statement about HotSpot's collector).

## Localised 2026-09-06: it is `System.gc()` that leaks, not the collector

**The generational collector reclaims this garbage perfectly. It only fails
when the collection is requested by `System.gc()`.**

`NoGcChurn` is `ChurnLoop` with the `System.gc()` call removed, so collections
happen only under allocation pressure. Round 33, `-Xmx256m`:

```
round=32 usedKb=34017
round=33 usedKb=1632      <- a natural young collection, reclaiming everything
round=34 usedKb=4192
```

Against the three ways of triggering it:

| trigger | path taken | reclaims? |
|---|---|---|
| allocation pressure (natural young GC) | moving (Cheney) | **yes** — 34017 -> 1632 |
| `System.gc()` | non-moving sweep | **no** — +2.9 MB every round |
| `System.gc()` + `CRATONVM_DBG_FORCE_MOVING=1` | moving | **yes** — flat at 1284 |

### The disjunct responsible

`gen_heap.rs`'s `collect_garbage_inner`:

```rust
let divert_non_moving = (has_conservative_roots && !moving_young)
    || unrewritable_conservative_jit_roots
    || honor_promotion_oom_risk
    || divert_for_incomplete_moving_coverage
    || explicit_full_gc                       // <- major_gc_requested()
    || gpu_relocation_forbidden;
```

`explicit_full_gc` is `gc_quiescence::major_gc_requested()`, so **every**
`System.gc()` diverts to the non-moving sweep. On that path
`CRATONVM_DBG_HEAP_TRACE=1` shows the sweep doing nothing at all:

```
[HEAP-TRACE] cycle=0 young_used=3513392  young_free_list=0 bytes_freed=0 objects_copied=0
[HEAP-TRACE] cycle=5 young_used=17931872 young_free_list=0 bytes_freed=0 objects_copied=0
```

and `CRATONVM_DBG=sweep-census` prints nothing, meaning `dead_regions` is
EMPTY — the walk classified all 125,000 dead objects as live.

### This divert cannot simply be deleted

Its comment states the reason: *"System.gc() requests an old-gen-inclusive
cycle. Route that cycle through the non-moving marker so it can follow
collection-overlay edges from live owners instead of globally rooting every
overlay."* Removing `explicit_full_gc` from the disjunction would take the
moving path and lose that, which is a correctness regression in exchange for a
retention one. The fix belongs in the non-moving sweep, or in a narrower
condition — not in dropping the term.

### Ruled out, each by measurement

* **The parallel sweep.** `CRATONVM_DBG=sweep-zero` disables it; the numbers are
  byte-identical to baseline (3431/6247/9063/11879/14695 both ways).
* **JIT frames.** `--nojit` still leaks — so this is not the
  "conservative JIT roots force a non-moving sweep" case, even though that is
  what the non-moving path was built for.
* **Compact TLAB allocation.** `CRATONVM_GC=-compact-tlab-alloc` still leaks.

## Mechanism, 2026-09-07: the sweep UNWINDS its own reclamation

The `MARKWHY` census answers its own question, and it is the second of the two:
not "everything classified live", but **spans collected and then thrown away**.
Armed with `CRATONVM_DBG_YOUNG_MARK_WATCH` (see below), one cycle reads:

```
walk ended:  cursor=0x259c30 used=0x359c30 objects_live=10286 dead_regions=0
disposition: side_marked=10286 forwarded=0 header_marked=0
             dead_pushed=294 unwinds=2 sites=[0, 2, 0, 0]
             unwound_entries=295 dead_regions_final=0 side_sorted=10289
sweep done:  objects_swept=0 bytes_swept=0 dead_regions=0 reclaimed_regions=0
```

Three things there:

* `objects_live=10286`, not 125,000. **The mark is correct** — it is the JDK's
  own live set, and the churn is not in it.
* `dead_pushed=294`, `unwound_entries=295`, `dead_regions_final=0`. The walk
  FINDS the dead spans and then discards every one.
* `cursor` stops at `0x259c30` against `used=0x359c30` — a **1 MiB tail never
  walked at all**.

`sites=[0, 2, 0, 0]` is index 1, the *oversized-object clamp*, whose unwind is:

```rust
// A zero span is anomaly evidence like any other: the walk can only claim
// `cursor` is on-grid if every stride since the last anchor was correctly
// sized ... Unwind the reclaim decisions collected since the anchor
// (over-retention is always safe), then re-anchor at the next free block,
// or stop if no anchor remains.
mw_unwinds += 1;
mw_site[1] += 1;
mw_unwound_entries += dead_regions.len() - dead_watermark;
dead_regions.truncate(dead_watermark);
```

and its warning names the spans:

```
non-moving sweep: unlisted all-zero span at offset 948480  (run 58752 bytes,   next anchor at 3513392)
non-moving sweep: unlisted all-zero span at offset 1416240 (run 1941248 bytes, next anchor at 3513392)
non-moving sweep: unlisted all-zero span at offset 2464816 (run 892672 bytes,  next anchor at 3513392)
```

Runs of 0.9–2.0 MB — far too large for a TLAB tail, and ~2 MB is exactly
125,000 x 16 bytes, the churn itself.

### It is the FIELD-LESS object

`ChurnKind` allocates the same count three ways, four rounds each:

| churn | round 0 -> 3 | verdict |
|---|---|---|
| `new Object()` (no fields, never read) | 3431 -> 11879, +2.8 MB a round | **leaks** |
| `new int[2]`, `o[0] = i` written | 4455 -> 7527 -> 7527 | plateaus |
| `new StringBuilder()`, appended | 19625, flat from round 0 | plateaus |

**Only the object with nothing written into it leaks.** A run of them is a run
of bytes the walk cannot parse as objects, so it reads as "unlisted all-zero
span", and one such span discards the whole cycle's reclamation.

That also explains the H2 test exactly: `TestValueMemory` Type 0 is
`ValueNull.INSTANCE`, and its 125,000 entries are references, so the arena fills
with small short-lived objects the sweep then refuses to parse.

### 2026-09-07: one half fixed and MEASURED, the leak still open

**Candidate (1) below is refuted.** `new Object()` DOES write a header; it is
simply all zero. The predicate's own note says so — `MARK_NEUTRAL`,
`ObjectKind::Object` and `ArrayElementType::Reference` all encode as `0`, "so a
freshly built empty object is wholly zero". There is nothing missing at the
allocator. Do not go looking for an elided header store.

What was wrong is one condition inside the empty-object-run recovery that
already exists for this shape (added 2026-08-12). It accepted 144 runs and
refused 13, and every refusal was the same one:

```
young_sweep_zero_refusals: misaligned=0 live_inside=17 implausible_next=0
```

A refusal unwinds every reclaim decision since the last anchor, so one live
object interleaved in the churn cost the whole cycle. `zero_run_verdict` now
resumes AT that live base instead — it comes from `side_sorted`, so it is a
marked object base by construction, more firmly on-grid than the `resume:
run_end` the function already returned.

Measured, same binary and probe:

| | before | after |
|---|---|---|
| `zero_spans` (unwind-forcing) | 13 | **0** |
| `live_inside` refusals | 17 | **0** (`live_resumes=10`) |
| `bytes_freed` per cycle | **0** | 197 784 -> 883 624 -> 883 696 |
| `young_free_list` | 0 | 197 784 -> 1 081 296 -> 1 178 448 |
| growth per round | +2.88 MB | +2.05 MB |

**Reclamation went from nothing to ~880 KB a cycle, and the leak is still
open.** That is the honest reading: the unwind was real and is gone, but it was
not the whole cause.

### What is left, and the hazard in it

The accepted empty-object run is **stepped over and RETAINED**, by design —
`cursor = resume; continue;` frees nothing. The churn is ~2.88 MB a round of
exactly that shape, so retaining it is the remaining 2.05 MB.

Reclaiming those runs is the rest of the fix, and it is not a one-liner:

* the span is bounded below by `cursor`, which is on-grid, and above by a marked
  live base — no live base lies strictly between, which the predicate checks;
* but a live object may START exactly at `cursor` and extend into the run. The
  check is `side_sorted[j] > base + cursor`, strictly greater, so a live base AT
  `cursor` is not "inside" — and freeing from `cursor` would free that live
  object. A field-less live object is itself wholly zero, so this is not a
  hypothetical shape;
* `!vouched_live` at the call site rules out the run being inside a vouched live
  object, which is a different guarantee from the one above.

So the reclaim must start after any live base at `cursor`, and wants its own
guard (`live_in_dead` already exists and is the right tripwire). "Over-retention
is always safe" is why the current code retains; trading that for a free is the
one change here that can corrupt a heap rather than grow one.

### The two candidate fixes, and which is which

1. **At the allocator.** If a bare `new Object()` can reach the heap with no
   parseable header, the collector cannot walk its own arena — a GC must be able
   to parse every allocated object. Whether the header store is being elided for
   an object that is never read (the probe's `if (o == null)` never reads it) is
   the thing to establish first; `--nojit` still leaks, so this is not purely a
   JIT dead-store question.
2. **At the sweep.** Teach the zero-run arm that a span between two proved
   anchors, exactly divisible by the minimum object size, is a run of empty
   objects rather than a broken grid. Riskier: the unwind exists so a mis-sized
   stride cannot free a live object, and weakening it trades a retention bug for
   a corruption one.

(1) is the real defect if the header is genuinely absent. (2) should not be
attempted before (1) is answered.

### Arming the census

`CRATONVM_DBG_YOUNG_MARK_WATCH=0xffffffffffff`. The gate is
`w != 0 && w >= from_base`, so the value must be ABOVE every from-space base —
`=1` looks like it should work and does not (it only reaches the `sweep enter`
line, which has its own gate). Any huge value arms the two per-cycle census
blocks under ASLR without naming a victim; the per-object paths compare
`w == addr` and never match it.

A caution that census's own code carries, and that applies to the table above:
one of its counters is written only in a later loop, so reading it early "prints
a structural zero on every run" and that zero "was read as *this sweep reclaimed
NOTHING* — a whole-VM reclamation failure inferred from a counter not yet
written". The claim here does not rest on a counter: `young_used` is the arena's
own used figure, it climbs monotonically, and the process degrades into GC
thrash. But anyone extending this table should keep that trap in view.

The default collector's metric is a separate, smaller fix: make
`freeMemory()` answer from the live set after a collection rather than from the
allocation high-water mark. Nothing leaks today, so this is a reporting
correctness item — but it is what puts the H2 test at 2.3x of its 3x threshold,
one regression away from failing.

## The rest of the corpus, as of 2026-09-04

| class | CratonVM | note |
|---|---|---|
| `store.TestCacheLIRS` | `rc=0` 12.9 s | fixed since 09-03 |
| `unit.TestBitStream` | `rc=0` 5.7 s | |
| `store.TestObjectDataType` | `rc=0` 0.8 s | |
| `store.TestDataUtils` | `rc=0` 6.4 s | |
| `store.TestSpinLock` | `rc=0` 0.7 s | |
| `unit.TestIntPerfectHash` | `rc=0` 10.5 s | |
| `unit.TestStringUtils` | `rc=0` 0.6 s | |
| `unit.TestValueMemory` | `rc=1` generational only | **this page** |
| `store.TestMVStore` | `rc=124` (timeout, 200 s) | also fails on HotSpot |
| `store.TestMVRTree` | `rc=1` | also fails on HotSpot |

`TestMVStore` is worth a second look on its own terms: it fails an assertion on
HotSpot but **hangs** on CratonVM, and a hang and a failed assertion are not the
same defect. It is not an oracle either way.
