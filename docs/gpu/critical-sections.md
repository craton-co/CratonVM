# GPU critical sections: token lifetime, the wait bound, and the leak paths

*Scope: bounding GPU critical-section waiting.*

The report's finding was: *"GPU coordination pins references and makes
collection yield-spin while critical tokens are alive"*, with the
race/liveness note *"token leaks or stalled submissions can starve
collection"*.

This document is the audit that finding asks for, the verdict on which
"pin" vocabulary the GPU path actually uses, and the design of the
bounded, owned replacement in
[`cuda-bridge/src/critical.rs`](../../cuda-bridge/src/critical.rs).

**Headline verdicts, up front:**

1. A GPU "critical token" is *not* a pin at all. It is a counter. There is
   no per-object no-relocation mechanism on the GPU path, and there does
   not need to be — see §2.
2. `Heap::pin_ref` is **dead code**. It exists only on the non-default
   semi-space `Heap`, and has no production caller anywhere in the
   workspace. The GPU path uses **neither** pin vocabulary.
3. The wait is genuinely unbounded, by explicit design, and that design is
   sound-but-not-live. §3.
4. Five leak paths exist. Three were already closed by RAII; **two were
   open**, and one of those (VM shutdown mid-submission) permanently wedges
   the collector for every later VM in the process. §4.

---

## 1. What a critical token is today

| Question | Answer | Evidence |
|---|---|---|
| What is a token? | A `+1` on a process-global `AtomicU32`. Zero-sized; carries no object, no identity, no deadline. | `gc/src/safepoint.rs:61-67` |
| Who creates one? | `VmHeap::enter_gpu_critical` (`gc/src/vm_heap.rs:800-802`) against the process-global `GPU_CRITICAL_COUNT` (`gc/src/vm_heap.rs:110`); `Heap::enter_gpu_critical` (`gc/src/heap.rs:1275-1278`) against a per-instance counter on the non-default semi-space heap. `SafepointToken::new` is also `pub` and takes any `&AtomicU32` (`gc/src/safepoint.rs:77-83`). | — |
| Who destroys one? | `SafepointToken::drop` — a single `fetch_sub` (`gc/src/safepoint.rs:86-94`). | — |
| What bounds its lifetime? | **Nothing.** The lifetime is the Rust scope of the guard value, and the guard is moved into a heap-allocated submission that may never be dropped (§4). | — |
| Is a token `Send`? | No, deliberately (`gc/src/safepoint.rs:57-60, 66`). This is why `vm::runtime::offload` had to hand-roll a *parallel* guard, `GcCriticalGuard` (`vm/src/runtime/offload.rs:1518-1538`), that manipulates the same counter without the `!Send` marker. **Two mechanisms, one counter, and only one of them is what the GC's docs describe.** | — |

So a "token" is really a *permission slip with no name and no expiry*. The
counter cannot answer "who is holding this?", which is precisely why a
leak surfaces as an unexplained hang.

### 1.1 The two token producers in the VM

`dispatch_method_from_native_on_stream` takes **both** at
`vm/src/runtime/offload.rs:2465-2466`:

* a `GcCriticalGuard` (`Send`), moved into `FinalizeState` at
  `offload.rs:2886-2889` so it brackets the whole submission —
  dispatch → device execution → writeback;
* a short-lived `SafepointToken` (`!Send`), purely to satisfy the
  `&SafepointToken<'_>` marker parameter on every `gpu_marshal::*`
  function (`vm/src/runtime/gpu_marshal.rs:63`, `:363`, …), dropped again
  at `offload.rs:2865` once marshalling is done.

`finalize_submission` synthesizes a *third*, fake token against a local
counter purely to satisfy the same marker parameter for the writeback
(`offload.rs:2945-2946`). That token gates nothing; the comment says so.

---

## 2. The pin-vocabulary verdict

`docs/threading/objectref-concurrency-contract.md` §4.3 records that the
workspace has two incompatible meanings for "pin", and asks which one the
GPU path uses. The answer is **neither**.

### 2.1 `Heap::pin_ref` is dead code

`Heap::pin_ref` / `Heap::unpin_ref` (`gc/src/heap.rs:1294-1303`) are:

* `#[cfg(feature = "gpu-offload")]`, and
* methods on the **semi-space `Heap`**, which is not the default backend
  (`docs/threading/objectref-concurrency-contract.md:303`), and
* **called from nowhere but `gc/src/heap.rs`'s own unit tests**
  (`heap.rs:2697`, `:2729`, `:2748`, `:2758`).

`VmHeap` / `GenerationalHeap` / `G1Collector` have no `pin_ref` at all.
Nothing in `vm/`, `native-builtins/`, `cuda-bridge/` or `jit-cuda/`
mentions it. `ARCHITECTURE.md:476-482` presents it as a general
GC-coordination primitive; it is not one, and the GPU path does not use
it.

### 2.2 Where the semi-space pin *is* honoured, it is keep-alive **plus
remap**, not no-relocation

For completeness, on the path that does exist: `Heap::collect_garbage`
splices `gpu_pinned_refs` into the root buffer, runs the collector, and
then **rewrites the pin set with the post-GC addresses**
(`gc/src/heap.rs:985-1018`). A pinned object therefore *does* move. This
is the `gc/src/pinned.rs` vocabulary — keep-alive only — not the G1
region-pinning vocabulary.

### 2.3 Is that sound? Yes, and for the same reason JNI's is

**The device never holds a JVM heap address.** Every marshal path copies:

* `gpu_marshal::host_view_i32` and its siblings allocate a fresh host
  `Vec<T>` and `copy_nonoverlapping` the array payload into it
  (`vm/src/runtime/gpu_marshal.rs:83-95`), then that `Vec` is uploaded to
  a `cuda_bridge::DeviceBuffer` in device memory;
* the kernel's arguments are device pointers into device memory;
* the writeback copies *back* out of the device buffer through the heap's
  own accessors.

This is structurally identical to the JNI argument in
`gc/src/pinned.rs:14-36` — native code gets a detached copy
(`is_copy = JNI_TRUE`), so relocating the source array is harmless.

**So the answer to "is it only kept alive while the GPU holds a raw
device-visible address?" is: the GPU never holds one.** Keep-alive is the
correct and sufficient vocabulary today. This is a *latency* problem, not
a correctness one — with three caveats, all of which are real and two of
which are pre-existing:

1. **`MarshalWriteback` holds bare `ObjectRef`s**
   (`vm/src/runtime/offload.rs:3607-3626`) that are in **no** rewritable
   root family. They are valid only because the `GcCriticalGuard` keeps
   the GC from running at all for the submission's whole life. Any change
   that lets a collection run while a submission is in flight — including
   a naive "time out and collect anyway" — makes those references stale.
   *This is why timeout must not mean "proceed".*
2. ~~**`input_cache` is an `ObjectRef`-keyed table with no remap**~~ —
   **CLOSED** on `dev`. This was the `HashMap<ObjectRef, _>` hazard of
   the `ObjectRef` contract §7 item 3, and it was **not** covered by the
   critical section, because the entries survive *across* submissions
   when no token is held.

   `input_cache::remap_and_sweep` now runs once per collection from
   `memory::gc::update_all_roots`, re-keying relocated survivors and
   dropping entries whose array died. Three details worth carrying
   forward, because the obvious fix gets each of them wrong:

   - It is **not** an `external_roots`/`native_roots` provider. Both
     registries are driven from *after* `update_all_roots`' empty-
     `pointer_map` early return, so a provider's `remap` never fires on
     a non-moving collection — and objects still die on that path. The
     call sits before the early return, next to
     `smuggled_longs::remap_and_sweep`.
   - The **dead** entry, not the moved one, was the real hazard. A moved
     key only costs a miss and a re-upload; a dead key sits over an
     address the allocator immediately reuses, so a later array collides
     with it and the kernel reads another array's device buffer. The
     `element_type`/`len` guard does not separate that from a genuine
     hit when the shapes match, which for a kernel argument list is the
     common case.
   - The cache is deliberately **not** a GC root. An array reachable
     only from a cache entry can never be named by a future submit, so
     rooting it would make the cache an immortality set rather than keep
     anything useful alive. This is consistent with §2.3's own argument:
     the device holds no heap address, so only the *key* needs fixing.

   The generic mechanism is `vm/src/memory/addr_keyed.rs`.
3. If any future path adopts unified memory, zero-copy mapping, or pinned
   host staging aliased to the heap, the device *will* hold a heap
   address, and keep-alive stops being sufficient. The new API forces that
   decision to be written down:
   [`Relocation::Forbidden`](../../cuda-bridge/src/critical.rs) vs
   [`Relocation::KeepAliveOnly`](../../cuda-bridge/src/critical.rs).

---

## 3. The wait, and its new bound

### 3.1 Where the collector waits today, and for how long

| Collector | Wait site | Bound |
|---|---|---|
| Generational | `gc/src/gen_heap.rs:3969-3974` → `vm_heap::wait_for_gpu_critical_drain` | **none** |
| G1 | `gc/src/g1.rs:7992-7995` → same | **none** |
| Semi-space `Heap` | `gc/src/heap.rs:953`, `:1059` → `Heap::wait_for_gpu_critical` (`heap.rs:1332-1362`) | **none** |

Both loops have the same shape: `loop { yield_now(); if count == 0 { return } }`,
with a one-shot `tracing::warn!` after `GPU_CRITICAL_DEADLINE_SECS = 5`
(`gc/src/safepoint.rs:45`). The five seconds is a **logging threshold on
an unbounded wait**, not a bound. The module header states the policy
explicitly (`gc/src/safepoint.rs:31-35`):

> "we emit a single `tracing::warn!` and keep waiting — we NEVER force a
> collection while a token is alive."

That rule is correct for *safety* and absent for *liveness*: it has no
"…and here is what happens if it never drains". Two further properties
make it worse than it reads:

* the loop is a **busy** yield-spin, so a stalled kernel burns a core per
  waiting thread;
* `wait_for_gpu_critical_drain` returns `()`, so a collector **cannot
  express** "I waited and it did not drain" even if it wanted to.

### 3.2 The bound

`Registry::wait_for_drain(budget)` returns a
[`WaitOutcome`](../../cuda-bridge/src/critical.rs), never blocks past
`budget`, and never force-releases a live in-lease token to make itself
succeed.

| Knob | Default | Env override | What it bounds |
|---|---|---|---|
| Collector wait budget | **50 ms** | `CRATONVM_GPU_CRITICAL_WAIT_MS` | how long *the collector* waits |
| Token lease | **30 s** | `CRATONVM_GPU_CRITICAL_LEASE_MS` | how long *one token* may exist |

Two levels, because they answer two different questions. 50 ms is chosen
to be shorter than a pause anyone notices, *not* longer than a kernel: the
point is that the collector stops waiting, not that the kernel finishes. A
legitimately long kernel simply causes the cycle to take the weaker
collection. 30 s is chosen to be far longer than any plausible kernel, so
only a plainly-abandoned submission is revoked.

### 3.3 What happens on expiry

**`TimedOut` is not permission to relocate.** The documented behaviour,
and the one the tests pin, is:

1. Run a **non-moving** cycle. The mechanism already exists: call
   `gc_quiescence::mark_moving_young_coverage_incomplete_because(...)`,
   which diverts the young half to the non-moving sweep and records the
   reason on the collector-decision record
   (`gc/src/gc_metrics.rs:536-538`).
2. Splice `WaitOutcome::keepalive_addrs` into the root buffer, exactly the
   way `gc/src/heap.rs:974-979` splices `crate::pinned::pinned_addrs()`.
   This is required, not optional: a non-moving cycle still *frees*, and a
   Java array reachable only from an in-flight `MarshalWriteback` is
   unreachable from every root family.
3. Call `Registry::record_forced_non_moving_collection()` so the
   diversion is attributable.

`WaitOutcome::may_relocate()` answers `false` for **every** timeout, not
just for `relocation_forbidden` ones, because of caveat 2.3(1): an
outstanding `KeepAliveOnly` token still has unrewritable `ObjectRef`s in
its writeback.

Nothing in this design ever degrades into "collect anyway".

### 3.4 Why the reaper does not violate §3.3

Revoking a leaked token *does* let the collector proceed. That is safe
only because revocation is **published to the holder**: `reap_expired`
sets the token's revoked flag with `Release` **before** removing the entry
from the map, and the holder is obliged to check
`CriticalToken::is_revoked()` immediately before any heap writeback and
suppress it. A revoked submission is a lost submission — its result is
dropped — which is a strictly better outcome than a permanently
non-collecting VM, and strictly better than writing through a possibly
stale `ObjectRef`.

---

> **Status, 2026-09-02: wired.** Everything §7 lists as "requires the call
> from …" has been made. `GcCriticalGuard` now wraps a `CriticalToken`
> (`vm/src/runtime/offload.rs`); every collector goes through
> `cratonvm_gc::vm_heap::gpu_coordination` before a cycle
> (`VmHeap::collect_garbage`); the legacy counter wait is bounded; VM
> teardown calls `Registry::shutdown_vm`; and the writeback reads its
> target addresses back through the token after the collector's remap. Two
> refinements over what §7 proposed: the collector waits only for
> `Relocation::Forbidden` holders (`wait_for_relocation_clearance`), so a
> keep-alive token held for the life of a kernel no longer holds
> collection off at all; and `Registry::begin_moving_cycle` gates a
> `Forbidden` acquisition from an unstopped thread (the completion
> reaper) for the length of a moving cycle. `may_relocate` changed
> accordingly: a keep-alive-only timeout permits the move. ZGC, which
> consulted no GPU state before, is covered by the same wrapper.

## 4. The leak paths

| # | Path | Status before | Status now |
|---|---|---|---|
| 1 | **Cancelled kernel / pre-launch failure** — `record_failed_submission` early-returns from `dispatch_method_from_native_on_stream` after the guard was taken (`offload.rs:2501`, `:2510`, `:2519`, `:2554`, …) | **Closed.** `gc_guard` is a function local; every early `return` drops it. | Unchanged; now also counted as a *voluntary* release. |
| 2 | **Device error / timeout** — `event.synchronize()` fails in `finalize_submission` (`offload.rs:2925-2937`), or `Event::query` fails in `poll_submission_status` (`offload.rs:3132-3150`) | **Closed.** Both arms explicitly `take()` and drop the `FinalizeState`. | Unchanged. |
| 3 | **Host thread panic** mid-marshal — `gpu_marshal::host_view_i32` `assert!`s on a non-`int[]` (`gpu_marshal.rs:65-74`) and `panic!`s on an unexpected `Value` (`gpu_marshal.rs:106`) | **Closed under `panic = unwind`.** The guard is a local and unwinds. Would leak under `panic = abort`, but then the process is gone anyway. | Unchanged; pinned by a test. |
| 4 | **VM shutdown mid-submission** — `finalize_enqueued_handle` does `let Some(shared) = weak_vm.upgrade() else { return; }` (`offload.rs:2102-2105`). Nothing else ever drains `SUBMISSIONS` (`offload.rs:1916`); `release_submission` (`:1950`) is only called from Java-driven paths; `REAPER_SHUTDOWN` (`:2018`) is **never stored `true`** anywhere. | **OPEN — and the worst one.** The `FinalizeState`, and with it the `GcCriticalGuard`, is never dropped. `GPU_CRITICAL_COUNT` stays ≥ 1 **for the rest of the process**, so *every subsequent VM in that process can never collect*. The `REAPER_QUEUE` comment (`offload.rs:1993-2010`) documents that multi-`SharedVm` processes are real and that real-hardware validation already caught a related bug on this exact path. | **Closed** by `Registry::shutdown_vm(vm_id)`, which revokes and releases exactly that VM's tokens and leaves a concurrently-live VM's alone. Requires the one-line call from VM teardown listed in §7. |
| 5 | **A submission that never completes** — a wedged kernel, or a submission whose host callback never fires and which no Java thread ever polls. `finalize_submission`'s `event.synchronize()` (`offload.rs:2925`) blocks indefinitely by CUDA contract. | **OPEN.** Guard held forever; collector spins forever. | **Closed** by the lease: `reap_expired` revokes it after 30 s, names the holder at `error` level, and lets the wait drain. |

A sixth, adjacent finding that is **not** a token leak: `input_cache` was
an `ObjectRef`-keyed table with no post-GC remap. Not closed by this
work — it is orthogonal to token ownership — but closed separately on
`dev`; see §2.3 caveat 2.

---

## 5. Ownership identity

Every token now records an [`OwnerId`](../../cuda-bridge/src/critical.rs):
VM id, thread sequence number, thread name, submission handle, and a
`&'static str` acquisition site. The record is complete at acquisition, so
a token reaped much later still names the submission that abandoned it.

A leak therefore produces:

```
ERROR gpu critical: LEAKED token reaped after 30001.412ms (lease 30000.000ms)
      — vm=1 thread=7(cratonvm-gpu-completion-reaper) submission=9001
        site=dispatch_method_from_native [2 keep-alive root(s), keep-alive-only].
      Its holder never released it; the writeback is now poisoned via
      CriticalToken::is_revoked.
```

rather than a hang.

---

## 6. Counters and the report

Shaped after `gc/src/gc_metrics.rs` so the two reports read alike; `[GPU]`
prefix instead of `[GC]`, every ratio `0.0` rather than `NaN` when its
denominator is zero.

| Counter | Meaning |
|---|---|
| `tokens_outstanding` | gauge — tokens alive right now |
| `tokens_acquired` / `tokens_released` | totals; `released` counts **voluntary** exits only |
| `tokens_reaped` / `tokens_reaped_at_shutdown` | **every one is a repaired leak** |
| `double_release_suppressed` | a token released twice, or after a reap |
| `longest_held_nanos` | max hold time seen |
| `waits_entered` / `waits_drained` / `waits_timed_out` | collector waits |
| `wait_nanos` | total time collectors spent waiting on the GPU |
| `forced_non_moving_collections` | **the GC-latency figure attributed to GPU waiting** |

`Registry::report()` prints the counters, then names every live holder,
then every token the registry had to reap. A clean run says
`critical leaks: none — every token was released by its holder`
explicitly, so an empty line is never mistaken for a truncated log.

`forced_non_moving_collections` is bumped by the **collector**, not inside
`wait_for_drain`, so it measures *collections actually diverted* and not
*waits that expired* — a cycle that was already going to be non-moving for
its own reasons must not inflate the number GPU coordination is blamed
for.

---

## 7. What still has to change outside `cuda-bridge/`

This change lands entirely inside `cuda-bridge/`. The following edits are
in `gc/` and `vm/`, which are owned by other work in flight, and are
**reported, not made**:

1. **`gc/src/vm_heap.rs:121-144`** — `wait_for_gpu_critical_drain()` must
   return an outcome instead of `()`, and must stop at a budget. `gc`
   cannot depend on `cuda-bridge` (`gc/Cargo.toml` deps are `types`,
   `parking_lot`, `tracing`, `rustc-hash`), so the practical wiring is the
   **mirror**: the VM calls
   `cuda_bridge::critical::global().bind_mirror(&cratonvm_gc::vm_heap::GPU_CRITICAL_COUNT)`
   once at init, after which every acquire/release/**reap** moves the
   legacy counter too, and the existing spin loops terminate on a reap
   instead of wedging. That alone closes leaks 4 and 5 with **zero** `gc`
   edits. Bounding the wait itself still needs the signature change.
2. **`gc/src/heap.rs:1332-1362`** — same change for the semi-space
   `Heap::wait_for_gpu_critical`.
3. **`gc/src/gc_quiescence.rs:379-422`** — add
   `incomplete_reason::GPU_CRITICAL_WAIT_EXPIRED = 14` and bump
   `COUNT` to `15`, plus a label
   (`"gpu-critical-wait-expired"`). `every_incomplete_reason_has_a_label`
   (`gc_quiescence.rs:1181`) enforces the label.
4. **`vm/src/runtime/offload.rs:2465`** — replace
   `GcCriticalGuard::acquire()` with a `CriticalToken` carrying an
   `OwnerId` naming the class/method and the submission handle. Delete
   `GcCriticalGuard` (`offload.rs:1518-1538`); the new token is `Send`, so
   the parallel implementation is no longer needed.
5. **`vm/src/runtime/offload.rs:2969-2986`** — check
   `token.is_revoked()` before draining writebacks and skip them if set,
   stamping the submission `Failed { "GPU critical token revoked" }`.
6. **VM teardown** (`vm/src/vm/vm_init.rs` / `vm_exec.rs` shutdown path) —
   call `cuda_bridge::critical::global().shutdown_vm(vm_id)`. This is the
   line that closes leak 4.
7. **`Cargo.lock`** — `cratonvm-cuda-bridge` gains a `tracing` edge
   (entry at `Cargo.lock:508-514`). No `--locked` build exists in CI, so
   cargo will regenerate it; the file is outside this change's ownership.
8. **`docs/threading/objectref-concurrency-contract.md:303-304`** — its
   row on `Heap::pin_ref` can be strengthened from "not the default
   backend" to "**no production caller at all**" (§2.1).

---

## 8. What still requires real hardware to validate

Everything here is host-side and is covered by `#[cfg(test)]` unit tests
with a mocked device layer. The following claims are **not** validated by
those tests and need a driver-backed run
(`--features cuda`, `.github/workflows/cuda-bridge.yml`):

1. **The 50 ms budget is the right number.** On a machine with real
   kernels, the interesting figure is `wait_timeout_rate`: if a real
   workload times out on most cycles the budget is too tight (throughput
   cost, no correctness cost) and the number should move. Only a
   hardware run produces that distribution.
2. **The 30 s lease never fires on a legitimate kernel.** A single
   `tokens_reaped > 0` on a healthy hardware run falsifies the choice.
3. **The device-fault path really is bounded.** Leak 2 is closed by code
   inspection and a mocked fault; an actual `CUDA_ERROR_LAUNCH_FAILED` /
   sticky-context error, and an actual `cuEventSynchronize` on a hung
   kernel, have not been observed against this code.
4. **The host-callback ordering.** `dispatch_async` attaches
   `finalize_state` before registering the `cuLaunchHostFunc` callback
   (`offload.rs:1846-1894`) specifically because the callback can fire
   first on real hardware. The token's acquire/release ordering inherits
   that race and has only been exercised in stub mode, where the callback
   runs synchronously.
5. **Whether any real kernel path ever hands the device a heap address.**
   §2.3's verdict is derived from reading every `host_view_*` /
   `write_back_*` function. A driver-backed run with a device-pointer
   audit would confirm no path bypasses them.
6. **Multi-VM shutdown under load.** Leak 4's closure is unit-tested with
   two synthetic VM ids; the real scenario (an embedding host creating and
   dropping `Vm::new()` while GPU submissions are in flight) is what the
   `REAPER_QUEUE` comment says real-hardware validation caught before.
