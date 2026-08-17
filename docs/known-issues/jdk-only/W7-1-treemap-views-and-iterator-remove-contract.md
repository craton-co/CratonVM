# Four families the widened differential found: TreeMap views, `Iterator.remove`, `String.format` floats, and a stream that kills the run

> **CLOSED 2026-08-12 — ALL FOUR FAMILIES VERIFIED GREEN. The differential was
> re-taken, which is the thing every earlier pass of this record said it needed
> and could not do.**
>
> `probes/ShadowDifferentialProbe.java`, HotSpot 25.0.3+9 as oracle against
> CratonVM `--real-jdk`, same class files, same image, on this Windows host:
> **864 lines each, 863 identical.** Every row of both acceptance tables at the
> top of this record now matches, including the two families this record left
> untouched and handed to other lanes:
>
> | acceptance row | HotSpot 25 | CratonVM `--real-jdk` |
> |---|---|---|
> | `TreeMap.descendingMap()` | `{d=4, c=3, b=2, a=1}` | **same** |
> | `TreeMap.descendingKeySet()` | `[d, c, b, a]` | **same** |
> | `headMap("c").remove("a")` writes through | `{b=2, c=3, d=4}` | **same** |
> | `TreeMap.pollFirstEntry()` | `b=2` | **same** |
> | `iterator().remove()` before `next()` | `IllegalStateException` | **same** |
> | `Iterator.removeTwice` | `IllegalStateException` | **same** |
> | `ListIterator.set`+`add` | `[B, B2, c, d]` | **same** |
> | `for (x : list) list.add(x)` | `ConcurrentModificationException` | **same** |
> | `String.format("%.3f\|%e\|%g", …)` | `0.333\|1.234500e+03\|0.000100000` | **same** |
> | `new StringBuilder("ab").delete(5, 6)` | `StringIndexOutOfBoundsException` | **same** |
> | `IntStream.rangeClosed(1,5).summaryStatistics()` | prints the stats | **prints the stats** |
>
> `SECTION-DIED.streamsSurface` is gone. **The two `ListIterator` follow-up
> questions this record left open are answered by the same run:** the
> `[B, B2, c, d]` row was a cascade of the `lastRet` defect exactly as §"Family 2"
> predicted — there is no second `set`/`add` defect to file.
>
> **The ONE surviving divergence in 864 lines, and it belongs to W7-2, not here:**
>
> ```text
> HotSpot   stream.summaryStats=IntSummaryStatistics{…, average=3,000000, …}
> CratonVM  stream.summaryStats=IntSummaryStatistics{…, average=3.000000, …}
> ```
>
> The host default locale is `ru_RU`; `%f` renders a comma there. That is
> W7-2 §5's declared KNOWN GAP in `p56_format_java_f`, now measured live rather
> than predicted. Filed against W7-2, not against this record's four families.
>
> **Read the encoding before reading the diff.** A raw `diff` of the two
> transcripts also shows `String.strip`, `Character.toUpperCase`,
> `format.charFromSupplementary` and `format.localeFrance` differing. Those are
> **not** VM divergences: `java` writes stdout in the console codepage on this
> host and CratonVM writes UTF-8. Re-running the oracle as
> `java -Dstdout.encoding=UTF-8` collapses all four. A pass that reported them
> as four new defects would have been reporting its own terminal.
>
> **Still open, unchanged and deliberately:** `sort`/`replaceAll` do not bump
> `modCount`, and the view cache has no version stamp. Both reasons below still
> hold — and the first one's stated blocker is now DISCHARGED, since the CME
> machinery has been executed and the differential's CME row is green. It is a
> `al_bump_mod_count` helper plus two call sites; take it with a Spring
> Boot/Tomcat arm, as the residual section says.
>
> **What this run does NOT clear:** it is `--real-jdk` only, and it is one
> binary of unpinned provenance (a default-feature release build dated
> 2026-08-12 15:27, behaviourally confirmed to contain `TmViewSpec`, the
> primitive-stream terminal wiring and the `LogRecord` retirements). It
> **predates commit `67146db71`** (17:49), which changed no-`Locale`
> `String.format` — so the `format.*` rows above are the pre-locale-fix
> behaviour agreeing with HotSpot, and the four `format.*` rows should be
> re-read on a later binary before this record is called finished. `probes/`
> is never run by `regression-suite/run.sh` at any `SUITE=` value — grepped
> again today — so **no suite run schedules this evidence**; re-taking the
> differential stays a by-hand step. The scheduled cover for the collection
> half is `regression-suite/src/RJdkViews.java` (`CORE_CLASSES`), which passes
> 107/107 in all three arms today.

**Status: families 1 and 2 CHANGED 2026-08-11, NOT VERIFIED. Families 3 and 4
still OPEN and untouched.** Nothing below has been built or run — the source
changes landed on `fix/jdk-only-treemap-views-and-iterator-contract-20260811`
and the differential has not been re-taken. See "What landed" at the end for
what changed, what deliberately did not, and what still needs measuring.

*(Status line above superseded by the 2026-08-12 block. Kept as written: it is
the record of what was believed before the differential was re-taken.)*

Found 2026-08-10 by the widened
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

## What landed, 2026-08-11 — families 1 and 2 only

All of it in `native-collections/src/lib.rs`. **Not built, not run, not
verified.** No row of either table above has been re-measured.

### Family 1 — the navigable views are now genuinely backed

The view is a real `java.util.TreeMap` object whose own storage is a CACHE, not
the truth. A `TmViewSpec` side table names the backing map, the range and the
direction, and `tm_sync_native_state` — the funnel every TreeMap content native
already called on entry — rebuilds the cache from the backing map before that
native reads it.

That shape is what made the change small. Every READ native (`get`, `size`,
`firstKey`, `ceilingEntry`, `keySet`, `entrySet`, `values`, `forEach`,
`toString`, the iterator) became view-correct **without being edited**, because
they all already went through that one call. Only the mutators needed a branch:

  * `put` — redirects to the backing map, and raises `IllegalArgumentException`
    for a key outside the range, as `NavigableSubMap.put` does;
  * `remove` — redirects, and answers `null` (not an exception) for an
    out-of-range key, as `NavigableSubMap.remove` does;
  * `clear` — deletes the view's entries **from the backing map**;
  * `pollFirstEntry` / `pollLastEntry` — take the end entry in the VIEW's order
    and delete its key from the backing map;
  * `putIfAbsent` — was the one conditional mutator writing the array itself
    (`computeIfAbsent` and `merge` already route through `get` + `put`, so they
    inherited the redirect for free).

The six range-view natives — `headMap`/`tailMap`/`subMap`, both arities — are
now one call each into a single `tm_new_range_view`, which is what stops their
bound-comparison edge cases from drifting apart again.

**`descendingMap()` and `descendingKeySet()` had no native registration at
all.** That is the whole of the first two rows: the call reached the real
`TreeMap` bytecode, which builds a `DescendingSubMap` over the `root` field, and
a natively-managed TreeMap never populates `root` — hence `{}` and `[]`. Both
are registered now (on `java/util/TreeMap` and on the `NavigableMap` interface),
as an unbounded view with `descending = true`. `navigableKeySet` was registered
alongside them: same missing-native shape, not measured by the probe.

A descending view stores `Collections.reverseOrder(sourceComparator)` in its own
comparator slot. That single field is what makes `firstKey`, `ceilingKey`,
`pollFirstEntry` and the binary search all agree with the view's iteration order
without a second code path for descending.

The spec table is wired into all four overlay GC hooks (root scan, post-move
remap, dead-key prune, recycled-identity sweep) exactly like
`snapshot_itr_backing_table`, because it holds three references — the backing
map and the two bound keys — that are reachable no other way.

### Family 2 — `Iterator.remove()` has a state machine

**The ArrayList half was a single missing store, and it explains four rows, not
two.** `alloc_arraylist_iterator` wrote `this$0` and `cursor` and left `lastRet`
at whatever `alloc_object` zero-initialises an `int` to — **0**, which
`native_al_itr_remove` reads as "`next()` returned index 0". So
`l.iterator().remove()` with no `next()` in front of it did not throw: it
deleted `l.get(0)`, and every later `next(); remove()` pair was one element out
of step.

That cascade is the whole of the `ListIterator.set` + `add` row.
`[B, B2, d]` against HotSpot's `[B, B2, c, d]` is not a `set`/`add` defect:
`set` and `add` were operating correctly on a list that the earlier
`Iterator.remove` had already left one element short. **Nothing was changed in
`set` or `add`.** If that row still diverges after a rebuild, it is a second,
genuinely separate defect and should be filed as one.

`ConcurrentModificationException` is now possible: `al_set_size` — the one
funnel every structural modification of an ArrayList-layout receiver passes
through — bumps the real `AbstractList.modCount`, the iterator seeds
`expectedModCount` from it at creation, and `next()`/`remove()` compare
(`hasNext()` deliberately does not, matching the JDK, which is why the failure
surfaces on the iteration after the offending `add`).

The `TreeSet$Itr` snapshot iterator gained a fourth slot, `lastRet`. Its old
guard was `cursor <= 0`, which catches `remove()` before any `next()` but not
`remove()` twice in a row: the snapshot does not shift when the backing set
loses an element, so the second call found the cursor unchanged and silently
re-deleted. The `--jdk-only` stand-in for that iterator (a real
`Arrays$ArrayItr` with `SnapshotItrBacking::last_removed_cursor`) already had
the equivalent and was already correct.

### What deliberately did NOT change

  * **Families 3 and 4 — `String.format` floats, `StringBuilder.delete` bounds,
    `IntStream.summaryStatistics()` killing the run.** Out of scope for this
    branch and untouched. `SECTION-DIED.streamsSurface` is still live.
  * **`HashMap$KeyItr`'s state machine.** It was already correct — `lastRet` is
    seeded to `-1` at every creation site and reset by `remove()`. It is not the
    broken snapshot iterator.
  * **`native_map_key_itr_next` returning `null` past the end** where the JDK
    throws `NoSuchElementException`. Real, adjacent, and not on either table
    above; left alone rather than widened into. **CLOSED 2026-08-12 — see the
    residual section at the end of this record.**
  * **`sort` / `replaceAll` do not bump `modCount`.** The JDK bumps there and we
    do not, so those two remain undetected. Under-reporting is the safe
    direction — it is exactly the pre-fix behaviour — where a spurious bump
    would fail a loop the real JDK runs to completion. **Still open on purpose;
    the residual section at the end says why the 2026-08-12 pass did not take
    it either.**
  * **No version stamp on the view cache.** It is rebuilt on every operation.
    A stamp has to be bumped at every site that mutates a TreeMap's contents,
    and a site missed there is a silently stale view — the exact defect class
    this record is about.

### What still needs measuring

  1. **Re-take the differential.** Nothing here is verified. The eight rows
     above are the acceptance list; the `String.format` and stream rows must
     still fail.
  2. **Does the CME change break a workload?** This is the one change with real
     blast radius: code that mutates a list while iterating it used to get away
     with it on CratonVM and now will not. Correct code cannot trip it (HotSpot
     would already have thrown), so the risk is *our own* natives performing a
     structural modification behind a user's iteration. Spring Boot and Tomcat
     are the arms that would show it.
  3. **View cost on a RETAINED view.** `tm.headMap(k)` used to cost one
     allocation plus N comparator-driven `put`s at CREATION (O(N log N)), where
     a rebuild is one allocation plus N bound comparisons — so the common
     build-and-discard shape should be no worse, and probably better. A view
     that is held and read repeatedly is the case that got more expensive, and
     nobody has measured how common that is.
  4. **A descending view in an image with no usable `java.util.Collections`**
     falls back to the source comparator and therefore iterates ASCENDING rather
     than failing. That is the same trade `native_ts_descending_set` already
     makes; it has not been exercised.
  5. **`descendingMap().headMap(k)` and the other view-of-a-view compositions.**
     Bounds are recorded in the immediate source's ordering and a view of a view
     records its parent, so composition should fall out without interval
     arithmetic. Untested — the probe does not build one.
  6. **`TreeSet$Itr` is now allocated with four fields instead of three.**
     `native_ts_itr_remove` falls back to the old cursor heuristic for a
     3-field shape, so a foreign construction path would degrade rather than
     read past the object, but no such path is known to exist.

## The three residuals, 2026-08-12 — one closed, two left with a reason

Source-only again. Nothing here was built or run.

### `native_map_key_itr_next` past the end — CLOSED

`native-collections/src/lib.rs`, `native_map_key_itr_next`. The exhausted branch
raised `NoSuchElementException("No more elements")` instead of answering
`Value::Object(None)`.

`null` was the worst answer available and for the reason this record keeps
finding: **a null is also a legitimate ELEMENT of a `HashMap` key set**, so a
caller that over-ran its own `hasNext()` received something it could not tell
from a real entry, and failed one or more frames away from the mistake. The
message is `native_snapshot_itr_next`'s verbatim — that function is the other
half of this iterator family (it *delegates here* for a `HashMap$KeyItr`
receiver, and already threw exactly this on all three of its own exhausted
paths), so the two cannot drift into two different reports.

**This makes a call throw that previously returned.** The only in-tree caller is
`native_snapshot_itr_next`'s delegation; every out-of-tree caller is bytecode
written against the real `HashMap$KeyIterator`, which throws here, so correct
code cannot reach it. Covered by `RJdkViews.keyIteratorExhaustion`.

### `sort` / `replaceAll` and `modCount` — NOT TAKEN, and not by oversight

The change is small and it is spec-correct: `ArrayList.sort` and
`ArrayList.replaceAll` both `modCount++` in the JDK, `al_set_size` is the only
funnel this crate has and neither changes the size, so it takes a
`al_bump_mod_count` helper plus two call sites.

It was not made because **the comodification machinery it would extend has
still never been executed.** Everything under "Family 2" above is source that
no build has seen; the record's own "What still needs measuring" item 2 asks
whether the *existing* CME change breaks a Spring Boot or Tomcat workload, and
that question is open. Stacking a second, wider set of bumps onto an unverified
first set is the merge-time hazard the campaign records as "concurrent fix
combination untested": if the arms then go red, no bisect separates the two.
Take this one *after* the first CME measurement, not with it. The vector is one
line — a `for (String s : list) list.sort(..)` beside `RJdkViews.failFast`'s two
existing bounded loops — and writing it now would have been a gate for code
nobody can run.

### The view-cache version stamp — NOT TAKEN

Unchanged from the reasoning above: a stamp has to be bumped at every site that
mutates a TreeMap's contents, and a site missed there is a silently stale view.
That is worse than the rebuild-every-time cost it replaces, and "did I find
every mutation site" is not a question a source read can answer honestly in a
file of this size. It needs the retained-view cost measurement (item 3) to
justify it at all.
