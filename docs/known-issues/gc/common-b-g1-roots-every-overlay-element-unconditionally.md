# G1 roots every collection-overlay element unconditionally, every pause

> **STATUS (2026-09-26): OPEN, parked on a G1-collector round.** Unchanged
> since filing. `vm/src/memory/native_roots.rs::scan_collection_overlays`
> (line 388) still reads `GcAlgorithm::G1 => false`, and
> `rg -l external_roots_for_owner gc/src` lists `external_roots.rs`,
> `gen_heap.rs`, `gen_heap_oldmark_census.rs` and `zgc.rs`: no `g1*.rs`. No
> root-side step is sound: flipping the arm before a G1 marker follows owner
> edges frees live overlay contents. A root-side "skip OLD overlay elements on
> a G1 young pause" shortcut was considered (wave 2) and rejected without a
> build proving G1's young evacuation never reads fields of non-CSet roots it
> is handed. The fix below is a G1 marker change, out of the common round's
> scope (collector internals); the evidence, scenario and fix are current.
> The w12-c report also notes the markers' `external_roots_for_matching_owners(&|_| true)`
> seeds return every VM's elements, which is this same parked item.

**Status:** OPEN — filed 2026-09-23 by gc-common round, wave 1, lane B.
**Backend:** G1 only. (Generational and ZGC defer to owner propagation when
their marker follows it.)

## Evidence

`memory::native_roots::scan_collection_overlays`
(`vm/src/memory/native_roots.rs`, the `"collection-overlays"` root source)
decides per backend whether overlay-backed collections (`LinkedList`,
`LinkedHashMap`, `TreeMap`, `TreeSet` — backing arrays and nodes held in Rust
side tables) are rooted outright or left to owner-based propagation:

```rust
GcAlgorithm::Generational => young_marker_follows_side_tables() && !generational_concurrent_mark_open(shared),
GcAlgorithm::Zgc => true,
GcAlgorithm::G1 => false,   // always scan_external_roots(roots)
```

G1's markers (`gc/src/g1*.rs`) never call `external_roots_for_owner` /
`with_external_roots_for_owner` (`rg -l external_roots_for_owner gc/src` lists
`gen_heap.rs`, `zgc.rs`, `zgc/mark_roots.rs` only), so the unconditional scan is
the only thing keeping overlay contents alive under G1. Two consequences:

1. **Retention / leak.** Every element of every live-or-dead overlay is a root
   until `prune_external_roots` drops the entry, which it does only when the
   OWNER is dead by the cycle's `is_marked`. An overlay whose elements
   reference their owner (a parent pointer, a listener that holds the map, an
   `Entry` whose value is the map) keeps the owner marked through its own
   elements: the cycle is **never** collectible under G1.
2. **Per-pause cost.** `scan_external_roots` walks every element of every
   overlay in the process on every young pause — proportional to the whole
   overlay population, not the young set. `CRATONVM_DBG_ROOTPROF=1` attributes
   it (`collection-overlays=…ms` on the `[rootprof] scan_all_roots` line).

## Failure scenario

A long-lived service creates and drops `LinkedHashMap<K, Holder>` instances
whose `Holder` values keep a reference back to the map (common for
cache/registry patterns). Under `-XX:+UseG1GC` each dropped map and its whole
element graph stays live forever; old-gen occupancy climbs until mixed pauses
can reclaim nothing and the run ends in `OutOfMemoryError` where Generational
and ZGC hold steady.

## Proposed fix

Teach G1's marking closures (young evacuation's root-to-copy trace, the
concurrent mark, remark) to follow owner edges with
`external_roots::with_external_roots_for_owner(addr, class_id, visit)` for every
object they mark or copy — the same hook Generational's non-moving young marker
and ZGC's mark loop already use — and then flip the G1 arm above to `true`.
Until that marker change lands the arm must stay `false`: deferring without a
following marker frees live overlay contents.

## Confirm

```
rg -n "GcAlgorithm::G1 => false" vm/src/memory/native_roots.rs
rg -l "external_roots_for_owner" gc/src
```

Open while the first matches and no `g1*.rs` file appears in the second.

## Retire when

G1's markers follow external owner edges, the arm is conditional, a probe that
drops self-referencing overlay collections in a loop holds a flat old-gen
occupancy under G1, and this page moves to `docs/internal/`.
