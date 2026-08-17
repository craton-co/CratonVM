# Collection-view carrier residuals: the Set-shaped views, `subList`, and three liveness rows — FIXED but for one row

**Status:** ✅ FIXED 2026-08-13, **11 of 12 rows**.
`probes/MapViewBehaviourProbe` is byte-identical to HotSpot JDK 25 (194/194
assertions, was 6 diverging) and `probes/ViewClassProbe` is 29 of 30 lines (was
18 diverging). The one row still open is `arrayList.subList(0,2).getClass()`,
and it is open for a reason this page MEASURED rather than assumed — see
[the one row that stayed open](#the-one-row-that-stayed-open). Filed as the
residual of
[the values-view carrier fix](arraylist-native-overhead-and-view-carrier-FIXED-20260813.md),
which closed the `values()` family and left this.

## What was wrong

Two unrelated families on one page.

### 1. Carrier classes for the Set-shaped views

| expression | HotSpot JDK 25 | before | after |
| --- | --- | --- | --- |
| `hashMap.keySet().getClass()` | `java.util.HashMap$KeySet` | `java.util.HashSet` | ✅ |
| `hashMap.entrySet().getClass()` | `java.util.HashMap$EntrySet` | `java.util.HashSet` | ✅ |
| `linkedHashMap.keySet().getClass()` | `…$LinkedKeySet` | `java.util.HashSet` | ✅ |
| `linkedHashMap.entrySet().getClass()` | `…$LinkedEntrySet` | `java.util.HashSet` | ✅ |
| `treeMap.keySet().getClass()` | `java.util.TreeMap$KeySet` | `java.util.TreeSet` | ✅ |
| `hashtable.keySet().getClass()` | `…$SynchronizedSet` | `java.util.HashSet` | ✅ |
| `hashtable.values().getClass()` | `…$SynchronizedCollection` | `java.util.Hashtable$ValueCollection` | ✅ |
| `arrayList.subList(0,2).getClass()` | `java.util.ArrayList$SubList` | `cratonvm.internal.ArrayListSubList` | ⚠️ unchanged |
| `linkedHashSet.iterator().getClass()` | `…$LinkedKeyIterator` | `java.util.HashMap$KeyItr` | ✅ |

### 2. Three liveness rows

```
tm.keys.afterPut.size       HotSpot 3   CratonVM 2
props.keys.afterPut.size    HotSpot 3   CratonVM 2
props.entries.afterPut.size HotSpot 3   CratonVM 2
```

A `TreeMap.keySet()` and a `Properties.keySet()`/`entrySet()` captured BEFORE a
later `put` did not see it. Older than the carrier work and independent of it —
identical before and after the values fix.

## The fix

### The undeclared-slot rule, and the two ways it can be wrong

The values fix established that a `cratonvm/internal/*` carrier is only needed
when the native state **collides** with the real class's declared fields. This
page is where that rule was applied four more times, and where both of its edges
turned up.

**Edge 1 — the collision is a TYPE collision, and slot 0 is not always free.**
The first cut of the Set carriers put the backing map at `HS_FIELD_MAP`, i.e.
absolute slot 0, reasoning that `HashSet` declares nothing before `map` and that
`HashMap$KeySet` declares exactly one reference (`this$0`) there. That is true —
for `HashMap`:

```text
java.util.HashMap$KeySet              final java.util.HashMap this$0;
java.util.LinkedHashMap$LinkedKeySet  final boolean reversed;
                                      final java.util.LinkedHashMap this$0;
```

The JDK 21+ sequenced-collection views declare `reversed` **first**, so slot 0
there is a `boolean`. Writing a reference into it made every
`linkedHashMap.keySet()` read back EMPTY (`lhm.keys.size` 0 against HotSpot's 2)
and sent `AbstractMap.toString` into unbounded recursion. The fix is
`hs_map_slot`: anchor on `class_num_total_fields`, i.e. **past** the declared
fields, never at a fixed index. The values carriers were already safe by
accident — they write ArrayList's absolute slots 1 and 2, and slot 1 is
`this$0`, a reference.

**Edge 2 — `subList`'s stated reason was wrong, and it still did not land.**
`cratonvm/internal/ArrayListSubList` existed with a comment saying a sublist
view "has its own native field layout" and therefore cannot be the real
`java.util.ArrayList$SubList`. Its five fields do collide (`root`, `parent`,
`offset`, `size`, plus `AbstractList.modCount`) — but a collision at slots 0..4
only rules out writing at slots 0..4, and `asl_base` at 5..9 measures clean
under `--real-jdk`. The blocker turned out to be somewhere else entirely; see
[the one row that stayed open](#the-one-row-that-stayed-open).

The same shape closes the iterator row: `HashMap$HashIterator` declares
`next`/`current` (references) and `expectedModCount`/`index` (ints), so this
VM's `(array, cursor, total, backing, lastRet)` would have landed an `Int`
cursor in a reference slot. `key_itr_base` moves them past, and
`native_hs_iterator` picks `HashMap$KeyIterator` /
`LinkedHashMap$LinkedKeyIterator` / the `Entry` pair from the receiver — which
is what makes `linkedHashSet.iterator()` answer `LinkedKeyIterator` rather than
the fabricated `java.util.HashMap$KeyItr` that no JDK declares.

**Both bases are derived from the OBJECT's width, not from its class.** Every
mint site allocates exactly `declared + N` slots, so `base = n_fields - N` is
one header read — which matters, because `hasNext`/`next` run once per element
and a class lookup there would be paid by every iteration in the VM. It also
keeps the legacy narrow shapes (a three-field snapshot iterator, a four-field
sublist) at base 0, so the width guards that tell them apart still mean what
they meant.

### `Hashtable`, and what a wrapper costs in synthetic-JDK mode

`Hashtable.keySet()` is `Collections.synchronizedSet(new KeySet(), this)` in the
JDK, so matching `getClass()` means returning the wrapper, not the inner view.
`Collections.synchronizedSet` already built a real
`Collections$SynchronizedSet`, so `wrap_synchronized_view` is three lines.

What was NOT free is the synthetic-JDK half. The fabricated
`Collections$Synchronized{Collection,Set}` declared seven methods —
`add`/`contains`/`remove`/`size`/`isEmpty`/`iterator`/`toArray` — which is
everything `Collections.synchronizedCollection(…)` had ever been asked for and
nowhere near what a map view is asked for. Undeclared, `stream`/`forEach`/
`toString`/`containsAll`/`retainAll`/… would have fallen through to an
interface-level native that reads the WRAPPER as the collection and reports it
empty: a silent wrong answer, in the one mode whose gate is blocking. The
surface is now complete, forwarded by `sync_collection_delegate`, with
`equals`/`hashCode` on the Set wrapper only — matching the JDK, where
`SynchronizedCollection` inherits `Object` identity and `SynchronizedSet`
overrides both.

### The three liveness rows

* **`TreeMap.keySet()`** is a `TreeSet`-shaped view carrying its source in the
  trailing capacity slot. Every WRITE path consulted that marker
  (`remove`/`clear`/`pollFirst`/`iterator().remove()`); no READ path did. New
  `resync_ts_view`, called from the TreeSet read natives, is the counterpart of
  `resync_view_set` (HashSet-carried) and `resync_values_view`
  (ArrayList-carried). Only a MAP source is rebuilt: `descendingSet()` puts
  another `TreeSet` behind the same marker and its elements are the source's in
  REVERSE, so rebuilding it in sorted order would silently un-reverse the view.

* **`Properties.keySet()`/`entrySet()`.** The page asked whether
  [the 2026-07-24 `properties-keyset-view-not-live` fix](../springboot/properties-keyset-view-not-live-FIXED.md)
  had regressed or merely covered a narrower path. **Narrower path:** it fixed
  the WRITE direction only — `retainAll`/`remove` propagated through a native
  override on `LinkedHashSet` gated on a side table. Reads were never live at
  all, because the returned set was a plain snapshot.

  `Properties` is the one map whose keys cannot be collected by walking the
  receiver: half live in a Rust side-table and half in the `map` CHM field, so
  the resync path every other keySet view uses (`collect_keys_any` →
  `map_collect_keys`) sees only the second half — a WRONG answer, not a stale
  one. Hence a new view kind: `VIEW_KIND_KEYSET_STATIC`, whose sibling
  `VIEW_KIND_ENTRYSET_STATIC` already existed and meant "never resync". Both now
  mean "resync by asking the source for a fresh view and adopting its backing"
  (`adopt_fresh_view_backing`). The source's own accessor is the only code that
  knows how to assemble that source's contents; adopting its backing wholesale
  means the resync never has to.

  Because the view carrier brings write-through with it (`native_hs_remove`
  consults the view backing), the Properties-specific machinery is gone: the
  side table, its global `Mutex`, the per-snapshot global root, and both
  `LinkedHashSet` override arms. The two registrations stay — they are
  load-bearing for ORDINARY `LinkedHashSet`s, which must reach
  `try_native_hashset_remove` rather than the `PRESENT`-sentinel bytecode (see
  [the `@SuppressWarnings` record](../suppresswarnings-annotation-duplicate-value-bug-20260726.md),
  where that exact fall-through made every annotation with a `value` element
  uncompilable).

## The one row that stayed open

`arrayList.subList(0,2).getClass()` is still `cratonvm.internal.ArrayListSubList`,
and the reason is **receiver ownership, not layout**.

Putting the five native fields past the JDK class's five works: with the real
carrier, `ViewClassProbe` and `MapViewBehaviourProbe` are both byte-clean and
`probes/JdkOnlyCollectionViewProbe`'s `--real-jdk` arm is 0 diverging. Its
`--jdk-only` arm is 14 half-lines red:

```text
sublist.mid            HotSpot [b|c]/2   --jdk-only []/0
sublist.size           3                 0
sublist.of-sublist     [b|c]/2           NullPointerException (returned null)
Pattern.split          1|2|3             null|null|null
String.split           6|7               null|null
```

**Why.** Registering the `native_asl_*` family on `java/util/ArrayList$SubList`
is what makes a view of that class work — and it also hands the family every
`SubList` that **java.base's own bytecode** built. `--jdk-only` builds exactly
those: there `ArrayList.subList` runs its own body. Such an object has the
class's declared width and none of this VM's fields, so `asl_base` (width minus
five) lands on 0, `asl_state` reads `modCount` as `parent`, gets a non-object,
and answers "not a view" — which every native turns into empty/`null`. Baseline
CratonVM is green on that arm precisely because it refuses the internal class
there and lets real bytecode do the whole job.

Mirroring this VM's state into the JDK's `root`/`parent`/`offset`/`size`/
`modCount` was tried and measured: **no change to any of the 14**. It is the
wrong half of the problem — it makes the JDK's bodies correct on OUR objects and
does nothing about OUR bodies running on THEIRS.

**What a retry needs**, recorded at `ASL_REAL_CLASS` in the source: `asl_base`
answering `None` when `object_num_fields < class_num_total_fields +
ASL_NUM_FIELDS`, and every `native_asl_*` delegating through
`invoke_virtual_bytecode_only` on that answer instead of reporting zero. That is
a receiver test on the VM's most universally reached list path, so it wants its
own change and its own `--jdk-only` run — not a rider on a carrier commit. The
carrier itself, its force-native entries and its registrations were withdrawn;
what stayed is `asl_base` (verified, and the half a retry reuses) and the
measurement.

**The general lesson is the second oracle.** A carrier change has TWO of them:
`--real-jdk`, where this VM's natives win, and `--jdk-only`, where real bytecode
does. Replacing a `cratonvm/internal/*` name with a real one changes which
objects reach your natives in the second mode, and the first mode cannot see it.

## Measured

`probes/ViewClassProbe` and `probes/MapViewBehaviourProbe`, real JDK 25, Azure
host, `diff` against the HotSpot run of the same probe in the same directory:

| probe | before | after |
| --- | ---: | ---: |
| `ViewClassProbe` (30 lines) | 18 diverging | **1** — the `subList` row above |
| `MapViewBehaviourProbe` (194 assertions) | 6 diverging | **0** |
| `JdkOnlyCollectionViewProbe`, `--real-jdk` | 0 | **0** |
| `JdkOnlyCollectionViewProbe`, `--jdk-only` | 0 | **0** |

The intermediate build is worth recording as the warning it is: with the Set
carriers landed but the backing still at slot 0, `ViewClassProbe` went from 18
diverging lines to 11 — an apparent win — while `MapViewBehaviourProbe` went
from 6 to 12 and `ViewClassProbe` itself died mid-run. **A carrier change is not
verified by the class-name probe.** The behavioural probe is what caught it, for
the same reason the values fix quotes it.

## Repro

```bash
javac -d . probes/ViewClassProbe.java probes/MapViewBehaviourProbe.java
java ViewClassProbe > hs-view.txt
java MapViewBehaviourProbe > hs-mv.txt
cratonvm --java-home <jdk25> -cp . ViewClassProbe | diff hs-view.txt -
cratonvm --java-home <jdk25> -cp . MapViewBehaviourProbe | diff hs-mv.txt -
```

## What this was NOT

Not a throughput item, and it stayed that way. The per-call cost this family
used to carry was closed with the values-view carrier; what remains is dispatch
and heap-access cost that no collection change reaches. Anyone arriving here
from a slow collection-heavy class wants
the VM-wide per-call page.
