# The compilation broker — retired

`jit/src/tiered.rs` used to carry a `CompilationBroker`: a policy object with a
bounded, deduplicating request queue, a per-class invalidation epoch and a set
of counters, built to be testable without a backend. It was **deleted on
2026-09-12** without ever having been wired in. This page records why, and where
each of its properties lives now, so that nobody rebuilds it by accident.

---

## 1. Why it was deleted

The broker was constructed in `JitRealm`, and the VM fed it class invalidations
at the four events that replace a class's bytecode or remove the class (JVMTI
redefine, class unload, `defineClass` over a loaded name, JNI `DefineClass`).
Nothing ever called `on_invocation`, `next_request` or `complete`. So it admitted
no request, compiled nothing and protected nothing, and its per-class epoch map
grew by one entry for every redefined class name for the life of the process.

Wiring it in would have put a second scheduler beside
`TieredCompilationManager`, with two in-flight-slot tables that had to agree at
every door. The properties the broker was built to demonstrate were moved into
the live manager instead (§2).

The oracle tests that compared broker and manager went with it:
`threshold_policy_reproduces_todays_tier_decisions` and
`osr_backedge_trigger_matches_the_live_manager`. The second one's subject,
`TieredCompilationManager::on_backedge`, had no production caller and was
deleted in the same change.

---

## 2. Where each property lives now

| Broker property | Live equivalent |
|---|---|
| One request per method at a time (`RequestKey` dedup) | `CompilerCore::admit`: one in-flight slot per method, held from admission until completion, stale drop, deopt drop, unload or shutdown. A second request — identical or not — is refused and counted as `queue_deduplicated`. `enqueue_compilation`, the VM's deopt re-queue door, goes through it. This matters more now that each compile lane has its own workers: a duplicate would be the same method compiled concurrently into two code buffers. |
| A deoptimization drops the queued request | `TieredCompilationManager::on_deoptimization` drops the method's queued request when the deopt is a counted trap (`queue_dropped_deoptimized`); `on_c2_bailout` does the same. A request that is already compiling is left alone and releases its slot on completion. |
| Unload purges requests and state | `invalidate_class(ClassId, name)`, which is loader-aware: states, queued requests and OSR denials of that class are removed, open branch-profile windows are closed, and the process-wide compile verdicts about it are forgotten (`queue_dropped_class_invalidated`). |
| A redefinition refuses stale requests (class epoch) | The install-epoch dispatch gate drops requests queued before the redefinition (`docs/jit/broker-install-epoch.md`), and `on_class_redefined(name)` resets what was learned from the old bytecode: `ineligible`, `tier_fail_count`, `c2_bailout`, trap counts, OSR denials and the bail list. |
| Bounded queue with a shed rule | Not ported. The live queues are unbounded, which is safe because admission is per method: queue depth cannot exceed the number of distinct methods waiting for a compile. |
| Code-cache pressure as an admission input | Not ported. `JitCache::put` still refuses at the cap, and the manager sees that as a failed compile, bounded by `MAX_TIER_FAIL_RETRIES`. |
| Structured decline reasons | Not ported. `MethodState::ineligible` (a verdict that re-asking cannot change) versus `tier_fail_count` (a compile that ran and failed), plus the scheduling counters, carry the distinctions the stats dump needs. |
| `BrokerCounters` | `crate::metrics::SCHEDULING_EVENTS`, plus the per-manager mirrors `dropped_requests()`, `deduplicated_requests()`, `worker_panics()` and `branch_window_balance()`. |

---

## 3. No flag

There is nothing to enable. The `CRATONVM_TIER_BROKER` row this page once
proposed was never declared, and
`types/src/flag_groups.rs::knobs_without_a_consumer_are_not_declared` keeps it
that way.

---

## 4. What remains unvalidated

- **The shed rule was never needed in anger.** It was only exercised by unit
  tests with a capacity of 1 or 2. If a workload ever shows the compile queues
  growing without bound, per-method admission has failed somewhere first; start
  there.
- **`OverrideLoaded` still bumps no epoch.** A subclass that overrides a
  devirtualized method is handled by body invalidation, not by the scheduler.
  The argument that a compile starting after the load sees the new hierarchy
  has not been checked against the inliner.
- **An in-flight compile cannot be cancelled.** A dispatched compile runs to
  completion and `JitCache::put` refuses a stale body; the compile time is
  wasted, and `inflight_epoch_moved` counts it.
