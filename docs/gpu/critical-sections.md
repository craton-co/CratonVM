# GPU Critical Sections and Collector Coordination

GPU kernels and zero-copy DMA operations that read or pin JVM heap memory run inside a `Relocation::Forbidden` critical section managed by [`cuda-bridge/src/critical.rs`](../../cuda-bridge/src/critical.rs) and [`gc/src/vm_heap/mod.rs`](../../gc/src/vm_heap/mod.rs) (`gpu_coordination`).

## Critical Tokens and Ownership

- **`CriticalToken`** (`cuda_bridge::critical::Registry`): RAII, `Send` token carrying a per-VM `OwnerId`, `Relocation` mode (`KeepAliveOnly` or `Forbidden`), and acquisition timestamp.
- **`critical::registry()` & `GPU_CRITICAL_COUNT`**: `CycleGuard::before_collection` (`gc/src/vm_heap.rs`) queries `cuda_bridge::critical::registry()` directly (`outstanding`, `wait_for_relocation_clearance`, `relocation_forbidden`) alongside the legacy `AtomicU32` counter (`GPU_CRITICAL_COUNT` via `gpu_critical_enter` / `wait_for_gpu_critical_drain_until`).
- **ZGC concurrent-relocate barrier**: Before `Relocation::Forbidden` DMA or `HostRegionPinCache` heap-span pinning proceeds, `vm/src/runtime/offload.rs` calls `gpu_coordination::enter_relocation_forbidden()`, which drains any active post-pause ZGC lazy relocation (`wait_for_zgc_concurrent_relocate_idle()`). ZGC also gates concurrent relocation on `!gpu_relocation_forbidden()`.

## Heap-Span Pinning vs. Staged Transfers

- **Zero-copy (`ZeroCopyPinnedSpan`):** `HostRegionPinCache::pin_span` (`cuda-bridge/src/pinned_pool.rs`) caches `cuMemHostRegister_v2` registrations for ZGC/G1 arenas using refcounted spans. `gpu_coordination::register_span_unpin_hook` and `register_fallible_span_unpin_hook` unpin registered spans on `VmHeap` drop or veto granule decommit/reset while a span remains actively pinned.
- **Staged (`PinnedHostPool`):** When zero-copy is unavailable or disallowed, transfers stage through pooled page-locked host buffers (`PinnedHostPool` on CUDA) or pageable staging buffers on sibling backends, avoiding long-lived JVM heap pointers on the device.

## Bounded Collector Drain and Expiry

- **Shared drain budget:** Before a moving GC cycle begins, `CycleGuard` (`gc/src/vm_heap.rs::before_collection`) drains both `cuda_bridge::critical::registry()` and `GPU_CRITICAL_COUNT` under a single shared `collector_wait_budget()` deadline.
- **Non-moving fallback:** If the drain budget expires while a `Relocation::Forbidden` token is still active, the collector records a relocation veto (`COORD.vetoing_cycles`) and falls back to a non-moving collection for that cycle rather than spinning indefinitely.
- **Orphaned-token reaper and VM shutdown:** `critical::Registry` reaps expired leases (30 s default) and `shutdown_vm(vm_id)` revokes tokens belonging to a tearing-down VM so one VM cannot wedge subsequent VMs in the same process.
