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
the thirteen of the companion dispatch-door record (those thirteen are
CLOSED as of 2026-08-29 — see §6.5).

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

#### The ArrayDeque row that is still open, and why it is not a size check

```text
apps/probes/DequeListShadowSweep
  81 ad fail fast on ADD during iteration      HotSpot CME        here no-throw
  82 ad fail fast on REMOVE during iteration   HotSpot no-throw   here no-throw
```

**HotSpot's `ArrayDeque` is fail-fast on one and not the other.** `DeqIterator`
detects a modification only when the ring buffer shifts under the cursor, which
an `add` that wraps does and a `remove` from the far end does not. It also
declares no `expectedModCount` for the third door to seed.

Giving it one was tried: its `remaining` slot is free (every declared field on a
carrier this crate mints is unused, since the mint writes only the three
snapshot fields past them), seeded from the source's SIZE. That closed row 81
and OPENED row 82, because size moves for both — one wrong row traded for
another, in the worse direction, since a spurious
`ConcurrentModificationException` is the failure this file has already paid for
once. Reverted. Closing row 81 needs a generation counting structural GROWTH
rather than size, and the deque has no field to hold one.

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

### 6.4b `ResourceBundle.getBundle` fabricates a bundle HotSpot refuses — 2 rows, OPEN

```text
ResourceBundle.getBundle("sun.util.resources.CurrencyNames", Locale.ENGLISH)
  HotSpot   MissingResourceException: Can't find bundle for base name ...
  CratonVM  a java.util.ResourceBundle, whose getString("USD") answers "$"

ResourceBundle.getBundle("sun.util.resources.LocaleNames", Locale.ENGLISH)
  same shape
```

A fabricated SUCCESS, which is the more serious direction: an application
probing for a bundle it does not expect to exist is told it does. The
campaign's own fabrication screen does not catch it, because
`java.util.ResourceBundle` is a REAL class — what is fabricated is the
resource, not the type.

Not fixed here, and the reason is worth stating rather than leaving as silence:
the VM's own locale shims consume these synthetic bundles, so making
`getBundle` refuse them is a change to the locale/resource surface with its own
blast radius, not a `java.util` collections fix. Probe:
`apps/probes/CurrencyNameProbe.java`, rows 17-21.

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

### 6.6 `Properties.keySet().iterator()` answers the wrong iterator class — 1 row

Found on 2026-08-29 while diagnosing §6.5, by a probe written to check an
assumption rather than to find a defect:

```text
Properties.keySet().iterator().getClass().getName()
  HotSpot   java.util.concurrent.ConcurrentHashMap$KeyIterator
  CratonVM  java.util.HashMap$KeyIterator          (both modes)
```

Real: `Properties` stores its entries in a `ConcurrentHashMap` in JDK 25, so its
key-set iterator is the CHM one. This VM keeps them in the side table and mints
the `HashMap` carrier. Behaviourally the two agree on all 182 rows of
`PropertiesShadowSweep` — this is a `getClass().getName()` difference, which is
the shape `an-identity-only-probe-understates-a-behavioural-gap` warns can be
either cosmetic or the visible edge of a real one. Recorded rather than fixed
because changing the minted carrier is the
`two-producers-of-one-carrier-class-is-a-failure-family` trap: the CHM key
iterator name already has a producer (`MAP_KEY_ITR_CARRIERS`), and adding a
second one to it is what made `elements()` never terminate. Probe:
`apps/probes/ItrClassNameProbe.java`.

## 7. The final verification, on the tree every lane landed into

The numbers above were taken as the lane went. They were RE-TAKEN at the end, on
the binary built from the merge of all seven lanes plus the release-day
reorganisation, `cargo fmt` and the GPU work — a tree that differs from the one
the fixes were written against by far more than this lane contributed. All
twelve probes recompiled from source and re-run, both modes, one run:

| probe | rows (HotSpot / compat / strict) | differing rows compat / strict |
| --- | --- | --- |
| `PropertiesShadowSweep` | 182 / 182 / 182 | 0 / 0 |
| `TreeShadowSweep` | 232 / 232 / 232 | 2 / 2 |
| `DequeListShadowSweep` | 172 / 172 / 172 | 1 / 1 |
| `HashtableVectorShadowSweep` | 136 / 136 / 136 | 0 / 0 |
| `ArrayListShadowSweep` | 164 / 164 / 164 | 0 / 0 |
| `LinkedSequencedShadowSweep` | 102 / 102 / 102 | 1 / 1 |
| `PqOptionalShadowSweep` | 125 / 125 / 125 | 2 / 2 |
| `CollectionsShadowSweep` | 172 / 172 / 172 | 0 / 0 |
| `LocaleDateTzShadowSweep` | 123 / 123 / 123 | 1 / 1 |
| `MapViewsShadowSweep` | 300 / 300 / 300 | 0 / 1 |
| `UtilTailShadowSweep` | 146 / 146 / 146 | 0 / 0 |
| `MethodRefDoorProbe` | 25 / 25 / 25 | 0 / 0 |

Row counts equal and the trailing `DONE` present on all thirty-six runs, so no
run is a truncated tail reading as clean. The eight differing rows are §6's
eight and the thirteen are the companion record's thirteen — the same rows, not
merely the same count. Nothing the other six lanes landed moved a row of this
one, and nothing this lane landed moved after the merges.

Gates on that tree: all six RC=0. Arms: 112/112 under `--jdk-only`, 112/112
`SUITE=all`, 72/72 `SUITE=core`.

## 8. Reproduce

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
