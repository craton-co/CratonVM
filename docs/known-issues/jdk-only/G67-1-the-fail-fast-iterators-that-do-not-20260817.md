# G67-1 — the fail-fast iterators that do not fail

> **THE RETIREMENT ROUTE IS MEASURED CLOSED — read this before trying it.**
> The values-view `G63-1`
> (`G63-1-the-values-view-iterator-is-not-fail-fast-20260817.md` §3.2) built
> three trial binaries retiring the map-view carrier natives, and each one fixed
> what the last broke and broke something new: two entries restored fail-fast and
> broke `values().toArray()`; the whole 28-entry carrier surface fixed `toArray`
> and broke `map.size()` after a view removal; two more entries fixed `size()`
> and broke `LinkedHashMap.keySet().iterator().remove()` — that last attributed
> against a purpose-built BEFORE binary. All reverted, none landed.
>
> The diagnosis is `retired_shadow.rs`'s own header rule, now measured rather
> than asserted: the map families' state is still two sources, so moving any one
> reader onto real bytecode desynchronises a writer that was pairing with the
> native it replaced. Fixing `modCount` needs the state unified first
> (`P2-COLLECTIONS-SHADOWS-20260812.md`), not a table entry.
>
> Also worth carrying across: `RJdkMapViews` caught one of those three
> regressions and was blind to another, and `probes/MapSizeField.java` is the
> technique that found the blind one — read the collection's REAL field by
> reflection (`--add-opens java.base/java.util=ALL-UNNAMED`) alongside what its
> native accessor reports.

**Status:** MEASURED (defect) / NOT FIXED, deliberately — §4 states the scope
and why it is not a corner of this session. **Provenance:** both VMs, oracle
HotSpot 25.0.3+9-LTS, CratonVM `C:/craton/target-rel9` under `--jdk-only`.
Probe: `scratchpad/g68/Sweep5.java`, 29 rows, ASCII and deterministic.

> **CONVERGENT with `G63-1-the-values-view-iterator-is-not-fail-fast`**, which
> another lane measured independently and at the same time, on a different host
> (Linux/Azure, JDK 25.0.4+7) with different probes. It found
> `map.values().iterator()`; this found `keySet()`, `HashSet` and `TreeMap`.
> Same mechanism, reached twice from opposite ends — which is worth more than
> either measurement alone.
>
> **Adopt its framing on one point where it is better than mine.** It measured
> all THREE arms and found the defect in Compatible mode too, so this is a
> **compatibility defect, not a strict-mode one**. §0 below reports only the
> `--jdk-only` arm, which understates the reach: the fix is owed to every mode,
> and the iterator-`remove()` write-through rows it checked are correct in all
> three.

---

## 0. The defect

```text
                 HotSpot                                CratonVM
hm_failfast      ConcurrentModificationException        "a"
tm_failfast      ConcurrentModificationException        1
hs_failfast      ConcurrentModificationException        1
al_failfast      ConcurrentModificationException        ConcurrentModificationException
```

Each row iterates, structurally modifies the collection, and calls `next()`.
`ArrayList` throws, as HotSpot does. `HashMap.keySet()`, `TreeMap.keySet()`
and `HashSet` **return the next element as though nothing happened**.

`ConcurrentHashMap` correctly does NOT throw — its iterators are weakly
consistent by specification, and the probe asserts that too, so this is not
"iterators should throw" as a blanket rule.

## 1. Why this matters more than a wrong value

Fail-fast is not a feature applications call; it is a **defect detector they
rely on**. `ConcurrentModificationException` is how a Java program discovers
that it mutated a collection it was iterating — usually a real bug, often a
threading bug. Silently returning the next element does not produce a wrong
answer at the point of the defect; it lets the application's own bug run to
completion and surface somewhere else, or not at all.

So the severity is not "three rows diverge". It is that **every
concurrent-modification bug in every application running on this VM is
invisible** for `HashMap`, `TreeMap` and `HashSet`.

## 2. Why it happens

`native_hs_iterator` — and the map key/value/entry iterators with it —
**snapshots**. It collects the live contents into a fresh array and returns an
iterator over that array. The iterator holds no reference to the source
collection at all, so there is nothing it could compare against.

The snapshot is not an accident: its own comment explains that
`alloc_ref_array` can trigger a moving GC, so the contents are collected
twice, once for the count and once for real, to avoid stale refs. That
reasoning is sound and the snapshot is what makes it work.

**`modCount` is not maintained.** The only place `java/util/HashMap`'s
`modCount` field is touched in `native-collections` is the real-JDK layout
materialiser (`resolve_field_index("java/util/HashMap", "modCount")`, one
site). No `put`, `remove`, `clear`, `putAll`, `merge` or `compute*` bumps it.
So neither half of fail-fast exists for maps: not the counter, not the check.

## 3. The template already exists on the list side

This is not new machinery. `java.util.ArrayList` gets it right, and the parts
are:

* `al_mod_count_slot` — resolves `AbstractList.modCount` on the real-JDK
  layout, returning `None` when there is no such slot so a build without it
  loses nothing.
* `al_state` — returns `(size, modCount)` for a receiver.
* `asl_check_comod(ctx, parent, expected)` — compares and raises
  `RuntimeError::ConcurrentModificationException`.

The sublist views use exactly this, and `ArrayList`'s row passes because of
it. What the map side needs is the same three pieces against
`HashMap.modCount` / `TreeMap.modCount`, plus a bump at each structural
mutation.

## 4. Why this is NOT fixed here

Two halves, and the first one is wide:

1. **Every structurally-mutating map/set native must bump `modCount`** —
   `put`, `remove`, `clear`, `putAll`, `merge`, `compute`, `computeIfAbsent`,
   `computeIfPresent`, `putIfAbsent`, plus the `keySet`/`values`/`entrySet`
   view removals that write through, across `HashMap`, `LinkedHashMap`,
   `TreeMap`, `HashSet`, `LinkedHashSet` and `TreeSet`.
2. **Every snapshot iterator must carry the source and the expected count**,
   and check on `next()`. Today it carries neither.

Doing half of that is worse than doing none: a `modCount` that some mutations
bump and others do not produces `ConcurrentModificationException` on some
correct programs and silence on some incorrect ones, which is a worse contract
than consistent silence. This is the same standard applied to `G61-1` N2 and
`G63-1` N1 in this session — wide change, taken whole or not at all — and here
the evidence is stronger, so it should be taken, just not as a corner of
something else.

**No vector rows were added.** A row for a defect nobody is fixing turns the
suite red and trains people to ignore it. The rows belong in the same change
as the fix, and `Sweep5.java` is checked in so they can be lifted from it.

## 5. What the same sweep found to be CORRECT

26 of 29 rows are exact, and they are worth recording so the next person does
not re-probe them:

* **View aliasing** — `subList` writes through; a structural change to the
  parent invalidates the view; `keySet().remove` and `values().remove` write
  through to the map; `entrySet` `setValue` writes through; `keySet().add`
  throws `UnsupportedOperationException`.
* **Iterator misuse** — `remove()` twice, `remove()` before `next()`, and
  `next()` past the end all throw the right exception.
* **`equals`/`hashCode` across implementations** — `ArrayList` equals
  `LinkedList`, `HashSet` equals `TreeSet`, `HashMap` equals `TreeMap`, and
  all three collection `hashCode` formulas match.
* **Sorted-map navigation** — `subMap` bounds inclusive/exclusive,
  out-of-range `put` into a submap throws, `descendingMap`.
* **Null policy** — `HashMap` null key accepted, `TreeMap` null key throws,
  `List.of(null)` throws, `ArrayList` null accepted.

## 6. NOMINATIONS

**N1 — the fix, whole, as described in §3 and §4.** The list-side template
makes it mechanical rather than novel; the width is the cost. Lift the rows
from `scratchpad/g68/Sweep5.java` and land them with it.

**N2 — audit whether any OTHER snapshot iterator has lost a contract.** The
snapshot shape is used beyond these three, and fail-fast is only the contract
this probe happened to ask about. A snapshot iterator also cannot observe
writes made through the collection during iteration, which is a second
observable difference nobody has measured.
