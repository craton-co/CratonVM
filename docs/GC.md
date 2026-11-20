# Garbage collection

`gc/src/vm_heap.rs::VmHeap` dispatches allocation, collection, reference
processing, and usage accounting to the configured collector. Java-visible
behavior must match HotSpot in both foundational and compatible execution.

## Collectors

| Collector | Selection | Current behavior |
|---|---|---|
| ZGC | Default; `-XX:+UseZGC` | TLAB allocation, parallel collection machinery, concurrent SATB marking profile enabled by default. Concurrent relocation is opt-in. |
| Generational | `-XX:+UseGenerationalGC` | Moving young generation and concurrent old-generation mark/sweep; young moving cycles require complete rewritable roots. |
| G1 | `-XX:+UseG1GC` | Experimental region collector with concurrent marking and remembered sets. |

ZGC's concurrent profile is controlled by `CRATONVM_ZGC_CONCURRENT`.
`CRATONVM_ZGC_CONCURRENT_RELOCATE=1` enables the colored-reference relocation
stack on eligible non-generational heaps. `CRATONVM_ZGC_GENERATIONAL=1` selects
ZGC's separate generational configuration; it does not select the Generational
backend. Read individual flag defaults at their source sites.

## Heap and usage accounting

`-Xms` and `-Xmx` control initial commitment and the heap limit. Reserved address
space, committed memory, and used bytes are distinct quantities. Runtime and
management answers must use the appropriate per-heap quantity, including after
uncommit, collection, or allocation failure. Collector selection must not cause
an unavailable collector's accounting to stand in for the active one.

See [GC tuning](gc-tuning.md) and the [configuration inventory](CONFIG.md) for
heap, worker, trigger, uncommit, and diagnostics options.

Address-space reservation does not mean that all of `-Xmx` is resident at launch.
The arena backing store reserves the envelope and commits storage as allocation
reaches it. G1 commits regions; ZGC and the Generational arenas use granules.
`Runtime.totalMemory()` and management heap `committed` describe committed
capacity, not live bytes or the reservation. `Runtime.freeMemory()` is free
space within that committed capacity. OS commitment and the Generational
collector's usable semispace capacity are separate accounting quantities.
The owning accessors are `VmHeap::committed_bytes` and `os_committed_bytes`.

ZGC uncommit preserves the `-Xms` floor. Its per-VM `-XX:+/-ZUncommit` and
`-XX:ZUncommitDelay=<seconds>` policy controls giving unused memory back to the
OS. A wholly committed backing-store fallback cannot provide the same decommit
behavior. More heap headroom may be needed when root coverage prevents a moving
cycle: a non-moving sweep can reclaim dead objects without eliminating holes.

## Roots and moving-object safety

A moving collection needs a complete inventory of references and the ability to
rewrite their storage. Interpreter frames, compiled stack maps, JNI handles,
scoped native handles, class/static roots, and attached thread state all
participate. An unrewritable native local is not made safe by a plausible-pointer
test. A cycle with incomplete coverage must refuse movement or use an admitted
non-moving fallback.

Native code holding a reference across allocation or a safepoint uses a scoped
root handle. Compiled frames publish their location metadata at every relevant
poll, including OSR and helper transitions. Blocking threads must follow the
safepoint participation protocol; a blocking syscall cannot strand a counted
mutator outside the rendezvous.

## Barriers and reference processing

Remembered-set/card barriers preserve young-to-old reachability. Concurrent
marking preserves overwritten references through SATB. The optional ZGC
relocation stack additionally requires barrier-aware decoding, forwarding, and
repair of reference loads and stores, including arrays and atomics.

Soft, weak, phantom, finalizer, and queue behavior remains Java-visible.
Collector internals must preserve reachability and publication ordering across
reference discovery, clearing, and enqueueing. Performance and correctness gaps
are recorded in [known GC issues](known-issues/gc/).

## Reading pause diagnostics

`--verbose:gc` reports individual pauses and shutdown aggregates. The pause
record in `vm/src/runtime/interpreter/gc_events.rs` separates waiting for
mutators from collection work:

| Field | Meaning |
|---|---|
| `door` | The request source, such as allocation failure, `System.gc`, remark, or a stack walk. |
| `ttsp_us` | Time from accepting the request until counted mutators are parked or taken over. |
| `xt_pass_us` | Cross-thread takeover work within `ttsp_us`; do not add it again. |
| `xt_post_quota_us` | Frame/root work after the arrival quota is met and before collection starts. |
| `collect_us` | Collection, reference processing, and remap work inside the pause. |
| `work_us` | Work in a non-collection pause, replacing `collect_us`. |
| `straggler` | The last participating thread to arrive, or `none`. |

The request-to-release pause is approximately
`ttsp_us + xt_post_quota_us + collect_us`, excluding diagnostic verifiers.
The `-Xlog:gc` and JFR collection duration excludes time to safepoint. The
shutdown `[GC] ttsp:` count also includes non-collection pauses, so it can
exceed the number of garbage collections. A large `ttsp_us` points to mutator
rendezvous; a large `collect_us` points to collection work.

ZGC's `[GC] zgc-relocate:` record reports moved bytes. Its
`zgc-relocation-refusal-term` counters count evaluated refusal terms, while
`zgc-relocation-skip-reason` counts decisions that actually stopped a slide.
Use these records to distinguish an enabled relocation policy from cycles that
actually moved objects. Collector selection alone does not establish movement,
HotSpot pause-time parity, or HotSpot collection scheduling.

## Implementation contracts

- [ZGC architecture](feature-designs/zgc-architecture.md) and [concurrent GC](feature-designs/concurrent-gc-maturation.md)
- [Generational architecture](feature-designs/generative-gc-architecture.md) and [moving young generation](feature-designs/default-moving-young-gen.md)
- [Native handles](feature-designs/native-handle-discipline.md) and [compact reference fields](feature-designs/compact-ref-field-layout.md)
- [JIT barriers](feature-designs/jit-barrier-lowering.md), [load barriers](feature-designs/zgc-jit-load-barrier.md), and [reference slots](feature-designs/zgc-reference-slot-representation.md)
- [Safepoint polls](architecture/jit-safepoint-polls.md) and [reference publication](architecture/inline-allocation-and-reference-publication.md)

## GPU coordination and VRAM pressure

When `gpu-offload` is enabled, `gc/src/vm_heap.rs::gpu_coordination` coordinates
moving collectors with active GPU critical sections and zero-copy host DMA.
Before a moving cycle begins, `CycleGuard` (`before_collection()`) drains active
`Relocation::Forbidden` critical tokens across both `critical::Registry` and
`GPU_CRITICAL_COUNT` under a single shared `collector_wait_budget()` deadline and
holds an RAII guard over `GPU_COLLECTING` (`CollectingGuard`) for the duration of
the pause; `enter_relocation_forbidden()` drains any active post-pause ZGC
concurrent relocation work (`wait_for_zgc_concurrent_relocate_idle()`) before
`Forbidden` DMA or page-locked heap-span pinning proceeds, and ZGC gates
concurrent relocation on `!gpu_relocation_forbidden()`.

To close the native-memory blindspot where small Java wrapper objects retain
large device allocations, `gpu_coordination` mirrors live VRAM bytes and the
configured reservation limit via lock-free atomics and exposes
`collect_garbage_for_gpu_pressure()`, which snapshots and invokes registered
per-VM collection callbacks outside `COORD` locks before OOM fallback. On
`VmHeap` drop, span-unpin hooks release any `cuMemHostRegister` heap-span pins
before backing arena memory is unmapped.

## Validation

Collector tests live in `gc/tests/` and the VM's integration tests. Compare
Java-visible behavior with HotSpot under both execution policies and exercise
moving roots, JNI/foreign-thread transitions, allocation failure, references,
and concurrent mutation. Hardware/host timings and debug counters are local
evidence; they do not establish a general throughput or pause-time guarantee.
