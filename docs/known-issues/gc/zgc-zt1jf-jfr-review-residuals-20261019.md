# ZGC JFR review residuals after zt t1/jf

> **STATUS (2026-10-19, zt t6/ob4): R9 tail FIXED IN CODE, PENDING BUILD/ROW.** A ZGC collection's `-Xlog` block now prints, after its `gc,metaspace` line, HotSpot 25.0.3's three `gc,heap` capacity lines (capture `hs-zgc-gcstar.log`): `GC(n) O: Min Capacity: 8M(3%)`, `GC(n) O: Max Capacity: 256M(100%)`, `GC(n) O: Soft Max Capacity: 256M(100%)` (`y:` for a minor; the `gc,init` block's min (`hotspot_min_heap_size`) and max, the heap's current soft max clamped to the max; percent of the max, `%.0f`), behind `CRATONVM_ZGC_XLOG_PAUSE_PHASES` (`=0`: none). Test `zt6ob4_zgc_capacity_lines_have_hotspots_shape`, 904e316709. Row: `cratonvm -XX:+UseZGC -Xmx256m "-Xlog:gc*=info:file=cvm.log:uptime,level,tags" Zt1JfVmOperations`, `grep 'Capacity:' cvm.log` against the HotSpot twin's per-collection lines (one `O:` triple per collection here; HotSpot prints a `Y:` and an `O:` triple per major, the same single-block shape t5/jf chose for the metaspace line); kill control `=0`: no such line. Still open here: R3, R5, R8 (other owners).

> **STATUS (2026-10-19, zt t5/jf, after the R9 capture): R9 FIXED IN CODE, PENDING BUILD/ROW.**
> Against the local HotSpot 25.0.3 `-Xlog:gc*` capture: a ZGC collection now prints its three
> pause lines under `gc,phases` (`Y: Pause Mark Start (Major)`, `O: Pause Mark End`, `O: Pause
> Relocate Start`; `y:` for a minor) and its `gc,metaspace` line between its `gc` start and end
> lines (`CRATONVM_ZGC_XLOG_PAUSE_PHASES`, `=0` kill). Everything else in HotSpot's block is an
> intended difference recorded in `docs/GC.md`. Still open here: the `gc,heap Min / Max / Soft Max
> Capacity` lines (true figures exist; they need the `gc,init` facts at collection time), R3,
> R5, R8.

> **STATUS (2026-10-19, zt t5/jf): R1 FIXED IN CODE (second fix), R9 waiting for a capture.**
> The t3 door row still ended 16-24 ms after its collection on the local box (`Zt1JfSystemGcSpan`
> `gap_us=24069 / 22952 / 15786`, HotSpot `102 / 786 / 140`): the door's end included the
> collection's own event emission and post-pause work. Now both writers end the row where the last
> ZGC collection inside the request ended (`CRATONVM_ZGC_JFR_SYSTEM_GC_ENDS_WITH_COLLECTION`,
> `jfr_vm_more::zgc_system_gc_duration`), so the `FROM_DOOR=0` arm passes too. R9 (`-Xlog:gc+phases`
> writes no ZGC pause line): capture requested, `java -XX:+UseZGC -Xmx256m
> "-Xlog:gc*=info:file=hs-zgc-gcstar.log:uptime,level,tags" Zt1JfVmOperations` (the same with
> `cratonvm` into `cvm-zgc-gcstar.log`); the writer is added only against that file's line shape
> and order. R3, R5, R8 unchanged.

> **STATUS (2026-10-19, zt t5/pg3): OPEN (R1, R9, and zs7jf item 7's soft half).** R1 is FIXED in code but the battery still DIFFs the `jdk.SystemGC` span (`Zt1JfSystemGcSpan`: `ends_with_collection=false`) with `CRATONVM_ZGC_JFR_SYSTEM_GC_FROM_DOOR` on: trace where the door's row (`system_gc_by`, native-builtins/src/jfr_vm_more.rs:834; gate vm/src/runtime/interpreter/gc_events.rs:1087) ends against the collection's end. R9: no `-Xlog:gc+phases` ZGC pause lines (capture HotSpot's shape locally first). Carried: the `GCReferenceStatistics` soft count (a `soft_refs_kept` stat in gc/src/reference.rs). R2, R4, R6, R7 FIXED; R8 structural; R3 is the G1 / Generational owners' (one gate); R5 goes with the zr4jf legacy-writer page.

> **STATUS (2026-10-19, zt t3/jf): R1 FIXED IN CODE; R9 added.** R1: local HotSpot 25.0.3
> `Zt1JfSystemGcSpan` printed `ends_with_collection=true` (gaps 93-112 us), this VM `false`; ZGC's
> `jdk.SystemGC` is now the door's row (`CRATONVM_ZGC_JFR_SYSTEM_GC_FROM_DOOR`, default ON, `=0`
> kill), with the calling thread's pool record taken off its `Thread` object first. Pending the
> row and the `Zs7JfGcEvents sysgc` `system_gc` lines. R9 (new): `-Xlog:gc+phases` writes no ZGC
> pause line (HotSpot: `GC(n) y: Pause Mark Start 0.0NNms`-shaped lines per pause, generation
> prefix `y:` / `Y:` / `O:`), so the log books none of the 3 pauses JFR and the beans book; the
> exact line shape and its place among the cycle's lines need a local HotSpot capture
> (`-Xlog:gc,gc+phases`) before a writer is added (lane ob's `-Xlog` code, or this file's
> `xlog_collection_lines`). R3, R5, R8 unchanged.

> **STATUS (2026-10-19, zt t2/jf): PARTLY FIXED IN CODE, PENDING BUILD/PROBE** (branch `zt2-jf`).
> R2 fixed (the cycle's `Before GC` heap summary is the heap at its mark-start pause, at the cycle
> start, both writers; rides on `CRATONVM_ZGC_JFR_CONCURRENT_CYCLE_SPAN`). R4 fixed by the
> orchestrator's application of t1's hunk X1 (`5960eb8b11`). R6 fixed (the one-row fallback is
> `ZMarkStartYoungAndOld` / `ZMarkStartYoung`; `CRATONVM_ZGC_JFR_PAUSE_VM_OPERATIONS=0` keeps
> `Collect`). R7 fixed (the field and its setter removed: nothing read it). R1: the opt-in is the
> existing `CRATONVM_ZGC_JFR_SYSTEM_GC_ON_CALLER=0`; a possible refuting case found by reading
> (the door's first-sight thread record says `is_virtual: false`), so the flip waits for that row
> too -- see the proposal's STATUS. Open: R1 (decision), R3, R5 (outside this round /
> the writer proposal), R8 (structural).

> **STATUS (2026-10-19, zt t1/jf): OPEN.** Filed by lane `jf` of wave t1: the review findings of
> the JFR GC-event area (`vm/src/runtime/interpreter/gc_events.rs`,
> `native-builtins/src/jfr_vm_more.rs`, `jfr/`, `gc/src/zgc/jfr_facts.rs`) that the lane did not
> fix, each with what would close it. Line numbers are on branch `zt1-jf`. The fixed findings are in
> `docs/internal/zgc-round-zt-20261018/t1-jf-report.md`.

## R1. The caller-side `jdk.SystemGC` spans the door's drains, HotSpot's spans the collection

On ZGC `jfr_vm_more::force_gc_as_system_gc` (`native-builtins/src/jfr_vm_more.rs:1335`) reads the
start before `ctx.force_gc()` and commits after it, and `force_gc` runs the whole
`gc_and_alloc::full_gc_door`: the collection, then the forced finalizer, cleaner and
GC-notification drains. HotSpot's `JVM_GC` commits `EventSystemGC` around
`Universe::heap()->collect(...)` only. So `jdk.SystemGC.duration` here includes Java work HotSpot
does not count (the `System.gc()` workload with finalizable objects shows it most). Since the
2026-10-18 dev merge the door's own row (`system_gc_by`, gci i19/i23: the calling thread, the
request's stack, the collection's span) is HotSpot's shape on every collector, so the caller-side
writer is no longer needed for thread or stack. Closes with the proposal
`docs/internal/proposals/gc/zgc-zt1jf-proposal-system-gc-row-from-the-door-20261019.md`.

## R2. A concurrent cycle's `Before GC` heap summary is taken at the closing pause

With `CRATONVM_ZGC_JFR_CONCURRENT_CYCLE_SPAN` the cycle's `jdk.GarbageCollection` starts at its
mark-start pause, but `jdk.GCHeapSummary` / `jdk.MetaspaceSummary` / `MetaspaceChunkFreeListSummary`
`Before GC` keep the closing collection's start and figure (`jfr_vm_more.rs:633`). HotSpot traces
the heap before the cycle (`ZDriverMajor` scope, before its mark start). Moving the timestamp alone
would put a figure measured later on an earlier instant; the figure would have to be sampled in the
mark-start pause (`gc_events.rs` `zgc_mark_start_jfr`, the heap's used/committed then) and carried
on `ConcurrentCycleJfr` with the pause. Opt-in mode only (`CRATONVM_ZGC_CONC_START`).

## R3. `MetaspaceChunkFreeListSummary` is written on ZGC only

HotSpot 25 writes the four all-zero rows for every collector (`GCTracer::report_metaspace_summary`).
The writer (`jfr_vm_more.rs:702`) is gated on the ZGC half because Generational and G1 are out of
the ZGC round's scope. Closes by dropping the `c.zgc` gate (one line) in a G1 / Generational round,
with `Zs7JfGcEvents sysgc`'s `metaspace_chunks` line run under `-XX:+UseG1GC` / `-XX:+UseSerialGC`.

## R4. An untimed collection loses `marked_concurrently`, so its `Concurrent Mark` row

`ZgcRealHeap::record_metrics_for_cycle` (`gc/src/zgc.rs:27786`) notes the JFR laps -- and with
them `marked_concurrently` -- only `if laps.timed` (`gc/src/zgc.rs:27823`); `timed` needs
`-Xlog:gc`, a non-zero pause target or a sampling director (`gc/src/zgc.rs:28970`). A run whose
pause target gave up (`pause_target_ms` stored 0 after three consecutive overruns,
`gc/src/zgc.rs:5287`, `:5460`) with the rate trigger off then writes no `Concurrent Mark` row for a
real concurrent cycle, and under the cycle span writes its mark start on its own. Closes with the
cross-lane hunk X1 of the t1/jf report (note `marked_concurrently` outside the `timed` block;
`ZJfrCycle::is_empty`, `gc/src/zgc/jfr_facts.rs:133`, then counts it).

## R5. The legacy (`-XX:StartFlightRecording`) writer has no `ZYoungGarbageCollection` and no per-pause VM operations

The real backend now writes both (`CRATONVM_ZGC_JFR_MAJOR_YOUNG`,
`CRATONVM_ZGC_JFR_PAUSE_VM_OPERATIONS`); `jfr/src/builtin.rs` registers no
`jdk.ZYoungGarbageCollection` type (its built-in count is pinned by `s41_builtin_event_count_47`)
and the legacy collection rows write no safepoint operations. Closes with the zr4jf page's writer
proposal (route `-XX:StartFlightRecording` to the real backend) rather than a second copy here.

## R6. Without named pause rows a ZGC collection's VM operation is still `Collect`

`gc_vm_operation` (`jfr_vm_more.rs:187`) falls back to the historical spelling when the caller
names none, and `jfr_vm_operation` names none on ZGC (and ZGC's collections are never `old`, so
never `GC_HeapInspection`). With `CRATONVM_ZGC_JFR_HOTSPOT_PAUSES=0`, or a collection whose pause
split does not fit, the one collection-wide row says `Collect` -- an operation HotSpot's ZGC never
runs. Closes by
naming ZGC's one-row fallback `ZMarkStartYoungAndOld` (the operation that opens a major) or
leaving the row out; decided by `Zt1JfVmOperations` with `CRATONVM_ZGC_JFR_HOTSPOT_PAUSES=0`.

## R7. Dead field `GcCycle::system_gc_on_caller`

`native-builtins/src/jfr_vm_more.rs:131` says nothing reads it since the gci i12/ge merge;
`gc_events.rs:6874` still computes it (one flag read per collection). Remove it with the gci test
literal (`gci_i11_go_tests::cycle`) in a cleanup pass.

## R8. Structural, unchanged

HotSpot's `System.gc()` major writes two `ZYoungGarbageCollection` (pre-clean and roots young
collections) and eight pauses / VM operations; this VM's whole-heap collection writes one young
event and three pauses. The `Zt1JfVmOperations` and `Zs7JfGcEvents` stdout lines compare the
shape both have (names drawn from HotSpot's set, one operation per pause row, the major's old-side
operations present) and print the counts on stderr.
