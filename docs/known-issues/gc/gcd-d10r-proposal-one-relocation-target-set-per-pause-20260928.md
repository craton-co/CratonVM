# Proposal: build a pause's relocation-target set once and hand it through the whole epilogue

> **STATUS (2026-09-28, gcd d10/r): PROPOSAL, not started.** Performance only;
> follows `../../internal/gc/gcd-d10r-in-place-verdicts-keep-rows-of-objects-a-slide-overwrote-FIXED-20260929.md`.

*Filed 2026-09-28 by the gc defects round, wave d10, lane r (refs10).*

## Where it stands

gcd d10/r made every stop-the-world epilogue's in-place survival verdict
refuse an address the pause relocated another object ONTO
(`addr_keyed::RelocationTargets`). The destination set is built lazily per
verdict VALUE, and the epilogue of one pause holds up to three of them:

1. `process_references_after_gc` (`InPlaceVerdict::capture_after`, hoisted to
   the top of the function; the class-metadata reconcile, the weak native
   sweeps and the reference processor share it);
2. `run_collection_pause`'s JNI weak-global sweep (once lane o switches it to
   `capture_after`, the cross-lane request of the d10/r report);
3. `memory::gc::update_all_roots` (smuggled `long`s, Throwable traces,
   inherited `ThreadLocal` buckets, the monitor-index prune).

Each builds an `FxHashSet` of every moved pair's destination: O(relocated
objects) per build, on ZGC every cycle (its slide) and on Generational every
cycle that reclaimed old storage. G1 and a young-only Generational cycle never
build one.

## Proposal

Build the set once, in `run_collection_pause`, right after
`collect_garbage_with_finalizers` returns, as a `RelocationTargets` over
`result.pointer_map`, and pass `&RelocationTargets` down:
`process_references_after_gc(shared, &map, &roots, exact, &targets)`,
`update_all_roots(shared, thread, &map, &targets)`, and
`InPlaceVerdict::capture_with(heap, &targets)` for the JNI sweep. The laziness
stays (a pause whose tables are empty builds nothing).

Cheaper still, where a collector already knows its destinations in order: the
ZGC slide produces its pairs ascending per region (`zgc/forwarding.rs`), so a
sorted `Vec<usize>` of destinations -- or the forwarding table itself -- could
answer `claims` by binary search with no hash set at all. That needs a
`VmHeap` accessor (G1 / ZGC owners).

## Gate

A ZGC pause-time A/B on a relocation-heavy run (`ZgcRefArrayFragProbe`, the
Spring Boot sample) with `CRATONVM_DBG=gc-stats`: the epilogue's share of the
pause before and after, interleaved binaries, medians (in-JVM timings swing
about 3x between runs on the Linux host).
