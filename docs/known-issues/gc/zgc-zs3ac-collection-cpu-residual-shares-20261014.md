> **STATUS (2026-10-19, zt t6/ob4): item 2 FIXED IN CODE, item 4 FIXED IN CODE (opt-in), PENDING BUILD; item 6 HOST row; 5 unchanged (not wired).** Item 2: `run_collection_pause` (gc_and_alloc.rs) opens a span before the safepoint request and closes it after `gc_event_finish_by`; `ZgcRealHeap::book_vm_pause_cpu` books the thread CPU less the heap's own `pause_thread_ns` delta (`zgc_vm_pause_share`) to a new `vm_pause_ns` share: `vm_pause_ms=` at the end of `[GC] zgc-cpu:`, inside `collection_ms` (`CRATONVM_ZGC_VM_PAUSE_CPU`, default ON, `=0` kill; test `zt6ob4_the_vm_side_of_a_pause_is_its_own_share`), 3333a8ffb5. Item 4: `CRATONVM_ZGC_THREAD_CPU_CYCLES=1` (opt-in) makes `mark::thread_cpu_nanos` read `QueryThreadCycleTime` scaled by the TSC rate, calibrated once against `QueryPerformanceCounter` (new `gc/src/zgc/thread_clock.rs`; opt-in because Microsoft documents the cycle count as not convertible to time); tests `zt6ob4_cycle_scaling_and_tsc_rate_arithmetic`, `zt6ob4_cycle_clock_is_opt_in_and_reads_a_sub_quantum_span`, 7e0d1d9814. Deciding row for a flip: Windows, `--verbose:gc` run of a GC-heavy probe, `[GC] zgc-cpu: collection_ms=` with and without the opt-in against the same run's process CPU, interleaved x5; pass = the opt-in's per-pass worker shares are non-zero where the quantum clock reads 0 and `collection_ms` stays below `process_ms`. Item 6: still needs the Linux HotSpot read: `java -XX:+UseZGC -XX:+UsePerfData -Xmx256m <GC-heavy probe>` then `jcmd <pid> PerfCounter.print | grep sun.threads` on the host; publish ours only if HotSpot fills them there.

> **STATUS (2026-10-19, zt t5/pg3): OPEN (items 2, 4, 6).** 1, 3 FIXED (row RL10 SAME); 5 DECIDED (page-evac pool not wired). Open: 2, the VM's side of a pause (rendezvous, common epilogue) is outside the span: open it in `run_collection_pause` (vm/src/runtime/interpreter/gc_and_alloc.rs:5263) and hand it to the heap; 4, `mark::thread_cpu_nanos` (gc/src/zgc/mark.rs:552) is `GetThreadTimes`, quantum-rounded, and the orchestrator's rows now run on Windows (`QueryThreadCycleTime`); 6 needs a Linux HotSpot read of `sun.threads.total_gc_cpu_time` first (host).

> **STATUS (2026-10-19, zt t3/rl): item 3 (the `ZUncommitter` thread) FIXED IN CODE, PENDING BUILD (t1/mk hunk X-rl2).** Each uncommitter pass runs under a thread-CPU span (`ZgcRealHeap::uncommit_pass_booked`) into a new `uncommit_ns` share of `ZCollectionCpuTotals`, printed as `uncommit_ms=` on `[GC] zgc-cpu:` and added to `collection_ms=`; the per-cycle `[GC] zgc-pause-cpu:` line leaves it out (it runs between collections). Kill: `CRATONVM_ZGC_MARK_CPU_ACCOUNTING=0` (543198eb0c).

> **STATUS (2026-10-19, zt t3/mk): items 1 and 3 FIXED IN CODE, PENDING BUILD; rv3 item 1 landed (5960eb8b11).** Item 1: one thread-clock pair per SATB flush (`satb_flush_cpu_ns`; `satb_flush_ms=` at the end of `[GC] zgc-cpu:`, NOT in `collection_ms` -- it is mutator CPU, the flush half of T2; the per-store half is still unmeasured), 80eab05008. Item 3: the `ZUncommitter` prices each pass (`uncommit_ns`, `uncommit_ms=` on `[GC] zgc-cpu:`, inside `collection_ms` as HotSpot's GC thread is), bde07726ec. Still open: item 2 (the VM's side of a pause, VM code), item 4 (Windows quantum clock), item 5 (page-evac pool, not wired), item 6 (`sun.threads.*`).

> **STATUS (2026-10-19, zt t1/mk): rv3 items 2 and 3 FIXED IN CODE, PENDING BUILD; the rest open.** rv3 item 2: under `CRATONVM_ZGC_MARK_INLINE_DRIVER=0` the spawned driver thread prices itself (`ZgcMarkCycleOutcome::driver_thread_cpu_ns`, gc/src/zgc_concurrent.rs:577) and `mark_with_controller_stw` books it with the stop-the-world pool's workers, so `collection_ms` stays a sum of disjoint shares; `mark_driver_cpu_us=` still reads the collecting thread only (~0 on that arm, by design). rv3 item 3: the driver-panic arm books the retired pool's CPU (a lower bound, read after the join). Both in 890b242f9e. rv3 item 1 (the pooled relocation pass) is relocation code: cross-lane hunk X-rl1 in `docs/internal/zgc-round-zt-20261018/t1-mk-report.md`. Items 1 (SATB flush clock), 2 (the VM's side of a pause), 3 (`ZUncommitter`, `start_uncommitter` gc/src/zgc.rs:3623: lane rl), 4 (Windows quantum clock), 5 (page-evac pool, not wired) and 6 (`sun.threads.*`) are unchanged.

# ZGC: collection CPU that the zs s3/ac accounting still does not see

> **STATUS 2026-10-15 (zs s5/pg): OPEN -- All six shares are still unbooked. `reloc_par_chunks` (gc/src/zgc.rs:12321) still says "the pooled pass charges no `relocate_pool_cpu_ns` yet". `ZRelocateWorkers::relocate_set` (gc/src/zgc/relocate.rs:3512) reads no CPU clock. `start_uncommitter` books no share. `mark::thread_cpu_nanos` still uses `GetThreadTimes` (mark.rs:541), and there is no `sun.threads` counter. The rv3 items 1-3 in the newest STATUS (pooled relocation pass, the `MARK_INLINE_DRIVER=0` driver, the driver-panic arm in `mark_with_controller_stw`) are also unfixed.**

> **STATUS (2026-10-14, zs s4/rv3): STILL OPEN; the wave-s3 review found three more shares**: the opt-in pooled relocation pass (`CRATONVM_ZGC_RELOCATE_POOL=1`) books no CPU, the `CRATONVM_ZGC_MARK_INLINE_DRIVER=0` arm's driver thread is in no share, and the driver-panic arm drops the failed pool's CPU. These are items 1-3 of `zgc-zs4rv3-wave-s3-review-residuals-CLOSED-20261019.md`.
>
> **STATUS (2026-10-14, zs s3/ac): OPEN.** Filed with the accounting it qualifies: `[GC] zgc-cpu:` /
> `[GC] zgc-pause-cpu:` (`docs/internal/zgc-round-zs-20261014/s3-ac-report.md`). Each item says what
> would close it. None of them changes Java-visible output.

The shutdown line's `collection_ms=` is the collecting thread across `collect_garbage` plus every
pool's workers. It leaves out the following, so `process_ms - collection_ms` is an UPPER bound on
mutator CPU, not the mutator CPU.

## 1. The SATB slow path on the mutators (T2 of the every-trigger proposal)

`ZgcRealHeap::satb_pre_barrier_slow` (`gc/src/zgc.rs`) and the hand-offs it makes run on Java
threads while a concurrent cycle is open. A clock pair per store is out of the question (the cost
rule: never per object). **Close:** a clock pair per FULL buffer flush
(`hand_thread_batch_to_the_marker` / `hand_satb_batch_to_the_marker`) prices the flush half; the
per-store half stays `records x` a per-record cost measured by a microbenchmark. Decided by A2 vs A1
of `zgc-zs1mc-proposal-every-trigger-cpu-attribution-20261014.md`.

## 2. The VM's side of a pause

The safepoint rendezvous before `collect_garbage`, and the common stop-the-world epilogue the VM
runs after it for every backend (weak lock-key sweep, reference enqueue, the GC event / bean seal in
`vm/src/runtime/interpreter/gc_events.rs`), are on the collecting thread but outside the span.
**Close:** open the span in the VM's ZGC door (`gc_and_alloc`) instead, and hand it to the heap;
VM code, not this lane's. Decided by a row whose `pause_cpu_us` is compared with the
`-Xlog:gc` pause wall time on a quiet host: a gap that grows with the thread count is the
rendezvous.

## 3. The `ZUncommitter` thread

`ZgcRealHeap::start_uncommitter` (`gc/src/zgc.rs`, the `ZUncommitter` spawn) returns memory
between collections; HotSpot's `ZUncommitter` is a GC thread (a `ConcurrentGCThread`). **Close:** charge the
thread's CPU per batch to a fifth share (`uncommit_ms=`). Cheap: one batch per wait period.

## 4. A Windows thread clock is quantum-rounded

`mark::thread_cpu_nanos` uses `GetThreadTimes` on Windows, which advances in scheduler quanta
(~15.6 ms). A span shorter than a quantum reads 0 or one quantum, so per-pass worker spans
undercount systematically there. Linux (`CLOCK_THREAD_CPUTIME_ID`) is exact. **Close:**
`QueryThreadCycleTime` scaled by the TSC rate, if a Windows number is ever needed; the host rows
are Linux.

## 5. The page-evacuation pool

`ZRelocateWorkers::relocate_set` (`gc/src/zgc/relocate.rs`) spawns its own workers and accounts no
CPU. It is not wired into the VM (its doc forbids it until the thread-stack handshake exists).
**Close:** when the page-evac arm lands, open a `mark::ZThreadCpuSpan` in each spawned worker and add
to `relocate_pool_cpu_ns`, as `zgc_par_chunks_with` does.

## 6. HotSpot's GC-thread CPU counters

HotSpot 25 publishes `sun.threads.total_gc_cpu_time` and per-group `sun.threads.cpu_time.*`
counters through hsperfdata (`jcmd <pid> PerfCounter.print`). On the local JDK 25.0.3 (Windows),
ZGC prints `sun.threads.total_gc_cpu_time=0`. CratonVM's perf-data file
(`vm/src/runtime/perf_data_file.rs`) has no `sun.threads.*` counter. **Close:** read the Linux
HotSpot ZGC values first; publish ours from `collection_cpu_totals()` only if HotSpot fills them on
Linux, and with HotSpot's grouping. Not a Java API (`GarbageCollectorMXBean` has no CPU figure;
`MemoryMXBean`'s total GC CPU is JDK 26).
