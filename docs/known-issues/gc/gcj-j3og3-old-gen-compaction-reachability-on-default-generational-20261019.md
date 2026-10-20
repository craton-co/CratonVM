# Which Generational majors compact by default, and why no j1c row compacted (gcj j3/og3)

> **STATUS (2026-10-19, gcj j3/og3): OPEN (parity / perf, found by reading; nothing measured).** Generational only. The two measurement artifacts below are closed by this wave's probe `tools/probes/Gcj3Og3CompactingMajorProbe.java` and the `[gcphase] old-gen compact-around-pins-forward-index` / `compact-phase0` lines (rows: `docs/internal/gc-round-gcj-20261019/j3-og3-report.md`, blocks FI, SL, P0). The parity gap (most default majors never compact) stays open; proposal `docs/internal/proposals/gc/gcj-j3og3-proposal-fragmented-in-place-majors-compact-through-the-pinned-compactor-20261019.md`. Line numbers are against branch `gcj3-og3`; search the names.

## What compacts on a default run (by reading)

An old-gen collection reaches one of three arms of `GenerationalHeap::old_gen_gc_inner`:

1. **The full-GC slide** (`OldGen::compact_with_walked_objects_shared`, `[gcphase] old-gen compact:`). Only from the MOVING young cycle's Phase 5 major (`collect_garbage_inner_with_pins` -> `major_gc_finalizing_with(.., compact = true)`), and only when `moving_major_compaction_plan` answers `major_compacts`:
   * `phase5_major_no_compact` is false: no compiled frame or conservative peer is live (`gc_quiescence::compiled_frames_live() || conservative_peers_live()`, under `CRATONVM_GC_MOVING_MAJOR_JIT_GUARD`, default ON) and the pause's young pin ledger did not license an in-place young cycle; **and**
   * a fragmentation request is pending (`OldGen::fragmentation_compaction_pending`: the OOM ladder's `CRATONVM_GC_OLD_OOM_COMPACT`, default ON, or the opt-in proactive `CRATONVM_GC_OLD_FRAG_COMPACT`), **or** `CRATONVM_OLDGEN_COMPACT` (default ON) and P-C cannot arm (`moving_major_true_root_gate`: `CRATONVM_GC_FULL_GC_TRUE_ROOTS=0`, `CRATONVM_GC_MOVING_MAJOR_TRUE_ROOTS=0`, or a pinned in-place cycle).
   With the defaults P-C always arms on a frame-free moving cycle, so **the slide runs only under a pending fragmentation request** (or a kill switch of P-C). `System.gc()` never reaches it: it takes the non-moving cycle (`gengc-core-system-gc-forces-non-moving`), unless the opt-in `CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG`.
2. **The pinned compaction** (`OldGen::compact_around_pins_watching`, `[gcphase] old-gen compact-around-pins:`). From the NON-moving cycle's old sweep (`sweep_old_gen_non_moving`), for a REQUESTED major (`CRATONVM_GC_REQUESTED_MAJOR_COMPACTS`, default ON) or a refused humongous allocation (`CRATONVM_GC_OLD_OOM_COMPACT`), **only when the pause pins nothing conservative** (`OldPinnedCompact::pins_nothing_conservative`); with any compiled frame live it reclaims in place and counts `oldsz_requested_major_compaction_declines` ("every JIT-warm `System.gc()` takes this arm", the code's own comment). `CRATONVM_GC_OLD_PINNED_COMPACT` (opt-in) lifts the gate.
3. **In place** (`close` + `free`): everything else -- every occupancy major on a JIT-warm cycle, every P-C major, every requested major with a compiled frame live.

## Why the j1c block F rows were VOID (two artifacts, both closed)

* **The probe's `System.gc()` ran under a compiled frame.** `Gcj1OgFullGcCompactProbe` calls `System.gc()` from `main`'s round loop, which the JIT compiles (OSR), so every requested major took arm 3. `Gcj3Og3CompactingMajorProbe` keeps `main` interpreted (it loops `rounds` times) and does every hot loop in a helper that has returned before the `System.gc()`, the shape of x1's `Gcj2X1SparseOldCompactProbe` (6 pinned compactions per row on j2a).
* **The pinned arm printed no forward-index line.** `[gcphase] old-gen compact-forward-index` was printed by the slide only, so "no such line" could not tell "no compaction" from "a pinned compaction" (which also resolves through `ForwardIndex`). The pinned arm now prints `old-gen compact-around-pins-forward-index: count=` (`note_compaction_phase0_lines`).

The loader-snapshot half of block F (`CRATONVM_GC_OLD_MARK_LOADER_SNAPSHOT`, mark sum 14.9 s vs 14.2 s) was not void: the mark runs on every major, compacting or not. It measured the in-place majors, so it stands as "no gain on that shape"; block FI re-measures it on compacting majors.

## The parity gap

HotSpot Serial's every full collection is a sliding mark-compact. On a default Generational run the old generation compacts only on an interpreted `System.gc()`, a refused humongous allocation or a pending fragmentation request; an application that never calls `System.gc()` and runs JIT-warm reclaims in place for its whole life, and its free list fragments until the allocation-failure ladder notices. `oldsz_compactions` against HotSpot's `Pause Full` count on the javac gate rows and `GenR4W4SteadyPromotionProbe` would show the size of it. What would close it is in the proposal above.
