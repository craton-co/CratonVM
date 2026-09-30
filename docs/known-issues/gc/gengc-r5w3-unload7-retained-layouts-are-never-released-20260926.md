# The compact layouts a concurrent remark retains for its sweep are never released

> **STATUS (2026-09-29, gce ve2): OPEN -- the four switches are default on; ve2 c-4 shows released <= retained wherever layouts were retained; the stop-the-world arm rows are still unrun.** Evidence: `docs/internal/gc-design-perf-round-20260929/ve2-verdicts.md`; next steps: `gce-handoff-gc-design-perf-round-close-20260929.md`.

> **STATUS (2026-09-29, gce e1/x): KEEP -- no e1 row runs the conc-unload switches.** **Remaining:** the stop-the-world arm with the four switches (the d5/r runs).

> **STATUS (2026-09-29, gce e1/c): reviewed, no change -- awaiting the d5/r runs (four switches, both arms).**

> **STATUS (2026-09-28, gcd d8/x, final verification on the round branch at `307f0c6a2`, wave d7, Linux release): OPEN -- the concurrent arm releases; the stop-the-world arm was not run with the four switches.** Concurrent arm, four switches with `CRATONVM_GC_CONC_START_PERCENT=40`, `GenR5W5ConcUnloadProbe` (`four_flags_unload_1..3`): HotSpot's line 3/3, `concunload_layouts_retained=1`, `concunload_layouts_released=1` each (released <= retained). The `u7_*` rows ran with two switches only: the dead loader is never unloaded there (`dead-loader-unloaded=false`), `concunload_layouts_retained=0`, so they cannot judge this page. **Remaining gate:** the d5/r block's runs exactly (four switches; the STW arm without the start percent, where `concunload_stw_layout_censuses>=1` must appear whenever a layout is retained; and `GenR5W3ConcUnloadProbe` in both arms).

## STATUS (2026-09-28, gcd d5/r): both halves landed and re-read (no defect, no code change); two runs retire it; reachable only under the opt-in conc-unload switches

- **Default path:** nothing is ever retained without
  `CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK` + `CRATONVM_GEN_CONC_CLASS_UNLOAD`,
  so the default run cannot leak here; the page is about the opt-in path.
- **Re-read on d916d1c40:** the concurrent census (`begin_sweep` takes the
  candidates; the sweep walk observes survivors; only the walk-to-the-end
  exit completes it; an epoch stop drops it), the STW census
  (`gc_and_alloc.rs::gen_stw_layout_census` after an old reclamation, both
  generations walked, only when both walks are complete), and
  `vm/src/memory/gc.rs::release_retained_layouts` (per-slot ownership since
  d1/c). d4/n's true-root seed changes which old objects a requested major
  frees, not what the census walks afterwards (it counts what the collection
  LEFT), so it cannot release a layout an instance still uses.
- **Retire when both runs show a release** (Linux, JIT on, 3 runs each):
  ```
  CU="CRATONVM_GEN_CONC_CLASS_UNLOAD=1 CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK=1 CRATONVM_GEN_Y2O_LIVE_SEED=1 CRATONVM_GEN_YOUNG_MIRROR_DEFER=1 CRATONVM_DBG=gc-stats"
  for p in GenR5W5ConcUnloadProbe GenR5W3ConcUnloadProbe; do
    env $CU CRATONVM_GC_CONC_START_PERCENT=40 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench $p 2>&1 | grep -E 'conc-unload |conc_unload:'   # concurrent arm
    env $CU cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench $p 2>&1 | grep -E 'conc-unload |conc_unload:'                                 # STW arm
  done
  ```
  Expected, every run: stdout `conc-unload dead-loader-unloaded=true
  dead-class-unloaded=true control-loader-alive=true control-ok=true`; on
  `[GC] conc_unload:`, whenever `concunload_layouts_retained>=1`, also
  `concunload_layouts_released>=1` with released <= retained, and in the STW
  arm `concunload_stw_layout_censuses>=1`. A run with
  `concunload_layouts_retained=0` cannot judge this page (nothing retained).
  The four switches are used (not two) because the JIT keeps the dead loader
  otherwise (the class-unload page). Owner: orchestrator.

## STATUS (2026-09-27, gcd d2/g, superseded above): the STOP-THE-WORLD half LANDED (the conc8 proposal); retire on the probe runs below

- **Landed** (`gengc-r5w4-conc8-proposal-stw-majors-take-the-retained-layout-census`,
  implemented): after a stop-the-world collection that reclaimed old storage
  (`gc_quiescence::old_gen_reclaimed_last_cycle`), with a layout pending,
  `vm/src/runtime/interpreter/gc_and_alloc.rs::gen_stw_layout_census` (called
  from `process_references_after_gc`, inside the pause, right after the
  unload transaction) walks both generations
  (`GenerationalHeap::young_object_class_ids` and the new
  `GenerationalHeap::old_object_class_ids`), and when BOTH walks are complete
  releases every pending id no object of either generation has
  (`ConcurrentGcState::complete_stw_layout_census`, then
  `memory::gc::release_retained_layouts` under the class manager's write lock,
  as the unload transaction on the same path does). The census is of what the
  collection LEFT, so a dead instance a conservative root kept still keeps its
  layout. Counted apart: `concunload_stw_layout_censuses` on the
  `[GC] conc_unload:` line.
- Default path: one thread-local read per collection, plus one leaf-mutex read
  after an old reclamation; nothing is pending without
  `CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK`, so no walk runs.
- Not done: the proposal's "one young walk per concurrent pause, not two"
  (perf only; left on the proposal page).
- **Verify:**
  - `cargo test -j 5 -p cratonvm-gc --lib gcd_d2g_a_stw_census_releases_the_pending_ids_no_object_has`
    and `cargo test -j 5 -p cratonvm-gc --lib r5w4_` (unchanged).
  - The concurrent arm (d1c's block below): with the fixed start,
    `concunload_layouts_released>=1`, released <= retained.
  - The STW arm: the same command WITHOUT `CRATONVM_GC_CONC_START_PERCENT`
    (the tail's old collections become stop-the-world majors):
    ```
    for p in GenR5W5ConcUnloadProbe GenR5W3ConcUnloadProbe; do
      CRATONVM_GEN_CONC_CLASS_UNLOAD=1 CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK=1 CRATONVM_DBG=gc-stats \
        cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench $p 2>&1 \
        | grep -E 'conc-unload |conc_unload:'
    done
    ```
    Expected: HotSpot's `conc-unload ... =true ... =true` stdout line, and,
    whenever `concunload_layouts_retained>=1`, `concunload_stw_layout_censuses>=1`
    and `concunload_layouts_released>=1` with released <= retained.
    (`concunload_layouts_retained=0` means no remark retained anything in that
    run and the arm cannot be judged there.) Retire when both arms show a
    release.

## STATUS (2026-09-27, gcd d1/c, superseded above): the census code re-read again (no defect); multi-VM release WIDENED; still to be exercised by the probe below

- Re-read `note_layouts_retained` / `set_layout_census` / the sweep's census /
  `take_releasable_layouts` / `release_retained_layouts` on 6d39e8dcc: no
  defect found.
- Changed: `vm/src/memory/gc.rs::release_retained_layouts` no longer gives up
  in a process with several `ClassStore`s; it releases each id whose slot the
  VM's store owns (`ClassStore::owns_layout_slot`, see
  `../../internal/gc/gengc-r5w4-conc8-class-store-remove-unregisters-another-domains-layout-FIXED-20260928.md`).
  Single-store processes: unchanged.
- Still open only because no run has exercised it: run the probe block of the
  wave-5 STATUS below, with all four switches (the orchestrator measured
  `GenR5W5ConcUnloadProbe` unloading under them with the JIT). Expected:
  `concunload_layouts_retained>=1 concunload_layout_censuses>=1
  concunload_layouts_released>=1`, released <= retained. Retire on that.
- Note: `CRATONVM_GEN_YOUNG_MIRROR_DEFER` alone failed that probe (a null live
  static); the cause and its fix are on
  `gengc-r5w6-conc10-a-young-mirror-root-is-pinned-forever-FIXED-20260929.md`.

## STATUS (2026-09-27, gen r5w5/conc9): fix re-read, no change; UNEXERCISED on the wave-4 build because nothing was unloaded; run it on the reworked probes

- The wave-4 run unloaded nothing (`concunload_classes=0`, see
  `gengc-r5w1-refs5-concurrent-cycle-cannot-unload-classes-20260926.md`), so
  nothing was retained and the census below never had a candidate. The census
  code was re-read (`begin_sweep` takes the candidates and the break baseline;
  `concurrent_sweep_budget_into` observes survivors on the walk it already
  makes; only the walk-to-the-end exit calls `complete_layout_census`; an
  epoch stop drops it with the progress): no defect.
- Run it on both reworked probes, with the fixed start so the tail rounds'
  cycles are concurrent:
  ```
  for p in GenR5W5ConcUnloadProbe GenR5W3ConcUnloadProbe; do
    CRATONVM_GC_CONC_START_PERCENT=40 CRATONVM_GEN_CONC_CLASS_UNLOAD=1 \
    CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK=1 CRATONVM_DBG=gc-stats \
      cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench $p 2>&1 \
      | grep -E 'conc-unload |conc_unload:'
  done
  ```
  Expected: the `conc-unload ... =true ... =true` stdout line, and
  `concunload_layouts_retained>=1 concunload_layout_censuses>=1
  concunload_layouts_released>=1` with released <= retained. Retire on that.
  `concunload_classes=0` again means the unload page is still open, and this
  page cannot be judged.

## STATUS (2026-09-26, gen r5w4/conc8, superseded above): FIXED, unbuilt — released by an instance census, not by a collection count

**What landed** (`gc/src/concurrent_mark.rs`, `vm/src/memory/gc.rs`,
`vm/src/runtime/interpreter/gc_and_alloc.rs`). This does not follow the
"first complete old collection after the stamp" rule proposed below. A
collection count cannot prove that no instance is left: a dead instance that a
conservative root keeps marked survives every collection. So the release is
decided by a CENSUS of the instances themselves:

1. The unload transaction reports the ids it actually re-registered
   (`restore_retained_layouts` now returns them, and counts one only if
   `register_class_layout` took it: that call silently refuses a slot owned by
   another `ClassStore`). They are held PENDING on this VM's
   `ConcurrentGcState` (`note_layouts_retained`). That state is per VM, so no
   global is added.
2. In the next cycle's initial-mark pause (world stopped), the driver walks the
   young generation. Every pending id with no young object, live or dead,
   becomes a census candidate (`gen_conc_layout_census_candidates` →
   `ConcurrentMarker::set_layout_census`). None can appear later, because the
   class is out of the store.
3. That cycle's sweep notes every candidate for which it walks a SURVIVING
   object (marked, or not sweep-eligible). This rides on the walk the sweep
   already makes: one `Option` test per object when no census runs.
4. Only a sweep whose walk reached the end with no epoch stop, and with no
   old-generation walk break anywhere in the process in between
   (`WALK_DESYNC_HITS + SCAN_REGION_BREAK_HITS` unchanged), completes the
   census. Its candidates with no survivor become releasable.
5. After `cycle.complete()` the driver unregisters them
   (`memory::gc::release_retained_layouts`). It holds the class manager's write
   lock, and acts only for an id still out of the store and only in a process
   with ONE layout domain (`single_layout_domain()`). With several
   `ClassStore`s a slot this VM gave up could hold another store's layout, so
   the layout is kept there, as before.

Anything short of a complete census keeps the layout, which is the old
behaviour: a leak, never a walk that cannot size an object. A pending id costs
one young walk per cycle in the initial-mark pause. In the default
configuration nothing is ever pending (retention needs
`CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK`), so the default path is byte-for-byte
unchanged.

**Census** (the `[GC] conc_unload:` line, printed under
`CRATONVM_GEN_CONC_CLASS_UNLOAD`): new keys `concunload_layouts_released` and
`concunload_layout_censuses`. The number still held is
`concunload_layouts_retained - concunload_layouts_released`.

**Verify.**
- Unit tests, `cargo test -p cratonvm-gc --lib r5w4_`:
  - `r5w4_a_census_moves_released_layouts_from_pending_to_releasable`
  - `r5w4_a_census_across_a_walk_break_releases_nothing`
  - `r5w4_a_complete_sweep_releases_the_layouts_it_found_no_survivor_of`
  - `r5w4_an_epoch_stopped_sweep_releases_nothing`
- Probe:
  ```
  CRATONVM_GC_CONC_START_PERCENT=40 CRATONVM_GEN_CONC_CLASS_UNLOAD=1 \
  CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK=1 CRATONVM_DBG=gc-stats \
    cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR5W3ConcUnloadProbe
  ```
  - stdout: `conc-unload dead-loader-unloaded=true dead-class-unloaded=true control-loader-alive=true control-ok=true`
  - stderr: `concunload_layouts_retained>=1 concunload_layouts_released>=1 concunload_layout_censuses>=1`,
    with released <= retained.
  - The probe runs 4 rounds past the unload so that a later cycle takes the
    census. The fixed start makes those later cycles concurrent; see the
    probe's javadoc.
  - `censuses>=1` with `released=0` means either that the process had more
    than one layout domain, or that the sweep kept seeing a surviving instance
    (a conservative root). Check `foreign_layout_refusals`.

*Filed 2026-09-26 by gen round 5 wave 3, lane `unload7`. Defect (memory);
severity low. Opt-in path only.*

## What is wrong

A generational concurrent remark that unloads classes runs the unload
transaction under `memory::gc::with_retained_unloaded_layouts`
(`vm/src/memory/gc.rs`): `ClassStore::remove` drops each unloaded class's
compact field layout, and the transaction puts the layout straight back, so
the concurrent sweep — which runs after the transaction and sizes every object
it walks from that layout — can still step over the class's dead instances.
See hazard 3 on
`gengc-r5w1-refs5-concurrent-cycle-cannot-unload-classes-20260926.md`.

Nothing ever unregisters those layouts again. Each costs one `CompactLayout`
(a few `Vec`s of offsets, a few hundred bytes) and one slot of the dense
`CLASS_LAYOUTS` table for the life of the VM. A server that redeploys an
application with a few thousand classes every few minutes retains a few MB per
day. The class ids themselves are already tombstones kept forever
(`ClassStore` never reuses an id), so the leak is of the same shape, a few
times larger.

## Where

- `vm/src/memory/gc.rs`: `with_retained_unloaded_layouts`,
  `save_layouts_to_retain`, `restore_retained_layouts`.
- `vm/src/runtime/interpreter/gc_and_alloc.rs`: `remark_unload_dead_classes`
  (both generational remark paths call the transaction through it).
- Census: `[GC] conc_unload: ... concunload_layouts_retained=K` counts them.

## Why it was not released in-session

A layout may go only when no instance of the class can remain anywhere a
walker looks. The sweep that follows the remark frees the dead instances that
existed at the initial mark, but an instance promoted AFTER the initial mark
is not sweep-eligible and survives this cycle unreachable; it is freed by the
NEXT old-generation collection (concurrent or stop-the-world), and a sweep
stopped between slices by an epoch move leaves others. So "after the sweep"
is not a safe release point.

## Proposed fix

Keep the retained ids on the VM's `ConcurrentGcState` (per VM, not a global),
stamped with the old generation's `reclaim_epoch` / collection count at the
remark. Release them (`cratonvm_types::unregister_class_layout`) at the end of
the first COMPLETE old-generation collection that started after the remark
(a concurrent sweep that ran to its end with no epoch stop and whose initial
mark came after the stamp, or any STW major): by then every instance allocated
before the remark has been judged by a full mark and, being unreachable, freed.
Under the class manager's write lock, as `ClassStore::remove` does.

## How to verify

- A unit test on the release rule (stamp, one partial sweep: kept; one complete
  later cycle: released).
- `GenR5W3ConcUnloadProbe` with a loop of 50 redeploys: `concunload_layouts_retained`
  grows, and a new `concunload_layouts_released` catches up with it one cycle
  later.
