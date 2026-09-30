# A young pause inside a G1 marking cycle drops the `metadata_pin` rows the cycle's marker still follows

> **STATUS (2026-09-29, gc defects round, orchestrator, wave d10 verification on the Linux release build `cratonvm-gcd-d10` (`b30b8abaa`) against wave d9's `cratonvm-gcd-d9`, same load, two runs per row per collector (`docs/internal/gc-defects-round-20260927/verify-d10/vd10.list`)): NOT REPRODUCED.** `-XX:+UseG1GC -Xmx256m Gcd1SideTableStaticsProbe` printed `sidetable-statics loaders=24 rounds=32 corrupt=0` (HotSpot's line) in both runs on d10 and on d9, as it did on ZGC and Generational. The defect is still plausible by reading, but this probe does not show it. Before landing the one-line fix, build a probe that forces a young pause inside a G1 marking cycle while a loader's statics are being traced.

> **STATUS (2026-09-28, gcd d10/r): OPEN, filed by reading; UNCONFIRMED.
> A probe is written (`tools/bench/Gcd1SideTableStaticsProbe.java`); no code
> changed.** Owner: whoever takes G1 marking next (the rows are published by
> common code, `vm/src/memory/roots.rs::collect_roots`, lane r in wave d10;
> the consumer is G1's concurrent marker, `gc/src/g1.rs`). The fix below is
> one `if` in `collect_roots`, but it changes what G1's marker reads for the
> rest of every cycle, so it should land only after the probe shows the
> defect (or with a G1 lane's review).
>
> Run (Linux, release, JIT on; 3 runs each):
> ```
> javac -d tools/bench tools/bench/Gcd1SideTableStaticsProbe.java
> java -XX:+UseSerialGC -Xmx256m -cp tools/bench Gcd1SideTableStaticsProbe   # oracle
> for gc in -XX:+UseG1GC -XX:+UseGenerationalGC -XX:+UseZGC; do
>   cratonvm --java-home "$JDK" $gc -Xmx256m -cp tools/bench Gcd1SideTableStaticsProbe
> done
> ```
> Oracle stdout (HotSpot 25 Serial and G1, measured 2026-09-28 on Windows, ~1.4 s):
> `sidetable-statics loaders=24 rounds=32 corrupt=0`. The defect shows as
> `corrupt=` > 0 or a crash under `-XX:+UseG1GC`; stderr's
> `[probe] first_corrupt_round=R` says when. Generational and ZGC are the
> controls (their markers do not read the registry across a mid-cycle pause;
> see "Scope"). A G1 run that prints the oracle line 3/3 does not refute the
> reading -- the window depends on the marker reaching a loader after a young
> pause -- but it lowers the priority.

*Filed 2026-09-28 by the gc defects round, wave d10, lane r (refs10), from the
review of `roots.rs`. Severity if confirmed: **use-after-free** (a static
field of a user-loader class naming a region G1's cleanup freed); G1 with
class unloading on, which is the default (`CRATONVM_LOADER_UNLOAD`).*

## What is wrong, by reading

1. With class unloading on, a root scan LICENSED to treat loader metadata as
   conditional does not root a user-loader class's static values (nor its
   class locks, condy values, `ClassValue` results, proxy `Method`s, defined
   `Package`s): it files each as a `metadata_pin` row under the loader
   (`roots.rs` steps 2-3 `metadata_deferrals`, `native_roots.rs::defer_or_root`,
   `scan_defined_packages`, the `ClassValue` sidetables), and a marker that
   reaches the loader follows the row. On G1 the licence is
   `gc_quiescence::class_unload_marking()` (`roots.rs::conditional_loader_metadata`),
   which only the initial-mark and remark scans set
   (`gc_and_alloc.rs::g1_concurrent_mark_cycle`, `g1_final_remark_cleanup`);
   `VmHeap::metadata_pin_deferrable` answers `true` for every G1 address.
2. G1's concurrent marker follows the rows from `scan_object_refs`'s tail
   (`MarkSideTables`, recaptured whenever `rset_cache_epoch` moves, or the
   per-object `metadata_pin::roots_for_loader` with the table flag off).
3. Every OTHER root scan -- in particular each young pause
   (`run_collection_pause` -> `collect_roots_registry_appended`) that runs
   while the cycle is still marking -- is unlicensed, and `collect_roots`
   step 1 then calls `metadata_pin::set_metadata_weak_mode(vm, false)`
   (which drops this VM's rows) and `replace_metadata_pins(vm, &[])`. The
   young pause roots the values directly, for itself. It bumps
   `rset_cache_epoch`, the marker recaptures, and from then until the remark
   the registry holds no row for this VM.
4. A loader the marker reaches in that window is scanned with no row, so the
   values filed under it at the initial mark are not followed. Nothing else
   marks them: a static field has no SATB pre-barrier and is not a heap
   edge, and the remark's licensed scan defers them again (they are not
   remark roots) and rebuilds the rows, but does not rescan a loader that is
   already marked (`G1Collector::remark` seeds only the roots and the SATB
   log).
5. So a static value reachable only through its static stays UNMARKED at the
   remark. `cleanup` frees a region whose objects are all unmarked -- a
   humongous static array is such a region on its own -- while the static
   still names it; remark-time reference processing clears a weak reference
   to it; `live_bytes` under-counts it.

## Scope

- **G1**: as above.
- **ZGC**: every scan is licensed (`conditional_loader_metadata` answers
  `true`), so no scan drops rows mid-cycle; the rows are rebuilt with current
  addresses at the relocation pause, which ends the cycle.
- **Generational**: the concurrent marker reads the side tables only from its
  own snapshot (`ClassUnloadTables`, opt-in `CRATONVM_GEN_CONC_CLASS_UNLOAD`),
  taken in the initial-mark pause; the young pauses' wipe does not reach it.

## Proposed fix (not applied)

Keep the initial mark's rows for the rest of the cycle: an unlicensed scan
taken while G1 is marking neither switches weak mode off nor empties the rows
(it roots the values itself anyway, and adds no row). The rows are then the
snapshot-at-the-beginning of the side-table edges, which is what SATB needs.
In `vm/src/memory/roots.rs::collect_roots`, replace

```rust
    let conditional_metadata = conditional_loader_metadata(shared);
    cratonvm_types::metadata_pin::set_metadata_weak_mode(shared.vm_identity, conditional_metadata);
    cratonvm_types::metadata_pin::replace_metadata_pins(shared.vm_identity, &[]);
```

with

```rust
    let conditional_metadata = conditional_loader_metadata(shared);
    // gcd d10/r: an unlicensed scan inside a G1 marking cycle (a young pause)
    // must not drop the rows the cycle's marker still follows; see
    // `gcd-d10r-g1-mid-cycle-young-pause-drops-the-metadata-pin-rows-20260928.md`.
    let keep_marker_rows = !conditional_metadata && shared.mem.heap.g1_is_marking_active();
    if !keep_marker_rows {
        cratonvm_types::metadata_pin::set_metadata_weak_mode(shared.vm_identity, conditional_metadata);
        cratonvm_types::metadata_pin::replace_metadata_pins(shared.vm_identity, &[]);
    }
```

and correct the w19-a comment above `replace_metadata_pins(.., &metadata_deferrals)`
("the scan emptied them above") -- it is unchanged in behaviour, since an
unlicensed scan collects no deferral.

**The trade-off to review.** G1 moves only young objects while a cycle is
marking, so a kept row's OLD loader key and OLD values stay exact. A row whose
value was YOUNG at the initial mark names an address the young pause vacated;
the marker then pushes that stale address, and the gray gate
(`classify_mark_scan_target`) either refuses it (a Free region: `NotAllocated`),
marks an unrelated object that starts there (one cycle of floating garbage),
or -- inside a newer object -- refuses it as `TornHeader`, which impugns the
cycle (no reclamation this cycle). Screening young values out of the rows at
the initial mark (root them directly instead) would close that, and needs a
G1 young-region query in `VmHeap::metadata_pin_deferrable` (`gc/src/vm_heap.rs`).

## How to verify the fix

The probe above: `corrupt=0` 3/3 under `-XX:+UseG1GC` where the base build
fails, the Generational and ZGC runs unchanged, and G1's reclamation not worse
(`-Xlog:gc` cleanup lines, or the probe's run time) on a class-loader-heavy
workload (the Tomcat fixture's `--compatible` census).
