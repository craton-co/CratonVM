# The compilation broker

`jit/src/tiered.rs`, section **`CompilationBroker`**.

The C2 review's P1 lane asks for compilation *policy* to be separable from the
compiler: *"Policy can be tested without emitting code and backend failures
return structured reasons."* Backlog step 22 adds: *"Tier decisions are
deterministic, observable, and independently testable."*

This document is the contract for the object that answers that: what its queue
promises, why a method cannot be compiled twice into two code buffers, what
happens to a request whose method is redefined or unloaded underneath it, which
counters exist and what a stuck queue looks like in them — and what is still
unvalidated.

---

## 1. What existed before this pass, and what did not

Read before believing. The state of the tree when this lane started:

| Claim | Reality |
|---|---|
| "Compilation is triggered from the interpreter" | True. `TieredCompilationManager::on_method_invocation_observed` (tiered.rs) is called from the interpreter's invocation hook, plus `on_backedge` / `request_osr` from the back-edge hook. |
| "Compilation runs on a background thread" | True, and exactly one. `TieredCompilationManager::start_background_compiler` claims a single-worker slot with a `compare_exchange` on `core.active`, and `ensure_background_compiler` wraps it in a `std::sync::Once`. `compiler_loop` is the only consumer. |
| "Requests queue" | True. `CompilationQueue` — three `VecDeque` bands (High/Normal/Low), drained highest-band-first, FIFO within a band. |
| "The queue is bounded" | **False.** `CompilationQueue` has no capacity and no shed rule. |
| "Requests are deduplicated" | **Partly.** One coarse `MethodState::queued_for_compilation` bool per method, consulted by `should_compile`/`on_backedge`/`request_osr`/`request_c2_upgrade`. `TieredCompilationManager::enqueue_compilation` — the public entry point the deopt path uses from `vm/src/jit/helpers.rs` — sets the flag and pushes **unconditionally**, with no dedup at all. |
| "A queued request can be invalidated" | **Only for class unload.** `TieredCompilationManager::invalidate_class` retains-out the method states and all three queue bands for a class name. There is **no** equivalent for class *redefinition*, and nothing at all for a request already handed to the worker. |
| "There is a broker" | Half. A `CompilationBroker` had landed in commit `ab31a74b2` ("wip(c2-review): partial wave-5 work recovered from interrupted agents") with its **tests missing**: its own doc comments referenced `broker_tests`, `threshold_policy_reproduces_todays_tier_decisions`, `osr_backedge_trigger_matches_the_live_manager` and `decisions_are_deterministic_given_identical_inputs`, none of which existed anywhere in the tree. This document did not exist either, though three comments pointed at it. |

Two live-manager defects fall out of the table. Both are **reported, not fixed**
— see §7.

---

## 2. Wiring status

The broker is **not wired in**. `TieredCompilationManager`, `CompilerCore` and
`jit::try_compile` are untouched by this lane; every path a running VM takes is
byte-for-byte what it was. That is why nothing here sits behind a flag: there is
no behaviour to gate. The flag becomes necessary at the moment the broker
replaces the live manager's request handling — see §7 for the exact declaration
that will be needed, since a flag must be declared in
`types/src/flag_groups.rs` or it is invisible to tests.

---

## 3. Queue policy

`BoundedCompileQueue`.

**Priority.** Three bands, same as the live `CompilationQueue`:

| Band | What lands there |
|---|---|
| `High` | every OSR request, and every method-entry C2 request |
| `Normal` | method-entry C1 / C1WithProfiling |
| `Low` | C1→C2 supersedes, and anything else |

Drain order is highest band first, FIFO within a band. `CompilationPriority::rank`
is a method rather than a `PartialOrd` derive on purpose: the variants are
declared `High, Normal, Low`, so a derive would rank them backwards and silently
invert the shed rule.

**Bound.** `DEFAULT_COMPILE_QUEUE_CAPACITY = 512`, far above any steady-state
depth observed on the app gauntlet, so it behaves as unbounded until something
goes wrong. A capacity of `0` clamps to `1`: a queue that can hold nothing sheds
every request and reports a permanently starved compiler, which is a worse
failure than a deep queue.

**Shed rule.** When the queue is full, an incoming request may displace exactly
one request from the **lowest occupied band that ranks strictly below it**, and
the victim is that band's **newest** entry. Two deliberate choices:

- *Strictly below.* An equal-priority request is **rejected**, not swapped in.
  Otherwise a burst of same-priority requests churns the queue forever without
  compiling anything.
- *Newest victim.* Entries that have already waited keep their place, so a
  saturated queue still makes progress in arrival order instead of starving its
  oldest work.

**Nothing is shed silently.** `EnqueueOutcome::AcceptedAfterShedding` hands the
victim back, and `CompilationBroker::apply` uses it to drop the victim's
outstanding record and release its in-flight slot. A queue that dropped work
without saying so would leave `queued_for_compilation` set forever and the
method would never be recommended again — a method that stays interpreted
forever and is nearly impossible to diagnose is exactly the failure mode this
lane is required to avoid.

**A rejection is a decline, not a loss.** `CompilationBroker::gate` refuses the
request before it reaches the queue and returns
`DeclineReason::QueueFull { depth, capacity }`, which is `is_transient() == true`:
the method keeps its counters and acquires no in-flight flag, so the next
invocation re-asks. The re-ask is the requeue.

---

## 4. Idempotence: one method, one code buffer

The requirement: *the same method must not be compiled twice concurrently into
two code buffers.* This VM has already shipped two use-after-frees in code
reclamation; a duplicate install is the same family.

**Request identity.** `RequestKey { method, tier, osr_bci }`. The tier is part of
identity because a C1 request in flight must not suppress the C2 upgrade that
follows it. The OSR bci is part of identity for the reason `ArtifactId`
documents: an OSR body and a method-entry body live in **different caches** and
are never interchangeable (`CompilerCore::complete_task` deliberately does not
advance `current_tier` on an OSR publish).

**The authority.** `CompilationBroker::outstanding: HashMap<RequestKey, _>` holds
every request that has been admitted and not yet completed — queued *or*
dispatched and still compiling. `apply` refuses to enqueue a `RequestKey` that is
already in it, counts `deduplicated`, and returns
`Decline(DeclineReason::AlreadyQueued)`.

**Why the flag is not enough.** `MethodState::queued_for_compilation` is a single
per-method bool, and *five* paths write it: admission, completion, a shed, a
deopt, a class purge. If any of them clears it while the request is still live,
the coarse tier gate re-opens and a second request for the same artifact is
admitted. In the broker that cannot happen, because admission is refused on the
request's own identity rather than on a flag whose clearing is somebody else's
job. `an_identical_request_is_deduplicated_not_queued_twice` and
`a_dispatched_request_still_blocks_a_duplicate_until_it_completes` clear the flag
by hand and assert the duplicate is still refused.

**One writer for the flag.** `CompilationBroker::release_slot` is the only place
that clears `queued_for_compilation`, and it clears it only once nothing is
outstanding for the method. The coarse flag is therefore a cache of the
authoritative table, never a second source of truth.

**Concurrency.** The broker is `&mut self`-driven with no locks, no threads and
no backend reference. Concurrency is the *integration's* problem: the wiring
wraps a broker in the mutex `CompilerCore` already holds. Keeping the policy
object lock-free is what makes its decisions reproducible, and it is why
`BrokerCounters` is plain `u64`s rather than atomics — a test reads exact
numbers instead of a racy sample.

---

## 5. Invalidation: redefinition, unload, deoptimization

> A stale request that installs code for a redefined method is a wrong-code bug.

The live manager has no answer for this. The broker's answer is a **class
epoch**.

**The mechanism.** `CompilationBroker::class_epoch(class)` starts at `0` and is
incremented by every `InvalidationEvent::ClassRedefined` and
`ClassUnloaded`. Each admitted request records the epoch that was current when
it was admitted. A request whose recorded epoch no longer matches its class's
epoch was formed against bytecode that is no longer loaded.

An O(1) counter bump rather than a queue scan per event: instrumentation
retransforms classes in the thousands, and a redefine that walks a 512-entry
queue three times over is a real cost for an event that is already expensive.

**Three enforcement points.**

1. **At dispatch** — `next_request` never hands a stale request to a backend. It
   drops it, counts `dropped_stale`, releases the in-flight slot, and *continues
   to the next band entry* so a burst of stale requests cannot starve a fresh
   one behind them.
2. **At completion** — `complete` discards the entire verdict and returns
   `CompletionOutcome::DiscardedStale { admitted_epoch, current_epoch }`. Not
   just the published body: a stale `Bailed` must not spend the *new* bytecode's
   retry budget, and a stale `Declined` must not mark the *new* bytecode
   permanently ineligible. Nothing in `MethodState` is touched.
3. **At unload** — `purge_class` additionally drains the class's queued requests
   eagerly (the tracked state is being discarded anyway, so there is nothing
   left for the lazy check to protect) and drops their outstanding records. It
   deliberately **keeps** the record of an already-dispatched request, because
   that record carries the pre-unload epoch that makes step 2 refuse the body
   the backend is about to hand back.

**`CompletionOutcome` is `#[must_use]`.** The whole point of `DiscardedStale` is
to tell the caller *not to publish*. If the backend already published, **the
caller must retire the body** — the broker never sees the code buffer and cannot
do it. A caller that drops this value on the floor installs code compiled from
replaced bytecode.

**Fail closed, never silently lost.** Every discard clears the method's
in-flight slot and leaves its invocation counters intact, so the next invocation
re-admits it and it is recompiled against the new bytecode. The drop is
explicit, counted, and re-derivable from the counters. The one thing that never
happens is a request evaporating with the method's in-flight flag left set.

**Deoptimization.** `CompilationBroker::on_deoptimization` drops the method's
queued requests explicitly. This is a **deliberate delta** from
`TieredCompilationManager::on_deoptimization`, which clears the flag and leaves
the task in the queue — see §7.1. An already-*dispatched* request is left alone:
it is mid-compile, and the broker cannot cancel a backend it does not own.

**Body invalidation** is separate and already existed: `Dependency` /
`InvalidationEvent::breaks` retire compiled bodies whose assumptions a class
redefinition, unload, or newly-loaded override falsifies, transitively through
`Dependency::DirectCall`, in a hash-order-independent sequence
(`sort_artifact_ids`). Only a retired **method-entry** body cascades — a direct
call is baked against an entry point, so retiring an OSR artifact leaves every
caller valid. Retiring returns the body's bytes to the code cache, which makes
invalidation a real input to admission: a method refused for
`DeclineReason::CodeCacheFull` becomes admissible again once something is
retired.

---

## 6. Observability

`BrokerCounters`, reported as `(name, u64)` pairs by `to_pairs()` and as one
JSON object by `to_json()` — the same shape `metrics::MetricsSummary` uses for
`by_outcome` / `by_path` / `bailout_categories`, and the names are an external
contract for the same reason `Bailout::category`'s are.

| Counter | Meaning |
|---|---|
| `admitted` | requests admitted and queued ("queued") |
| `osr_admitted` | of those, OSR artifacts |
| `declined` | requests declined, any reason |
| `deduplicated` | refused because an identical `RequestKey` was already outstanding |
| `shed` | shed to make room for a higher-priority request |
| `dispatched` | drained by `next_request` and handed to a backend ("started") |
| `completed` | reaching `complete`, whatever the verdict |
| `installed` / `bailed` / `declined_permanently` | completion verdicts |
| `completions_discarded_stale` | verdicts thrown away as stale (§5) |
| `unsolicited_completions` | completions for a request never dispatched — always a wiring bug |
| `retired` | bodies retired by invalidation |
| `requests_dropped` | queued requests dropped explicitly (unload, deopt) |
| `dropped_stale` | queued requests discarded at dispatch on an epoch mismatch |
| `dropped_orphaned` | queued requests discarded at dispatch because the broker no longer tracked them |
| `decline_reasons` | declines by `DeclineReason::category` (`BTreeMap`, so row order is stable) |
| `bailout_categories` | bailouts by `Bailout::category` — the same keys `bailout::bailout_counts` reports |

Two derived views: `dropped_total()` sums the four drop counters, and
`outstanding_requests()` lists what is currently between admission and
completion, sorted.

**Triage rules.**

```
outstanding_len() == admitted - completed - dropped_total()
admitted - dispatched                       = queue depth + drops
dispatched - completed                      = compiles in flight
```

- `admitted` climbing while `dispatched` is flat → the worker is not draining:
  wedged, never started, or parked on a lost wakeup.
- `dispatched - completed` stuck at a non-zero constant → a compile is hung.
  `outstanding_requests()` names it.
- `deduplicated` non-zero → some path cleared a method's in-flight flag while
  its request was still live. Harmless here (the duplicate was refused) but it
  means the coarse flag and the request table disagreed, which is worth a look.
- `unsolicited_completions` non-zero → a task reached a backend without going
  through `next_request`. Always a wiring bug.
- `decline_reasons` dominated by `already_queued` with a flat `dispatched` →
  the queue is stuck, not the policy.
- `decline_reasons` dominated by `retries_exhausted` or
  `permanently_ineligible` → not a queue problem. The distinction matters: a
  policy decline is stuck *by design* and a compile failure is a bug, and
  conflating them is what made an earlier "1531 of 1642 hot methods never
  compile" reading unactionable (see `MethodState::ineligible`).

---

## 7. Findings and required cross-file changes

### 7.1 `TieredCompilationManager::on_deoptimization` clears the flag but leaves the task queued

`on_deoptimization` sets `queued_for_compilation = false`, `queued_tier = None`
and `current_tier = Interpreter`, but does **not** remove the method's task from
`core.queue`. The next invocation past the threshold therefore passes the
`queued_for_compilation` gate and enqueues a *second* task for the same method.
`on_c2_bailout` has the identical shape.

Not a use-after-free **today**, because exactly one worker exists
(`start_background_compiler`'s `compare_exchange` on `core.active`, plus the
`Once` in `ensure_background_compiler`), so the two tasks are compiled
serially and the second publish replaces the first. It is wasted compile time
and a latent duplicate-install the moment a second worker is introduced.

The broker's `on_deoptimization` drops the queued request instead
(`a_deoptimization_drops_the_queued_request_instead_of_leaving_a_duplicate`).
Porting that to the live manager is a behaviour change and is **not** done here.

### 7.2 `TieredCompilationManager::enqueue_compilation` has no deduplication

The public enqueue path — used by the deopt `RecompileAndReinterpret` handler in
`vm/src/jit/helpers.rs` — sets `queued_for_compilation = true` and pushes
unconditionally. Combined with 7.1 this is the concrete way one method acquires
two queued tasks.

### 7.3 There is no invalidation path for a *redefined* class's queued requests

`TieredCompilationManager::invalidate_class` covers unload only. A class
redefinition leaves every queued and in-flight request for that class intact,
and there is no epoch, generation, or any other stamp by which a completion
could notice that the bytecode it compiled has been replaced. The broker's
class-epoch scheme (§5) is the answer; the live manager still has none.

### 7.4 Cross-file change that could not be made from this lane

Wiring the broker in will need one **declared flag** so the new request handling
can ship default-off. An undeclared flag is invisible to tests here — declared
flags are served from a latched snapshot, so `set_var` on an undeclared name
does not reach `runtime_var`. Declaring one means editing
`types/src/flag_groups.rs`, which this lane does not own. The row, matching the
`CRATONVM_TIER_*` entries already in `INVENTORY`:

```rust
E { group: Group::JIT, token: "compilation-broker", on_key: Some("CRATONVM_TIER_BROKER"), off_key: None, off_word: None },
```

`types/tests/flag_surface.rs` and `tools/flag-census/check-surface.sh` assert the
inventory stays complete and agrees with the reference docs, so the flag
reference doc needs the same row.

---

## 8. What remains unvalidated

- **Nothing here has run against a VM.** Every test drives the broker with
  synthetic counters; the oracle tests
  (`threshold_policy_reproduces_todays_tier_decisions`,
  `osr_backedge_trigger_matches_the_live_manager`) prove the extracted policy
  agrees with the live one on a 420-case table and on the back-edge trigger, but
  agreement on a table is not agreement on a workload.
- **The class epoch has no producer yet.** Nothing calls
  `invalidate(ClassRedefined(..))` in production, because the broker is unwired.
  When it is wired, the redefinition and unload sites in the VM must call it, and
  the epoch is only as good as the completeness of those call sites. A missed
  call site is a silent wrong-code hole, not a missed optimization.
- **`OverrideLoaded` does not bump the epoch**, on the argument that a compile
  that starts after the subclass loads will see it. That argument depends on the
  backend reading the class hierarchy at compile time rather than at request
  time, and has not been checked against the inliner.
- **The shed rule has never been exercised under load.** 512 is above every
  depth observed so far, so shedding has only ever happened in a unit test with
  a capacity of 1 or 2. Whether the "newest victim, strictly lower band" rule is
  the right one under real saturation is unmeasured.
- **`DEFAULT_COMPILE_QUEUE_CAPACITY` is a guess.** It was chosen to behave as
  unbounded, not from a measured distribution of queue depth.
- **Cancelling an in-flight compile is not modelled.** A dispatched request runs
  to completion and is refused at the end. That is correct but wasteful, and the
  broker has no way to signal a backend to stop.
- **The `unsolicited_completions` path still applies the state update.** That is
  the fail-open choice — better than leaving a method marked in-flight forever —
  but it means a mis-wired caller can move a method's tier without the broker
  ever having admitted the request.
