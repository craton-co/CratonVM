# Proposal: the concurrent marker's class-loader side tables are read once per drain, not once per object

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 46
> of 54).** Not built. Opt-in path only (`CRATONVM_GEN_CONC_CLASS_UNLOAD`, NOT
> YET). Now also carries the leftover of
> `../../internal/gc/gengc-r5w4-conc8-proposal-stw-majors-take-the-retained-layout-census-DONE-20260928.md`
> (item 4 below). d7 four_flags_unload_1: `concunload_edges=5`. **Gate:**
> per-cycle mark time A/B on `GenR4W4SteadyPromotionProbe` with the two unload
> flags, interleaved; the `r5w3_` marker tests unchanged. **Size:** S.

*Filed 2026-09-26 by gen round 5 wave 3, lane `unload7`. Proposal; perf of the
opt-in `CRATONVM_GEN_CONC_CLASS_UNLOAD` path.*

## Problem

With concurrent class unloading armed, `ConcurrentMarker::scan_object_into`
(`gc/src/concurrent_mark.rs`) calls `push_class_unload_edges` for EVERY scanned
object: a `parking_lot::RwLock` read acquisition on `class_unload`, then up to
three `FxHashMap` probes (`class_id → loader`, `owner → mirrors`,
`owner → metadata`). On a multi-million-object old generation that is an
atomic RMW pair per object and three hash probes, on the marker's hottest loop,
for tables that change only inside the cycle's two pauses. G1 paid the same
shape and cut it with a per-batch snapshot (`MarkSideTables`, lane W2-C).

Most scanned objects are neither user-loader instances nor loaders, so most
probes miss.

## Proposal

1. Take the read guard once per `drain_local` / `drain_unbounded` / overflow
   rescan call and pass `Option<&ClassUnloadTables>` down to
   `scan_object_into` (the tables are written only inside the pauses, by the
   same thread that drains, so a guard held across a drain cannot block anyone).
2. Replace the `class_id → loader` map with a dense `Vec<u32 → usize>` indexed
   by class id (ids are small and dense per VM), and the two owner maps with
   one `FxHashMap<usize, (u32, u32)>` of ranges into one flat `Vec<usize>`,
   plus a 64-bit Bloom-style pre-filter on the owner address, so the common
   miss costs one load and one test.
3. Count hits and misses (`concunload_edges` already counts hits) so the
   A/B can show the probe cost, not only wall time.

## Verification

- `GenR4W4SteadyPromotionProbe` with `CRATONVM_GEN_CONC_CLASS_UNLOAD=1
  CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK=1`: `concdrv_` Phase-2 slice counts and
  per-cycle mark time (`[GC] concurrent-cycle:`) before/after, interleaved
  binaries, medians (host noise is ~3x here).
- The `r5w3_` marker unit tests unchanged.

## 4. Merged from `gengc-r5w4-conc8-proposal-stw-majors-take-the-retained-layout-census` (d8/y, 2026-09-28): one young walk per initial-mark pause

That page is retired DONE (the stop-the-world census is built). Its leftover
is the same kind of opt-in-path cost as this page: with
`CRATONVM_GEN_CONC_CLASS_UNLOAD` and a pending layout, the initial-mark pause
walks young twice (`gen_conc_young_instance_loaders` and
`gen_conc_layout_census_candidates` each build the set of young class ids).
One `young_class_ids(shared)` helper, called once per pause and handed to
both, halves it. No flag needed; verify the `r5w4_` / `gcd_d2g_` census tests
unchanged and the `door=gen-initial-mark` pause line shorter with a pending
layout.
