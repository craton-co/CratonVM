# The shared-view-surface defect has BOTH polarities — and the second one was found by aiming at the prior

**Status: FIXED 2026-08-27.** Companion to the `TreeMap$KeySet` half fixed the
day before.

## 1. This probe was aimed, not swept

The survey's running observation is that **every defect so far sat in a family
whose registrar carried a stated justification that had drifted from its code**
— not in families that were merely thin. The one view defect found to that point
was exactly that shape: `native-collections` mirrors the whole `TreeSet` native
surface onto `java/util/TreeMap$KeySet` under a comment saying the view "needs
the identical native surface", and `add` was the method where that claim is
false.

So this probe asks *every* map view class the questions where a view and its
backing collection have OPPOSITE contracts:
`keySet()`/`values()`/`entrySet()` across `HashMap`, `LinkedHashMap`,
`TreeMap`, `Hashtable` and `ConcurrentHashMap` — add/addAll refusal,
remove/removeAll/clear write-through, `Entry.setValue` write-through,
`iterator().remove()` write-through, and liveness after a later `put`.

141 lines. **3 differing lines, identical in both modes** — and they are one
defect plus its two consequences.

## 2. The defect, and it points the OTHER WAY

```text
ConcurrentHashMap().entrySet().add(Map.entry("z", 9))
  HotSpot    accepted        CratonVM   UnsupportedOperationException
```

`ConcurrentHashMap$EntrySetView.add(Entry)` **is** supported: its body is
`map.putVal(e.getKey(), e.getValue(), false)`. It is the one entrySet in the
JDK that supports `add` — every other map's inherits `AbstractCollection.add`'s
bare throw. The other two differing lines are downstream of the add having
succeeded on HotSpot (`entrySet is LIVE after put` 5 vs 4, `iterator.remove
size` 4 vs 3).

`native_hs_add` refuses any receiver whose backing is a view marker. That is
right for all five keySets and for four of the five entrySets. CHM's entrySet is
the exception, and it now takes an explicit branch that reads the entry's
key/value and puts them into the backing map, returning whether the map changed
— matching CHM's own `putVal(..) == null`.

**So the shared-surface defect has both polarities.** A day earlier, a view
ACCEPTED what the JDK refuses (`TreeMap$KeySet.add`). Here a view REFUSES what
the JDK accepts. Both come from the same cause — one native body serving
carriers whose contracts differ on one method — and a fix that only looks for
the first polarity finds half of them.

## 3. What matched, which is most of it

The other 138 lines agreed, across all five map kinds:

* `keySet`/`values`/`entrySet` contents, size and `contains`;
* **add/addAll refused** on all five keySets, all five values views, and four
  of five entrySets;
* `remove`, `removeAll` and `clear` **writing through** to the backing map;
* `iterator().remove()` writing through;
* `Entry.setValue` writing through — including on `ConcurrentHashMap`, where
  the entry is a snapshot but `setValue` must still reach the map;
* the views being **LIVE**: a `put` after the view was taken is visible through
  it;
* `keySet().equals(HashSet)`, `entrySet().equals(copy)`, and all three views
  reporting empty after `map.clear()`.

## 4. Position

The bridge-kind retirement surface is 2,057 rows; ~1,600 are now covered by a
differential probe. What remains is a long tail — **519 rows across 144
classes**, of which 149 rows are in families of three rows or fewer — plus the
176-row StringBuilder cluster that stays out while `WORKER-3-NOTE-3` has it open
with a diagnosed mechanism.

The tail is not obviously worth one probe per class. The better next move is the
prior this record just cashed again: **find the registrars whose comments claim
a shared or identical surface, and probe the methods where the sharing is
false.** That is two defects for two aimed probes, against sixteen defects for
forty-four swept families.

## Reproduce

```bash
cratonvm --java-home "$JDK" --jdk-only -cp probes/out ViewFamilySweep
```
