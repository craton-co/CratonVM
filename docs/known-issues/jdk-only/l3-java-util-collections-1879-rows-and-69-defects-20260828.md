# L3 — `java.util` collections: 1879 probed rows, 69 defects, and what passed — 2026-08-28

> **The twelve probes live at `apps/probes/`.** `3b2901531` (*"major doc
> consistency update before the realeas"*, 2026-08-29) moved `probes/` to
> `apps/probes/`, and the move deleted the files that were still only on lane
> branches -- these twelve among them. They are restored at the new path,
> alongside the rest of the corpus. `apps/` is in `.gitignore`, so anything
> added there needs `git add -f`; that is why the move carried some probe files
> across and dropped others.


**Status: the lane is closed except for the residuals in §6, each of which has
its measurement here.** Twelve differential probes, 1879 rows, against HotSpot
25.0.4+7, run in BOTH modes on every one of five builds. Six of the twelve are
0-diff in both modes; the other six carry only the eight residual rows of §6 and
the thirteen of the companion dispatch-door record.

Lane brief: `HANDOFF-20260828-L3-util-collections.md` (retired). Method:
`HANDOFF-20260828-SCOPE.md` §3.

---

## 1. The surface, measured rather than assumed

`--dump-native-registry` on this branch, filtered to the rows that are the
adjudication surface (`kind == bridge` AND the real method has `Code` AND
`owns_slot`), and then to `java.util` outside `java.util.concurrent`:

```text
56 classes    609 rows
```

The lane brief sized L3 at "~380 rows: Properties 92, TreeMap 41, ArrayDeque 33,
LinkedList 31, TreeSet 31, HashMap 30, Collections 26, Hashtable 24, + the tail".
Two corrections that changed what got probed:

* **`Properties` is 40 owning rows, not 92.** The 92 counts every registration
  of the name; 52 of them are duplicates that lose the slot. Filtering on
  `owns_slot` is the same discipline the brief's own §5 asks for before editing.
* **the tail is bigger than the named families.** `Locale` 24,
  `ArrayList$SubList` 23, `Optional` 20, `ArrayList` 19, `Date` 19,
  `LinkedHashMap` 19, `PriorityQueue` 13, `LinkedHashSet` 12, `TimeZone` 10 and
  the eleven view carriers together outweigh `TreeMap` + `Properties`. Nine of
  the twelve probes are tail probes.

Every one of the 56 classes is now covered by a probe.

## 2. The twelve probes

| probe | rows | compat | strict |
| --- | ---: | ---: | ---: |
| `PropertiesShadowSweep` | 182 | **0** | **0** |
| `HashtableVectorShadowSweep` | 136 | **0** | **0** |
| `ArrayListShadowSweep` | 164 | **0** | **0** |
| `CollectionsShadowSweep` | 172 | **0** | **0** |
| `UtilTailShadowSweep` | 146 | **0** | **0** |
| `MapViewsShadowSweep` | 300 | **0** | 1 (§6.1) |
| `TreeShadowSweep` | 232 | 2 (§6.1) | 2 (§6.1) |
| `DequeListShadowSweep` | 172 | 1 (§6.1) | 1 (§6.1) |
| `PqOptionalShadowSweep` | 125 | 2 (§6.1, §6.2) | 2 |
| `LinkedSequencedShadowSweep` | 102 | 1 (§6.3) | 1 |
| `LocaleDateTzShadowSweep` | 123 | 1 (§6.4) | 1 |
| `MethodRefDoorProbe` | 25 | 13 (§6.5) | 13 |

## 3. The defects, by shape

Sixty-nine, across four rounds. Every one is on a contract edge — the pattern
`phase-2-worklist-mined-28-defects-in-four-families-20260828.md` states holds
through a fifth, sixth and seventh family without an exception.

### 3.1 Two crashes, and what they were hiding

```text
Properties.values().iterator()
  --jdk-only  NoClassDefFoundError: cratonvm/internal/ArrayListViewItr
  compatible  works

TreeSet.clone()
  both modes  NullPointerException: Cannot invoke
              "java.util.SortedMap.comparator()" because "m" is null
```

The first is a Phase-1 shape: a fabricated class refused (correctly) by strict
mode, with the refusal handed to the caller instead of degraded.
`native_ad_iterator` had already solved exactly this — land on the real
`java/util/Arrays$ArrayItr` through `real_snapshot_iterator` — so the fix is that
arm at the second site that needed it, with a new
`SnapshotItrRoute::ArrayListView` so `iterator().remove()` still writes through.

Degrading to `java/util/ArrayList$Itr` was REJECTED and the reason is worth
keeping: that class name is a load-bearing guarantee the bytecode yield in
`native_override.rs` reads ("an iterator over something with a real modCount"),
and a view carrier has none.

The second stopped `TreeShadowSweep` at row 173 of 209, so 36 rows of `TreeMap`'s
Map surface had never been measured at all — and four defects were waiting in
them. **A crash early in a probe masks every later defect** is the brief's rule;
this is the second lane to pay it.

### 3.2 The null argument, 31 rows

RULE F (the null functional argument) already existed. Two siblings did not:

* **RULE C, the null COLLECTION argument** — `addAll`, `removeAll`, `retainAll`,
  `toArray(T[])` and the copy constructors of `ArrayList`, `LinkedList`,
  `ArrayDeque`, `TreeSet`, `LinkedHashMap`, `HashSet`, plus
  `Arrays.asList((Object[]) null)`;
* **RULE S, the null argument of a `java.util.Collections` static** — eleven of
  twelve had none. `sort(null, cmp)`, `reverse`, `fill`, `swap`, `frequency`,
  `max`, `min`, `addAll` (both arguments), `shuffle`, `unmodifiableList`,
  `synchronizedList`.

And `Optional`'s whole surface, which is nothing but refusals:
`map`/`flatMap`/`filter`/`or` pre-validate and therefore throw on an EMPTY
optional, while `orElseGet` and `ifPresentOrElse` do not and throw only on the
branch they reach — two rules that look like one until both are measured.

Every one answered `false` / `no-throw` / an empty container: the
fabricated-success shape, where the caller is told the operation succeeded and
nothing happened.

### 3.3 Argument validation, 8 rows

`new PriorityQueue<>(0)` is `IllegalArgumentException` — the ONE container in
`java.util` where capacity 0 is illegal — and the body clamped with
`max(c, 1)`. **Clamping an argument is not validating it**: a legal
`new PriorityQueue<>(1)` and an illegal `new PriorityQueue<>(0)` became the same
call.

`LinkedHashMap(-1)`, `(16, -1f)`, `(16, NaN)`, `LinkedHashSet(-1)` and
`new Properties(-1)` had no guards. `HashMap`'s were written on 2026-08-28 and
the siblings share the contract and not the code, so the three now live in one
`map_ctor_capacity_load_check` that every hash-ordered constructor calls —
which is also how `HashSet(int, float)` gets the NaN guard it never had, its own
body having DROPPED the load factor before doing anything with it.

`new Properties(-1)` is the one that was not where it looked: real
`Properties(int)` bytecode runs `new ConcurrentHashMap<>(initialCapacity)`, and
`native_chm_init_capacity` validated nothing. **Recorded for L6**, whose family
`ConcurrentHashMap` is; the fix is the same one-line shared guard.

### 3.4 The registration that was never there, 11 rows

Five `TreeMap` mutators (`replace`, `replace(k,old,new)`, `remove(k,v)`,
`replaceAll`, and the `(Map)` and `(SortedMap)` copy constructors) and three
`ArrayDeque` bulk operations (`removeIf`, `retainAll`, `removeAll`) were not
registered at all, so real bytecode ran against state this VM does not keep:

```text
tm.replace("a", 100)             HotSpot 12 (the old value)   CratonVM null, no write
[a,b,c,d].removeIf(x -> x=="b")  [a, c, d]                    [a, c, d, null]
```

The deque one is state CORRUPTION rather than a wrong answer: real `ArrayDeque`
bytecode maintains `elements`/`head`/`tail` and knows nothing about this VM's
FOURTH, synthetic `size` slot, so every later read walked one element past the
end and found the hole.

`new TreeMap<>(Map)` worked and `new TreeMap<>(SortedMap)` did not, which is the
generalisable half: the first is `putAll`, which reaches the `Map` interface
native, and the second is `buildFromSorted`, which writes `root`. **A shim is
only ever as good as the JDK path that happens to route through it.**

### 3.5 The JDK 21 sequenced surface, 10 rows

Only `LinkedHashMap.reversed` was registered. `putFirst` appended instead of
prepending and did not move a present key; `pollLastEntry` answered the right
entry and LEFT IT IN PLACE; `sequencedKeySet()` answered `[]`;
`LinkedHashSet.reversed()` raised `NullPointerException` and its
`addFirst`/`removeFirst` did neither.

`pollLastEntry` is the worst of them: `while ((e = m.pollLastEntry()) != null)`
— the idiom the method exists for — never terminates.

Two of the ten needed the registration to name a different class:
`pollFirstEntry`/`pollLastEntry` are `SequencedMap` DEFAULT methods, which
`LinkedHashMap` does not declare, so a class-name registration can never fire.
The dump says so in one line — `has_code: false` next to `invocations: 0`, while
the sibling `putFirst` reads `has_code: true, invocations: 2` from the same run.

### 3.6 Views that were snapshots, 7 rows

A `TreeSet` range view carried neither its source nor its range:

```text
s.subSet("b","d").add("z")    HotSpot IllegalArgumentException   CratonVM no-throw
s.subSet("b","d").add("bb")   the SOURCE gains "bb"              it did not
```

`TreeMap`'s navigable views have carried both since `W7-1 family 1`
(`TmViewSpec`, wired into all four GC overlay hooks); `TreeSet`'s six range
accessors used none of it. They do now — the source through the same
trailing-slot marker `descendingSet` already uses and `ts_ensure_capacity`
already carries across a grow, and the range through `TmViewSpec` itself, which
is a source and a pair of bounds and says nothing about maps. A second table
would have meant a second copy of those four hooks.

And a view of a view could WIDEN: `m.subMap("b",true,"d",true)
.subMap("a",true,"d",true)` answered a wider view where the JDK refuses. A
caller can otherwise narrow a view and quietly get back everything it excluded,
which is the one thing a range view is for.

### 3.7 `java.util.Properties`, the lane's largest family, 8 rows

The side table exists because a synthetic `Properties` has a null inner `map`.
Every defect is on the seam between the two stores it now has:

* `keys()` dropped every non-String key while `size()` counted it — a container
  that reports four entries and enumerates two;
* `propertyNames()` FILTERED where the JDK CASTS and throws. That cast is the
  contract: a `Properties` holding a non-String key is already outside the
  class's invariant, and a filtered view is one no later reader can tell from a
  complete one. Not the same rule as `stringPropertyNames`, which filters by
  design and was already right;
* `setProperty` read its own return value out of the String-only side table, so
  the one case where the previous value is interesting — it was not a String,
  and the caller is about to lose it — answered null;
* the six conditional mutators were reading HashMap buckets a `Properties` never
  fills, or were unregistered and edited the CHM mirror while the side table
  kept the old entry;
* `store` wrote `é` where the JDK writes `é` (`saveConvert` indexes a
  `hexDigit[]` of `'0'..'9','A'..'F'`). `load` accepts either, so a round trip
  cannot see it and a byte comparison against a JDK-written file can;
* the synthetic entries are instances of the INTERFACE `java.util.Map$Entry`,
  which declares no `toString`, so `entrySet()` printed identity hashes.

### 3.8 Type safety and fail-fast, 5 rows

`List<String>.toArray(new Integer[4])` filled the array where the JDK throws
`ArrayStoreException` — `System.arraycopy` performs the aastore check, and an
array whose contents contradict its declared type is the P3-A hole reached
through a library method. Fixed with `aastore_element_assignable`, the
interpreter's own predicate, so the two doors cannot disagree.

A non-`Comparable` first element into a natural-ordered `PriorityQueue` was
accepted and surfaced as `NoSuchMethodError` from `compareTo` one call later,
where `siftUpComparable` opens with the cast. And a REFUSED offer left the
element in the heap at an unsorted position, because this body published the
size before sifting where the JDK sifts first.

`ArrayList.forEach` iterated a snapshot, so `l.forEach(x -> l.clear())`
completed quietly.

### 3.9 `Locale` and `TimeZone`, 4 rows

`Locale.of("en","US").getISO3Language()` answered `""`. The 184-entry
`iso2_to_iso3` table was complete and simply never got a code to map: neither of
the two places the body looked is where `Locale.of` puts the language, which
lives in `baseLocale.language`. `toLanguageTag()` lower-cased the variant while
`getVariant()` did not, so the locale disagreed with its own tag.

`TimeZone.getTimeZone((String) null)` substituted UTC — and an UNKNOWN id
legitimately answers GMT, so the fabricated answer for null was
indistinguishable from the documented one for a typo.

### 3.10 An immutable collection is not an unmodifiable wrapper, 9 rows

Seven of them in COMPATIBLE MODE ONLY — the eighth place this campaign finds
`--jdk-only` more correct than the default, because `List.of` and friends are
`SyntheticStub` registrations that strict drops in favour of real bytecode.

```text
List.of("a").contains(null)         HotSpot NPE   CratonVM false
List.of("a").indexOf(null)          HotSpot NPE   CratonVM -1
Map.of("a","1").containsKey(null)   HotSpot NPE   CratonVM false
List.copyOf(null) / Map.copyOf(null)  HotSpot NPE   CratonVM a usable collection
```

`ImmutableCollections` opens every query with `Objects.requireNonNull(o)`: a null
cannot BE in one of these, so asking about one is a caller bug rather than a
question whose answer is "no". `Collections.unmodifiableList(l).contains(null)`
is `false` for the opposite reason — the wrapper delegates to a list that permits
nulls — and this VM funnels both through ONE synthetic class, so the
discriminator has to be the immutability bit in slot 1 and not the class name.
The same split `unmod_list_oob_error` already makes for the three out-of-range
messages.

### 3.11 One spliterator, one set of characteristics, 6 rows

`native_spliterator_characteristics` returned the constant
`SIZED | SUBSIZED | ORDERED` for every producer.

```text
TreeSet.spliterator().hasCharacteristics(SORTED)     HotSpot true   false
TreeSet.spliterator().hasCharacteristics(DISTINCT)   HotSpot true   false
HashSet.spliterator().hasCharacteristics(ORDERED)    HotSpot false  true
```

Not cosmetic, twice over. `Spliterator.getComparator()`'s DEFAULT body is
`throw new IllegalStateException()` unless `SORTED` is reported, so
`treeSet.spliterator().getComparator()` threw where the JDK answers `null` — and
it killed the probe at row 143 of 146, which is the third crash this lane found
by a probe that kept going. And a stream pipeline reads these bits to decide
whether it may skip a sort or split in parallel: reporting `ORDERED` for a
`HashSet` claims an encounter order that does not exist.

The mask now lives in the object (a fourth slot the producer writes), with the
old constant as the default for the eight mint sites that keep three fields — so
nothing that was already right changes.

### 3.12 What the landing gates found that the probes could not, 9 more rows

`registrar_drift::no_new_mode_drift` reported six (pass, triple) pairs the
round-3 and round-4 registrations created, and answering its three questions
turned up a SECOND, WORSE implementation of five of the same methods, reachable
only under `--features synthetic-jdk`:

* `SequencedMap.pollFirstEntry` / `pollLastEntry` were registered to
  `native_p64_sm_first_entry` / `..._last_entry` — the same bodies
  `firstEntry`/`lastEntry` use, which answer the entry and do not REMOVE it.
  The identical defect this lane had just measured on the shipping path, in a
  copy no shipping probe can reach;
* `LinkedHashMap.sequencedKeySet` / `sequencedValues` / `sequencedEntrySet`
  walked the insertion-order chain by RAW SLOT — `get_field(node, 0)` for the
  key and `get_field(node, 5)` for `after` — against a node layout that is
  `HASH=0, KEY=1, VALUE=2, NEXT=3, BEFORE=4, AFTER=5`. They read the HASH as the
  key and the KEY as the value. Their own comment recorded the rest: "Return as
  ArrayList (simplification — real Java returns a Set view)".

All five are deleted rather than baselined: `register_linked_hashmap_natives`
runs in EVERY mode, and the drift existed only because
`register_synthetic_overrides` runs last and overwrote it under the feature.
The sixth pair (`Spliterator.getComparator`) was the redundant one — its
synthetic-only copy asks `this.characteristics()` virtually, so it lands on the
shipping body and now answers correctly too.

**And three unit tests pinned the lower-case escape §3.7 corrects.**
`save_convert_unicode_escaping` and two neighbours asserted the lower-case
`\uXXXX` forms — written FROM THE IMPLEMENTATION rather than from
`Properties.saveConvert`, whose `hexDigit[]` is `'0'..'9','A'..'F'`. `load`
accepts either case, so their round-trip halves could not see it and neither
could they. When a probe against the oracle disagrees with a unit test, the
probe has the oracle.

## 4. What PASSED, because that is where the work is not

Stated as precisely as the failures, because a lane's value is as much the
surface it clears as the defects it finds.

* **`Hashtable`, `Vector` and `Stack` are clean.** 136 rows, 0 differences in
  both modes, first run, no fixes. That includes the whole null axis on both
  sides (the trap the brief predicted — one body serving both `HashMap` and
  `Hashtable` — is not there), every constructor guard, the two exception TYPES
  `Vector` raises for one index through its two doors
  (`ArrayIndexOutOfBoundsException` from `elementAt`,
  `IndexOutOfBoundsException` from `get`), and `Stack`'s `EmptyStackException`.
* **`ArrayList`, `ArrayList$SubList` and `Arrays$ArrayList` are clean after six
  fixes** — including every `subList` contract: relative indices, write-through
  in both directions, `subList(a,b).clear()` as a range delete, and the stale-view
  `ConcurrentModificationException` after a structural parent write.
* **The 300-row map-VIEW battery is clean in compatible mode across all five
  families**: read-through, write-through through `remove`/`removeIf`/
  `retainAll`/`removeAll`/`clear`/`iterator().remove()`/`Entry.setValue`, the
  `add` refusals, equality against a plain `HashSet`, and fail-fast on a
  structural write with NO throw on a value-replacing one.
* **`java.util.Date` is clean** — all 19 rows, including the comparison family
  across the epoch and `Long.MIN_VALUE`.
* **`Optional`'s happy path was already right**; all 12 defects were refusals.
* **Every `TreeMap` navigation answer** (`ceiling`/`floor`/`higher`/`lower`, key
  and entry, at both ends and at a present key) was correct before this lane
  touched it. What was missing was only their REFUSALS.

## 5. Three process findings

**A method reference and a lambda are different dispatch doors.** `x::m` and
`() -> x.m()` disagree on this VM; it cost a build cycle reading as a
`Properties` defect. Its own record and reproducer are separate, because it is
not a `java.util` defect.

**`owns_slot` is not enough on its own: `has_code` is the other half.** A
registration can own its slot, be unique, and still be unreachable — because the
class does not DECLARE the method and dispatch resolves to the interface that
does. `has_code: false` on a class row is the tell.

**`dev`'s tip was red before this lane touched it, in two guards at once.**
`runtime::resolve::guard::no_unallowlisted_metadata_table_bypass_exists` and
`the_allowlist_has_no_dead_rows` both fail on pristine `origin/dev`: commit
`1dbbe2b36` ("perf(jit): bind an invokevirtual whose target is final") added a
fourth `find_method_recursive(` to `interpreter/invoke.rs` without moving a row.
Verified pristine — `invoke.rs` and `guard.rs` are byte-identical to
`origin/dev` and the count is 4 on both — before touching anything. Cleared
here, because a red gate blocks every lane, with the raise marked in the
allowlist as the ratchet moving the WRONG way and as debt for whoever owns the
devirt change. **Raising a ratchet silently is how it becomes a rubber stamp**;
the number moved and the reason moved with it.

**A fix for one row introduced a Phase-1 crash in another, and a probe found
it in the next run.** Making
`Collections.nCopies` immutable by calling this crate's
`cratonvm/internal/UnmodifiableList` wrapper — a FABRICATION — killed the strict
arm at row 36. The registered `Collections.unmodifiableList` is a
`SyntheticStub`, which strict mode DROPS in favour of real bytecode; calling the
native's BODY directly bypasses that drop. **Any fix that mints a
`cratonvm/internal/*` class needs a strict-mode arm, and calling a native body
directly is not the same as calling the method.**

## 6. The residuals, each with its measurement

### 6.1 The snapshot-iterator families are not fail-fast — 5 rows

```text
TreeSet      for (x : s) s.add(..)      HotSpot CME   CratonVM no-throw
TreeSet      for (x : s) s.remove(..)   HotSpot CME   CratonVM no-throw
ArrayDeque   for (x : d) d.add(..)      HotSpot CME   CratonVM no-throw
PriorityQueue for (x : q) q.add(..)     HotSpot CME   CratonVM no-throw
TreeMap.keySet()  (--jdk-only ONLY)     HotSpot CME   CratonVM no-throw
```

STRUCTURAL, not an omission. These four families hand out a SNAPSHOT iterator,
and under `--jdk-only` that iterator is the real `java.util.Arrays$ArrayItr` —
whose `next()` is real bytecode. A comodification check has nowhere to run.
Implementing it only in compatible mode, where the fabricated iterator's `next()`
IS a native, would make the two modes disagree, which is a worse state than a
recorded gap: this campaign's whole premise is that the two modes converge.

Closing it needs the iterator itself to change, which is the same change
`W7-1 family 1` made for `TreeMap`'s views and is a lane of its own.

### 6.2 `PriorityQueue.iterator().remove()` does not write through — 1 row

`native_pq_iterator` deliberately returns an `ArrayList$Itr` over an
ArrayList-shaped WRAPPER holding a heap-order snapshot, and its own comment
records why the alternative is worse (a real `PriorityQueue$Itr`'s slot 0 is
`cursor:int`, and the snapshot array stored there is coerced away). Rerouting
`remove` means teaching `values_view_source`/`propagate_list_removal` — the path
every map view shares — about a non-map source. Measured: `size()` 3 where
HotSpot answers 2.

### 6.3 `reversed()` is a snapshot, not a live view — 1 row

`LinkedHashMap.reversed()` and `LinkedHashSet.reversed()` answer the right
elements in the right order; a write to the SOURCE after the view was taken is
not visible in them. Measured on the map (`{c=3, a=1, b=2, d=4}` where HotSpot,
after a later `put`, answers `{e=5, c=3, a=1, b=2, d=4}`). The JDK's are
`ReverseOrder*View` classes; this needs the `TmViewSpec` treatment extended to
the `LinkedHashMap` family, which is the same lane as §6.1.

### 6.4 `Currency.getDisplayName(Locale.ENGLISH)` answers the CODE — 1 row

`USD` where HotSpot answers `US Dollar`. Not a collections defect: the currency
display name comes from the CLDR bundle through `LocaleServiceProvider`, and
`getDisplayName` is not registered at all — this is real bytecode failing to
find its resource. Filed against the locale-data surface, not L3's.

### 6.5 The method-reference dispatch door — its own record

See the companion page. 13 of `MethodRefDoorProbe`'s 25 rows.

## 7. Reproduce

```bash
CV=target/release/cratonvm
# The twelve sources live in `apps/probes/`; the repo moved `probes/` there on
# 2026-08-29 and this block is written for the layout after that move.
"$JDK/bin/javac" -d apps/probes/out apps/probes/*ShadowSweep.java apps/probes/MethodRefDoorProbe.java
for C in PropertiesShadowSweep TreeShadowSweep DequeListShadowSweep \
         HashtableVectorShadowSweep ArrayListShadowSweep \
         LinkedSequencedShadowSweep PqOptionalShadowSweep \
         CollectionsShadowSweep LocaleDateTzShadowSweep \
         MapViewsShadowSweep UtilTailShadowSweep MethodRefDoorProbe; do
  "$JDK/bin/java" -cp apps/probes/out "$C" > /tmp/$C.hs 2>/dev/null
  "$CV" --java-home "$JDK"            -cp apps/probes/out "$C" > /tmp/$C.compat 2>/dev/null
  "$CV" --java-home "$JDK" --jdk-only -cp apps/probes/out "$C" > /tmp/$C.strict 2>/dev/null
  diff /tmp/$C.hs /tmp/$C.strict
done
```

Check the ROW COUNT and the trailing `DONE <probe>` line before reading any
diff: a run that died partway produces a short file whose missing tail `diff`
reports as ordinary `<` lines, and this lane hit exactly that twice.
