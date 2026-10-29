# Compilation Scheduling and Broker Architecture

## Status

**Consolidated into `TieredCompilationManager` and `CompilerCore` (Shipped, default on).**

CratonVM historically explored a standalone `CompilationBroker` abstraction in `jit/src/tiered.rs` to handle request queueing, invalidation epochs, and deduplication. To prevent maintaining dual schedulers with diverging in-flight state tables, all broker responsibilities were consolidated directly into `CompilerCore` and `TieredCompilationManager`. There is no separate broker object or thread.

---

## 1. Why Broker Functionality Was Consolidated

The early standalone `CompilationBroker` operated beside `TieredCompilationManager` as an independent policy queue. However, maintaining two separate queuing abstractions created significant architectural drawbacks:

1. **Dual State Hazard**: In-flight compile slots, tier selection thresholds, and backedge triggers had to be coordinated across two layers. Inconsistencies could lead to race conditions or duplicate compilations.
2. **Disconnected Invalidation**: Class redefinition, unloading, and deoptimization events were split between the VM-side notification hooks and the broker's epoch trackers.
3. **Streamlined Pipeline**: By embedding request admission, invalidation epochs, and concurrency bounds directly into `CompilerCore` and `TieredCompilationManager`, request lifecycle decisions happen at a single, unified gate.

---

## 2. Compilation Management Properties in Production

All compilation scheduling and request filtering mechanisms operate within the live compiler infrastructure:

| Capability | Production Location | Implementation Details |
|---|---|---|
| **Per-Method Deduplication** | `CompilerCore::admit` | Provides two distinct in-flight compilation slots per method: one for standard method-entry compilation, and one for on-stack replacement (OSR) at any bytecode index (`bci`). A request arriving while its slot is occupied is rejected and accounted for as `queue_deduplicated` (or `CompilationStats::osr_refused_in_flight`). Standard and OSR compilations target separate artifact caches, preventing redundant work across parallel compilation worker threads. |
| **Deoptimization Request Eviction** | `TieredCompilationManager::on_deoptimization` | Drops any queued compile request for a method when a counted trap deoptimization fires (`queue_dropped_deoptimized`). Similarly, `on_c2_bailout` clears pending optimizing requests. Any compile already executing on a worker thread finishes cleanly and frees its slot on completion. |
| **Class Unloading Invalidation** | `invalidate_class(ClassId, name)` | Loader-aware invalidation purges method compile states, queued requests, and OSR denials associated with an unloaded class. Open branch-profile windows are closed and process-wide compilation verdicts are cleared (`queue_dropped_class_invalidated`). |
| **Class Redefinition & Epochs** | Install-epoch dispatch gate & `on_class_redefined` | An install-epoch mechanism rejects stale compilations that were queued before a class redefinition occurred. `on_class_redefined(name)` clears historical assumptions from prior bytecode, resetting `ineligible` status, failure retries (`tier_fail_count`), C2 bailouts, trap counts, and OSR denials. |
| **Code Cache Admission Control** | `CompilerCore::admit` | Checks `jit_code_cache_at_capacity`. If the code cache is saturated, the method is marked `deferred_at_cap` and recorded under `cap_refusals()`. Critically, this deferral does not burn the method's retry allowance (`tier_fail_count`). The method remains deferred until code cache reclamation moves the settled generation. |
| **Telemetry and Counters** | `metrics::SCHEDULING_EVENTS` | Dedicated scheduling metrics provide visibility into queue dynamics: `dropped_requests()`, `deduplicated_requests()`, `worker_panics()`, and `branch_window_balance()`. |

---

## 3. Queue Architecture and Invariant Guarantees

- **Unbounded Queues with Per-Method Caps**: Because admission is gated by per-method slots in `CompilerCore::admit`, the queue depth is naturally bounded by the number of distinct Java methods requesting compilation. Explicit queue shedding is unnecessary.
- **In-Flight Cancellation Semantics**: To avoid complex thread interruption within native compilation workers, an in-flight compilation runs to completion. However, if the epoch moved or the class was invalidated while the compile was active, `JitCache::put` rejects the resulting code artifact (`Completion::epoch_moved`). The compile time is discarded safely without penalizing the method's failure count.
