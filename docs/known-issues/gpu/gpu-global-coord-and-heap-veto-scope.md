# Heap-Wide Relocation Veto, Logical VRAM Accounting, and Whole-Method JIT Gate

**Status:** Accepted architectural limitations

## Overview

Several GPU/GC coordination and compilation boundaries operate at coarser granularity than individual heap regions, physical slabs, or call sites (per-VM `GpuCoordContext` and VM-scoped `critical::Registry` relocation isolation have been implemented):

1. **Heap-wide `Relocation::Forbidden` non-moving fallback (`gc-core/src/gpu_critical.rs`, `gc/src/vm_heap/`)**
   - When `CycleGuard::before_collection` exhausts its `collector_wait_budget()` waiting for active `Relocation::Forbidden` tokens in that VM, it falls back to non-moving collection for that VM's entire Java heap.
   - `register_fallible_span_unpin_hook` already tracks pinned `HostRegionPinCache` ranges per `(base, len)` span, but region-level eviction immunity is not yet integrated with moving collector compaction sets across G1/ZGC.

2. **Logical vs. physical slab/VMM VRAM accounting (`cuda-bridge/src/allocator.rs`, `cuda-bridge/src/vmm.rs`)**
   - `VRAM_USED_BYTES` and `ExternalGpuPressureTracker` account logical allocated or retained buffer bytes rather than physical slab and `VmmPool` virtual-address backing extents.
   - Internal fragmentation in retained suballocator slabs or uncompacted VMM regions can hit driver physical memory limits before logical byte watermarks fire.

3. **Whole-method `offload_jit_gate` refusal (`vm/src/runtime/offload_jit_gate.rs`)**
   - While `--gpu` is active on a CUDA-capable host, `caller_blocks_jit` keeps an entire Java method interpreted if it contains an eligible `invokestatic` offload call site (or primitive array writes when `input_cache` is enabled unless `CRATONVM_GPU_JIT_ARRAY_WRITERS=allow` is set).
   - Mixed workloads sharing hot CPU loops with offloadable calls in the same caller method lose JIT optimization for the CPU portions.

## Future Work

- Region-granular pinning and physical backing accounting are proposed in [Region-Granular Pinning, Physical VRAM Accounting, and Per-VM GPU GC Isolation](../../feature-designs/gpu-per-vm-gc-isolation.md).
- Stream-ordered retirement and callsite-adaptive JIT gating are proposed in [Stream-Ordered Retirement, Safepoint Pressure GC, Adaptive JIT Gating, and Fault-Injecting Driver Test Double](../../feature-designs/gpu-async-retirement-and-fault-mocking.md).
