# W7-36 — the collections families the round-2 differential left: views that do not write through, and refusals that never fire

> **VERIFIED AGAINST A BINARY 2026-09-02. The differential is ZERO.**
>
> ```text
> HotSpot 25              872 lines
> CratonVM --real-jdk     872 lines
> divergent observables   0        (this record's baseline: 43)
> ```
>
> W7-33's baseline was 43 and this record re-took it at 43. It is now **0**: the
> nineteen observables changed in source here, and everything the sibling lanes
> changed since, have closed the whole set.
>
> **The probe had to be recovered before it could be run.** This record's
> reproduce block points at `$SCRATCH/probesrc/ShadowDifferentialProbe.java` — a
> scratchpad path, not a tracked file — and `probes/ShadowDifferentialProbe.java`
> was deleted from the tree by `3b2901531` ("major doc consistency update before
> the release"), the same commit that removed `probes/` wholesale. Restored from
> `3b2901531^` (3002 lines) and re-added. A record whose only handle lives in a
> scratchpad has a handle that expires with the session.
>
> **The zero is trustworthy because `W7-42`'s ledger says so**, which is the
> whole point of that instrument:
>
> ```text
> PROBE-SECTIONS=28
> PROBE-LEDGER=missing:0,undeclared:0,duplicate:0,multiline:0,unrenderable:0
> ```
>
> Twenty-eight sections ran and nothing was lost, so the empty diff is agreement
> rather than a hole. Without that line a 0 here would be indistinguishable from
> a probe that stopped early — the exact failure `W7-42` was written to close.

**Status: 19 of 20 assigned observables CHANGED IN SOURCE 2026-08-12, NOT
REBUILT, NOT VERIFIED. 1 recorded and deliberately not attempted.**

> **Part 5, added later the same day**, discharges two of the three items this
> record left in "What was NOT changed": the five sorted-container refusals and
> `native_tm_get_or_default` — whose suspected defect turned out **not to
> exist**, while a different one at the same native did. Also source only.
> `stream.reuseThrows` (Part 4) is closed by `W7-65-stream-reuse-throws.md`.

Every "before" number below is an observation, taken by running the
already-built binary at `C:/craton/CratonVM/target/release/cratonvm.exe`
against Temurin `jdk-25.0.3.9-hotspot` on windows/x64 — one binary, the same
class files on both sides, `-Dstdout.encoding=UTF-8 -Duser.language=en
-Duser.country=US` pinned on both. **No "after" number exists.** Nothing on this
branch has been built, so every claim about the effect of a source change is a
claim about source and is labelled as one.

Branch: `fix/differential-view-families-20260812`.
Files changed: `native-collections/src/lib.rs`, `types/src/error.rs`, and this
record.

Predecessors: W7-32-round-2-differential-run (the 96-line differential),
W7-33-differential-dead-sections (the re-take at 43, and the three residuals
this record's first section discharges),
W7-1-treemap-views-and-iterator-remove-contract (the view machinery this reuses,
and the "silently stale view" warning it ends on).

---

## The measurement, and what a re-take can and cannot say

Reproduced the W7-33 baseline exactly — the tree's probe with the five
poisoning statements excised (the three `ArrayDeque` null adds,
`ArrayDeque.sizeAfterRefusedNulls`, `COW.addAllAbsent`), which is the only way
to reach the end of both sections on a binary that predates the `ArrayDeque`
null refusal:

```sh
javac -d $SCRATCH/probes $SCRATCH/probesrc/ShadowDifferentialProbe.java
java  -Dstdout.encoding=UTF-8 -Duser.language=en -Duser.country=US \
      -cp $SCRATCH/probes ShadowDifferentialProbe > hs.txt
./target/release/cratonvm.exe --real-jdk --java-home "$JDK25" \
      -Dstdout.encoding=UTF-8 -Duser.language=en -Duser.country=US \
      -cp $SCRATCH/probes ShadowDifferentialProbe \
  | grep -v '^\[cratonvm\]' | grep -v WARN > cv.txt
diff --strip-trailing-cr hs.txt cv.txt
```

| | divergent observables |
|---|---|
| W7-33 baseline | **43** |
| this branch, re-taken | **43** |

**The re-take is unchanged, and that is the expected result, not a failure:
the binary does not contain any of this branch's source.** It is worth stating
what the re-take DID establish, because it is not nothing. The main worktree's
binary was rebuilt by another lane between the two runs (`22:06` → `22:59`,
39 565 312 → 39 577 600 bytes), and the two transcripts are **byte-identical**.
So the 43 rows reproduce across two different builds of `dev`, and none of them
is a build artefact.

The only syntax check available without a build is `rustfmt --edition 2021
--emit stdout` over a scratch copy of each touched file. Both parse. That is a
parse check and not a type check, and it is the whole of the verification on
this branch.

---

## Part 1 — the three residuals W7-33 handed over

### R1 — `PriorityQueue.add(null)` returned `true`

| observable | HotSpot | CratonVM (before) |
|---|---|---|
| `PriorityQueue.nullAdd` | `java.lang.NullPointerException` | `no-throw` |

`java.util.PriorityQueue`: *"This queue does not permit null elements"*, and
both funnels declare `@throws NullPointerException if the specified element is
null`. `add` and `offer` are registered to the same native here exactly as `add`
delegates to `offer` in the JDK, so one check covers the surface. `ad_refuse_null`
is reused — only its name is `ArrayDeque`-specific, and the reason both classes
refuse is the same one: a null has no place in a comparison heap, and the very
next `siftUp` would dereference it.

### R2 — `Stack.pop()` on empty raised the wrong exception

| observable | HotSpot | CratonVM (before) |
|---|---|---|
| `Stack.popOnEmpty` | `java.util.EmptyStackException` | `java.util.NoSuchElementException` |

`java.util.Stack.pop`: `@throws EmptyStackException if this stack is empty`.
`EmptyStackException` extends `RuntimeException` **directly** — it is not a
`NoSuchElementException` and not a subtype of one — so a
`catch (EmptyStackException)` in application code never fired and the caller
fell through to whatever handler came next. **A mistyped refusal is worse than a
missing one, because it looks handled.**

W7-33 wrote the patch out; it was applied as written after checking it against
the source. `types/src/error.rs` gains a field-less
`RuntimeError::EmptyStackException` (field-less because the real class declares
only a no-arg constructor and sets no detail message) mapped in
`as_java_throwable` beside `ConcurrentModificationException`, the other
field-less `java.util` variant. `native_stack_pop` and `native_stack_peek` both
raise it; `peek` is not measured by the probe, but the two are one refusal in
the JDK (`pop` calls `peek` first) and leaving one behind is how the pair drifts
apart again.

**`match` arms over `RuntimeError` that had to be reached: exactly one.**
`as_java_throwable` in `types/src/error.rs` is the only exhaustive match over
the enum in the tree — `synthesised_detail_message` beside it ends in `_ =>
None`, and every other `RuntimeError::` mention outside that file is a
*construction*, not a pattern. Checked by grepping the rare field-less variants
(`ReadOnlyBufferException`, `BufferOverflowException`) tree-wide: every hit is
`return Err(RuntimeError::X.into())`. The new variant was also added to the
`variants_with_no_message_still_convert_to_none` test loop in the same file, so
the null-`getMessage()` property is locked rather than assumed.

**One follow-up this creates, in a file this branch does not own.**
`java/util/EmptyStackException` appears nowhere in the tree. In `--real-jdk`
mode that is fine — the real class file supplies its own superclass. Under
`--synthetic-jdk` the class would be fabricated, and `jdk_superclass` in
`classloading/src/class_manager.rs` has no arm for it, so it would default to
`java/lang/Object` and a `catch (RuntimeException)` would miss it. Two one-line
additions there close it (`jdk_superclass` → `java/lang/RuntimeException`, and
`synthetic_stub_fields` → `instance_fields(2)` beside `NoSuchElementException`).
This is **not a regression introduced here**: `BufferUnderflowException`,
`BufferOverflowException`, `ReadOnlyBufferException`, `IllegalCallerException`
and `PatternSyntaxException` are all existing `RuntimeError` variants in exactly
the same position. It is a pre-existing hole that now has one more resident.

### R3 — `CHM.reduceValues` / `CHM.searchKeys` answered `null`

| observable | HotSpot | CratonVM (before) |
|---|---|---|
| `CHM.reduceValues` | `6` | `null` |
| `CHM.searchKeys` | `found` | `null` |

Neither was registered, so both ran real bytecode. The cause is the one W7-33
suspected: every method in the bulk-operation family walks the real `table`
field through a `Traverser`, and a natively-backed `ConcurrentHashMap` keeps its
entries in the segmented side layout that never populates `table`. Ordinary
`entrySet` iteration of the same map was correct in the same run
(`CHM.sortedContent={a=6}` matches, and that is `new TreeMap<>(chm)`), which is
what pins the split to the traversal rather than to the map.

**`null` is the worst answer available for both.** It is also what an empty map
and a search that matched nothing legitimately return, so no caller can tell a
lost traversal from a real result. `forEach(long, BiConsumer)` was already
registered against this same failure.

Both now follow their `BulkTask`: `ReduceValuesTask` seeds with the first value
then folds, `SearchKeysTask` short-circuits on the first non-null. Both raise
`NullPointerException` for an explicitly-null function, as the real methods'
first statement does. `parallelismThreshold` is read and ignored, which is legal
— it is a hint, and a sequential evaluation is a correct answer for any value of
it.

**Deliberately still unregistered, and still silently answering null / doing
nothing:** `reduceKeys` (both arities), `reduceEntries`, `searchValues`,
`searchEntries`, `reduceKeysToInt/Long/Double`,
`reduceValuesToInt/Long/Double`, `reduceEntriesTo*`, the three-argument
transform overloads of `reduce*`, and `forEachKey`/`forEachValue`/`forEachEntry`
(both arities each). Same cause, same silence. They were not widened into
because nothing on this branch can be run.

---

## Part 2 — refusals that never fired

The campaign's dominant species. Each row names the exception the spec mandates
and the sentence that mandates it; **none of them is a refusal invented here**,
and each was confirmed against HotSpot on the same input before being added,
because W7-33's two dead sections were exactly the case of real code refusing
state we had corrupted rather than us over-throwing.

| observable | HotSpot | CratonVM (before) | raised now |
|---|---|---|---|
| `TreeMap.nullKeyPut` | `java.lang.NullPointerException` | `no-throw` | `NullPointerException` |
| `TreeMap.nullKeyGet` | `java.lang.NullPointerException` | `no-throw` | `NullPointerException` |
| `TreeSet.nullAddNaturalOrdering` | `java.lang.NullPointerException` | `no-throw` | `NullPointerException` |
| `TreeSet.incomparableFirstAdd` | `java.lang.ClassCastException` | `no-throw` | `ClassCastException` |
| `TreeMap.subMapReversedBounds` | `java.lang.IllegalArgumentException` | `no-throw` | `IllegalArgumentException` |
| `TreeMap.firstEntryIsImmutable` | `java.lang.UnsupportedOperationException` | `no-throw` | `UnsupportedOperationException` |
| `keySet.addUnsupported` | `java.lang.UnsupportedOperationException` | `no-throw` | `UnsupportedOperationException` |
| `values.addUnsupported` | `java.lang.UnsupportedOperationException` | `no-throw` | `UnsupportedOperationException` |
| `Map.mergeNullValueThrows` | `java.lang.NullPointerException` | `no-throw` | `NullPointerException` |
| `PriorityQueue.nullAdd` | `java.lang.NullPointerException` | `no-throw` | `NullPointerException` |
| `Stack.popOnEmpty` | `java.util.EmptyStackException` | `java.util.NoSuchElementException` | `EmptyStackException` |

The specifying sentences:

  * `PriorityQueue` — *"This queue does not permit null elements"*, plus
    `offer`/`add`'s `@throws NullPointerException if the specified element is
    null`.
  * `Stack.pop` / `Stack.peek` — `@throws EmptyStackException if this stack is
    empty`.
  * `TreeMap.put` / `TreeMap.get` — `@throws NullPointerException if the
    specified key is null and this map uses natural ordering, or its comparator
    does not permit null keys`.
  * `TreeSet.add` — `@throws ClassCastException if the specified object cannot
    be compared with the elements currently in this set`, and the same
    `NullPointerException` clause as `TreeMap.put`.
  * `NavigableSubMap`'s constructor — `throw new IllegalArgumentException("fromKey
    > toKey")`.
  * `AbstractMap.SimpleImmutableEntry.setValue` — `@throws
    UnsupportedOperationException always`.
  * `Map.keySet` / `Map.values` — *"It does not support the `add` or `addAll`
    operations"*; `AbstractCollection.add` is a bare `throw new
    UnsupportedOperationException()`.
  * `HashMap.merge` — `if (value == null || remappingFunction == null) throw new
    NullPointerException();`, javadoc `@throws NullPointerException if ... the
    value or remappingFunction is null`.

### The sorted-container pair is one cause with two halves

`TreeMap.nullKeyPut`/`nullKeyGet` and `TreeSet.nullAddNaturalOrdering`/
`incomparableFirstAdd` all come from the JDK's `compare(key, key)` — "type (and
possibly null) check" in its own comment. Under natural ordering every insert
and lookup runs `((Comparable) key).compareTo(other)`: the `checkcast` in front
of the call raises `ClassCastException` for a non-`Comparable` receiver, and the
invoke itself raises `NullPointerException` for a null one.

The two halves were missing for *different* reasons, which is why they looked
like one gap and were not:

  * The `ClassCastException` half already existed in `compare_via_compare_to` —
    but only once there is something to compare against. On an EMPTY container
    no comparison happens at all, which is precisely why the JDK writes the
    self-compare out by hand in `addEntryToEmptyMap`. **Only the first add
    diverged**, which is what the probe measured.
  * The `NullPointerException` half was missing everywhere, and deliberately:
    `compare_via_compare_to` orders `null` FIRST rather than throwing, because
    its other caller is `Arrays.sort(Object[])`, whose contract that is. A
    sorted map has the opposite contract, so the check had to live at the
    container natives, not there.

`tree_natural_order_key_check` is scoped to `comparator == null` because that is
where the JDK scopes it — `new TreeSet<>(Comparator.nullsFirst(..))`
legitimately holds a null, and a comparator over a non-`Comparable` element type
is the ordinary reason to supply one (this file already documents a
`Set<Class<?>>` keyed by a name-comparator). Refusing under a comparator would
be the over-throw. Its `ClassCastException` message is
`compare_via_compare_to`'s verbatim, so the two sites modelling the same
`checkcast` cannot drift into two different reports.

The `Comparable` walk runs **only when the container is empty**, matching the
JDK's placement. That also keeps a class-hierarchy walk off the fast-mode
`TreeMap` path, which performs no comparison at all today.

One risk worth naming: `implements_comparable` answers from
`ctx.class_interfaces`, and under `--synthetic-jdk` a fabricated
`java/lang/Enum` (and `java/util/Date`, `java/math/BigDecimal`, the `java.time`
types) declares no interfaces, so it would read as non-`Comparable`. That is
**not new** — `compare_via_compare_to` has used the same predicate for every
non-first comparison for months, so a synthetic-mode `TreeSet` of two enums
already fails. This moves the failure from the second add to the first.

### The exported entries were the mutable twin

All eight callers of `tm_make_entry` are the JDK's `exportEntry` sites —
`firstEntry`, `lastEntry`, `pollFirstEntry`, `pollLastEntry`, `floorEntry`,
`ceilingEntry`, `higherEntry`, `lowerEntry` — and `exportEntry(e)` is `new
AbstractMap.SimpleImmutableEntry<>(e)`, a snapshot. This allocated
`AbstractMap$SimpleEntry`. The two classes have the same two instance fields in
the same order (`javap -p` on both: `key`, `value`), so nothing about the direct
field stores changes; only which `setValue` the receiver resolves to.

This is measured, not assumed, in a useful way: the probe's `new
SimpleImmutableEntry.setValue` row **already matches** HotSpot's
`UnsupportedOperationException` on this binary, on a `SimpleImmutableEntry` the
program itself constructed. So the interface-level `java/util/Map$Entry.setValue`
native does not shadow that class's real bytecode, and allocating the same class
here should land on the same refusal.

`TreeMap.entrySet()`'s iterator yields the LIVE entry, whose `setValue` writes
through, and `TreeMap.entrySetEntryIsLive` already matched; that path allocates
its own `AbstractMap$SimpleEntry` through `alloc_live_entry` and is untouched.
Getting these two backwards in either direction is a divergence, so the split is
stated at `tm_make_entry` rather than left to the eight call sites.

`java/util/AbstractMap$SimpleImmutableEntry` needs nothing from
`classloading/`: it already has both a `jdk_interfaces` arm and a two-slot
`synthetic_stub_fields` arm, named on the same lines as `$SimpleEntry`.

### The two view `add`s land in the view, not the map

`keySet.addUnsupported` and `values.addUnsupported` were worse than a missing
throw. The element **landed in the view's own storage** — a `HashSet` over a
`MapViewBacking` for `keySet`, an `ArrayList` with a trailing source marker for
`values` — leaving a "view" that disagrees with the map it is a view of. That is
the same silent-divergence shape as the write-through rows in Part 3, reached
from the other side.

Both refusals key off the view markers that `native_al_remove_obj`,
`native_al_clear` and the iterators **already** consult to write removals
through, so no new notion of "is a view" is introduced. Neither can fire on a
plain collection: an ordinary `HashSet`'s backing is a real `java/util/HashMap`
with fewer than `VIEW_BACKING_FIELDS` slots, and a plain `ArrayList`'s trailing
capacity slot is null. Construction of either view is unaffected —
`make_view_set_of` populates its backing through `native_map_put` and never
reaches `native_hs_add`.

**Not widened to:** `addAll` on either view (`native_al_add_all` has its own
bulk fast path and does not loop through `native_al_add`), and
`TreeMap.keySet().add`, whose view is an array-backed `TreeSet` rather than a
`HashSet` and so takes a different native. Both are `UnsupportedOperationException`
in the JDK and both still return normally here.

---

## Part 3 — views that did not write through

### `unmodifiableList` is idempotent by identity

| observable | HotSpot | CratonVM (before) |
|---|---|---|
| `unmodifiableList.rewrapIsNewObject` | `true` | `false` |

`Collections.unmodifiableList` opens with `if (list.getClass() ==
UnmodifiableList.class || list.getClass() == UnmodifiableRandomAccessList.class)
return (List<T>) list;`. We minted a second wrapper. A second wrapper is not
merely wasteful: it is a different object, so a caller that re-wraps defensively
and then compares by identity (or uses the result as a map key) sees two lists
where the JDK has one, and every layer costs another delegation hop on every
read.

The test is exact class identity, as the JDK's is, and the immutable marker
**excludes** rather than includes: a `List.of(...)` shares the wrapper class
here but reports `ImmutableCollections$ListN` from `getClass()`, and the JDK
does wrap that one — its two `==` comparands are the `Collections$Unmodifiable*`
classes only.

### The nested `subList` was a view of a snapshot

| observable | HotSpot | CratonVM (before) |
|---|---|---|
| `subList.nestedClearWritesThroughToBase` | `[a, B, e]` | `[a, B, c, d, e]` |
| `subList.afterNestedClear` | `[B]` | `[B, c, d]` |
| `subList.sortWritesThrough` | `[9, 1, 5, 7, 3]` | `[9, 5, 1, 7, 3]` |

`subList` ON an `ArrayListSubList` was registered as `asl_delegate_snapshot`,
which materialises the slice into a fresh `ArrayList` and returns a view of
THAT. Reads were right; every write went into the throwaway.

`native_asl_sub_list` composes against the ROOT list — `offset =
enclosing.offset + fromIndex` — which is the choice that made the change small:
`ASL_FIELD_PARENT` stays the root with an absolute offset, so every existing
`asl_*` native works on a nested view **without being edited**, the same
property that made W7-1's `tm_sync_native_state` funnel worth building.

One thing the root cannot give, and it needs a new field. A structural mutation
through a nested view has to adjust the ENCLOSING views' `size`/`expected`, or
they immediately read as comodified — HotSpot answers `[B]` for `sub` after
`nested.clear()`, not a `ConcurrentModificationException`. `ASL_FIELD_VIEW_PARENT`
records the enclosing view and `asl_delegate_mutating` walks the chain, which is
what the JDK's `SubList.updateSizeAndModCount` does up its own `parent` chain.
**Ancestors only**: a SIBLING view over the same parent is genuinely comodified
and must keep failing, which is exactly what its now-stale `expected` does.

The field is an object field, so it is GC-scanned as part of the object — the
reason `TmViewSpec`'s three references had to be wired into all four overlay GC
hooks and this one does not. It is read behind an `object_num_fields` guard, so
a view built with the older four-field shape degrades to the previous behaviour
rather than reading past the object (the same defensive shape W7-1 gave
`TreeSet$Itr`'s fourth slot).

`sort` had no ASL registration at all, so `base.subList(1,4).sort(..)` reached an
interface-level native that read the ASL receiver through the `ArrayList` layout
and the sorted result landed nowhere. It is now `asl_delegate_mutating`, whose
snapshot-and-write-back is exactly right for an in-place reorder: the element
count does not change and the slice is written back over
`[offset, offset+size)`. **`replaceAll` is the same shape, unmeasured, and was
left alone.**

### `TreeSet.descendingSet()` — write-through, one direction

| observable | HotSpot | CratonVM (before) |
|---|---|---|
| `TreeSet.descendingWriteThrough` | `3:[9, 2, 1]:[1, 2, 9]` | `3:[9, 2, 1]:[1, 2, 3]` |

Read the middle field: the descending view's OWN contents were already right.
`pollFirst` returned `3` and the view ended `[9, 2, 1]`, both correct. Only the
backing set was untouched.

**The existing machinery covered most of it.** `ts_view_source` — a source
reference stashed in the trailing capacity slot of the element array — already
exists for `TreeMap` keySet views, and `pollFirst`, `pollLast`, `remove`,
`clear` and the iterator's `remove` all already consult it. Installing the
marker on the descending set is most of the fix. Three things had to be added
around it:

  1. `ts_source_remove` dispatches map-`remove(Object)Object` versus
     set-`remove(Object)Z`. This is not optional: the existing
     `source_map_remove`'s descriptor **does not exist on `TreeSet` at all**, so
     calling it on a set source raises `NoSuchMethodError` rather than removing
     anything. Every `ts_view_source` removal now routes through it.
  2. `native_ts_add` propagates to a SET source. It must not propagate to a MAP
     source: that is a `TreeMap.keySet()` view, whose `add` is
     `UnsupportedOperationException` in the JDK.
  3. `ts_ensure_capacity` reserves and carries the marker slot. Without it, an
     `add` that grew the array would copy only the elements and drop the source
     — and an `add` that merely filled the last slot would overwrite it. Either
     one silently orphans the view, which is the defect class W7-1's closing
     warning is about. It is detected exactly as `ts_view_source` detects it, so
     the two cannot disagree.

The marker goes on AFTER the population loop, deliberately: installed first,
every `native_ts_add` in that loop would write each copied element straight back
into the set it came from.

**This view is live in ONE direction only.** A write through `ds` reaches `ts`;
a later write to `ts` is not visible in `ds`, which remains the snapshot it
always was. The JDK's is live both ways. Closing the other direction needs a
rebuild-before-read funnel like `tm_sync_native_state`, and `TreeSet` has no
such funnel — every one of its natives reads `ts_state` directly, so there is no
single place to put it. That is a genuine remaining divergence and it is stated
in the function's own doc comment as well as here. The half-live shape is not
novel: the `TreeMap` keySet view has had exactly it since it was written.

Two latent stale-at-store hazards were fixed on the way, both in
`native_ts_descending_set`: the freshly allocated reverse comparator and the
result set were bare Rust locals held across allocations and then stored. The
function is now single-exit through a closure so no `?` can unwind past the pin
and strand it.

---

## Part 4 — the one row not attempted

| observable | HotSpot | CratonVM |
|---|---|---|
| `stream.reuseThrows` | `java.lang.IllegalStateException` | `no-throw` |

`java.util.stream.BaseStream`: a stream may be operated on once; a second
terminal or intermediate operation throws `IllegalStateException("stream has
already been operated upon or closed")`. The JDK carries a `linkedOrConsumed`
flag on `AbstractPipeline`.

The obvious place for it is `stream_elements`, which every terminal and every
materialising intermediate op already funnels through — mark on entry, refuse if
already marked. That would even get the intermediate-op case right for free
(`s.map(f); s.map(g)` throws on the second in the JDK, and would here). **It was
not done, and the reason is the count of call sites: 99 in
`native-collections/src/lib.rs` alone, plus cross-crate callers in
`native-builtins/src/phases_late/streams.rs`.** A flag that is set once too
often turns a working stream into an `IllegalStateException`, streams are the
most pervasive thing in the Spring Boot and Tomcat arms, and nothing on this
branch can be built or run. Marking only in `native_stream_count` — the one op
the probe uses — would be a probe-shaped fix: a change that can only ever be
exercised by the one statement that motivated it, and therefore a probe that
cannot fail. See W6-5-vacuous-tests.

What a future lane needs: a fifth synthetic-stream slot beside
`STREAM_FIELD_OP_CHAIN`, set and tested in `stream_elements`, plus an audit of
which of those 99 sites call it more than once for a single logical operation —
that audit is the whole of the work, and it needs a build to be worth anything.

---

## What was NOT changed

  * `probes/ShadowDifferentialProbe.java`. Untouched. The excised variant used
    for the measurement is a scratchpad copy, exactly as W7-33 did it.
  * `classloading/src/class_manager.rs`. The `EmptyStackException` synthetic-mode
    follow-up in Part 1 is recorded, not made — the file is outside this
    branch's scope and the change is not measurable without a build.
  * `native_tm_get_or_default`. `TreeMap` has its own `getOrDefault` native and
    can hold null values, so it plausibly has the same present-with-null defect
    as `HashMap.getOrDefaultOverNullValue`. Not on the probe, not measured, not
    touched. **ADJUDICATED 2026-08-12: the hypothesis is WRONG — see Part 5.**
  * The rest of the natural-ordering key surface. `TreeMap.containsKey(null)`,
    `TreeMap.remove(null)`, `TreeSet.contains(null)`, and the single-bound
    `headMap`/`tailMap` type check all still return normally where the JDK
    refuses. Real, adjacent, and not on the table above. **CLOSED IN SOURCE
    2026-08-12 — Part 5.**
  * `TreeSet.subSet(hi, lo)`. The `TreeMap` twin of it is fixed; the `TreeSet`
    one is not measured and was not widened into. **CLOSED IN SOURCE
    2026-08-12 — Part 5.**
  * Compatible mode is where all of this lands, and every row is a
    Compatible-mode behaviour change from a wrong value or a missing throw to
    the specified one. No in-tree caller depends on any of the old behaviours:
    nothing under `regression-suite/`, `probes/`, `apps/` or `test_classes/`
    names `EmptyStackException`, offers a null to a `PriorityQueue`, builds a
    `Stack`, calls `setValue` on a `firstEntry()`, or passes reversed bounds to
    `subMap`/`subSet` — and `RTreeRangeGc`'s key type implements `Comparable`,
    so the new type check cannot fire on it.

---

## Part 5 — the two handed-over rows, 2026-08-12

Source only. Nothing built, nothing run; the "before" wordings below are read
off `dev`'s source, not re-measured. `native-collections/src/lib.rs` only.

### 5.1 `native_tm_get_or_default` — the hypothesis was wrong, and there is a
### different defect at the same native

**Present-with-null is NOT broken here.** Both storage paths read the mapping
itself rather than the value's nullity: fast mode is
`bt.get(&tk).copied()` folded with `v.unwrap_or(default)` — a key present with
`Value::Object(None)` yields `Some(Object(None))` and `unwrap_or` never fires —
and the array path is `Ok(idx) => get_array_element(data, idx * 2 + 1)`, the
stored slot. So `TreeMap.getOrDefault(k, d)` over a null value already answers
`null`, which is what `Map.getOrDefault` specifies. It never needed
`native_map_get_or_default`'s `containsKey` re-ask, because unlike that one it
has the node, not just the value.

This is the "a known-issue hypothesis can be wrong, not just stale" case: the
suspicion came from the shape of the sibling defect, and the shape is where the
two natives differ.

**What IS wrong at that native is the refusal.** `TreeMap.getOrDefault` is
`getEntry(key)`, the same null-check-plus-`(Comparable)`-checkcast every other
`TreeMap` lookup runs, and this native never called
`tree_natural_order_key_check`. `new TreeMap<String,Integer>().getOrDefault(null, d)`
answered `d` where HotSpot throws `NullPointerException` — a refusal laundered
into a plausible value, which is the species this whole record is about, in the
one place the record predicted a *different* defect.

The check is placed after the pins are taken and unwinds them explicitly on the
error path rather than through `?`, because the surrounding function holds three.

### 5.2 The five sorted-container refusals

Each is `tree_natural_order_key_check` at one more of the natives that reaches
the JDK's `compare(key, key)`, with the same `container_is_empty` argument
`native_tm_get`/`native_tm_put`/`native_ts_add` already pass — plus one new
`TreeSet` helper.

| native | JDK path | now raises |
|---|---|---|
| `native_tm_contains_key` | `containsKey` is `getEntry(key) != null` | NPE / CCE |
| `native_tm_remove` | `remove` is `getEntry(key)` first | NPE / CCE |
| `native_ts_contains` | `TreeSet.contains` is `m.containsKey(o)` | NPE / CCE |
| `native_tm_get_or_default` | `getOrDefault` is `getEntry(key)` | NPE / CCE |
| `tm_new_range_view`, single bound | `NavigableSubMap` ctor's `else` arm | NPE / CCE |
| `native_ts_sub_set`, `native_ts_sub_set_inclusive` | `NavigableSubMap` ctor's `if` arm | `IllegalArgumentException` |

Four placement facts that are not interchangeable:

  * **`native_tm_remove`'s check goes BEFORE the view branch**, exactly where
    `native_tm_put`'s does and for the reason stated there: a descending view
    carries a `Collections.reverseOrder` comparator so the check no-ops on it
    and the redirect lets the backing map raise, while an ascending view carries
    the source's own null comparator and `NavigableSubMap.remove`'s `inRange`
    raises there too.
  * **`native_ts_contains`'s check goes BEFORE the `data_opt` early return.** An
    empty `TreeSet` has no backing array at all, so a check placed after it
    would answer `false` for `contains(null)` — precisely the row.
  * **The single-bound view check passes `container_is_empty = true`
    unconditionally.** `m.compare(hi, hi)` does not consult `root`; the JDK
    refuses on an empty map as well, and asking the map would under-throw
    exactly there. It fires only when `lo.is_some() != hi.is_some()`: two bounds
    take the other arm (already covered by `tm_refuse_reversed_bounds` at the
    two `subMap` entry points) and `descendingMap` supplies neither.
  * **`ts_refuse_reversed_bounds` is `tm_refuse_reversed_bounds` transposed**,
    message verbatim, including the pin-compare-re-read contract — `tree_compare`
    dispatches a user `Comparator` and all three of receiver and bounds are used
    afterwards.

### 5.3 What Part 5 did NOT do

  * **`TreeSet.headSet`/`tailSet`'s single-bound type check.** The `TreeMap`
    half is closed above; the `TreeSet` views are snapshot copies that do not go
    through `tm_new_range_view`, so they need their own call and were not
    measured. Same species, one file, still open.
  * **The `--synthetic-jdk` `EmptyStackException` follow-up in
    `classloading/src/class_manager.rs`.** Still recorded, still not made — that
    file belongs to another lane.
  * The widened `implements_comparable` exposure this creates is **not new**:
    that predicate has decided every non-first comparison for months, and under
    `--synthetic-jdk` a fabricated `java/lang/Enum` declares no interfaces and
    already reads as non-`Comparable`. What changes is that four more entry
    points now consult it on the FIRST operation. No suite runs
    `--synthetic-jdk` mode, so nothing in-tree can observe the difference; a
    lane that turns that mode on should read this paragraph first.

### 5.4 Coverage

`regression-suite/src/RJdkViews.java`, new `sortedContainerRefusals()` section
(`CORE_CLASSES`, default invocation). Every refusal is paired with the case that
must NOT refuse — a null-permitting comparator, a valid bound, a valid
`subSet` — because a container that threw from every key would satisfy the
positive half on its own, which is `W6-5-vacuous-tests`' shape. The
present-with-null row is asserted too: it is not a change, it locks 5.1's
correction so the next reader does not "fix" it.

---

## Part 6 — which of these rows has a scheduled witness, 2026-08-12 (doc-only lane)

Checked against the tree. No cargo, no Rust; this section adjudicates
**scheduling**, not correctness, and the distinction is the point — a row can be
correct in source and still have nothing that will ever tell you it stopped
being correct.

### 6.1 Covered, and the coverage really runs

`RJdkViews` is in `CORE_CLASSES` at `regression-suite/run.sh:106` — verified,
including the file's own comment explaining why an `RJdk*`-named vector sits in
the CORE list. `sortedContainerRefusals()` is present at
`RJdkViews.java:506` and called at `:633`. So the sorted-container refusals of
Part 2 and Part 5.2, R1's `PriorityQueue.add(null)`, and 5.1's
present-with-null lock are all on a default invocation.

R2's pair is covered from the other side as well: `RExceptions` is also in
`CORE_CLASSES` and asserts both `Stack.pop()` and `Stack.peek()` **by exception
type**, with a `NoSuchElementException` arm that makes it non-vacuous. The
`--synthetic-jdk` follow-up Part 1 recorded and 5.3 left open **has since been
applied** — both arms are in `classloading/src/class_manager.rs` today
(`| "java/util/EmptyStackException"` in `jdk_superclass` at `:10673`, and in
`synthetic_stub_fields` at `:12116`). Part 5.3's *"still recorded, still not
made"* is therefore **stale**; it is made. It remains unobservable in-tree for
the reason both records give — no suite runs `--synthetic-jdk` MODE — and that
is a property of the mode, not of the change.

### 6.2 Not covered by anything, and the number is the finding

Every row in Part 1 R3 (`CHM.reduceValues` / `searchKeys`), all of Part 3
(`unmodifiableList` identity, the nested `subList`, `TreeSet.descendingSet()`),
and Part 4's `stream.reuseThrows` have **no scheduled witness**. Their sole
observable is `probes/ShadowDifferentialProbe.java`, and:

* the string `probes` appears **zero** times in `regression-suite/run.sh`, at
  any `SUITE=` value;
* the only scheduled consumer of `probes/` is
  `scripts/jdk-only-strict-probes.sh` (`.github/workflows/ci.yml:315`, `:1404`),
  and its `PROBE_LIST` default is three names —
  `JdkOnlyCensusLoadProbe JdkOnlyBreadthProbe JdkOnlyPlatformProbe` — out of
  **449 `.java` files in `probes/`**;
* `ShadowDifferentialProbe` is named by no `.sh` and no `.yml` anywhere in the
  tree.

So this record's "before" column can never be re-taken by CI, and its "after"
column — every row of which is explicitly a claim about source — has nothing
scheduled that would convert it into a measurement. **A green suite run is not
evidence for any Part 3 row.** Part 3's own most load-bearing sentence, *"this
view is live in ONE direction only"*, is exactly the kind of half-fix that a
later well-meaning edit silently completes or silently breaks, and nothing here
would notice either.

The cheapest repair is not to schedule the probe — a 1,400-line HotSpot
differential is the wrong shape for a gate, for the reason `run.sh`'s
`extract()` filter exists (W7-60) — but to give the three Part 3 rows the same
treatment 5.4 gave the refusals: three `check()` triples in `RJdkViews`, each
paired with the case that must NOT change. Concretely, and each is a handful of
lines:

* `Collections.unmodifiableList(u) == u` for an already-unmodifiable `u`, paired
  with `Collections.unmodifiableList(List.of(1,2)) != that list` — the JDK does
  wrap `ImmutableCollections$ListN`, so the negative half is what stops the
  positive half from being satisfied by "always return the argument";
* `base.subList(1,4).subList(0,2).clear()`, then assert the **base**'s contents
  and the enclosing view's contents — the enclosing-view assertion is the one
  that fails if `ASL_FIELD_VIEW_PARENT` is dropped, and it must be paired with a
  SIBLING view that **must still** raise `ConcurrentModificationException`,
  because a chain-walk that updated siblings too would pass the first assertion;
* `ts.descendingSet().pollFirst()`, then assert `ts` shrank — paired with the
  known one-directional limit stated as a comment so the next reader does not
  read the missing reverse assertion as an oversight and "fix" it into a red.

### 6.3 A better instrument for the unregistered list

Part 1 R3 ends with a long list of bulk operations *"deliberately still
unregistered, and still silently answering null / doing nothing"* —
`reduceKeys` (both arities), `reduceEntries`, `searchValues`, `searchEntries`,
the `reduce*To{Int,Long,Double}` family, the three-argument transform overloads,
and `forEachKey`/`forEachValue`/`forEachEntry`. That list was compiled by
reading registrations, which is the method this campaign has repeatedly caught
out — a triple can be registered in a second file, and `register()` is
last-write-wins, so a source audit predicts the wrong answer.

There is now a direct instrument and it needs no differential:
`cratonvm --jdk-only --explain-jdk-only --jdk-only-report r.json -cp <cp> <Main>`
emits `native-shadows-bytecode` rows carrying class, method, descriptor and the
requester's `file:line`. Whether a `java/util/concurrent/ConcurrentHashMap`
triple is shadowed on **this build** is one query against that JSON. Two
cautions for whoever runs it: the flags are **silently ignored if placed after
the main class** — no file, no warning, exit 0 — and the census
**over**-reports, because a requested-and-refused row is not a failure (callers
recover onto real bytecode). Only the intersection of the census and a probe
that actually exercises the method is the blocking set.
