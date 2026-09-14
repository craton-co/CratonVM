# The install epoch: what invalidates a queued or running compile

What invalidates a compilation request that is already queued or already
running, where the epoch is stamped and read, what a dropped request looks like
in the metrics, and how per-class invalidation works now that the per-class
epoch is gone.

**Companion documents.** `docs/jit/code-cache-lifetime.md` owns the install
epoch and the per-cache flush barrier, and is authoritative on what happens at
*publication*. `docs/jit/compilation-broker.md` records the retired
`CompilationBroker` and where its properties moved. This document owns the seam
between them: the scheduler in `jit/src/tiered.rs`.

---

## 1. One epoch, and what replaced the second

| | **Install epoch** |
|---|---|
| Where | `jit/src/lib.rs`, `JIT_INSTALL_EPOCH` |
| Shape | One process-wide `AtomicU64`, starting at 1 so `0` is never a legitimate stamp |
| Question | *"Has anything invalidated compiled code since this request was formed?"* |
| Read via | `crate::jit_install_epoch() -> u64` |
| Advanced by | `bump_jit_install_epoch()`, called from `bump_redefine_epoch()` and `JitCache::clear_all` |
| Producers | JVMTI redefine (`vm/src/vm/vm_exec.rs`) and every `JitCache::clear_all`, including `jit_invalidate_adapter` on the layout-upgrade path |
| Gates | The scheduler's dispatch drop (§2), OSR-denial expiry, compile-state-dependent verdict expiry, and `JitCache::publication_epoch_is_current` at publication |

There used to be a second, per-class epoch on `CompilationBroker`. It never had
a consumer — the broker was never wired in — and it was deleted with the broker
on 2026-09-12. The question it was meant to answer, *"is this class's bytecode
still the bytecode this request was formed against?"*, is now answered without
an epoch:

- **Queued requests** are dropped by the install-epoch gate. The gate is global,
  so one redefinition drops every queued request in the process. That is blunt
  but safe: a dropped request is re-admitted at the method's next stride.
- **Verdicts learned from the old bytecode** are reset by
  `TieredCompilationManager::on_class_redefined(name)`, which the VM calls at
  the four sites that replace a class's bytecode — JVMTI redefine, both
  `defineClass` paths in `vm_exec.rs`, and JNI `DefineClass`. It resets
  `ineligible`, `tier_fail_count`, `c2_bailout`, trap counts and OSR denials,
  and forgets the process-wide compile verdicts (`forget_jit_verdicts_for_class`).
- **Unload** is `invalidate_class(ClassId, name)`, which is loader-aware.
- **OSR denials** are stamped with the install epoch and expire when it moves.

The layout-upgrade sites (`upgrade_synthetic_class`,
`recompute_subclass_layouts`) still reach the VM only through
`JitInvalidateHook`, a `fn(u32)` that carries no class name, so they reset no
verdicts. They do move the install epoch through `clear_all`, which drops the
queued requests and expires the OSR denials. Resolving the name inside
`jit_invalidate_adapter` would self-deadlock on the class-manager write lock its
caller holds; widen the hook to carry the name if this is ever needed.

---

## 2. What is stamped where

All of this is `jit/src/tiered.rs`.

| Point | What happens |
|---|---|
| **Enqueue** | `CompilationQueue::enqueue` wraps the task in a private `QueuedRequest { install_epoch, task }`, stamped from `CompilerCore::current_install_epoch()`. The stamp is on the queue entry, not on `CompilationTask`, because the VM constructs that type too, and a field a caller has to fill in will eventually be filled in wrong. Every door funnels through `CompilerCore::admit` → `CompilerCore::enqueue`, so no request is unstamped. |
| **Dispatch** | Each compile lane's workers call `CompilationQueue::dequeue_fresh(current, &mut stale)`, which returns the highest-priority request whose stamp is still current and pushes every older one onto `stale`. It returns `None` only when the lane is empty, so a run of stale requests can neither starve a fresh one behind them nor make a worker read an occupied queue as empty. The manual drains (`next_fresh_task`, `dequeue_compilation`) apply the same rule across both lanes. |
| **In flight** | A worker records the dispatch epoch (`TieredCompilationManager::inflight_install_epoch()`; `0` when every worker is idle, and a sample rather than a per-compile record when several are busy) and compares it again on return. |
| **Publication** | Not this file's job: `JitCache::put`/`put_osr` compare `CompiledMethod::install_epoch` against the owning cache's `flush_barrier` and refuse a stale body. |

### The window each one closes

```
enqueue ──────────── dispatch ──────────── backend runs ──────────── install
   │                    │                                              │
   └── queue stamp ─────┘                                              │
       closed by dequeue_fresh                                         │
                        └──── in-flight window ────────────────────────┘
                              NOT closable here — the artifact already
                              exists. Recorded as `inflight_epoch_moved`;
                              actually refused by `flush_barrier`.
```

The dispatch gate is waste avoidance, not the correctness gate. A compile that
starts after a redefinition reads the current bytecode from the class manager
and is stamped with the current epoch, which the barrier then accepts.
Correctness lives at publication.

### Fail-closed: what a drop does and does not do

`CompilerCore::retire_stale` is the only exit for a dropped request. It:

- releases the method's in-flight slot — but only if that request still holds
  it, so an old request cannot free a slot that was re-granted to a newer one;
- closes the branch-profile window the request opened, if it opened one;
- increments the counter for the specific reason (§3);
- traces the drop under `CRATONVM_DBG_TIER_ENQUEUE`, next to the enqueue line.

It deliberately does **not** touch `current_tier`, `tier_fail_count` or
`ineligible`. Routing the drop through a failed completion would spend one of
`MAX_TIER_FAIL_RETRIES`, so three redefinitions during warmup would leave a hot
method permanently uncompilable with no diagnostic. A stale request is not a
compile failure and not a policy decline: nothing was compiled, and nothing was
decided.

### Lock order

`retire_stale` takes `core.methods`. The established order in `tiered.rs` is
**`methods` → lane queue** (`should_compile_inner` holds `methods` across
`CompilerCore::admit`, which takes the queue). A worker discovers stale
requests while holding its lane's queue, so it collects them into a `Vec`,
drops the queue guard, and only then retires them. Taking `methods` under a
queue would invert the order and deadlock against any thread on the
interpreter's invocation hook. This is why `StaleRequest` exists as a type.

---

## 3. The counters

`jit/src/metrics.rs`, section **Scheduling counters**, built as a copy of
`jit/src/bailout.rs`'s table: fixed `&'static str` names, a parallel array of
relaxed counters, read back as `Vec<(&'static str, u64)>` in a stable order
*including the zeroes*. The names are an external contract, and the counters
are not gated on `metrics::enabled()`.

| Name | Meaning |
|---|---|
| `queue_dropped_stale_install_epoch` | A queued request discarded at dispatch: the install epoch moved after it was queued. Non-zero is *expected* under an instrumenting agent; the method re-admits at its next stride. |
| `queue_dropped_class_invalidated` | A queued request discarded by `invalidate_class` on class unload. **Final**: the class is gone, so nothing re-admits it. |
| `inflight_epoch_moved` | The install epoch moved *while* a compile was running. The artifact is refused by `JitCache::put` (counted by `crate::stale_install_epoch_refusals()`); this counts the window the dispatch gate cannot close. |
| `queue_shutdown_abandoned` | Requests still queued when the workers were shut down. Correct at teardown; non-zero mid-run means the workers were stopped with work outstanding. |
| `queue_deduplicated` | A request refused because its method already held the in-flight slot. Not a drop — nothing was queued. Expected near zero: the policy doors check the slot themselves. |
| `queue_dropped_deoptimized` | A queued request dropped because its method took a counted trap or bailed out of C2. |
| `worker_panic` | A compile callback panicked and was contained. The method is recorded as ineligible and the worker keeps running. Any non-zero value is a compiler bug; the first occurrence also logs a warning. |

Read them with `metrics::scheduling_counts()` (all rows),
`metrics::scheduling_count(name)` (one row), or
`metrics::scheduling_dropped_total()`, which sums the stale, invalidated and
shutdown drops only. Per-manager mirrors, for a caller that must not read a
process-wide table: `TieredCompilationManager::dropped_requests()`,
`::deduplicated_requests()`, `::worker_panics()`, `::install_epoch()` and
`::inflight_install_epoch()`.

**How to read them together.** `dropped_requests` climbing while
`completed_compilations` stays flat is a queue being invalidated faster than it
is drained. A method that never appears in `metrics::compilation_reports()` and
never appears in `bailout_counts()` is not necessarily cold — check the
`queue_dropped_*` rows first, because a dropped request produces no row
anywhere else.

---

## 4. Tests

All in `jit/src/tiered.rs`, all deterministic, no sleeps and no wall-clock
bounds. `TieredCompilationManager::with_install_epoch_source(policy,
Some(Arc<AtomicU64>))` is the seam: a manager reads "the current install epoch"
from an injected counter, so a "redefinition" is one `store`.

| Test | Property |
|---|---|
| `a_request_queued_at_the_current_epoch_dispatches` | The gate does not refuse the ordinary case |
| `a_request_queued_before_an_epoch_bump_is_dropped_before_it_is_compiled` | The gate fires, and the drop is counted |
| `a_dropped_request_releases_the_slot_without_spending_a_retry` | The fail-closed contract |
| `stale_requests_do_not_starve_a_fresh_one_behind_them` | `dequeue_fresh` loops past stale entries |
| `priority_order_survives_the_epoch_gate` | The gate filters; it does not reorder |
| `an_empty_queue_is_not_a_drop` | No phantom counts |
| `every_enqueue_door_stamps_the_epoch` | The policy and OSR doors are gated like a direct enqueue |
| `drops_reach_the_process_wide_scheduling_counters` | The drop is visible through the metrics idiom |
| `invalidate_class_counts_the_requests_it_discards` | Unload drops are counted and gate no other class |
| `shutdown_counts_the_requests_it_abandons` | Teardown abandonment is drained and counted |
| `an_osr_denial_expires_when_the_install_epoch_moves` | OSR denials expire with the epoch and are forgotten on redefinition |
| `every_branch_window_is_closed_exactly_once_whichever_way_its_request_leaves` | Stale drop, OSR completion and invalidation all leave the window census balanced |

---

## 5. What remains

- **The dispatch gate is global, so it over-drops.** One redefinition drops
  every queued request in the process. Bounded and self-healing, but measurable
  queue churn under a retransform burst.
- **`inflight_epoch_moved` is observation only.** Nothing cancels an in-flight
  compile; the artifact is built and then refused at publication.
- **`background_compile_task` still declines a redefined class.**
  `named_class_was_redefined` returns `declined(0)`, which marks the method
  ineligible. `on_class_redefined` clears that on the next redefinition, but
  between redefinitions a retransformed class stays out of the background
  compiler. That is a VM policy decision, not recorded as a defect here.
- **The layout-upgrade hooks carry no class name** (§1), so they expire queued
  requests and OSR denials but reset no per-method verdicts.
- **`TieredCompilationManager::dequeue_compilation` is stamp-blind.** It has no
  caller outside tests; a production caller that compiles the result must use
  `next_fresh_task`.
- **Nothing here has been measured against a workload that retransforms
  classes.** `queue_dropped_stale_install_epoch` staying at 0 in a normal run is
  the expected reading and is not evidence the gate works.
