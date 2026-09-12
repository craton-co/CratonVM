# JIT code-cache lifecycle

Installation and invalidation protocol, epoch retirement, and the counters that
make both auditable.

**Why this exists.** The C2 review
has two adjacent P1 lanes:

> **Add code-cache lifecycle metrics and reclamation** — Installed/reclaimed
> bytes, fragmentation, sweeps, failed allocations, recompilations.

> **Add code installation and invalidation protocol** — Allocate under
> code-cache synchronization, apply relocations, flush instruction cache,
> transition RW→RX, publish metadata atomically, and retire code only after no
> thread can execute it. Acceptance: concurrent compile/install/invalidate/
> unload stress shows no execution of partial or reclaimed code.

Before `vm/src/jit/code_cache_lifecycle.rs`, the VM could not answer "how many
bytes of compiled code has this process installed, and how many has it given
back?" at all. Reclamation happened — `jit/src/lib.rs::defer_jit_owner` — but
silently, so *"the code cache is growing"* was indistinguishable from *"the code
cache cannot reclaim"*.

Everything below is produced by `cratonvm_vm::jit::code_cache_lifecycle`.
Nothing in this document requires reading a log line.

---

## The retirement protocol

The protocol lives in `jit/src/lib.rs`. `vm/src/jit/code_cache_lifecycle.rs`
reports on it: `pending_retirements()`, `sweep_if_quiescent()` and
`code_cache_lifecycle_raw()` read or drive the real queue. (An earlier
revision described a vm-side model queue with its own sweep; that model's
counters were never fed by the real installs and retirements, and the two
were joined on 2026-09-12.)

| Phase | Who | What |
|---|---|---|
| 1. Unpublish | the withdrawing site, **before** `defer_jit_owner` | Remove the body from the `JitCache` shard maps (`retire_withdrawn_body`), from every inline-cache way (`CompiledMethod::retired` is set first, then the slot word is cleared under the slot's writer lock), from external dispatch caches (the cache generation), and from future baked direct calls (`prepare_for_publication` refuses a retired callee). After this, no *new* activation can begin. |
| 2. Grace period | `defer_jit_owner` / `drain_deferred_jit_owners` | Queue the owner stamped with a retirement generation, and release it once every thread is shown to be outside it (below). |
| 3. Reclaim | the same, **outside** the queue lock | Drop the owner; the last `Arc<CompiledMethod>` unmaps the body in `ExecutableBuffer::drop`, which counts it as reclaimed. |

Phase 1 is the caller's obligation: the queue can prove *"no thread is inside
this body"*, but only the caller can guarantee *"and no thread can enter it
again"*.

### The quiescence signals

Two, in order of cost.

**The fast path: `ACTIVE_JIT_EXECUTIONS` reads zero.** Every compiled-code
activation is bracketed by `jit_execution_enter` / `jit_execution_leave`
(`JitExecutionToken` records the stripe it incremented, so a decrement during
thread-local teardown lands on the same stripe). When the striped sum reads
zero with the queue lock held, everything queued is released. The argument is
the next section.

**Per-thread evidence, when some thread is inside compiled code.** A thread
parked in compiled code (blocked in a monitor, `Object.wait`, I/O, a sleep)
kept the sum above zero indefinitely and stalled reclamation for the whole
process. Each thread that enters compiled code therefore registers a
`JitThreadQuiescence` record:

* `depth` — its in-JIT nesting;
* `quiescent_gen` — the retirement generation it observed the last time its
  depth reached zero;
* a **blocked-stack summary** — captured when the thread enters a blocking
  transition (`gc_barrier::enter_blocked` and `mark_blocked_region_enter`, via
  `conservative_roots::note_blocking_transition_enter` →
  `jit_thread_blocked_enter`): the set of code buffers named by return
  addresses in its stack band from the current SP to its outermost compiled
  entry. It is invalidated if the thread re-enters compiled code inside the
  window, and it gives up (and holds everything) past a 1 MiB band.

`drain_deferred_jit_owners` takes the queue lock, collects every thread's
evidence (`collect_thread_quiescence_evidence`), and releases each queued
owner that every thread is shown to be outside: depth zero, or back at depth
zero since the owner's stamp, or parked with a valid summary that does not
name the owner's buffer. Everything else stays queued. Executions without a
thread record (`UNTRACKED_JIT_EXECUTIONS`) hold the whole queue.

Drains run from `defer_jit_owner`, from `jit_execution_leave` (on a leave that
sees the sum at zero, and every `DRAIN_PUMP_INTERVAL` = 32 outermost leaves
while the queue is non-empty), from `jit_code_cache_at_capacity` before it
refuses a compile, and on demand from `reclaim_retired_jit_code()`. The same
observations advance the graced generation that lets an inline-cache way be
reused.

A thread parked at a **safepoint** inside compiled code does not pass through
the blocking transitions above, so it has no summary and holds the queue until
it leaves. That is the fail-safe direction.

### Why one striped-sum zero read is a real grace period

A striped counter's `is_zero()` walks 64 stripes one at a time, so a `true`
answer does **not** mean every thread was out of JIT *simultaneously*. For a
*count* that race matters. For a *grace period* it does not:

1. A thread's stripe index is assigned once and never changes
   (`types/src/striped_counter.rs`), so a stripe read at instant `t` returning
   `0` is a **simultaneous** observation that every thread mapped to that stripe
   had in-JIT depth zero at `t`.
2. A full walk returning `true` therefore yields, for *every* thread in the
   process, some instant at which that thread was outside compiled code.
3. `drain_deferred_jit_owners` takes the queue lock **first** and performs the
   walk **while holding it**. Every queued body was therefore unpublished
   strictly before the walk began, so each thread's witness instant is after
   every queued body's unpublication. (`defer_jit_owner`'s unlocked fast path is
   sound for the same reason: its caller unpublished the body before calling.)
4. A thread outside compiled code at its witness instant can only re-enter
   through a dispatch surface, and every such surface was cleared in phase 1.
   So it cannot be executing any of them.

**The lock ordering is the correctness argument, not a performance choice.**
Observing quiescence and *then* taking the queue would let a body unpublished in
between be freed on the strength of a witness that predates its unpublication.

### The OS-frozen peer

`docs/threading/objectref-concurrency-contract.md` and §6.2/§10.4 of the
transition-states document describe the hard case: a peer can be **OS-frozen
inside compiled code** by the cross-thread STW takeover
(`vm/src/jit/xt_root_scan.rs`). It cannot run any cooperative handshake, so no
"ask every thread to acknowledge" protocol can complete while it is frozen.

It does not need to. A frozen peer entered compiled code through
`jit_execution_enter` and has not left, so its depth stays elevated for as long
as it is frozen. An OS freeze does not pass through a blocking transition, so
it has no stack summary either: it provides no evidence, and every owner it
might be inside stays queued. The same holds for the **helper window** (a
compiled frame that called a Rust helper): the depth stays elevated across the
helper even though the peer's `Rip` is outside every JIT range. That is the
over-approximating polarity, which is the correct one for "may I free this?",
exactly as it is for "may I relocate?".

§10.4's warning applies unchanged: if the in-JIT depth is ever narrowed to
`Rip`-based classification, this protocol loses the helper window and starts
freeing bodies whose frames are still live.

### Fail-safe

If quiescence cannot be established for an owner, it is **retained** and the
drain is counted. A leak is a bug; freeing live code is a crash.

The counters exist so the leak is *visible*
(`cratonvm_jit::jit_code_reclamation_stats()`): a permanently wedged thread
shows up as `queued_bytes` that never falls while `drains_deferred` climbs.
`drains_quiescent` and `drains_by_thread_evidence` say which signal released
what. `cratonvm_jit::jit_retirement_queue_len()` is the queue length.

Independently of the queue, `ExecutableBuffer::drop` records every unmap
(`record_code_free`), and `recent_code_free_covering` lets the crash handler
say whether a faulting address was inside a buffer released recently.
`published_code_free_audit().1` counts published bodies released without the
queue's authorisation and must stay zero.

---

## W^X

`WxState` has four variants — `Unmapped`, `Writable`, `Executable`, `Poisoned` —
and **no variant that is both writable and executable**. W^X is a property of
the type, not of a runtime check someone can forget to call.

| Protocol | Events | States traversed |
|---|---|---|
| `INSTALL_WX_PROTOCOL` | `Allocate`, `Publish` | `Unmapped → Writable → Executable` |
| `RETIRE_WX_PROTOCOL` | `Release` | `Executable → Unmapped` |

This matches `jit/src/platform.rs`: RW `mmap`/`VirtualAlloc`, then
`mprotect`/`VirtualProtect` to RX, with the aarch64 I-cache flush issued
*before* the flip while the page is still RW and no fetch can race it.

Reclamation touches page permissions not at all — it drops the owning `Arc`,
whose `ExecutableBuffer::drop` unmaps (or, under `CRATONVM_JIT_POISON_FREE=1`,
`mprotect`s to `PROT_NONE`). In particular the protocol never patches a retired
body into a trap sequence, which would require an RX→RW→RX round trip:
`retire_protocol_never_reopens_a_page` asserts the retirement event sequence
never enters `Writable`.

## The installation order

`install_step` restates the review's acceptance criterion as something a test
can check. `install_sequence_is_legal` requires every step exactly once, in
order:

| # | Step | Why it is where it is |
|---|---|---|
| 0 | `ALLOCATE_UNDER_CACHE_LOCK` | "Allocate under code-cache synchronization." |
| 1 | `APPLY_RELOCATIONS` | Into the RW buffer, before it becomes executable. |
| 2 | `FLUSH_ICACHE` | **Before** the RW→RX flip: while the page is still writable there is no chance of an instruction fetch racing the flush. |
| 3 | `TRANSITION_RW_TO_RX` | |
| 4 | `PUBLISH_METADATA` | Oop maps, deopt metadata, the code range. |
| 5 | `PUBLISH_ENTRY` | **Last.** From this instant a peer can enter the body and immediately be stack-walked, so its metadata must already be there. |

---

## Counter inventory

Monotone unless marked *gauge*.

**Which of these the process report fills.** `code_cache_lifecycle_raw()`
copies these from `cratonvm_jit::jit_code_reclamation_stats()`:

| Report field | Real source |
|---|---|
| `installs`, `installed_bytes`, `installed_code_bytes` | `installed_*`, counted when a body is published (`ExecutableBuffer::mark_published`) |
| `retirements`, `retired_bytes` | `withdrawn_bodies` / `withdrawn_bytes` (`retire_withdrawn_body`: superseded, invalidated or flushed) |
| `reclaimed_bodies`, `reclaimed_bytes`, `reclaimed_code_bytes` | `reclaimed_*`, counted when a published buffer is unmapped |
| `sweeps`, `sweeps_deferred` | `drains`, `drains_deferred` |
| `deferred_bodies`, `deferred_bytes` | `queued_owners`, `queued_bytes` (gauges) |
| `capacity_bytes` | `jit_code_cache_cap_bytes()` |

The other fields below (`recompilations`, `methods_compiled`,
`retirements_by_reason`, `deferrals`, `max_deferral_sweeps`, the allocation
failures and free extents) are kept by the `CodeCacheLifecycle` model type for
its own tests and read zero in the process report until something real feeds
them.

### Installation

| Counter | Meaning |
|---|---|
| `installs` | Bodies installed. |
| `installed_bytes` | Reserved (mapped) bytes installed. |
| `installed_code_bytes` | Machine-code bytes installed. |
| `recompilations` | Installs that replaced an earlier version of the same method. |
| `methods_compiled` | Distinct methods compiled at least once. |
| `max_versions_for_one_method` | *gauge* — highest version reached by any one method. |
| `peak_live_bytes` | *gauge* — high-water mark of mapped bytes. |

### Retirement and reclamation

| Counter | Meaning |
|---|---|
| `retirements`, `retired_bytes` | Bodies / bytes handed to `retire`. |
| `retirements_by_reason[6]` | `superseded-by-recompilation`, `assumption-invalidated`, `deoptimized`, `evicted-cache-pressure`, `class-unloaded`, `shutdown`. |
| `reclaimed_bodies`, `reclaimed_bytes`, `reclaimed_code_bytes` | Actually unmapped. |
| `sweeps` | Sweeps run. |
| `sweeps_deferred` | Sweeps that reclaimed nothing because quiescence failed. **The fail-safe's visible half.** |
| `deferrals` | Body×sweep deferral events. |
| `deferred_bodies`, `deferred_bytes` | *gauges* — awaiting reclamation right now. |
| `max_deferral_sweeps` | *gauge* — most sweeps survived by any one queued body. |

### Allocation and occupancy

| Counter | Meaning |
|---|---|
| `failed_allocations`, `failed_allocation_bytes` | Requests the cache refused. |
| `failed_allocations_by_reason[4]` | `os-refused-mapping`, `code-cache-cap-exceeded`, `no-free-extent-large-enough`, `size-estimate-overrun`. |
| `capacity_bytes` | *gauge* — configured cap; `0` = uncapped. |
| `free_extents`, `free_bytes`, `largest_free_extent` | *gauges* — the arena's free-space shape. |

### Derived

`live_bodies = installs - reclaimed_bodies`,
`live_bytes = installed_bytes - reclaimed_bytes`,
`live_code_bytes`, and `reachable_bytes = live_bytes - deferred_bytes`.

The identity `installed - reclaimed == live` is what
`installed_minus_reclaimed_is_live` and the concurrency test pin. `reachable`
is the number that matters operationally: a retired body is still *mapped*, it
is just no longer in service.

Ratios: `occupancy`, `internal_fragmentation`, `external_fragmentation`,
`deferred_fraction`, `reclaim_completion`, `sweep_success_rate`,
`recompilation_ratio`, `versions_per_method`, `allocation_failure_rate`,
`mean_body_bytes`. Every one is `0.0` when its denominator is zero — a report
taken before the first compilation reads as "nothing observed", never as `NaN`.

### Fragmentation, twice

Conflating these is how a code cache gets called "fragmented" when it is merely
padded.

* **Internal** — `1 - live_code_bytes / live_bytes`. Every body is allocated in
  a page-rounded buffer sized from a codegen *estimate*, so the slack between
  emitted and reserved bytes is retained memory no allocator policy recovers.
* **External** — `1 - largest_free_extent / free_bytes`. Answers "can a 40 KiB
  body be installed even though 400 KiB is free?". CratonVM currently allocates
  each body as its own OS mapping, so there is no shared arena and this reads
  `0.0` until one publishes a `FreeSpace` gauge. That is a real limitation, not
  a measured zero — see *Reconciliation* below.

---

## Reading it

```rust
use cratonvm_vm::jit::code_cache_lifecycle as ccl;

// Per-process report, one Display with five lines.
println!("{}", ccl::code_cache_lifecycle_report());

// Raw counters, for a harness that wants numbers rather than prose.
let raw = ccl::code_cache_lifecycle_raw();
assert_eq!(
    raw.installed_bytes - raw.reclaimed_bytes,
    ccl::code_cache_lifecycle_report().live_bytes,
);
```

Nothing is gated on an environment variable. Every recording entry point is
per-compilation or per-retirement, never per call, which is the same reasoning
`gc/src/gc_metrics.rs` gives for leaving its collector-side counters ungated:
gating them would make the default report empty, which is the failure mode the
item exists to prevent.

---

## Reconciliation — what is wired, and what is not

`vm/src/jit/` is this change's file ownership. The install and retire *call
sites* live outside it, so they still have to be routed in. Each one below is a
single call.

### 1. ~~`drain_deferred_jit_owners_if_quiescent` has the ordering backwards~~ — DONE

`drain_deferred_jit_owners_if_quiescent` now takes `deferred_jit_owners().lock()`
first and evaluates `ACTIVE_JIT_EXECUTIONS.is_zero()` with the guard held, and
`defer_jit_owner`'s fast path carries the comment saying why its own unlocked
`is_zero()` is sound (the check follows the caller's unpublication). The window
described here is closed.

**The obligation that replaced it**
is not about ordering at all, and it is the one to check when touching this
area:

> A published body's mapping may be returned **only** from a reclamation the
> retirement queue authorised, and a thread inside a compiled body always holds
> an owning reference to it.

The second clause is what makes a reference count reaching zero a proof rather
than a guess, and it was false until `try_call_compiled_entry_reentrant` was
made to pin. Any new long-lived holder of an `Arc<CompiledMethod>` that backs a
raw entry must be a `cratonvm_jit::RetainedCode`, not a bare `Arc`;
`published_code_free_audit().1` is the check, and it must stay zero.

### 2. ~~`ExecutableBuffer::new` is the install accounting point~~ — DONE, differently

The counters moved into `jit/src/lib.rs` (the first option this section
proposed), and the vm report re-reads them. Installs are counted at
publication (`mark_published`), not at allocation, so a discarded compile is
not an install. Still open: allocation failures (`alloc_executable` refusing,
and `ExecutableBuffer::overflowed` as a size-estimate overrun) are not counted.

### 3. ~~`impl Drop for ExecutableBuffer` is the reclaim accounting point~~ — DONE

It calls `record_code_free(ptr, capacity, ACTIVE_JIT_EXECUTIONS.get(), flags)`,
and for a published buffer also counts `reclaimed_bodies` / `reclaimed_bytes` /
`reclaimed_code_bytes`.

**Do not** treat a non-zero active count as the protocol violation — an earlier
revision of this section said to, and it is wrong in both directions. The count
is recorded for discarded compile attempts nothing can point into, and it is
sampled at the `munmap` rather than at the decision, so a legal reclamation
routinely reports a non-zero value; conversely a genuine bypass frequently
reports zero. Measured on `BasicErrorControllerIntegrationTests`: of 90 real
violations in one run, 73 had an active count of zero.

The violation is `CODE_FREE_PUBLISHED && !CODE_FREE_AUTHORISED`, which
`ExecutableBuffer::drop` now records into `published_code_free_audit()` and the
crash handler prints in words. Aggregate *that*.

### 4. ~~`JitCache::put` / `JitCache::put_osr` are the retire sites~~ — DONE, without reasons

`put`, `put_osr`, `invalidate_matching` and `clear_all` hand withdrawn bodies to
`retire_withdrawn_body`, which counts `withdrawn_bodies` / `withdrawn_bytes` and
defers the owner. Inline-cache withdrawals (`clear_compiled_entry`, retired PIC
ways) defer their owner references without counting a withdrawal, since the
body itself is withdrawn once, from the cache. Still open: no reason split
(`retirements_by_reason`), and no `recompilations` count.

### 5. `vm/src/runtime/jit_integration.rs` — the model cache

`CodeCache::install` returns `CodeCacheError::Full { available, needed }`:
that is `record_allocation_failure(needed, alloc_failure::CAP_EXCEEDED)`
verbatim. `CodeCache::sweep` drops every `is_valid == false` entry
**with no quiescence check of any kind**.

This is safe *today* only because `CompiledMethodState` holds
`entry_point: usize` — a bare address, not an owner — and because a workspace
grep finds no constructor of this `CodeCache` outside its own module and tests.
It is a model of the cache, not the cache. Both facts should be recorded at the
type, because the moment it becomes an owner, `sweep()` is an unconditional
use-after-free of executable memory.

### 6. Nothing publishes a `FreeSpace` gauge

`external_fragmentation` reads `0.0` until something does. There is no arena to
publish: `ExecutableBuffer::new` takes one OS mapping per body. That is itself
the finding — a per-body `mmap` has no external fragmentation but pays a page of
internal fragmentation per compiled method plus one VMA per method, which the
`internal_fragmentation` gauge now measures.

---

## What a real concurrent stress test would still have to prove

The unit tests pin the protocol's *logic*: the grace period defers while a
thread is in JIT and proceeds once it leaves, N recompilations reclaim N−1
superseded versions, a failed allocation corrupts nothing, and 1,600 concurrent
install/retire pairs against two racing sweepers lose nothing and free nothing
live. They do **not** execute a single machine instruction from a reclaimed
page, because the "owner" they drop is a counter probe rather than an
`ExecutableBuffer`.

The review's acceptance — *"concurrent compile/install/invalidate/unload stress
shows no execution of partial or reclaimed code"* — needs, in addition:

1. **Real bodies.** A workload that compiles, recompiles and invalidates real
   methods while N mutators execute them, with `CRATONVM_JIT_POISON_FREE=1` so a
   jump into a retired body faults at a still-mapped `---p` range that
   positively identifies it (rather than at an address with no provenance).
   `recent_code_free_covering` + the crash handler already print the verdict.
2. **The frozen-peer schedule.** Force a cross-thread STW takeover
   (`CRATONVM_XT_JIT_ROOT_SCAN=1`) *concurrently* with invalidation, so the
   drain is asked to reclaim while a peer is OS-frozen mid-body. Assert
   `drains_deferred` rises and `reclaimed_bytes` does not.
3. **The wedged-chain schedule.** Leak a `JitEntryGuard` deliberately and assert
   that reclamation of what that thread may be inside stops (`queued_bytes`
   does not fall) and then resumes after `prune_returned_jit_entries` heals the
   chain. This is the test that distinguishes "the fail-safe works" from "the
   fail-safe is stuck on". (The parked-thread half — a thread blocked inside
   compiled code holds only what its stack names — is covered by
   `a_thread_parked_in_compiled_code_holds_only_what_its_stack_names` in
   `jit/src/lib.rs`.)
4. **Partial code.** Nothing here tests the *install* half against a concurrent
   reader, because the install-ordering check is a validator over a recorded
   sequence, not an observation of one. Proving "no execution of partial code"
   needs the entry publication to be instrumented so a reader that resolves an
   entry can assert the metadata for it is already present — i.e. step 5 must be
   observably after step 4 at runtime, not merely in a replayed list.
5. **A model checker.** The review's own P0 lane names Loom/Shuttle for
   "code-cache invalidation" specifically. The grace-period argument in §1.2
   above is a hand proof over stripe-read instants; it is exactly the kind of
   claim a bounded-schedule checker settles.

---

## Related

* `jit/src/lib.rs` — `defer_jit_owner`, `drain_deferred_jit_owners`,
  `JitThreadQuiescence`, `jit_code_reclamation_stats`.
* `vm/src/jit/code_cache_lifecycle.rs` — the report, and the model type's tests.
* `vm/src/jit/conservative_roots.rs` — `note_blocking_transition_enter` /
  `_leave` (blocked-stack summaries) and the prune wake-up;
  `vm/src/threading/gc_barrier.rs` calls them.
* `docs/threading/thread-transition-states.md` §7.2, §10.3, §10.4 — where
  `CompiledUninterruptible` is recorded, and why the two in-JIT classifiers are
  deliberately mismatched.
* `docs/threading/objectref-concurrency-contract.md` — the OS-frozen peer.
* `jit/src/platform.rs` — the W^X primitives this protocol is modelled on.
* `gc/src/gc_metrics.rs` — the counter/report idiom this mirrors.
* `docs/jit/compiler-metrics.md` — per-compilation reports, including
  `code_cache_bytes_at_install`.
* the C2 review — the review this closes two P1
  items of.
