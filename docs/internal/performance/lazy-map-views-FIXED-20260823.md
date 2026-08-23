# Live map views were O(n) per CALL and O(n) per READ — both terms removed

**Status: FIXED 2026-08-23.** This is the fix named by
`perf/springboot-configurationpropertysources-native-collections-floor-20260821.md`
for its two largest terms. The page it replaces
(`lazy-map-views-plan-and-blockers-20260822.md`) was a plan with three blockers;
one of them had already been closed by the time the work started, and the other
two turn out to block only the design that was NOT chosen.

## What was wrong

`map.keySet()` is a LIVE view, and both halves of "live" were O(n) on every
single call:

1. **CONSTRUCTION.** `native_map_key_set` -> `make_view_set_of` allocated a
   carrier, allocated a backing map, and inserted every key through
   `native_map_put` — hashing each key through the full `map_hash_key` triage
   and probing the bucket chain for duplicates.
2. **ITERATION.** For a non-STATIC view kind, `resync_view_set` REBUILT the
   whole backing again on every read: a fresh bucket array plus a re-insertion
   of every key, from `size()`, `iterator()`, `contains()`, `toArray()`,
   `stream()`, `forEach()` and friends — eleven call sites.

So Spring's `StringUtils.toStringArray(map.keySet())` — which is what
`ConfigurationPropertySources`' `getPropertyNames()` is — rebuilt a 1000-entry
hash map TWICE per call, 101 910 times per test run.

The design intent was right: a non-STATIC view is meant to be *live*, and
resyncing is how it stays live. It was the implementation of "live" that was
O(n)-per-read instead of O(1).

## Measured

`probes/KeySetBench`, width=1000, µs per call, **one binary, A/B by
`CRATONVM_MAP_VIEW_CACHE`**:

| rung | LinkedHashMap OFF | ON | HashMap OFF | ON |
|---|---:|---:|---:|---:|
| `viewOnly`  (`map.keySet()` alone) | 1284.0 | **1.5** | 1330.0 | **2.5** |
| `sizeOnly`  (`keySet().size()`)    | 3132.0 | **5.0** | 3422.5 | **4.0** |
| `perCall`   (what Spring does)     | 5206.5 | **1192.5** | 5506.0 | **1246.5** |
| `hoisted`   (one view, read in a loop) | 2974.5 | **1217.5** | 3079.5 | **1197.5** |
| `mapSize`   (control)              | 2.0 | 2.0 | 3.0 | 3.0 |

`viewOnly` is **~600-850x**; `sizeOnly` **~630-850x**; `perCall` **4.4x**;
`hoisted` **2.4x**.

`hoisted` was the row that proved iteration was the bigger half — the view was
built once and reading through it still cost 2.8 ms against 2 µs for
`map.size()`. What remains of it after the fix (~1200 µs for 1000 elements,
i.e. ~1.2 µs per element) is the per-element ITERATOR cost, which is a
different subject with its own page (the five-doors iteration note); the
rebuild is gone, which is what `perCall` converging on `hoisted` says.

## The fix, and what makes it sound

Two guards, and they are not the same guard.

**A. The view is cached on the source.** `keySet()` / `entrySet()` / `values()`
return the view this map already minted, stored in the `keySet` / `entrySet` /
`values` fields real `java.util.AbstractMap` and `java.util.HashMap` declare —
which is where HotSpot caches its own. This needs **no generation at all**: the
view is live, so the same object stays correct forever. It also fixes a
behavioural divergence — `map.keySet() == map.keySet()` is `true` on HotSpot
and was `false` here, for all three accessors and all three map families.

**B. `resync_view_set` early-outs when the source has not changed since the
backing was last rebuilt.** This is where the soundness argument lives, and it
has four parts.

* **Only keySet views take it.** For a keySet, only STRUCTURAL change matters:
  a value-replacing `put` cannot change the key set. That is exactly the class
  of change `modCount` records — and exactly the class it records *completely*,
  because `modCount` is also the JDK's fail-fast iterator version, so a mutator
  that failed to bump it would be a visible CME bug today. An **entrySet** view
  is not eligible: its elements carry a snapshot of the VALUE (`Map$Entry` slot
  1) and a value-replacing `put` bumps nothing. A **values** view is not
  eligible for the same reason. Both still get A, which removes their
  construction term while leaving the rebuild that keeps them fresh.
* **The stamp has three terms, not one.** `modCount` is the primary and is
  sufficient on its own. The source's `size` is a second term because a mutator
  that changed cardinality without bumping is the one way the primary could be
  incomplete, and catching it costs one field read. (`size` alone would NOT do:
  `remove(k1); put(k2)` leaves it unchanged — which is why the monotonic
  counter is primary and not the other way round.) The third term is the
  BACKING's own size, and it is what makes the guard safe against edits made
  through the VIEW: `keySet().remove(k)` deletes from the backing and only then
  propagates to the source, so between those two writes the source terms alone
  would still say "in sync". Comparing the backing's cardinality closes that
  without having to find and annotate every such site.
* **The stamp is +1-encoded.** A view backing is allocated with its slots
  zeroed and `0` is a legal `modCount`, so without the encoding a never-stamped
  backing over a freshly built map would compare equal to `(0,0,0)` and skip
  its first rebuild. `0` therefore means "never stamped, do not trust", and the
  guard fails OPEN. Same encoding, for the same reason, that the iterator
  fail-fast work settled on for `expectedModCount`.
* **The stamp is read BEFORE the rebuild and written after it.** If the source
  changes while the rebuild runs, the stored stamp is the OLD one and the next
  read rebuilds again. `make_view_set_of` does not stamp at all, because its
  keys were collected by its caller before it was entered; with A in place that
  costs exactly one extra rebuild per map, ever.

`CRATONVM_MAP_VIEW_CACHE=0` turns both off. `CRATONVM_VERIFY_MAP_VIEW_CACHE=1`
takes the guard's decision, then rebuilds anyway and compares — size equality
plus one-way containment through `native_map_contains_key`, i.e. the same
`equals` route a rebuild would have used — and panics on divergence, naming the
conclusion ("some mutator does not bump the invalidation generation").

### One unguarded assumption made into a switch

`alloc_view_backing` gives a view backing the REAL `java/util/HashMap` layout
and then writes its kind/source markers at fixed slots 14/15, on the strength
of "loadFactor is the last real field, ≤ slot 7". The new stamp uses 11-13 on
the same strength. That assumption was never checked.
`view_backing_marker_slots_are_free` resolves the real indices once and asks:
on a layout that does not leave the slots free the stamp is simply never
written, so every read rebuilds and the fix turns itself off rather than
type-punning a real field.

## The three blockers the plan page named

**Blocker 1 — "the invalidation generation is NOT maintained for
LinkedHashMap" — was already CLOSED** before this work started, by the
`lhm_set` change that bumps `modCount` on a `size` write. Verified rather than
assumed, `probes/MapModCountProbe2`, CratonVM against HotSpot 25.0.3+9:

```
                 bumpOnPut  bumpOnRemove     HotSpot
  HashMap             YES        YES         YES YES
  LinkedHashMap       YES        YES         YES YES
  TreeMap             YES        YES         YES YES
  Hashtable           YES        YES         YES YES
```

`ConcurrentHashMap` is out of scope by construction: its `keySet()` returns a
real `ConcurrentHashMap$KeySetView` through `make_key_set_view`, never a
`make_view_set_of` carrier, so it never reaches `resync_view_set`. `TreeMap`
likewise mints a `TreeMap$KeySet` through `native_tm_key_set`. The only sources
that reach the guard are the HashMap family and `LinkedHashMap`.

The plan page also asked whether `bump_map_mod_count`'s doc claim — that it is
the invalidation generation for "the bounded String-node lookup cache below" —
was stale or named a cache that had moved. **It was stale**: no cache of that
name exists anywhere in `native-collections`. The sentence now has a real
referent again and says so.

**Blockers 2 and 3 — "the backing escapes to JDK bytecode" and "the accessor is
nearly, but not quite, a chokepoint" — block design B, not design A, and both
remain true.** They are the reason B was not taken:

* `alloc_view_backing` deliberately gives the backing a real
  `java/util/HashMap` field layout because JDK bytecode reads it directly:
  `keySet().spliterator()` constructs `HashMap.KeySpliterator(map, …)` and does
  `getfield map.table` / `map.modCount` / `map.size`. A lazily-unpopulated
  backing cannot simply be handed out — anything reaching it through bytecode
  sees an empty table and answers "empty" with no error.
* `hs_backing_map` is nearly the single reader (22 call sites) but at least one
  site resolves `backing_slot` and does `ctx.get_field(coll, backing_slot)`
  itself, and `hs_backing_map` takes `&dyn NativeContext` where materialisation
  needs `&mut`.

Design A populates the backing eagerly exactly as before, so neither applies.
B — lazy backing plus source-delegating reads — is still the better end state
for the remaining per-element iteration cost, and would still have to close
both.

## How it was checked

* `probes/MapViewCacheProbe` (new, this change): 138 rows over `HashMap`,
  `LinkedHashMap` and `Hashtable`, covering view identity, liveness of a
  hoisted view across put / value-replace / remove / clear / a size-invariant
  `remove+put` pair, `toArray` / `stream` / `iterator` / `isEmpty`,
  write-through by `remove` and by `iterator().remove()`, entrySet value
  freshness and `setValue` write-through, values-view liveness, and a view
  taken from an EMPTY map and then read after a put. **All 138 rows byte-identical
  to HotSpot 25.0.3+9** — including the six identity rows that were wrong
  before this change — and identical between `CRATONVM_MAP_VIEW_CACHE=1` and
  `=0`.
* `probes/MapViewBehaviourProbe` (the pre-existing gate, the one that caught
  `Properties` views being live in the write direction only): identical on
  cache-ON, cache-OFF and HotSpot.
* Under `CRATONVM_VERIFY_MAP_VIEW_CACHE=1` the behavioural probe produces the
  same 138 rows with no divergence panic.
* The `native-collections` unit tests: 136 + 9 + 12 + 8 + 15 + 3 + 4 + 6 + 8 +
  8 passed, 0 failed.
* `regression-suite/run.sh`.

## Files

* `native-collections/src/lib.rs` — the "Live map views" module note,
  `map_view_cache_enabled` / `map_view_cache_verify`,
  `view_kind_takes_generation_guard`, `view_source_stamp`,
  `view_backing_sync_stamp` / `set_view_backing_sync_stamp` /
  `view_backing_is_in_sync`, `view_backing_marker_slots_are_free`,
  `cached_map_view` / `cached_values_view`, `verify_view_matches_source`, and
  the guard in `resync_view_set_inner`.
* `probes/MapViewCacheProbe.java`, `probes/KeySetBench.java` (new
  `valuesOnly` / `entryOnly` rungs).
