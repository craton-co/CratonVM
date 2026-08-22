# Lazy/live map views — the plan, and the three things that block the obvious version

**Status: OPEN — 2026-08-22.** This is the fix named by
`perf/springboot-configurationpropertysources-native-collections-floor-20260821.md`
for its two largest terms. It is **not implemented**; this page exists because a
first attempt found three blockers by reading the code, and every one of them
would have been discovered the expensive way — by shipping a silently stale
collection.

## What the current implementation does, and why it is worse than documented

`map.keySet()` → `native_map_key_set` / `native_lhm_key_set` → `make_view_set_of`,
which allocates a carrier, allocates a backing map, and **inserts every key
through `native_map_put`** — hashing each key through the full `map_hash_key`
triage and probing the bucket chain for duplicates.

That was the known half. The other half is worse: for a non-STATIC view kind,
**`resync_view_set` rebuilds the whole backing again on every read**. It
allocates a fresh bucket array and re-inserts every key, and it is called from
`size()`, `iterator()` and friends (11 call sites). So Spring's

```java
StringUtils.toStringArray(map.keySet())   // getPropertyNames()
```

rebuilds a 1000-entry hash map **twice per call**, 101 910 times per test run.
That is the ~190 s + ~230 s the perf page attributes to construction and
iteration — together ~90 % of that workload.

The design intent is right: a non-STATIC view is meant to be *live*, and
resyncing is how it stays live. It is the implementation of "live" that is
O(n)-per-read instead of O(1).

## Blocker 1 — the invalidation generation is NOT maintained for LinkedHashMap

Both viable designs (below) need a cheap "has the source changed since?"
signal. The natural one already exists: `bump_map_mod_count`, whose own doc
says it is "the invalidation generation" for a cache. It has **six** call sites:

```
native_map_put_evict_pinned      (HashMap family put)
native_map_remove_pinned         (x2)
native_map_clear
tm_set_slot                      (TreeMap)
```

`native_lhm_put_evict` and `native_lhm_remove` **do not bump it** — verified by
reading both bodies; LHM keeps its own state through `lhm_set`/`lhm_state` and
never touches `modCount` or `set_map_size`. So any cache keyed on `modCount` is
**stale for a LinkedHashMap**, which is exactly the source type in the workload
this fix is for.

Completing the coverage means auditing every map family's mutators —
`LinkedHashMap`, `Hashtable`/`Properties`, `ConcurrentHashMap`, `TreeMap` — and
**missing one produces a silently stale collection**, the worst failure mode
available here. That audit is the first task, and it should land on its own,
with tests, before any caching is built on top of it.

Worth checking while in there: `bump_map_mod_count`'s doc claims it is the
invalidation generation for "the bounded String-node lookup cache below". No
such cache is findable by that name today. Either the comment is stale or the
cache moved — if any live cache keys on `modCount`, the LHM gap is a
**correctness** bug today, independent of this work.

## Blocker 2 — the backing escapes to JDK bytecode

`alloc_view_backing` deliberately gives the backing a real `java/util/HashMap`
field layout because **JDK bytecode reads it directly**: `keySet().spliterator()`
constructs `HashMap.KeySpliterator(map, …)` and does `getfield map.table` /
`map.modCount` / `map.size`. The synthetic layout previously made `arraylength`
panic on `Int(capacity=16)`.

So a lazily-unpopulated backing cannot simply be handed out — anything that
reaches it through bytecode sees an empty table and answers "empty" with no
error. Every escape point must materialise first.

## Blocker 3 — the accessor is nearly, but not quite, a chokepoint

`hs_backing_map` is the single reader (22 call sites) and `hs_set_backing_map`
the single writer, which is exactly what lazy materialisation needs. But at
least one site reads the slot directly rather than through it — the
collection-conversion helper around `native-collections/src/lib.rs:44370`
resolves `backing_slot` itself and does `ctx.get_field(coll, backing_slot)`.
Grep for `hs_map_slot` and for direct `get_field` on a carrier before assuming
the funnel is complete. `hs_backing_map` also takes `&dyn NativeContext`;
materialisation needs `&mut`, so the 22 sites need the mutable twin.

## The two designs

**A. Cached live view (JDK-shaped, smaller).** `keySet()` returns a view cached
on the source (real `HashMap` declares `keySet`/`values`/`entrySet` fields, and
`try_set_jdk_map_field` already writes JDK-named fields), and `resync_view_set`
early-outs, both keyed on the source's modification generation recorded on the
backing at last sync. Kills both terms. Needs Blocker 1 fully closed.

*Soundness note that makes this cheaper than it looks:* for a **keySet** view
only structural changes matter, and a value-replacing `put` cannot change the
key set. So the generation may over-invalidate safely; it must never
under-invalidate. `remove(k1); put(k2)` leaves the size unchanged, so the guard
must be a monotonic counter, **not** the size.

**B. Lazy backing + source-delegating reads (bigger, removes the work
entirely).** `make_view_set_of` allocates the carrier and stores the backref but
does not populate; `size`/`isEmpty`/`contains`/`iterator`/`toArray` answer from
the source map; everything else materialises through the `hs_backing_map`
mutable twin. Safe by construction for anything unspecialised, but needs
Blockers 2 and 3 closed and touches more surface.

**Recommendation: A, after the Blocker-1 audit lands separately.** A is smaller,
matches what the JDK actually does, and its failure mode is bounded by one
invariant that can be tested directly. B is the better end state and can follow.

## How to know it is right

* A **verify mode** (`CRATONVM_VERIFY_MAP_VIEW_CACHE=1`) that takes the fast
  path, then rebuilds anyway and compares, panicking on divergence. Run the
  suites under it. This turns the soundness claim into something measured
  rather than argued — the guard the codebase already uses elsewhere.
* `probes/MapViewBehaviourProbe` is the existing behavioural gate for view
  liveness (it is what caught `Properties` views being live in the write
  direction only); it must stay green.
* The 142 `native-collections` unit tests, plus `cratonvm-vm` and `cratonvm-gc`.
* `probes/KeySetBench` (`viewOnly`, `hoisted`, `perCall`) for the numbers, and
  `ConfigurationPropertySourcesTests` end to end — 598.7 s serial on ZGC today.
* A kill switch, so the whole thing is an A/B inside one binary.

## Expected payoff

~420 s of the ~475 s arm, i.e. the class should drop from ~10 minutes to well
under one, and every `keySet()`/`values()`/`entrySet()`-in-a-loop workload in
the VM benefits. That is why it is worth doing properly rather than quickly.
