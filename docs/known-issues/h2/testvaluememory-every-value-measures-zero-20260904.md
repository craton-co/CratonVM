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

## Next step

The generational arm first — it is the real leak, and its reproducer is now four
lines. Two questions in order: does a young collection ever consider these
objects at all (they die before any promotion, so they should never leave the
nursery), and does `System.gc()` on this collector run a whole-heap mark or only
the young path. `[GC]` logging is gated behind `--verbose:gc` (see
`gc/src/zgc.rs`'s logging block) and is the first place to look.

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
