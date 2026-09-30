# Proposal: resolve a compiled frame's active safepoint facts in ONE pass, from an index

> **STATUS (2026-09-29, gce e1/f): PROPOSAL, not started.** Performance of the
> compiled-frame root scan; no correctness change. Size: S-M.

*Filed 2026-09-29 by gce wave e1, lane f, from the review of the frame-root
code.*

## Why

Every compiled frame the band scan visits (`vm/src/jit/conservative_roots.rs`,
`scan_precise_frames_and_bands` -> `scan_active_oop_map_at_rbp` +
`scan_one_compiled_frame_with_layout` -> `scan_one_frame_filtered`) re-reads
the frame's safepoint-id slot and re-filters `cm.oop_maps` by
`bytecode_pc == sp_id` once per question:

1. `scan_active_oop_map_at_rbp` (the precise half);
2. `active_map_facts` (register mask, live cursor, staged-unmapped);
3. `ir_prim_slots` (optimizing tier's primitive colours);
4. `sp_local_claim` (gce e1/f, single-pass java-local homes);
5. `frame_active_map_slots` / `moving_young_frame_live_hi` on the moving
   young verifier path, and `band_word_context` per rooted word under the
   census.

`cm.oop_maps` is in emission order, so each pass is linear in the method's
safepoint count (hundreds in a large method) and all of them find the same
entries. The per-frame claims also read their switches per frame through
`runtime_flag_default_on`; for a switch not declared in
`types/src/flag_groups.rs` that is a `std::env::var_os` per frame per scan
(on Windows, a lock on the process environment block).

## What

1. At install (`x64/driver.rs`, `ir_lower.rs` finalisation), build a
   `CompiledMethod::oop_map_index: Box<[(u32 /* bytecode_pc */, u32 /* first */,
   u32 /* count */)]>` over a copy of `oop_maps` sorted by `bytecode_pc`
   (stable, so the "union of every map of the id" semantics are unchanged).
2. One function `active_safepoint_facts(rbp, cm) -> Option<ActiveSafepoint<'_>>`
   reads the id once, binary-searches the index, and returns the slice of maps
   plus the merged facts every consumer above needs (register mask with its
   abstain rule, live cursor, staged-unmapped, prim slots, the single-pass
   java-local claim, the union of named slots). Each consumer takes it as an
   argument instead of recomputing.
3. Declare every per-frame switch (`CRATONVM_GC_CALLEE_SAVED_IMAGE_LIVENESS`,
   `CRATONVM_GC_IR_PRIM_SLOT_ROOTS` already are; this wave's
   `CRATONVM_GC_SP_LOCAL_MAP_ROOTS` and `CRATONVM_GC_DEOPT_IMAGE_RESIDUE_ROOTS`
   are requested in the e1/f report), so each read is a table lookup.

## Hazards

- The "union of every map of the id" rule (gen r4w2/youngmark) must survive:
  the sorted copy keeps all entries of an id adjacent.
- `oop_maps` is also read by native pc (`native_pc_offset`) elsewhere; keep
  the original vector, add the index beside it.
- `frame_holds_no_references` bodies: the id slot is stale by contract;
  `active_safepoint_id` already refuses them and the new function must too.

## How to verify

Same roots, fewer cycles: `CRATONVM_DBG_ROOTPROF=1` stack-scan time on a
deep-recursion workload with large compiled methods (`R10SelfRecCatch`,
the H2 `TestRandomMapOps` driver), A/B interleaved (host noise ~3x, take
medians); and the `[regoop]` line's counters identical between arms on a
fixed-seed run. A unit test: for a method with maps at ids {3, 3, 7}, the
facts for id 3 merge both entries exactly as `active_map_facts` does today.
