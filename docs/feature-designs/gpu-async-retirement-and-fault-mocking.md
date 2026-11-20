# Stream-Ordered Retirement, Safepoint Pressure GC, Adaptive JIT Gating, and Fault-Injecting Driver Test Double

**Status:** Partially implemented (Safepoint pressure GC rendezvous and `cuda-bridge` fault-injecting test double implemented; stream-ordered reaper retirement and adaptive JIT gating remain proposals)  
**Related limitations:** [Heap-Wide Relocation Veto, Logical VRAM Accounting, and Whole-Method JIT Gate](../known-issues/gpu/gpu-global-coord-and-heap-veto-scope.md)

## Motivation

Four operational areas govern GPU reclaim predictability and test coverage: mutator-side VRAM pressure GC rendezvous; centralizing stream-ordered host cleanups on the async completion reaper; avoiding whole-method JIT exclusion in `offload_jit_gate`; and deterministic host CI coverage of driver FFI error/rollback paths.

## Detailed Design

### Reaper-Polled Stream-Ordered Host Retirement Queue (Proposal)

- Introduce a bounded `StreamRetirementQueue` drained by the existing async completion reaper alongside submission completion polling.
- Associate completed CUDA/HIP events with deferred staging buffer returns (`PinnedHostPool`), span unpin decrements, and `future_vram` / `SubAllocator` reclamation so host callbacks (`CRATONVM_GPU_HOST_CALLBACK=1`) remain strictly an opt-in comparison toggle and all normal reclamation runs lock-free outside driver callback threads.

### Safepoint-Aware Mutator Pressure GC Rendezvous (Implemented)

- `trigger_vm_pressure_gc` (`vm/src/runtime/offload.rs`) parks foreign-attached callers (`with_foreign_idle_safepoint` in `vm/src/native/jni/part_01.rs`) or claimed mutators (`with_mutator_pressure_gc_safepoint` in `vm/src/vm/vm_exec/part_03.rs` using `ThreadRegistry::is_tid_in_blocked_region`) cleanly at a safepoint and joins the `GpuPressureGc` helper (`done_rx.recv()`) before returning.
- Guarantees `force_gc_for_vm` and `reclaim_all_completed_pools()` finish before the calling thread retries VRAM allocation.

### Callsite-Adaptive Offload JIT Gating (Proposal)

- Allow caller methods containing offloadable `invokestatic` sites to compile to native JIT code by emitting an explicit runtime stub call to `offload::try_dispatch` at eligible callsites instead of rejecting the entire caller in `offload_jit_gate::caller_blocks_jit`.
- Retain per-callsite feedback counters so callsites that repeatedly fall back to CPU (below `MIN_WORK_GIVEUP`) inline on CPU cleanly on subsequent Tier-2 recompilation while active GPU callsites keep offloading without penalizing surrounding CPU loops.

### Fault-Injecting Driver FFI Test Double (Implemented)

- Added internal test-only fault injection in `cuda-bridge/src/fault_mock.rs` (`FaultMockVmmDriver`, `FaultMockSlabBackend`, `FaultMockHostPinOps`) backed by test seams on `VmmPool::driver` (`cuda-bridge/src/vmm.rs`) and `HostPinTestOps` (`cuda-bridge/src/pinned_pool/mod.rs`).
- Exercises `CUDA_ERROR_OUT_OF_MEMORY`, map/access errors, partial multi-chunk rollback, and `cuMemHostRegister_v2` failure degradation/eviction deterministically in default host CI builds without GPU hardware.

## Verification Status

- Verified safepoint-aware pressure GC rendezvous in `vm/src/runtime/offload/gpu_gc_roadmap_unit_tests.rs` (`trigger_vm_pressure_gc_waits_synchronously_when_foreign_attached`, `trigger_vm_pressure_gc_waits_synchronously_for_main_mutator`).
- Verified fault-injecting driver rollback and leak-zero invariants in `cuda-bridge/src/fault_mock.rs` (5 unit tests in both stub and `--features cuda` builds).
- Pending proposal checks: stream-ordered retirement queue and mixed CPU+GPU callsite-adaptive JIT microbenchmark.
