> **MOVED (20261019, zt round orchestrator):** out of the ZGC set: its open part belongs to another owner (zt t5/pg3 audit, `docs/internal/zgc-round-zt-20261018/t5-pg3-report.md`). Former name `docs/known-issues/jfr-start-flight-recording-uses-the-legacy-writer-20261012.md`.

# ZGC JFR events still not produced, and what the `-XX:StartFlightRecording` writer cannot model

> **STATUS (2026-10-19, zt t5/jf): OPEN, the remainder is the legacy writer and structural
> events.** No change since t3: `ZMarkFlushOperation` / `ZRendezvousGCThreads`, the major's
> young-collection pauses, `ZPageAllocation`, `ZUnmap`, `ZStatistics*` are structural (recorded in
> `docs/GC.md` for the pause count); the `-XX:StartFlightRecording` writer section stands (boot
> path, outside this lane's files).

> **STATUS (2026-10-19, zt t5/pg3): OTHER-OWNER (JFR boot path).** The not-produced table is structural and is recorded in the GC.md paragraph 'Pauses, beans and JFR' of `docs/internal/zgc-round-zt-20261018/t5-pg3-report.md` (no page allocator, no unmap step, no ZStat counters, no concurrent phases besides `Concurrent Mark`, no `ZMarkFlushOperation` / `ZRendezvousGCThreads`). The one fixable part is collector-independent: route `-XX:StartFlightRecording` to the real backend instead of the legacy writer (vm_init.rs / config.rs boot path; also zs7jf item 10, zt1jf-review R5). Move to `docs/known-issues/jfr-start-flight-recording-uses-the-legacy-writer-20261012.md`.

> **STATUS (2026-10-19, zt t3/jf): PARTLY FIXED; one more not-produced pair recorded.** HotSpot
> 25.0.3 writes `ZMarkFlushOperation` (caller `ZWorker*`) and `ZRendezvousGCThreads` VM
> operations inside every ZGC cycle; this VM writes neither (structural: no concurrent mark to
> flush in the default mode, no relocation worker rendezvous), see `zgc-zs4ob3-*`. The legacy
> writer section stands.

> **STATUS (2026-10-19, zt t1/jf): PARTLY FIXED IN CODE, PENDING BUILD/PROBE.** The s10 fixes
> are ported (branch `zt1-jf`). Produced now as well (real backend): `jdk.ZYoungGarbageCollection`
> for a major (`CRATONVM_ZGC_JFR_MAJOR_YOUNG`; one per major, where HotSpot's `System.gc()` writes
> two), and HotSpot's ZGC VM operations per pause (`ZMarkStartYoungAndOld` / `ZMarkEndOld` /
> `ZRelocateStartOld`, `CRATONVM_ZGC_JFR_PAUSE_VM_OPERATIONS`). The legacy
> (`-XX:StartFlightRecording`) writer still has no `ZYoungGarbageCollection` type and no
> per-pause VM operations; the writer section below stands.

> **STATUS (2026-10-16, zs s10/jf3): PARTLY FIXED; the rest recorded.** Produced now (real backend): `jdk.ZYoungGarbageCollection` for a generational-mode `ZGC Minor` (s7), `GCReferenceStatistics` (s7, majors only s8), collector-thread names (s8), `MetaspaceChunkFreeListSummary` (s10, HotSpot's all-zero rows), concurrent-cycle spans and the `Pause Mark Start (Major)` row (s10). Still not produced, with the reason: `ZYoungGarbageCollection` for a major, `ZPageAllocation`, `ZUnmap`, `ZStatistics*` / `ZThreadPhase`, the non-`Concurrent Mark` concurrent phases, HotSpot's 8 pauses per `System.gc()` -- all structural (table below and `zgc-zs7jf-jfr-gc-event-residuals-CLOSED-20261019.md`). The `-XX:StartFlightRecording` writer section stands: routing the flag to the real backend is a boot-path change outside the JFR lane's files (`vm_init.rs` / `config.rs`); the proposal below is the way to close it.

> **STATUS (2026-10-15, zs s7/jf): PARTLY FIXED IN CODE, PENDING HOST PROBE.** (1) `jdk.GCPhasePause`: a ZGC collection's pause is now written as HotSpot 25's named pauses over its own phase boundaries (zs s4/ob3's split): `Pause Mark Start (Major)` (`Pause Mark Start` for a generational-mode minor), `Pause Mark End`, `Pause Relocate Start`; `GarbageCollection.longestPause` is the longest of them (both writers, `CRATONVM_ZGC_JFR_HOTSPOT_PAUSES`). (2) `jdk.ZYoungGarbageCollection` is written for a generational-mode `ZGC Minor` collection, `tenuringThreshold` = promotion age - 1 (`VmHeap::zgc_jfr_tenuring_threshold`), real backend only. (3) `jdk.GCReferenceStatistics` (four rows per collection) on both writers. (4) `jdk.SystemGC` on the real backend comes from the `Runtime.gc()` native: calling thread, its stack, spanning the call (it was the VM thread with no stack). (3)+(2) `CRATONVM_ZGC_JFR_REF_AND_YOUNG_EVENTS`, (4) `CRATONVM_ZGC_JFR_SYSTEM_GC_ON_CALLER`, all default ON with `=0`. Still open (structural or out of lane), each with what would close it: `docs/internal/gc/zgc-zs7jf-jfr-gc-event-residuals-CLOSED-20261019.md`. The "not produced" table below stands for its other rows; the `-XX:StartFlightRecording` writer section stands.

> **STATUS 2026-10-15 (zs s5/pg): PARTLY -- The Level-1 rows are gone (`CRATONVM_ZGC_JFR_HOTSPOT_PHASE_SET`; host zr z13 `GCPhasePauseLevel1 present=false` in both modes, kill arm 36 rows). The configuration events are fixed (zs s2/ob 8a274f1c70, `CRATONVM_ZGC_JFR_HOTSPOT_CONFIGURATION`, ON), and rows ob_OB4a/OB4b are not among the s2b DIFFs. Still open: `jdk.ZYoungGarbageCollection` (`VmHeap::tenuring_threshold` is None on ZGC, gc/src/vm_heap.rs:3949-3955), `-XX:StartFlightRecording` still on the legacy writer, and the structural rows.**

> **STATUS (2026-10-14, zs s2/ob): two configuration events corrected on ZGC (FIXED IN CODE, PENDING HOST PROBE); the table below is unchanged.** The real backend's `jdk.GCHeapConfiguration` wrote `Runtime.totalMemory()` as both `minSize` and `initialSize` and `compressedOopsMode = Non-zero disjoint base`; HotSpot 25.0.3 ZGC writes `MinHeapSize`, `InitialHeapSize` and `32-bit` (`-Xmx256m -Xms64m`: `min=67108864 initial=67108864`). `jdk.GCConfiguration` hard-coded `isExplicitGCDisabled = false` and ignored explicit `-XX:ParallelGCThreads` / `-XX:ConcGCThreads`. Now stated from the VM's configuration on ZGC (`CRATONVM_ZGC_JFR_HOTSPOT_CONFIGURATION`, default ON, `=0` kill); probe `Zs2ObJfrConfig`. The ergonomic thread counts and `jdk.GCSurvivorConfiguration` stay generic (`zgc-zs2ob-observability-review-residuals-CLOSED-20261019.md` item 5).

> **STATUS (2026-10-13, zr z9/hp): the level-1 residual FIXED IN CODE, PENDING HOST PROBE.** HotSpot 25 ZGC writes no `jdk.GCPhasePauseLevel1` (25.0.3 locally, `Zr4JfJfrEvents sysgc` then `read`: `type jdk.GCPhasePauseLevel1 present=false`; its pause phases are `Pause Mark Start`, `Pause Mark Start (Major)`, `Pause Mark End`, `Pause Relocate Start`, and the work is in `GCPhaseConcurrent` / `GCPhaseConcurrentLevel1` / `Level2` rows). This VM wrote the collector's laps as level-1 rows; they run inside the one pause, so they cannot be re-labelled concurrent either, and are no longer written (`CRATONVM_ZGC_JFR_HOTSPOT_PHASE_SET`, default on, both JFR backends; `=0` restores the z4/jf rows). HotSpot also writes `jdk.ZPageAllocation` (9 on the sysgc shape) and `jdk.ZYoungGarbageCollection` (8): both stay unproduced for the reasons in the table below.

> **STATUS (2026-10-12, zr z4/jf): OPEN (partly structural).** After z4/jf this VM's ZGC writes `GarbageCollection`, `GCHeapSummary`, `GCPhasePause` (the collection's pause and the mark-start pause), `GCPhasePauseLevel1` (the collector's laps), `GCPhaseConcurrent` (`Concurrent Mark` of a cycle that really marked concurrently), `ZAllocationStall`, `ZRelocationSet`, `ZRelocationSetGroup`, `ZOldGarbageCollection`, and `ZUncommit` once the uncommit step times itself (cross-lane hunk for lane `un` in the z4-jf report). The types below are not written, each for the reason given.

## Not produced

| type | why | what would close it |
|---|---|---|
| `jdk.ZYoungGarbageCollection` | its payload beside `gcId` is `tenuringThreshold`; `VmHeap::tenuring_threshold` is `None` on ZGC (the generational mode's promotion age is not exposed), and the default mode has no young collection at all | expose the generational mode's promotion age (`ZgcRealHeap::promotion_age`) through `VmHeap::tenuring_threshold` and write the event for `ZGC Minor` collections |
| `jdk.ZPageAllocation` | HotSpot commits one per page allocation (thresholded by its settings); this VM's ZGC hands out TLABs and objects from an arena, not pages, so there is no "page allocation" with a `flushed`/`committed`/`segments` breakdown | none honest short of a page allocator; a slow-path arena refill event would be a CratonVM event, not this one |
| `jdk.ZUnmap` | HotSpot unmaps virtual memory of relocated pages; this VM decommits granules (`ZUncommit`) and has no separate unmap step | structural |
| `jdk.ZStatisticsCounter` / `ZStatisticsSampler` / `ZThreadPhase` | experimental HotSpot events over HotSpot's `ZStat` counters and worker phases | structural |
| `jdk.GCPhaseConcurrent` for anything but `Concurrent Mark` | the other HotSpot concurrent phases (`Concurrent Mark Continue`, `Concurrent Mark Free`, `Concurrent Process Non-Strong`, `Concurrent Reset/Select Relocation Set`, `Concurrent Relocate`, `Concurrent Remap Roots`) do not run concurrently here: reference processing, selection and the slide are inside the collection's stop-the-world pause and are reported as its level-1 rows | concurrent relocation (a design change, not an observability one) |
| HotSpot's three pause names per cycle | a collection here is one stop-the-world pause; only a concurrent cycle's mark start is a separate pause (`Pause Mark Start`). The collection's pause keeps the level-0 name `GC Pause` (the real backend's name since SBR5) because naming it `Pause Mark End` or `Pause Relocate Start` would claim one of HotSpot's three pauses for a pause that contains all three | structural (see `zgc-zr1tr-pause-beans-count-one-pause-per-cycle-20261012.md`) |

## What the legacy writer cannot model (`-XX:StartFlightRecording`)

`-XX:StartFlightRecording` records into the legacy recorder (`cratonvm_jfr`) and
dumps through `jfr/src/jdk_chunk.rs`, whose metadata comes from
`jfr/src/builtin.rs`'s registry, not from the JDK's `metadata.bin`. That writer
emits no constant pools and no struct-typed fields (its module doc), so:

* `ZPageTypeType` (`ZRelocationSetGroup.type`, `ZAllocationStall.type`),
  `GCName`, `GCCause`, `GCWhen` are plain `string` fields: `getString(..)`
  reads the same text, `ValueDescriptor.getTypeName()` differs.
* `GCHeapSummary.heapSpace` is a `string` (HotSpot: a `VirtualSpace` struct
  with `start`, `committedEnd`, `committedSize`, `reservedEnd`,
  `reservedSize`); this was so before z4/jf.
* No `eventThread` / `stackTrace` on any event (`getThread() == null`).
* HotSpot's `ulong`/`uint` fields are signed `long`/`int`.

A recording started from Java (`jdk.jfr.Recording`, `jcmd JFR.start`) goes
through the real backend (`native-builtins/src/jfr_backend.rs`), which writes
with the JDK's own metadata and has none of these limits. Closing the gap for
`-XX:StartFlightRecording` means routing the flag to the real backend (start a
`jdk.jfr` recording at boot, as HotSpot's `DCmdStart` does) rather than growing
the legacy writer.
