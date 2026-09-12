# Two epochs: the install epoch and the class epoch

What invalidates a compilation request that is already queued or already
running, which of the two counters answers which question, where each one is
stamped and read, which VM call sites must produce the one that has no
producer, and what a dropped request looks like in the metrics.

**Companion documents.** `docs/jit/compilation-broker.md` owns the
`CompilationBroker`'s policy contract and is authoritative on the class epoch's
*semantics*. `docs/jit/code-cache-lifetime.md` owns the install epoch and the
per-cache flush barrier, and is authoritative on what happens at
*publication*. This document owns the seam between them: the scheduler.

**Why it needed its own document.** Two lanes arrived at the same hole from
opposite sides. The broker lane found that the live `TieredCompilationManager`
stamps nothing, so a completion cannot notice its bytecode was replaced — and
that the broker's own class epoch has no producer anywhere. The code-cache lane
closed the *install* side (`JitCache::flush_barrier` refuses a body compiled
before the flush) and handed over one line: *"the broker should stamp the
install epoch on a queued `CompilationTask` and drop stale ones before
compiling."* Both halves are about the same request, and confusing the two
epochs would make one of them silently unreachable — so they are written down
together, once.

---

## 1. The two epochs

| | **Install epoch** | **Class epoch** |
|---|---|---|
| Where | `jit/src/lib.rs`, `JIT_INSTALL_EPOCH` (lib.rs:8845) | `jit/src/tiered.rs`, `CompilationBroker::class_epochs` |
| Shape | One process-wide `AtomicU64`, starts at 1 so `0` is never a legitimate stamp | `HashMap<String, u64>` per broker, keyed by class name, starts at 0 |
| Question | *"Has anything invalidated compiled code since this request was formed?"* | *"Is **this class's** bytecode still the bytecode this request was formed against?"* |
| Read via | `crate::jit_install_epoch() -> u64` (lib.rs:8850) | `CompilationBroker::class_epoch(&str) -> u64` (tiered.rs) |
| Advanced by | `bump_jit_install_epoch()` (lib.rs:8856), called from `bump_redefine_epoch()` (lib.rs:8828) and `JitCache::clear_all` (lib.rs:9519) | `CompilationBroker::invalidate(ClassRedefined \| ClassUnloaded)` and `purge_class` |
| Producers today | **Real.** `vm/src/vm/vm_exec.rs:4736` (JVMTI redefine) and every `JitCache::clear_all` — including `jit_invalidate_adapter` (`vm/src/vm/vm_init.rs:3583`), which the layout-upgrade path fires | **None.** `CompilationBroker` has zero references outside `jit/src/tiered.rs`; nothing in the tree calls `invalidate` or `purge_class` |
| Gate it drives | `JitCache::publication_epoch_is_current` (lib.rs:9083) refuses a body stamped below the owning cache's `flush_barrier` | `next_request` / `complete` refuse a request whose class moved |

The accessor is spelled **`jit_install_epoch`**, at the crate root — the
handover's `jit_install_epoch()` guess happened to be right, but it is verified
here at `jit/src/lib.rs:8850`, along with the barrier
(`JitCache::flush_barrier`, lib.rs:8445, raised only by `clear_all` at
lib.rs:9519) and the witness (`open_compile_epoch_witness`, lib.rs:8893, opened
by `try_compile` at lib.rs:11687).

### Which one the live scheduler uses, and why

**The live `TieredCompilationManager` stamps the INSTALL epoch.** Not the class
epoch. The reasons are not stylistic:

1. **The class epoch has no producer and no plumbing.** `CompilerCore` holds no
   broker, no `SharedVm` and no class-manager handle; the broker is not
   constructed anywhere in the VM. A dispatch gate written against the class
   epoch would compare 0 to 0 forever — a gate that reads as protection and
   provides none, which is strictly worse than no gate.
2. **The install epoch already has real producers** (table above), so the gate
   is live the moment it is written.
3. **It answers the question the handover actually asked.** The wasted work the
   code-cache lane wanted stopped is "a compile whose body the flush barrier
   will refuse", and the flush barrier is driven by the install epoch. Gating
   dispatch on a *different* counter would not have stopped it.

**The class epoch is not made unreachable by this.** It answers a strictly
finer question, and the two are not substitutes:

- The install epoch is global. One redefinition anywhere invalidates every
  queued request in the process. That is safe (a dropped request is re-admitted
  on the method's next invocation) but blunt: under an instrumenting agent that
  retransforms in bursts, unrelated hot methods lose their place in the queue.
- The class epoch is per class. `com/example/Instrumented` being retransformed
  does not touch `java/util/HashMap`'s queued C2 request.

So the intended end state is: the class epoch replaces the install epoch as the
*dispatch* gate once it has producers, and the install epoch stays what it is —
the publication gate, which must remain global because a cache flush really does
invalidate everything in that cache. Section 4 is the producer list that has to
land first.

---

## 2. What is stamped where (live path, as of this change)

All of this is `jit/src/tiered.rs`.

| Point | What happens |
|---|---|
| **Enqueue** | `CompilationQueue::enqueue` wraps the task in a private `QueuedRequest { install_epoch, task }`, stamped from `CompilerCore::current_install_epoch()`. The stamp is on the **queue entry**, not on `CompilationTask`, because that type is also constructed by the VM (`vm/src/jit/helpers.rs:11211`) — a field a caller has to fill in is a field that will eventually be filled in wrong, or filled in with a *later* epoch than the request really carries, which reads as fresh. Every door into the queue funnels through `CompilerCore::enqueue`, so no caller can produce an unstamped request. |
| **Dispatch** | `CompilationQueue::dequeue_fresh(current, &mut stale)` returns the highest-priority request whose stamp is still `current`, pushing every older one onto `stale` on the way past. It returns `None` only when the queue is empty, so a run of stale requests can neither starve a fresh one behind them nor make the worker read an occupied queue as empty. |
| **In flight** | `compiler_loop` records the dispatch epoch in `CompilerCore::inflight_epoch` (readable as `TieredCompilationManager::inflight_install_epoch()`; `0` = idle) before entering the backend, and compares it again on return. |
| **Publication** | Unchanged, and not this file's job: `JitCache::put`/`put_osr` compare `CompiledMethod::install_epoch` against the owning cache's `flush_barrier` and refuse a stale body (lib.rs:9083). |

### The window each one closes

```
enqueue ──────────── dispatch ──────────── backend runs ──────────── install
   │                    │                                              │
   └── queue stamp ─────┘                                              │
       closed by dequeue_fresh (this change)                           │
                        └──── in-flight window ────────────────────────┘
                              NOT closable here — the artifact already
                              exists. Recorded as `inflight_epoch_moved`;
                              actually refused by `flush_barrier`.
```

The dispatch gate is a pure waste-avoidance measure, and it is honest about
that. It is **not** the correctness gate — a compile that starts after a
redefinition re-reads the class from the class manager and produces a body for
the *current* bytecode, stamped with the current epoch by
`open_compile_epoch_witness`, which the barrier then accepts. Correctness lives
at publication. What the dispatch gate buys is: a compile that was going to be
thrown away is not run, and — more valuably — the method is not pushed through
`background_compile_task`'s `named_class_was_redefined` check
(`vm/src/runtime/interpreter/invoke.rs`), which returns `declined(0)` and
therefore sets `MethodState::ineligible` **permanently**. Today a class
redefinition permanently disqualifies every method of that class that happened
to be queued at the time; dropping the request first avoids burning that
eligibility for the wrong reason. See §6.

### Fail-closed: what a drop does and does not do

`CompilerCore::retire_stale` is the only exit for a dropped request, and it:

- clears `MethodState::queued_for_compilation` / `queued_tier`, so the very
  next invocation re-admits the method against the bytecode that is loaded now;
- increments the counter for the specific reason (§3);
- traces it under `CRATONVM_DBG_TIER_ENQUEUE`, next to the enqueue line.

It deliberately does **not** touch `current_tier`, `tier_fail_count` or
`ineligible`. Routing the drop through `complete_task(success = false)` — the
obvious shortcut — would spend one of `MAX_TIER_FAIL_RETRIES`, so three
redefinitions during warmup would leave a hot method permanently un-compilable
with no diagnostic whatsoever. A stale request is not a compile failure and not
a policy decline: nothing was compiled, and nothing was decided.

### Lock order

`retire_stale` takes `core.methods`. The established order in `tiered.rs` is
**`methods` → `queue`** (`should_compile_inner` holds `methods` across
`CompilerCore::enqueue`, which takes `queue`). The worker discovers stale
requests while holding `queue`, so it collects them into a `Vec`, drops the
queue guard, and only then retires them. Taking `methods` under `queue` would
invert the order and deadlock against any thread on the interpreter's
invocation hook. This is the reason `StaleRequest` exists as a type at all.

---

## 3. The counters

`jit/src/metrics.rs`, section **Scheduling counters**. Deliberately built as a
copy of `jit/src/bailout.rs`'s table — fixed `&'static str` names, a parallel
array of relaxed counters, read back as `Vec<(&'static str, u64)>` in a stable
order *including the zeroes*. Two properties are load-bearing:

- **The names are an external contract.** A test or a dashboard keys on them.
- **They are not gated on `metrics::enabled()`.** A dropped request is a
  correctness-adjacent event, not a measurement, and it has to be visible in a
  default run where `CRATONVM_JIT_METRICS` is unset. This is the same choice
  `bailout.rs` makes for the same reason.

| Name | Meaning |
|---|---|
| `queue_dropped_stale_install_epoch` | Queued request discarded at dispatch: the install epoch moved after it was queued. Non-zero is *expected* under an instrumenting agent and is not by itself a fault — the method re-admits on its next invocation. |
| `queue_dropped_class_invalidated` | Queued request discarded by `TieredCompilationManager::invalidate_class`, which the VM's class-unload path calls (`vm/src/memory/gc.rs:167`). Counted separately because it is **final**: the class is gone, so there is no next invocation to re-admit anything. |
| `inflight_epoch_moved` | The install epoch moved *while* a compile was running. The artifact is not lost here — `JitCache::put` refuses it, counted by `crate::stale_install_epoch_refusals()`. This counts the window the dispatch gate structurally cannot close. |
| `queue_shutdown_abandoned` | Requests still queued when the background compiler was shut down. Correct at teardown; a non-zero value mid-run means the worker was stopped with work outstanding, which is not. |

Read them with `metrics::scheduling_counts()` (all rows),
`metrics::scheduling_count(name)` (one row), or
`metrics::scheduling_dropped_total()` — which excludes `inflight_epoch_moved`,
because that compile *ran*, and whether its artifact survived is the code
cache's question and not the scheduler's. `MetricsSummary::scheduling` carries
the same table into `summary()` and its JSON.

Per-manager mirrors, for a caller that must not read a process-wide table
shared with every other manager: `TieredCompilationManager::dropped_requests()`,
`::install_epoch()`, `::inflight_install_epoch()`.

**How to read them together.** `dropped_requests` climbing while
`completed_compilations` stays flat is a queue being invalidated faster than it
is drained. A method that never appears in `metrics::compilation_reports()` and
never appears in `bailout_counts()` is not necessarily cold — check
`queue_dropped_*` first, because a dropped request produces no row anywhere
else in the summary. That is exactly the failure mode these counters exist to
make findable.

---

## 4. The class epoch's missing producers

This is the larger half, and it is a specification rather than a change,
because every site is under `vm/` or `classloading/`.

**An epoch nobody bumps is worse than no epoch**, because the code reads as
protected. `CompilationBroker::invalidate`'s doc comment already says a
redefine "also bumps the class's epoch" — true of the broker, false of the
process, since nothing calls it.

### 4.0 Prerequisite: the broker has no instance

`CompilationBroker` is referenced nowhere outside `jit/src/tiered.rs`. Before
any line below can be applied, the broker needs a home. The natural one is
`JitRealm` (`vm/src/vm/realms/jit_realm.rs`), next to `tiered_manager`:

```rust
    /// Compilation policy — admission, tier selection, dependency
    /// invalidation. See `docs/jit/compilation-broker.md`.
    pub compilation_broker: parking_lot::Mutex<cratonvm_jit::tiered::CompilationBroker>,
```

initialized alongside `tiered_manager` with
`parking_lot::Mutex::new(cratonvm_jit::tiered::CompilationBroker::with_default_policy())`.
Every insertion below is written against that field. `Mutex` because the broker
is `&mut self`-driven by design and carries no interior locking — that is
deliberate (see `compilation-broker.md` §"The broker"), and the integration is
where concurrency is supposed to be added.

### 4.1 Sites where a class's bytecode is replaced or its layout changes

Derived by reading, not by pattern-matching a name. The complete set of events
that falsify a queued request's assumptions:

| # | Event | Where the VM already reacts | Has the class **name** in hand? |
|---|---|---|---|
| 1 | JVMTI `RedefineClasses` (JEP 109) | `vm/src/vm/vm_exec.rs:4736-4743` | Yes (`name`, bound at the top of `redefine_class`) |
| 2 | Class unload | `vm/src/memory/gc.rs:167` | Yes (`class.name`) |
| 3 | `defineClass` over an already-loaded name | `vm/src/vm/vm_exec.rs:6645`, `:6719`, `:6824` | Yes (`name`) |
| 4 | JNI `DefineClass` | `vm/src/native/jni.rs:4021` | Yes (`n`) |
| 5 | Synthetic-stub layout upgrade | `classloading/src/class_manager.rs:8816` → `jit_invalidate_adapter` | **No** — see §4.3 |
| 6 | Subclass field renumbering after (5) | `classloading/src/class_manager.rs:8950` → same adapter | **No** — see §4.3 |
| 7 | Subclass loaded that overrides a devirtualized method | `vm/src/vm/vm_init.rs:5486`, `:5488` | Yes — but see §4.4 |

### 4.2 The exact insertions (sites 1-4)

> 2026-09-12: `SharedVm::invalidate_jit_for_class` was deleted with the
> `InvalidationManager` it queried. The anchor lines quoted below name it;
> the broker insertions stand on their own.

Each is one line, in the existing invalidation block, with the name already in
scope. Add `use cratonvm_jit::tiered::InvalidationEvent;` to each file's imports
(or spell it fully, as written below).

**1. JVMTI redefine.** `vm/src/vm/vm_exec.rs`, immediately after line 4743
(`let _ = self.shared.invalidate_jit_for_class(&name);`), inside
`redefine_class`:

```rust
        let _ = self
            .shared
            .jit
            .compilation_broker
            .lock()
            .invalidate(&cratonvm_jit::tiered::InvalidationEvent::ClassRedefined(
                name.clone(),
            ));
```

*Why here and not next to `bump_redefine_epoch()` at 4736:* the epoch bump and
the cache clear at 4736-4737 are the *install*-epoch producers and must stay
adjacent (the comment there explains the ordering). The class-epoch bump belongs
with the class-scoped invalidation at 4743, which is where `name` is used.

**2. Class unload.** `vm/src/memory/gc.rs`, immediately after the
`tiered_manager.invalidate_class(class.name.as_ref())` call that ends at line
170, inside the `for class in &unloaded` loop:

```rust
        let _ = shared
            .jit
            .compilation_broker
            .lock()
            .purge_class(class.name.as_ref());
```

`purge_class`, not `invalidate` — an unload must also discard the broker's
tracked `MethodState` and drain the class's queued requests eagerly, which is
exactly what `TieredCompilationManager::invalidate_class` does one line above.
Using `invalidate(ClassUnloaded(..))` here would bump the epoch but leave the
per-method state behind forever.

**3. `defineClass` over an existing name.** `vm/src/vm/vm_exec.rs`, after each
of lines 6645, 6719 and 6824 (`let cha_evicted = self.shared.invalidate_jit_for_class(name);`):

```rust
                let _ = self
                    .shared
                    .jit
                    .compilation_broker
                    .lock()
                    .invalidate(&cratonvm_jit::tiered::InvalidationEvent::ClassRedefined(
                        name.to_string(),
                    ));
```

(Indentation differs per site — 6645 and 6719 are inside a `match` arm, 6824 is
at function level. The statement is identical.)

**4. JNI `DefineClass`.** `vm/src/native/jni.rs`, after line 4021
(`let _ = shared.invalidate_jit_for_class(n);`), inside the
`if let Some(n) = class_name.as_deref()` block:

```rust
            let _ = shared
                .jit
                .compilation_broker
                .lock()
                .invalidate(&cratonvm_jit::tiered::InvalidationEvent::ClassRedefined(
                    n.to_string(),
                ));
```

### 4.3 Sites 5 and 6 cannot be done this way — the hook carries no name

The layout-upgrade paths (`upgrade_synthetic_class`,
`recompute_subclass_layouts`) reach the VM only through
`fire_jit_invalidate_hook`, whose type is `fn(u32)`
(`classloading/src/class_manager.rs:1202`) — a `ClassId`, no name. The obvious
fix, resolving the name inside `jit_invalidate_adapter`
(`vm/src/vm/vm_init.rs:3583`), **self-deadlocks**:

- `fire_jit_invalidate_hook` is called from inside `redefine_class`
  (class_manager.rs:7060), `upgrade_synthetic_class` (:8816) and
  `recompute_subclass_layouts` (:8950), all `&mut self` methods on
  `ClassManager`;
- so the caller holds `shared.classes.class_manager_write()` — see
  `vm/src/vm/vm_exec.rs:4716`, which takes the write guard before calling
  `redefine_class`;
- `shared.classes.class_manager.read()` inside the adapter would therefore
  block on a write lock held by this same thread. `parking_lot::RwLock` is not
  reentrant.

The adapter is safe today only because it touches nothing but
`shared.jit.jit_cache`. This is a live landmine for anyone who adds a
"resolve the class name" line to it.

**The fix, which is a `classloading/` change:** widen the hook to carry the
name, which the class manager already has under `&mut self` and can read with
no additional lock.

`classloading/src/class_manager.rs:1202`, replace:

```rust
pub type JitInvalidateHook = fn(u32);
```

with:

```rust
pub type JitInvalidateHook = fn(u32, &str);
```

`classloading/src/class_manager.rs:1273`, change `fire_jit_invalidate_hook` to
take and forward `class_name: &str`, and give each of the three call sites
(:7060, :8816, :8950) the name it already has in scope from
`self.class_store.get(id)`. Then `jit_invalidate_adapter`
(`vm/src/vm/vm_init.rs:3583`) gains, inside its existing `for shared in
live_hook_vms()` loop:

```rust
        let _ = shared
            .jit
            .compilation_broker
            .lock()
            .invalidate(&cratonvm_jit::tiered::InvalidationEvent::ClassRedefined(
                class_name.to_string(),
            ));
```

Until that widening lands, sites 5 and 6 have **no** class-epoch producer. They
do bump the *install* epoch (via `clear_all` in the adapter), so the live
manager's dispatch gate already covers them — which is a second, concrete
reason the live gate had to be built on the install epoch.

### 4.4 Site 7 is deliberately excluded

`OverrideLoaded` does not bump the class epoch, by the broker's own design
(`compilation-broker.md` §8): a compile that *starts* after the subclass loads
will see it. That argument is still unchecked against the inliner and is
recorded there, not re-litigated here. If it turns out to be wrong, the
insertion point is `vm/src/vm/vm_init.rs:5486`, with
`InvalidationEvent::OverrideLoaded { class_name, method_name }`.

### 4.5 Completeness argument

The set above is closed over "a request's assumptions can be falsified" as
follows. A `CompilationTask` names a method by `(class_name, method_name,
descriptor)` and carries nothing else; the backend re-resolves everything from
the class manager at compile time. So the only things that can invalidate a
queued request are: the class's bytecode changing (1, 3, 4), the class's field
layout changing (5, 6), the class ceasing to exist (2), or a hierarchy
assumption the *backend* made being broken (7 — which is a property of the
compile, not of the request, hence its exclusion). Every path in the tree that
does one of those things fires either `fire_jit_invalidate_hook` or one of the
name-bearing VM invalidation blocks listed, which is why the table is derived
from those two sets rather than from a grep for "redefine".

---

## 5. Tests

All in-file, all deterministic, no sleeps and no wall-clock bounds.

`TieredCompilationManager::with_install_epoch_source(policy, Some(Arc<AtomicU64>))`
is the seam: a manager reads "the current install epoch" from an injected
counter instead of the process-wide one, so a "redefinition" is one `store` and
the scheduling rule is exercised with no thread, no backend and no clock.
Driving the real `JIT_INSTALL_EPOCH` would be non-deterministic in both
directions — every `JitCache::clear_all` anywhere in the test binary advances
it, and there is no way to hold it still.

`jit/src/tiered.rs`:

| Test | Property |
|---|---|
| `a_request_queued_at_the_current_epoch_dispatches` | The gate does not refuse the ordinary case |
| `a_request_queued_before_an_epoch_bump_is_dropped_before_it_is_compiled` | The gate fires, and the drop is counted |
| `a_dropped_request_releases_the_slot_without_spending_a_retry` | The fail-closed contract: slot cleared, `tier_fail_count`/`ineligible`/`current_tier` untouched, method re-admits and then dispatches |
| `stale_requests_do_not_starve_a_fresh_one_behind_them` | `dequeue_fresh` loops instead of stopping at the first stale entry |
| `priority_order_survives_the_epoch_gate` | The gate filters; it does not reorder |
| `an_empty_queue_is_not_a_drop` | No phantom counts |
| `every_enqueue_door_stamps_the_epoch` | The policy path and the OSR path are gated identically to a direct `enqueue_compilation` — the property that makes queue-side stamping better than task-side |
| `drops_reach_the_process_wide_scheduling_counters` | The drop is visible through the metrics idiom, not only on the manager |
| `invalidate_class_counts_the_requests_it_discards` | Unload drops are counted, and one class's invalidation does not gate another's |
| `shutdown_counts_the_requests_it_abandons` | Teardown abandonment is drained and counted rather than implied |

`jit/src/metrics.rs`:
`scheduling_counts_report_every_event_in_a_fixed_order`,
`scheduling_events_are_distinct_names`,
`recording_a_scheduling_event_is_visible_without_metrics_enabled` (the
metrics-flag independence, which is the property that separates these counters
from the report ring), `summary_carries_the_scheduling_table_and_json`.

The tests that assert exact global counts take `metrics::METRICS_TEST_LOCK` and
reset the table, because the scheduling counters are process-wide and shared
with every sibling test in the binary.

---

## 6. What remains

- **The class epoch still has no producer.** §4 is a specification, not a
  change. Until it lands, the per-class question is unanswerable in production
  and the global install epoch is doing the work of both.
- **Sites 5 and 6 need a `classloading/` signature change** (§4.3) before they
  can produce a class epoch at all. The self-deadlock described there is the
  trap that makes the "obvious" fix wrong.
- **`background_compile_task` makes a redefined class permanently ineligible.**
  `vm/src/runtime/interpreter/invoke.rs` returns `declined(0)` when
  `named_class_was_redefined` is true, and `CompileOutcome::declined` sets
  `MethodState::ineligible`, which is by contract never retried. Since
  `class_redefine_generation` is monotone, *every* method of a class that was
  ever redefined is permanently barred from the JIT. The dispatch gate now
  intercepts the queued ones first, so it no longer happens for the wrong
  reason — but the underlying policy is still "one retransformation, no JIT for
  that class, forever", and that is almost certainly not intended under an
  instrumenting agent. Not changed here (VM file, and it is a policy decision,
  not a bug in the mechanical sense).
- **The dispatch gate is global, so it over-drops.** One redefinition
  invalidates every queued request in the process. Bounded and self-healing —
  each method re-admits on its next invocation, and the interpreter consults
  the manager on a 64-invocation stride — but under a retransform burst this is
  measurable queue churn. The per-class epoch is the fix; see the first bullet.
- **`inflight_epoch_moved` is observation only.** Nothing cancels an in-flight
  compile; the artifact is built and then refused at publication. The broker
  documents the same gap (`compilation-broker.md` §8, "Cancelling an in-flight
  compile is not modelled").
- **`TieredCompilationManager::dequeue_compilation` is still stamp-blind.** It
  is a manual/diagnostic drain with no callers outside tests; if it ever
  acquires a production caller that *compiles* the result, it must move to
  `next_fresh_task`.
- **Nothing here has been measured against a workload.** The gate has never
  fired outside a unit test, because nothing in the app gauntlet retransforms
  classes. `queue_dropped_stale_install_epoch` staying at 0 in a normal run is
  the expected reading and is not evidence the gate works.
