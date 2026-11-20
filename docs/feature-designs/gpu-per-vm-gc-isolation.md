# Region-Granular Pinning, Physical VRAM Accounting, and Per-VM GPU GC Isolation

**Status:** Partially implemented (Per-VM `GpuCoordContext` implemented; region-granular pinning and physical VRAM accounting remain proposals)  
**Related limitations:** [Heap-Wide Relocation Veto, Logical VRAM Accounting, and Whole-Method JIT Gate](../known-issues/gpu/gpu-global-coord-and-heap-veto-scope.md)

## Motivation

Moving VM-to-GPU coordination contracts from process-global boundaries to per-VM, per-region, and physical-backing boundaries eliminates cross-VM interference and improves compaction/headroom predictability under sustained GPU workloads.

## Design Goals

1. **Implemented:** Isolate mutable GPU/GC coordination state inside each `SharedVm` / `VmHeap` (`GpuCoordContext`) and scope `cuda_bridge::critical::Registry` relocation vetoes and moving-cycle gates per `vm_identity`.
2. **Proposal:** Replace whole-heap non-moving fallback on GPU drain timeouts with region-granular pinning or staged transfers.
3. **Proposal:** Track physical slab and VMM backing allocations alongside logical buffer bytes in VRAM quota and pressure hooks.

## Detailed Design

### Per-VM `GpuCoordContext` (Implemented)

- Each `ExternalGpuPressureTracker` (inside `GenerationalHeap`, `G1Collector`, and `ZgcRealHeap`, exposed via `VmHeap::gpu_coord()`) owns an isolated `Arc<GpuCoordContext>` bound to `SharedVm::vm_identity` (`gc-core/src/gpu_critical.rs`, `vm/src/vm/vm_init/part_02.rs`).
- `GpuCriticalBridge` and `cuda_bridge::critical::Registry` provide VM-scoped queries (`relocation_forbidden_for_vm`, `outstanding_for_vm`, `wait_for_relocation_clearance_for_vm`, `try_begin_moving_cycle_for_vm`, `end_moving_cycle_for_vm`, `outstanding_keepalive_addrs_for_vm`) so `Relocation::Forbidden` tokens or VRAM pressure mirrors in one VM never stall or force non-moving collection in sibling VMs.
- Process-global `gpu_coordination::*` facades delegate to `COORD` only for standalone unit tests or unscoped tokens (`vm == 0`).

### Region-Granular Pinning and Conditional Staging Fallback (Proposal)

- Extend the span registry used by `register_fallible_span_unpin_hook` so G1 and ZGC query whether a candidate evacuation/compaction region intersects an active DMA span instead of switching the entire GC cycle to non-moving mode.
- Regions without active `ZeroCopyPinnedSpan` pins proceed with normal relocation; pinned regions stay in place with exact refcount tracking until asynchronous kernel events retire.
- When a `CycleGuard::before_collection` drain approaches `collector_wait_budget()` before dispatch, prefer marshaling queued small/medium buffers through `PinnedHostPool` rather than acquiring new long-lived `Relocation::Forbidden` heap pins.

### Physical Slab and VMM Backing Byte Accounting (Proposal)

- Distinguish **logical bytes** (`size` requested by `GpuArray` / `GpuFuture`) from **physical backing bytes** (total committed `SubAllocator` slabs and mapped `VmmPool` chunks retained by the backend).
- Report physical backing bytes to `ExternalGpuPressureTracker` and `--gpu-vram-soft-quota` / `--gpu-vram-hard-quota` checks so partial slab fragmentation triggers reclamation and GC before the CUDA/HIP driver returns `OUT_OF_MEMORY`.
- Emit both logical and physical residency totals in GPU JFR memory statistics (`emit_vm_gpu_memory_stats_jfr`).

## Verification Status

- Verified per-VM `GpuCoordContext` and `CudaCriticalBridge` isolation across sibling `VmHeap` / `SharedVm` instances in `gc/tests/gpu_vram_pressure.rs` (`per_vm_gpu_coord_isolates_relocation_veto_and_vram_pressure`) and `vm/tests/gpu_gc_roadmap_it.rs` (`per_vm_cuda_critical_token_isolates_sibling_vm_heap`).
- Pending proposal checks: G1/ZGC single-region pinning compaction test and suballocator fragmentation physical-watermark test.
