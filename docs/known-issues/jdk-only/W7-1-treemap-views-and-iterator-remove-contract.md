# Four families the widened differential found: TreeMap views, `Iterator.remove`, `String.format` floats, and a stream that kills the run

**Status: OPEN.** Found 2026-08-10 by the widened
`probes/ShadowDifferentialProbe.java`, in **`--real-jdk` mode** — so this is a
compatibility defect, not a strict-mode one. It is the first thing that probe
found after being widened past `java.util`'s immutable factories, which is the
point the widening was for.

## What diverges

Measured against HotSpot 25 on Linux, same class files, same image:

| observable | HotSpot 25 | CratonVM `--real-jdk` |
|---|---|---|
| `TreeMap.descendingMap()` | `{d=4, c=3, b=2, a=1}` | `{}` |
| `TreeMap.descendingKeySet()` | `[d, c, b, a]` | `[]` |
| `tm.headMap("c").remove("a")`, then `tm` | `{b=2, c=3, d=4}` | `{a=1, b=2, c=3, d=4}` |
| `TreeMap.pollFirstEntry()` (after the above) | `b=2` | `a=1` |
| `list.iterator().remove()` **before** `next()` | `IllegalStateException` | `no-throw`, **and it removes the first element** |
| `list` after that iterator's `next(); remove()` | `[b, c, d]` | `[c, d]` |
| `ListIterator.set` + `add` on the same list | `[B, B2, c, d]` | `[B, B2, d]` |

and, once the probe was fenced so one section could not truncate the rest:

| observable | HotSpot 25 | CratonVM `--real-jdk` |
|---|---|---|
| `for (x : list) list.add(x)` | `ConcurrentModificationException` | no CME at all (the probe's own 100-iteration guard fires) |
| `String.format("%.3f\|%e\|%g", 1.0/3, 1234.5, 0.0001)` | `0.333\|1.234500e+03\|0.000100000` | `0.333\|1.2345e3\|1.0E-4` |
| `new StringBuilder("ab").delete(5, 6)` | `StringIndexOutOfBoundsException` | `no-throw` |
| `IntStream.rangeClosed(1,5).summaryStatistics()` | prints the stats | **kills the run** — reported as `SECTION-DIED.streamsSurface` |

Four distinct defect families. The `Iterator.remove` one is the dangerous one.

## 1. The navigable views are snapshots

`headMap` / `tailMap` / `subMap` / `descendingMap` / `descendingKeySet` are
specified as **views**: a write through one is a write to the map. CratonVM's
answer to `headMap(..).remove(..)` does not reach the backing map, and
`descendingMap()` / `descendingKeySet()` answer an EMPTY collection rather than
a reversed one.

`pollFirstEntry` is a cascade, not a third defect: HotSpot answers `b=2` only
because the `headMap.remove("a")` above it landed.

An empty view is the failure mode that reads as a pass everywhere a caller only
iterates — which is why `probes/JdkOnlyCollectionViewProbe` was written to report
CONTENT. That probe covers `subList` and the unmodifiable family; it does not
cover `TreeMap`'s navigable views, and this is what was behind that gap.

## 2. `Iterator.remove()` has no state machine

`Iterator.remove` is specified to throw `IllegalStateException` unless `next`
has been called since the last `remove`. CratonVM's snapshot iterators accept it
unconditionally, and — worse — **it removes an element anyway**. A
`remove()` before any `next()` deletes the first element, so a loop that guards
itself with the exception silently deletes one extra item per iteration instead
of failing.

`ListIterator.set`/`add` are wrong in the same direction: the list ends
`[B, B2, d]` where HotSpot has `[B, B2, c, d]`, one element short.

## Why this was not found earlier

`ShadowDifferentialProbe` exercised `java.util`'s immutable factories,
`Map.entry`, and the entry-set views — a few dozen observables against a census
that counts ~1,600 inherited shadows. The honest reading of "they match" was
always "the ones anybody looked at match", which is what the record it belongs
to said. Widening it to twelve further families found this on the first run.

## Reproducing

```sh
javac -d /tmp/probes probes/ShadowDifferentialProbe.java
java -cp /tmp/probes ShadowDifferentialProbe > /tmp/hotspot.txt
cratonvm --real-jdk --java-home "$JAVA_HOME" -cp /tmp/probes ShadowDifferentialProbe \
  | grep -v '^\[cratonvm\]' > /tmp/cratonvm.txt
diff /tmp/hotspot.txt /tmp/cratonvm.txt
```

## One probe hazard fixed on the way, worth stating

The first widened run stopped dead at the `ConcurrentModificationException`
section and printed nothing after it. `for (String s : l) { l.add(s); }` relies
on the very exception it is testing for to terminate: on a VM whose iterator
does not throw, it grows the list until the heap is gone and takes the rest of
the probe with it. **A differential that cannot reach its next line cannot
report a difference**, and a truncated transcript reads exactly like a short
clean run — the specific way the first two strict census runs lied. Both CME
cases are bounded now and report `no-CME-after-100` instead of hanging, and every
section is fenced: a section that throws prints one `SECTION-DIED.<name>` line
instead of removing every line after it from the transcript. That fence is what
turned `IntStream.summaryStatistics()` from "the probe stops at line 284" into a
named defect.
