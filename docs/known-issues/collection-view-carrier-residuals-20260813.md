# Collection-view carrier residuals: `keySet()`, `entrySet()`, `subList()`, and three liveness rows

**Status:** OPEN (2026-08-13). What is left after
[the values-view carrier fix](../internal/fixed-suite-bugs/netty/arraylist-native-overhead-and-view-carrier-FIXED-20260813.md)
closed the `values()` family. Everything here is measured against a HotSpot
JDK 25 oracle with `probes/ViewClassProbe` and `probes/MapViewBehaviourProbe`;
neither is a guess and neither has a known failing application test.

## 1. Carrier classes still wrong for the Set-shaped views

| expression | HotSpot JDK 25 | CratonVM |
| --- | --- | --- |
| `hashMap.keySet().getClass()` | `java.util.HashMap$KeySet` | `java.util.HashSet` |
| `hashMap.entrySet().getClass()` | `java.util.HashMap$EntrySet` | `java.util.HashSet` |
| `linkedHashMap.keySet().getClass()` | `…$LinkedKeySet` | `java.util.HashSet` |
| `linkedHashMap.entrySet().getClass()` | `…$LinkedEntrySet` | `java.util.HashSet` |
| `treeMap.keySet().getClass()` | `java.util.TreeMap$KeySet` | `java.util.TreeSet` |
| `hashtable.keySet().getClass()` | `…$SynchronizedSet` | `java.util.HashSet` |
| `hashtable.values().getClass()` | `…$SynchronizedCollection` | `java.util.Hashtable$ValueCollection` |
| `arrayList.subList(0,2).getClass()` | `java.util.ArrayList$SubList` | `cratonvm.internal.ArrayListSubList` |
| `linkedHashSet.iterator().getClass()` | `…$LinkedKeyIterator` | `java.util.HashMap$KeyItr` |

The `values()` rows that used to sit here are fixed; these are the ones that
did not move.

**Two of these are cheaper than the rest.** `Hashtable.values()`/`keySet()` need
the JDK's `Collections$Synchronized*` wrapper around the inner view, which is a
wrapper the VM already models. The others need what the values fix needed: a
carrier class with the natives registered on it and an entry in both
force-native gates.

**`subList` is the expensive one and should not be attempted the same way.**
`cratonvm/internal/ArrayListSubList` has its OWN native field layout (parent,
offset, size, expected, view-parent), so it cannot be laid over the real
`java.util.ArrayList$SubList` the way a values view could be laid over
`HashMap$Values` — that view keeps ArrayList's own `elementData`/`size` slots
and writes nothing the JDK class declares. See the values-view record's *"Why
the REAL JDK class names were affordable here"* for the distinction; it is the
whole reason one family moved and the other did not.

## 2. Three liveness rows, unrelated to the carrier

`probes/MapViewBehaviourProbe` runs 194 assertions across seven map families.
Exactly three diverge from HotSpot, identically before and after the values fix,
so they are older than it:

```
tm.keys.afterPut.size       HotSpot 3   CratonVM 2
props.keys.afterPut.size    HotSpot 3   CratonVM 2
props.entries.afterPut.size HotSpot 3   CratonVM 2
```

A `TreeMap.keySet()` and a `Properties.keySet()`/`entrySet()` captured BEFORE a
later `put` do not see it. The `values()` view of the same maps does — that is
the `resync_values_view` marker doing its job — and so does `HashMap`'s and
`LinkedHashMap`'s `keySet()`. So this is not a general "views are snapshots"
gap; it is the two view kinds whose backing carrier (`TreeSet` via
`ts_view_source`, and the `Properties` side table) does not resync on read the
way the ArrayList-backed one does.

That shape has bitten before —
`docs/internal/fixed-suite-bugs/springboot/properties-keyset-view-not-live-FIXED.md`
is the same sentence about the same class — so check whether that fix regressed
or merely covered a narrower path before writing new code.

## 3. What this is NOT

It is not a throughput item. The per-call cost that this family used to carry
was closed with the values-view carrier; `values().size()` went from 15.6 µs to
1.0 µs and `ArrayList.size()` from 29× plain-Java to 16.6×. Anyone arriving here
from a slow collection-heavy class should read
[the VM-wide per-call page](vm-per-call-dispatch-cost-20260813.md) instead —
what remains is dispatch and heap-access cost that no collection change reaches.

## Repro

```bash
javac -d . probes/ViewClassProbe.java probes/MapViewBehaviourProbe.java
java ViewClassProbe > hs-view.txt
java MapViewBehaviourProbe > hs-mv.txt
cratonvm --java-home <jdk25> -cp . ViewClassProbe | diff hs-view.txt -
cratonvm --java-home <jdk25> -cp . MapViewBehaviourProbe | diff hs-mv.txt -
```
