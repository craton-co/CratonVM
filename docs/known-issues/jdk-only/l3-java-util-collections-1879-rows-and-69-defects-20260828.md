# L3 — `java.util` collections: 1879 probed rows, 69 defects, and what passed — 2026-08-28

> **The twelve probes live at `apps/probes/`.** `3b2901531` (*"major doc
> consistency update before the realeas"*, 2026-08-29) moved `probes/` to
> `apps/probes/`, and the move deleted the files that were still only on lane
> branches -- these twelve among them. They are restored at the new path,
> alongside the rest of the corpus. `apps/` is in `.gitignore`, so anything
> added there needs `git add -f`; that is why the move carried some probe files
> across and dropped others.


**Status: CLOSED 2026-08-30. Every residual in §6 and §6b is fixed, and the
`java.util` corpus is 0-diff against HotSpot in both modes.** Twenty-three
differential probes, 2530 rows, against HotSpot 25.0.4+7, run in BOTH modes on
one build. Twenty-two are 0-diff in both modes; the twenty-third,
`ImmutableSplProbe`, is 0-diff on every characteristics row and differs only on
class-IDENTITY rows, which belong to a carrier this lane does not own — see
§6d.

The last three rounds are worth reading even though they are closed, because
each one closed by finding that the recorded diagnosis was wrong rather than
incomplete:

| round | the record said | what it was |
| --- | --- | --- |
| §6.1 `ArrayDeque` | "needs a generation counting structural GROWTH, which the deque has no field to hold" | it needs no counter at all; the fail-fast is the ring buffer's LAYOUT, and the shadow was the thing preventing it |
| §6b immutable spliterators | "the discriminator is SIZE, not class" | the discriminator is the CLASS; size only decides which class the factory built |
| §6c serialization | not recorded — no probe had ever asked | `--jdk-only` could not DESERIALIZE any `List.of`/`Set.of`/`Map.of` at all |

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
| `MapViewsShadowSweep` | 300 | **0** | **0** |
| `TreeShadowSweep` | 232 | **0** | **0** |
| `DequeListShadowSweep` | 172 | 1 (§6.1) | 1 (§6.1) |
| `PqOptionalShadowSweep` | 125 | **0** | **0** |
| `LinkedSequencedShadowSweep` | 102 | **0** | **0** |
| `LocaleDateTzShadowSweep` | 123 | **0** | **0** |
| `MethodRefDoorProbe` | 25 | **0** | **0** |

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

### 6.1 The snapshot-iterator families are not fail-fast — 4 of 5 rows FIXED 2026-08-29

Fixed, and the fix was the CLASS, exactly as the re-diagnosis above predicted.
Each of the four families now hands out the iterator HotSpot hands out, minted
through one shared helper, and three of them became fail-fast for free because
the real class declares the `expectedModCount` the third door seeds:

| family | was | now | fail-fast |
| --- | --- | --- | --- |
| `TreeSet` | `Arrays$ArrayItr` | `TreeMap$KeyIterator` | **yes** |
| `TreeMap.keySet()` | `Arrays$ArrayItr` | `TreeMap$KeyIterator` | **yes** |
| `PriorityQueue` | `ArrayList$Itr` | `PriorityQueue$Itr` | **yes** |
| `ArrayDeque` | `Arrays$ArrayItr` | `ArrayDeque$DeqIterator` | no — see below |

`apps/probes/FailFastShapeProbe` is 0-diff in both modes, so the four
iterator-class identity rows are closed as well; `TreeShadowSweep`,
`PqOptionalShadowSweep` and `MapViewsShadowSweep` are 0-diff in both modes.

Two FABRICATIONS went with it — `java/util/TreeSet$Itr` and
`java/util/ArrayDeque$Itr`, both of which `--jdk-only` refused and landed off,
so one receiver answered two different wrong class names depending on the mode.
`TreeSet$Itr` survives only for `descendingIterator`, whose HotSpot class is a
different carrier and a separate row.

**Three things this cost, each of which is the same shape.** A registration and
its force-native gate row are ONE edit: without the gate the natives are silent
and the real bodies run, and they report the collection EXHAUSTED rather than
erroring, so `PqOptionalShadowSweep` died at row 54 with `NoSuchElementException`
on a three-element queue. A registration keyed on a class NOBODY PRODUCES is a
trap armed for whoever produces one later: `java/util/PriorityQueue$Itr` had sat
in a 2-field-snapshot registrar since before anything minted it, and won the slot
(`owns=True inv=4`) over the registration that matched the shape actually minted.
And the JDK can be the second producer of your carrier class:
`ArrayDeque$DescendingIterator extends DeqIterator`, so a real descending
iterator arrived at natives expecting a snapshot block and walked to `[]` — the
`al_itr_alt_base` width test is now EXACT rather than a bound, and
`al_itr_delegate_foreign` sends anything that is not ours to its own bytecode.

#### The ArrayDeque row — CLOSED 2026-08-30, by DELETING four registrations

```text
apps/probes/DequeListShadowSweep
  81 ad fail fast on ADD during iteration      HotSpot CME        was no-throw
  82 ad fail fast on REMOVE during iteration   HotSpot no-throw   was no-throw
```

**HotSpot's `ArrayDeque` is fail-fast on one and not the other**, and the
previous entry here concluded that closing row 81 "needs a generation counting
structural GROWTH, which the deque has no field to hold". That is true and it is
the wrong question. `DeqIterator` declares no `expectedModCount` because the
JDK's fail-fast here is not a counter at all: the iterator holds a PHYSICAL
index into the ring buffer, and `nonNullElementAt` reports any null it reads as
a `ConcurrentModificationException`. It fires exactly when a mutation moves the
elements out from under that index. An `add` that fills the buffer grows it, and
`grow` slides the first leg to the far end and nulls the slots it came from,
straight under a cursor that has already advanced; a `remove` from the head
moves `head` PAST the cursor and nulls nothing the cursor will read. No
generation reproduces that asymmetry, which is why seeding one closed 81 and
opened 82.

**The control that settled it was already in the tree.** `descendingIterator()`
was never registered, so it ALREADY ran real JDK bytecode over our array —
whatever it answered was what a retired `iterator()` would answer.
`apps/probes/AdRetireProbe` asked it, and the answer was not the one either
diagnosis predicted:

```text
  fields after remove   HotSpot  cap=4 head=0 tail=2 es=[a, b, null, null]
                        CratonVM cap=4 head=0 tail=2 es=[a, b, null, null]
  size()                HotSpot  2         CratonVM 3
  toString()            HotSpot  [a, b]    CratonVM [a, b, null]
```

The buffer was byte-for-byte right and only the COUNT was wrong — and a null was
leaking out of the deque into `toString`, `toArray`, `stream` and a re-walk. It
also killed compatible mode outright at row 13 of that probe. `ad_refuse_null`'s
own doc explains the cost: the JDK's `nonNullElementAt` reads such a null as
"another thread mutated me" and kills the next iteration.

The cause is that slot 3 held the element count. The real `java.util.ArrayDeque`
declares only `elements`/`head`/`tail`, so every real JDK body that touches the
buffer desynced us. `native_ad_remove_first_occurrence` carries the scar in its
doc comment — it exists to keep real `delete` bytecode away from the deque,
after that desync stranded H2's `waitingSessions` queue and dead-ended every
later DDL in "Timeout trying to lock table SYS". **Shadowing every mutator is
the wrong level to fix that at, because the leak is any real body at all**, and
`DeqIterator.remove()` is the proof.

The fix made the representation the JDK's, in three parts, and only then stood
aside:

* `ad_state` DERIVES the count from `head`/`tail`, as `size()` does. Slot 3 is
  no longer written by anything.
* `ad_grow` is a port of `ArrayDeque.grow` — `Arrays.copyOf` slot-for-slot plus
  the wrap-slide, where we used to normalise to `head = 0`. `addFirst`/`addLast`
  store BEFORE growing, as the JDK does, which also retires the Family-1
  pin/refresh dance rather than maintaining it.
* `ad_remove_at_logical` is a port of `ArrayDeque.delete` — close the gap from
  the NEARER end. We always slid backwards, which yields the same deque and a
  different layout: invisible to every accessor of ours, and not to a physical
  cursor.

`apps/probes/AdFieldProbe` reads `elements`/`head`/`tail` back through
reflection (it needs `--add-opens java.base/java.util=ALL-UNNAMED`) and the
deque is now byte-identical to HotSpot's through growth, both `delete` branches
and a wrapped buffer. `ArrayDeque.iterator()` and `ArrayDeque$DeqIterator`'s
`hasNext`/`next`/`remove` are then RETIRED, and `ArrayDeque` leaves
`VALUES_ITR_CARRIERS`: a snapshot could carry the right class name but never the
fail-fast.

`DequeListShadowSweep` and `AdRetireProbe` are 0-diff in both modes.

**The general shape, which is the reason this entry is long.** The count was a
DERIVED quantity that had been stored, and storing it made a workaround
necessary — one that then outlived any chance of being complete, because you
cannot register every real body. The 2026-08-29 entry above it treated the
symptom the workaround produced.

#### A FOURTH cost, found 2026-08-29 after the lane landed: one unit test left red, and the mock hole under it

`cargo test -p cratonvm-native-collections --test gc_native_pins` went red on
`array_deque_iterator_roots_snapshot_graph_across_allocations`, deterministically,
5/5. The lane's own verification is arms and probes — `114/114 --jdk-only,
74/74 SUITE=core` — and neither runs this crate's Rust unit tests, so a red that
`cargo test` catches in 0.01 s survived a full landing gate.

**The message named a defect that is not there.** It read
`iterator lost backing deque: Int(-1)`, because the test hard-coded
`get_field(iterator, 2)` against the retired fabrication's layout
(`0 = array, 1 = cursor, 2 = backing deque`) and the real carrier's slot 2 is
`lastRet`, which the mint sets to `-1`. `Int(-1)` in a reference slot is exactly
the shape of the `gc::guard` "descriptor-aware field access DESTROYED the value"
family, so the obvious reading of that panic sends the next reader after a
coercion bug in a path that has none. The graph is fine; the test was pinning a
contract the change deliberately replaced:

```text
  DeqIterator[b + 0] -> ArrayList-shaped wrapper
                           wrapper[elementData] -> Object[n + 1]
                                                     [0..n) elements
                                                     [n]    THE SOURCE DEQUE
  DeqIterator[b + 1] = cursor  = 0
  DeqIterator[b + 2] = lastRet = -1        b = object_num_fields - 3
```

The test now WALKS that graph — trailing-three convention, the wrapper's one
array-valued field, the array's trailing capacity slot — rather than indexing
three slots that belong to three different classes, and asserts the carrier
class by name so a silent return to a fabrication cannot leave it green.

**The mock hole is the part worth keeping.** `MockCtx` did not override
`class_num_total_fields`, so it used the trait default: **zero declared fields
for every class**. That is not a neutral default here. `alloc_arraylist_iterator_as`
mints `class_num_total_fields + 3`, and `al_itr_alt_base` recognises this crate's
own mint by that EXACT width — behind an early-out for anything four fields or
narrower. A carrier reporting zero is minted three wide, trips the early-out, and
`al_itr_delegate_foreign` sends it to real bytecode, of which a mock has none.
So `hasNext` answered `Ok(None)` and **the entire `VALUES_ITR_CARRIERS` path was
unreachable from unit tests, silently** — the same species as the two traps
above, one level further out: not a registration nobody produces, but a test
context in which nobody can produce one. `MockCtx::declare_class_fields` closes
it; the test declares `DeqIterator`'s real four (`this$0`, `cursor`,
`remaining`, `lastRet`) and asserts the minted width is 7.

Falsified before landing, twice, each against the restored source: skipping the
source's `read_native_pin` fails the `assert_ne!` that names it, and skipping an
element's fails on the element's class id. No production code changed.

**The latent coupling this leaves.** `AL_ITR_PLAIN_MAX_FIELDS` (4) must stay
below every carrier's mint width, i.e. below `class_num_total_fields + 3` for
every entry in `VALUES_ITR_CARRIERS`. Today the narrowest is 4 declared fields
(`TreeMap$KeyIterator`, `ArrayDeque$DeqIterator`), so the margin is three. A
future carrier with one declared field would be delegated to its own bytecode
without a word.

### 6.2 `PriorityQueue.iterator().remove()` does not write through — FIXED 2026-08-29

Was 1 row: `size()` answered 3 after an `iterator().remove()` where HotSpot
answers 2.

The snapshot the iterator walks is now marked with its queue, using the same
trailing-capacity-slot marker every `values()` view uses, so
`native_al_itr_remove`'s existing write-through fires. What made this tractable
after the first reading called it "the path every map view shares" is that the
wrapper NEVER ESCAPES: it is minted inside `native_pq_iterator`, handed to the
`ArrayList$Itr`, and never returned. So the blast radius is not the marker's 21
consumers but the two this iterator actually reaches —

* `resync_values_view`, on the `next()` path via `al_state_for_read`, which
  would have rebuilt the snapshot as EMPTY because `collect_entries_any` knows
  six MAP families and a queue is none of them. It now declines for a queue
  source, exactly as `resync_ts_view` declines for a non-`TreeMap` one;
* `propagate_list_removal`, which now routes a queue source through the
  queue's own `remove(Object)` native — the one that owns the sift-down the
  heap invariant needs.

Both guards are keyed on a `PriorityQueue` source specifically, so no source
that exists today changes behaviour.

### 6.3 `reversed()` is a snapshot, not a live view — FIXED 2026-08-29

`LinkedSequencedShadowSweep` is 0-diff in both modes. Was: a write to the source
after the view was taken was invisible — `{c=3, a=1, b=2, d=4}` where HotSpot,
after a later `put`, answers `{e=5, c=3, a=1, b=2, d=4}`.

The snapshot STAYS: this VM has no real `head`/`tail`/`before`/`after` chain for
the JDK's `ReverseOrderLinkedHashMapView` bytecode to walk, which is why the view
was built by hand in the first place. What changed is that it is now rebuilt on
READ when its source has moved, which is observationally the same thing for
everything a caller can ask.

The source and the generation it was last built from live in the `lhm_overlay`
under two reserved names, rather than in a heap field or a new side table: the
overlay is already wired into all four collector hooks, so a source held there is
rooted, remapped and pruned for free. `resync_reversed_map` is one line at the
top of the eleven `LinkedHashMap` read natives and returns any other receiver
unchanged for the price of one overlay lookup. The generation is the source's
SIZE and can only MISS, never rebuild spuriously — a false positive would be an
O(n) rebuild on every read.

The generation is stamped BEFORE the rebuild, not after. The rebuild calls
`native_lhm_clear` and `native_lhm_put`, and a read native reached from either
lands back in the resync; with the old generation still recorded it would rebuild
again, and again. The clear also drops the source marker, which stops it a second
way — but relying on the ORDER of two side effects for termination holds only
until someone reorders them.

### 6.4 `Currency.getDisplayName(Locale.ENGLISH)` answers the CODE — FIXED 2026-08-29

Was `USD` where HotSpot answers `US Dollar`. `LocaleDateTzShadowSweep` is now
0-diff in both modes.

The first reading — "the CLDR bundle is not reachable" — was wrong, and the
probe that found the defect is the one that showed it: **the data was already
there.** `ResourceBundle.getString("usd")` returns `US Dollar` on this VM
today. CLDR keys `CurrencyNames` with the UPPERCASE code for the SYMBOL and the
lowercase code for the NAME, a convention `cldr_currency_symbol`'s own doc
comment had recorded, and nothing read the second half of it.

What was missing was the plumbing: `Currency.getDisplayName` ran real bytecode,
which reaches the name through `LocaleServiceProviderPool` +
`CurrencyNameProvider` — an SPI this VM does not serve. The pool answered null
and the JDK's documented last resort (the code itself) took over. `Locale
.getDisplayCountry` works on this VM for the opposite reason: it is a registered
bridge, and it fires.

Fixed with the twin of the symbol helper — `cldr_currency_display_name`, same
table, lowercase key — and a `getDisplayName(Locale)` bridge beside
`getSymbol(Locale)`. The no-arg form delegates to it in real bytecode, so one
override fixes both call forms.

**This ADDS a shadow while the campaign is retiring them, and that is debt, not
a win.** The right end state is the provider pool serving
`CurrencyNameProvider`, after which `Currency` needs no bridge at all; both
halves should be deleted together. It is the same trade `getSymbol(Locale)` and
the whole `Locale.getDisplay*` family already made in that file, and it buys a
right answer where real bytecode gives a wrong one.

Two things fell out of it. The curated fallback table in `phases_early.rs` (the
compatible-mode `getDisplayName()`) said **`British Pound Sterling`** where
HotSpot says `British Pound`, and had no entry for CNY/CHF/CAD/AUD at all; it
now goes through the shared helper so the two copies cannot drift, and the eight
fallback names are HotSpot's own, measured rather than guessed. And:

### 6.4b `ResourceBundle.getBundle` fabricates a bundle HotSpot refuses — FIXED 2026-08-29

`CurrencyNameProbe` is 0-diff in both modes, 27 rows.

`getBundle` is CALLER-SENSITIVE: it resolves against the caller's module.
`sun.util.resources.*` lives in `java.base` and is not exported, so an
unnamed-module caller cannot see it and HotSpot answers
`MissingResourceException` — while `java.base`'s own code loads it fine. This VM
had no such distinction and handed its synthesized locale bundle to everyone.

Widening the probe past the two rows that failed is what made the fix obvious:

```text
getBundle sun.util.resources.CurrencyNames        HotSpot MissingResourceException
getBundle sun.util.resources.LocaleNames          HotSpot MissingResourceException
getBundle sun.text.resources.FormatData           HotSpot MissingResourceException
getBundle sun.util.resources.CalendarData         HotSpot MissingResourceException
getBundle sun.util.resources.cldr.CurrencyNames   HotSpot MissingResourceException
getBundle com.example.NoSuchBundleAtAll20260829   HotSpot MissingResourceException  <- already agreed
```

The last row is the tell. **The refusal path already existed** — `rb_get_bundle`
throws for every base name `is_jdk_internal_bundle` does not claim. All the fix
adds is that a JDK-internal name is JDK-internal to APPLICATION code too, which
is one `&& !caller_is_app` on that gate.

The synthesized bundles stay for a `java.*`/`sun.*`/`jdk.*` caller, which is what
keeps this VM's own locale shims working: `populate_format_data_en` exists
because real `DateFormatSymbols` bytecode needs it. The caller is read from
`capture_stack_trace(0).last()` — the immediate caller, since that capture is
outermost-first — before anything allocates, and it is the same reading
`caller_bundle_class_loader` next door already relies on.

This was the residual filed as "not fixed here, because the VM's own locale
shims consume these synthetic bundles". That was true and it was not a reason to
stop: the shims and the application are different callers, and the VM could
already tell them apart.

### 6.5 The method-reference dispatch door — FIXED 2026-08-29

Was 13 of `MethodRefDoorProbe`'s 25 rows; the probe is now 0-diff in both modes.
The cause was not the missing gate the companion page first named but the class
the gate is asked ABOUT: ordinary virtual dispatch probes the registry with the
RECEIVER's class, and the lambda door probed with the class that DECLARES the
method. `java/util/HashMap$KeyIterator` carries the native and does not declare
`remove()`; `java/util/HashMap$HashIterator` declares it and carries nothing.
Fixed in `vm/src/runtime/interpreter/lambda.rs`. Full account, including the 977
registrations that share the shape and were deliberately NOT activated, in
`a-bound-method-reference-is-a-different-dispatch-door-20260828.md`.

### 6.6 `Properties.keySet().iterator()` answers the wrong iterator class — FIXED 2026-08-29

`ItrClassNameProbe` is 0-diff in both modes.

```text
Properties.keySet().iterator().getClass().getName()
  HotSpot   java.util.concurrent.ConcurrentHashMap$KeyIterator
  was       java.util.HashMap$KeyIterator          (both modes)
```

JDK 25 backs `Properties` with a `ConcurrentHashMap` and `Properties.keySet()`
is `map.keySet()`, so the iterator belongs to the CHM family. Widening the probe
showed the other two thirds already agreed — the view's own class is
`Collections$SynchronizedSet` on both, and the view is live on both (a `put`
after the view is taken moves its `size`) — which is what narrowed this to the
iterator's class alone and made it a four-line change: a key/entry view whose
SOURCE is a `Properties` answers the CHM pair, checked ahead of the class-name
tests because this VM's `Properties` keySet is carried by a `LinkedHashSet`.

**Not the two-producers trap, and the difference is the point.** This mints the
same class the CHM views mint. Every name in `MAP_KEY_ITR_CARRIERS` shares ONE
object shape — five fields at `key_itr_base`, derived from the object's WIDTH —
and ONE registrar. A second producer is a problem when the two SHAPES differ, as
the dormant `PriorityQueue$Itr` registration in §6.1 was; here the class name is
a label over an identical object and the natives cannot tell the producers
apart, because there is nothing to tell apart.

## 6b. The coverage sweep, and the twelve spliterator cells it found

**Written after the residuals closed, from the registry rather than from a
hypothesis.** A dump taken DURING all sixteen probes, merged, says the corpus
reaches 445 of the 602 owning `java.util` registrations that sit over a real JDK
body. The other 157 were never invoked.

The first reading of that number is "retirement candidates". It is not: the
never-invoked set is almost entirely the NavigableSet surface of
`TreeMap.keySet()` (18 rows), HashMap's conditional mutators on the plain
family (11), and the sublist `ListIterator` (7) — **a coverage gap in the
probes, not dead code**. That is `a-zero-invocation-count-is-evidence-about-a-
counter` in its exact shape: `invocations` is a floor.

`apps/probes/UtilCoverageSweep` closes it — 141 rows over precisely those
registrations — and found a defect on its first run.

### The spliterator characteristics matrix — 12 cells

§3's spliterator work moved the characteristics mask into the object and gave
three producers their own answer. It was right about those three, because the
probe that drove it asked about three receivers. Asking all twenty-eight:

```text
                 keySet   values   entrySet    standalone
  HashMap           65       64       65             65
  LinkedHashMap  16465    16464    16465          16465
  TreeMap           85       80       85             85
  Hashtable      16449    16448    16449             --
  Properties      4353     4352    16449             --
```

**Twelve of those cells were wrong** — every map view, plus the standalone
`LinkedHashSet`. Every ArrayList-shaped view took the list default, so
`HashMap.values()` claimed an encounter order it does not have and
`TreeMap.entrySet()` claimed neither the DISTINCT nor the SORTED it does; every
set-shaped view took the plain `HashSet` cell, so `LinkedHashMap.keySet()` lost
both ORDERED and SUBSIZED.

Three things in the matrix are not derivable, which is why it is measured and
not computed:

* **`values` is never DISTINCT**, and for `HashMap` not ORDERED either. A values
  view can repeat, and a hash map has no encounter order. `TreeMap`'s values are
  ORDERED but not SORTED — the sort is on the keys.
* **`SUBSIZED` follows the JDK's CONSTRUCTION, not the container.** The
  `LinkedHashMap` and `Hashtable` families go through
  `Spliterators.spliterator(Collection, ..)`, which adds `SIZED | SUBSIZED`;
  `HashMap`'s and `TreeMap`'s have hand-written spliterator classes that do not.
  That is the whole reason `LinkedHashSet` is 16465 and `HashSet` is 65 despite
  being the same shape of container.
* **`Properties` is CONCURRENT | NONNULL and NOT SIZED** — JDK 25 backs it with
  a `ConcurrentHashMap`, the same fact behind §6.6 — and its `entrySet` takes
  the `Hashtable` cell rather than the concurrent one. That asymmetry is
  HotSpot's, and a derived table would have smoothed it away.

These are not cosmetic: a stream pipeline reads DISTINCT to decide it may skip a
`distinct()`, SORTED to skip a sort, and SIZED/SUBSIZED to decide how to split
in parallel.

### The immutable factories — 8 of 8 closed 2026-08-30

```text
                       HotSpot   was     now
  Set.of("a")            17745    65    17745
  Set.of()               16449    65    16449
  Set.of("a","b")        16449    65    16449
  Set.of x3 (SetN)       16449    65    16449
  List.of("a")           17745  16464   17745
  Map.of().keySet()      16449    65    16449
  Map.of().values()      16448    64    16448
  Map.of().entrySet()    17745    65    17745
```

**The discriminator is the CLASS, not the size** — and the previous entry here
said the opposite, which is why the first fix closed four cells and left four.
"A size-1 immutable routes to `Collections.singletonSpliterator`" explains
`Set.of("a")`, `List.of("a")` and a one-entry `entrySet`, and is then flatly
contradicted by the SAME map:

```text
  Set.of("a")              17745  ImmutableCollections$Set12 / Collections$2
  Map.of("a",1).entrySet() 17745  ImmutableCollections$Set12 / Collections$2
  Map.of("a",1).keySet()   16449  AbstractMap$1 / Spliterators$IteratorSpliterator
  Map.of("a",1).values()   16448  AbstractMap$2 / Spliterators$IteratorSpliterator
```

`apps/probes/ImmutableSplProbe` asks all four factories at sizes 0, 1, 2 and 3
and prints the CLASS of the view and of the spliterator beside every mask. That
column is the rule. Every 17745 is `Collections$2`, reached when the factory
built a `List12`/`Set12` whose second slot is the empty sentinel — so "size 1"
is right, but only for a collection of that shape. A `Map.of`'s keySet and
values are not: they are the anonymous `AbstractMap$1`/`$2` views, which carry
no immutable bits at ANY size. `entrySet` is the odd one only because
`Map1.entrySet()` is literally `Set.of(entry)`; a two-entry map's `MapN$1`
entrySet drops back to 16449.

Two changes. `native_unmod_spliterator` now MINTS a spliterator when the
delegate returns one of the JDK's own — the `List.of` cell, where the delegate
lands on java.base's `ArrayList.spliterator()`, whose answer is right for the
`ArrayList` behind the wrapper and wrong for a `List12`, and which we may not
write into. And map VIEWS carry a marker saying the map they view came from an
immutable factory, deliberately NOT the existing `UNMOD_FIELD_IMMUTABLE`: that
slot is a cross-crate contract with `getclass_immutable_marker`, which uses it
to choose between two class names, and a `Map.of` keySet is NEITHER of them on
HotSpot.

**A defect this lane SHIPPED and then caught, one commit later.** The first
attempt wrote the mask into slot 3 of whatever `spliterator()` returned. For
`List.of` that is java.base's real `ArrayList$ArrayListSpliterator`, whose slot
3 is its `this$0`; the write clobbered it and the next `estimateSize()` died in
`getFence` with `NullPointerException: Cannot read field "modCount" because
"this.this$0" is null`. It reached `dev` and stood for one commit. What found it
was the next probe row written for an unrelated reason — `estimateSize` — which
killed the compatible run at row 30 of 160. `two-producers-of-one-carrier-class`
in its most direct form, and the third time this lane met that family.

## 6c. The fourth coverage round, and the crash under it

The 45 registrations no probe had reached were not a long tail of singletons.
They were five CLUSTERS, and four had never been asked at all:

```text
  java.util.Date, deprecated instance surface   19 rows
  serialization: writeReplace / read+writeObject 11 rows
  the views' own toArray(T[]) and forEach         8 rows
  OptionalInt / OptionalLong / OptionalDouble     7 rows
  Locale / TimeZone / ResourceBundle display     20 rows
```

`apps/probes/UtilCoverage4Sweep` asks them. 152 rows, now 0-diff in both modes,
and two defects on the way there.

### `--jdk-only` could not DESERIALIZE any immutable collection

```text
  ser List.of(1)   HotSpot ImmutableCollections$List12 [a]
                   strict  THREW java.lang.NoClassDefFoundError
                           cratonvm/internal/UnmodifiableList
```

— and the same for `List.of(3)`, `Set.of(1)`, `Set.of(3)`, `Map.of(1)` and
`Map.of(3)`. WRITING worked and produced the same 59 bytes HotSpot writes; the
READ side reached `native_collser_read_resolve`, which rebuilds through
`of_list` and freezes into a `cratonvm/internal/Unmodifiable*` — a class strict
mode refuses to fabricate, by design.

Every producer of that carrier is supposed to be dropped under `--jdk-only`, and
`alloc_immutable_wrapper`'s doc says so and enumerates them. This one was missed
because it is registered from a DIFFERENT registrar than the factories it
mirrors, and that registrar sets `Bridge` for its whole window. `Bridge` asserts
"no working real-bytecode fallback exists" — for this family, a compatible-mode
claim wearing a mode-independent tag.

The reason the native exists is real, and recorded at
`native_collser_read_resolve`: the JDK's own body rebuilds maps through real
`ImmutableCollections` constructors, producing a `table`-backed object this
crate's map natives read as empty. True in COMPATIBLE mode, where those natives
run; exactly false under `--jdk-only`, where they are dropped and a real `Map1`
is the only right answer. The family is `SyntheticStub` now: compatible mode
unchanged, strict drops all eight rows and agrees with HotSpot down to the class
name.

### A `Locale` variant BCP-47 cannot carry went out raw, in both modes

```text
  new Locale("de","AT","x").toLanguageTag()
    HotSpot   de-AT-x-lvariant-x
    CratonVM  de-AT-x
```

`de-AT-x` is not merely different, it is malformed — a bare `x` singleton with
nothing after it — and `forLanguageTag` read the variant back as EMPTY where
HotSpot recovers `"x"`. Measured across eleven shapes rather than fixed from the
one that failed, because the rule has an end no one would guess:

```text
  POSIX       de-AT-POSIX                  5 alphanum: a subtag
  1234        de-AT-1234                   4, digit-first: a subtag
  x           de-AT-x-lvariant-x           1 char: private use
  POSIX_WIN   de-AT-POSIX-x-lvariant-WIN   split at the first ill-formed one
  x_POSIX     de-AT-x-lvariant-x-POSIX     ill-formed first: all of it
  abcdefghi   de-AT                        9 chars: DROPPED ENTIRELY
```

The last row is the end of the rule: a private-use subtag is itself 1-8
alphanumerics, so a 9-character variant does not fit there either and the whole
sequence is dropped. The locale has no tag that can express it.

**Two producers, and the first fix moved nothing.** `locale_tag` builds the tag
twice — once from the `locale_populate` side table the constructors fill, once
from a real `baseLocale` — and the side-table branch RETURNS FIRST for anything
built by `new Locale(..)`. Fixing the `baseLocale` branch alone changed not one
probe row across a full build. Both call one `append_locale_variant` now.

### A probe bug that read as a coverage gap

`UtilCoverage3Sweep` asks for a view's typed `toArray` as
`new TreeSet<>(m.keySet()).toArray(new String[0])`. The copy was there to make
the order deterministic, and it silently retargeted every row in the block to
`TreeSet.toArray` — which is why eight view registrations read as unreachable
for two rounds. Round 4 asks the view directly and sorts the RESULT instead.

## 6d. What is left, and why it is not more probe rows

Coverage ended at **566 of 586** owning registrations with a real body. The
remaining 20 do not want another sweep, because most of them are not reachable
at all.

`apps/probes/DeadDoorProbe` calls each one with the most favourable receiver
available and then reads the registry back. They stay at `invocations = 0`:

```text
  AbstractCollection.toArray()      AbstractSet.hashCode()
  Collection.stream()               Collection.toArray(IntFunction)
  Map.forEach(BiConsumer)           SequencedMap.pollFirst/LastEntry
  TimeZone.getOffset(J)             TimeZone.getDisplayName() x2
  TimeZone.getOffsets(J[I)          TimeZone.setDefaultZone()
```

**An instance-method registration on an abstract class or an interface is
unreachable by every door.** `dispatch_virtual` probes the registry with the
RECEIVER's runtime class, and the lambda and stackless paths probe with the
DECLARING class of the resolved method — which for `plain.forEach` is
`AbstractMap`, never `Map`. The probe tests both, including bound method
references, which is the door that reaches a declaring class. The CONTROL is in
the same dump: `TimeZone.getTimeZone` (inv=4) and `TimeZone.getDefault` (inv=1)
fire normally, because a STATIC call names the class directly.

For `TimeZone` the point is sharper still — no factory ever returns a
`java.util.TimeZone`. `getTimeZone`, `getDefault` and `getTimeZone("UTC")` all
hand back `sun.util.calendar.ZoneInfo`, and `TimeZone` is abstract, so no
instance of the registered class can exist.

**This is a retirement work-list, not a coverage gap, and it needs per-row
evidence rather than the rule.** The exception proves why: registrations on
`java/util/Spliterator` ARE reached, because this crate mints its spliterator as
a concrete object whose class name is the interface — the corpus exercises one
of that class's two rows. So "registered on an interface" does not imply "dead";
it implies "dead unless something produces a carrier with that name", and that
question is per-row.

The rest of the 20 are `ResourceBundle` (needs a real bundle on the classpath,
which no probe here supplies), the `readObject`/`writeObject` pair on `HashMap`
and `TreeSet` (serialization round-trips correctly, so the reflective path that
runs them does not consult the registry), and a handful of constructors and
`<clinit>`.

## 7. The final verification

Every number re-taken at the end, on one binary built from the merge of this
lane with `dev`. Twenty-three probes recompiled from source and re-run, both
modes, one run:

| probe | rows (HotSpot / compat / strict) | differing rows compat / strict |
| --- | --- | --- |
| `PropertiesShadowSweep` | 182 / 182 / 182 | 0 / 0 |
| `TreeShadowSweep` | 232 / 232 / 232 | 0 / 0 |
| `DequeListShadowSweep` | 172 / 172 / 172 | 0 / 0 |
| `HashtableVectorShadowSweep` | 136 / 136 / 136 | 0 / 0 |
| `ArrayListShadowSweep` | 164 / 164 / 164 | 0 / 0 |
| `LinkedSequencedShadowSweep` | 102 / 102 / 102 | 0 / 0 |
| `PqOptionalShadowSweep` | 125 / 125 / 125 | 0 / 0 |
| `CollectionsShadowSweep` | 172 / 172 / 172 | 0 / 0 |
| `LocaleDateTzShadowSweep` | 123 / 123 / 123 | 0 / 0 |
| `MapViewsShadowSweep` | 300 / 300 / 300 | 0 / 0 |
| `UtilTailShadowSweep` | 146 / 146 / 146 | 0 / 0 |
| `UtilTail2Sweep` | 87 / 87 / 87 | 0 / 0 |
| `UtilCoverageSweep` | 160 / 160 / 160 | 0 / 0 |
| `UtilCoverage3Sweep` | 75 / 75 / 75 | 0 / 0 |
| `UtilCoverage4Sweep` | 152 / 152 / 152 | 0 / 0 |
| `MethodRefDoorProbe` | 25 / 25 / 25 | 0 / 0 |
| `FailFastShapeProbe` | 12 / 12 / 12 | 0 / 0 |
| `ItrClassNameProbe` | 12 / 12 / 12 | 0 / 0 |
| `CurrencyNameProbe` | 27 / 27 / 27 | 0 / 0 |
| `PqItrProbe` | 11 / 11 / 11 | 0 / 0 |
| `AdRetireProbe` | 28 / 28 / 28 | 0 / 0 |
| `DeadDoorProbe` | 29 / 29 / 29 | 0 / 0 |
| `ImmutableSplProbe` | 58 / 58 / 58 | **15 / 2** |

2530 rows. Row counts equal and the trailing `DONE` present on all sixty-nine
runs, so no run is a truncated tail reading as clean.

**Every differing row is a class NAME, and every characteristics row matches.**
They belong to a carrier this lane does not own, and are carried forward in
`l3-followups-the-carrier-identity-and-the-dead-registrations-20260830.md`.

The residuals as first recorded were 8 rows plus the companion record's 13. All
21 are closed, and five of them closed by finding that the first diagnosis
pointed at the wrong thing:

| residual | the first reading | what it actually was |
| --- | --- | --- |
| §6.5 dispatch door | "the MethodHandle path does not consult the force-native gate" | it consults it about the DECLARING class where virtual dispatch uses the RECEIVER's |
| §6.4 currency name | "the CLDR bundle is not reachable" | the data was already there under the lowercase key; the plumbing was missing |
| §6.1 fail-fast | "the check has nowhere to run", then "needs a growth counter the deque has no field for" | the fail-fast is the ring buffer's LAYOUT, and the shadow was what prevented it |
| §6.4b bundle | "the VM's own shims consume these, so it cannot refuse" | the shims and the application are different CALLERS, and the VM could already tell them apart |
| §6b spliterators | "the discriminator is SIZE, not class" | the CLASS; size only decides which class the factory built |

Gates: all six RC=0. Arms: 118/118 under `--jdk-only`, 118/118 `SUITE=all`,
78/78 `SUITE=core`.

## 8. Reproduce

```bash
CV=target/release/cratonvm
"$JDK/bin/javac" -d apps/probes/out \
    apps/probes/*ShadowSweep.java apps/probes/UtilTail2Sweep.java \
    apps/probes/UtilCoverageSweep.java apps/probes/UtilCoverage3Sweep.java \
    apps/probes/UtilCoverage4Sweep.java apps/probes/MethodRefDoorProbe.java \
    apps/probes/FailFastShapeProbe.java apps/probes/ItrClassNameProbe.java \
    apps/probes/CurrencyNameProbe.java apps/probes/PqItrProbe.java \
    apps/probes/ImmutableSplProbe.java apps/probes/AdRetireProbe.java \
    apps/probes/DeadDoorProbe.java
for C in PropertiesShadowSweep TreeShadowSweep DequeListShadowSweep \
         HashtableVectorShadowSweep ArrayListShadowSweep \
         LinkedSequencedShadowSweep PqOptionalShadowSweep \
         CollectionsShadowSweep LocaleDateTzShadowSweep \
         MapViewsShadowSweep UtilTailShadowSweep UtilTail2Sweep \
         UtilCoverageSweep UtilCoverage3Sweep UtilCoverage4Sweep \
         MethodRefDoorProbe FailFastShapeProbe ItrClassNameProbe \
         CurrencyNameProbe PqItrProbe ImmutableSplProbe AdRetireProbe \
         DeadDoorProbe; do
  "$JDK/bin/java" -cp apps/probes/out "$C" > /tmp/$C.hs 2>/dev/null
  "$CV" --java-home "$JDK"            -cp apps/probes/out "$C" > /tmp/$C.compat 2>/dev/null
  "$CV" --java-home "$JDK" --jdk-only -cp apps/probes/out "$C" > /tmp/$C.strict 2>/dev/null
  diff /tmp/$C.hs /tmp/$C.strict
done
```

`apps/probes/AdFieldProbe` is run separately: it reads `ArrayDeque`'s private
fields back through reflection and needs
`--add-opens java.base/java.util=ALL-UNNAMED` on all three arms.

Check the ROW COUNT and the trailing `DONE <probe>` line before reading any
diff: a run that died partway produces a short file whose missing tail `diff`
reports as ordinary `<` lines, and this lane hit exactly that three times.
