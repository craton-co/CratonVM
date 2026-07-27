# Virtual threads — actual state, 2026-07-26

Scope: `vm/src/threading/virtual_threads.rs`, `vm/src/threading/virtual_scheduler.rs`,
`vm/src/threading/event_loop.rs`. Written against `dev` @ `6495a191c`.

Everything below was read out of the code, not out of doc comments. Where a doc
comment contradicts the code, the code is reported and the comment is named.

---

## 1. Are virtual threads genuinely multiplexed?

**Yes.** This is real, not a fallback to one OS thread per virtual thread.

`vm/src/vm/vm_exec.rs::thread_start` splits at `if is_virtual`. The virtual
branch (~line 6914) builds a `Box<JvmThread>`, hands it to
`VirtualThreadManager::install_runtime`, calls `start()` (which enqueues the id)
and **returns before reaching `std::thread::Builder::spawn`**. Only platform
threads reach the spawn. Carriers come from
`VirtualThreadManager::start_carriers_once`, which spawns exactly
`available_parallelism()` OS threads named `ForkJoinPool-carrier-N`.

Continuation unmount is also real, and does not serialize frames:

* an unpinned blocking native returns `VmError::ContinuationYield`;
* every interpreter boundary passes it through untouched
  (`interpreter.rs:7407`, `:10570`), so frames keep their exact pc / locals /
  operand stack;
* `resume_virtual_continuation` catches it, calls `deposit_root_snapshot()`,
  and hands the whole `Box<JvmThread>` back via `suspend_runtime` — then
  **returns**, releasing the carrier's native stack;
* the Java stack survives in heap-resident frames at a stable `JvmThread`
  address; a later `take_runtime_for_mount` on any carrier resumes it through
  `interpreter::resume_continuation`.

So a parked virtual thread costs one boxed `JvmThread`, not an OS stack.

### Dead machinery in the same file

A second, older, frame-*copying* continuation implementation is still present
and is entirely unreachable outside tests. Nothing in the tree calls:

`VirtualThreadManager::park_virtual`, `::park_with_frames`,
`::unpark_with_frames`, `::pin_thread`, `::unpin_thread`, `::join`;
`VirtualThread::park`, `::park_with_frames`, `::unpark_with_frames`, `::pin`,
`::unpin`, `::is_pinned`; `Continuation::freeze`, `::thaw`, `::freeze_frames`,
`::thaw_frames`; `FrozenFrame` (outside `runtime/frame.rs`'s converters);
`ThreadBuilder`; `PinReason`.

Consequences worth knowing before trusting anything in this file:

* `VirtualThread::pin_count` / `pin_reason` are **always zero/None** at runtime.
  The live pin state is `JvmThread::pin_count` (`vm_exec.rs`), a different
  field. Tests such as `p81_synchronized_park_blocks_carrier` and
  `p81_monitor_state_preserved_in_continuation` assert on the dead struct and
  prove nothing about VM behaviour.
* `SchedulerStats::total_pins` is never incremented on the live path.
* `VirtualThread::unpark_permit` was, until this change, write-only.

`virtual_scheduler.rs` (`VirtualThreadScheduler`) is a **second, vestigial
carrier bound** — `thread_realm.rs:63` already calls it "legacy". Its permits
are disjoint from the real carrier pool, so releasing one frees nothing. Its
`acquire()` at `vm_exec.rs:6992` and `release()` at `:7135` / `:7251` sit inside
the platform-thread spawn closure, which the `is_virtual` early return at
`:6955` makes unreachable for virtual threads. The only live callers are
`vt_acquire_carrier` / `vt_release_carrier` (`vm_exec.rs:7906`) and
`NativeContextImpl::park` (`:8676`, `:8717`) — where a "release" advertises a
carrier that is in fact still occupied by the blocked OS thread.

---

## 2. Carrier-pool starvation

### Pinning is detected for park, but nothing pins for `synchronized`

`vt_park_for` / `vt_wait_on_key` gate on `JvmThread::pin_count`, and
`native-builtins` consults `vt_pin_count()` before yielding. That much works.
But `pin_count` is raised in only two places:

* `NativeContextImpl::monitor_enter` / `monitor_enter_gc_safe`
  (`vm_exec.rs:6516`, `:6537`) — the paths **native code** uses;
* `Continuation.pin` (`native-builtins/src/phases_late/concurrent.rs:5895`).

The interpreter's `Instruction::Monitorenter` (`interpreter.rs:17120`) and
`monitor_enter_synchronized_method` do **not** touch `pin_count`. So ordinary
Java `synchronized` does not pin, and a virtual thread parks and unmounts while
holding a monitor. That is JDK 24+ / JEP 491 behaviour and is fine on its own —
monitor ownership is keyed by the stable `ThreadId`, not the carrier, so it
survives migration.

### What is not fine: three ways a virtual thread blocks its carrier outright

None of these is virtual-thread aware; each parks the carrier's OS thread with
the continuation still mounted:

1. **Contended `monitorenter`** — `interpreter.rs:17182` calls
   `vm::monitor_enter_blocking`, which ends in `m.block_enter(tid)`
   (`vm_exec.rs:1471`). No `is_virtual` check anywhere in that function.
2. **`Object.wait`** — `NativeContextImpl::monitor_wait` (`vm_exec.rs:6567`)
   has no virtual-thread branch at all.
3. **The park fallback** — `NativeContextImpl::park` (`vm_exec.rs:8650`) for
   any virtual thread with `pin_count > 0`.

Combine (1) with the no-pin-on-`synchronized` finding and you get the classic
wedge: virtual thread A unmounts while holding monitor M; virtual threads
B..B+N-1 mount on every carrier and hit contended `monitorenter` on M; all N
carriers are now blocked in `block_enter`; A is queued but there is no carrier
left to remount it and let it release M. The pool is deadlocked, permanently,
with no timeout.

### Fix landed: starvation compensation (default on, no gate)

`ForkJoinScheduler` now tracks `busy_carriers`, `live_carriers` and a monotonic
`dispatch_count`. `start_carriers` also starts one
`ForkJoinPool-starvation-watchdog` thread which samples every 50 ms and, after
four consecutive samples where

* work is queued, **and**
* every live carrier is inside a task body, **and**
* `dispatch_count` has not moved,

spawns one compensating carrier. Bounded by `MAX_CARRIER_THREADS = 256` (the
JDK's `jdk.virtualThreadScheduler.maxPoolSize` default) via a CAS reservation,
so concurrent growth cannot overshoot. Compensating carriers retire after 30 s
idle and never shrink the pool below its base parallelism.

There is **no environment variable and no gate** — a compensation mechanism
that defaults off is worth nothing, and this converts a hard deadlock into a
slowdown. Compensating carriers get indices `>= parallelism`, which is why
`next_task` now uses `work_queues.get(idx)` instead of indexing directly (the
old form would have panicked on exactly those indices).

This is mitigation, not a cure. The cure is making the three blocking sites
above yield a continuation instead of parking a carrier — see cross-owner
requests.

---

## 3. GC visibility of parked continuation stacks

### Parked stacks ARE precisely scanned — and here is why

`suspend_runtime`'s caller runs `NativeContextImpl::deposit_root_snapshot()`
first, which walks `thread.frames` with `scan_local_objects` +
`stack.scan_object_refs` — the *typed* `Value::Object` scan, not an address
sweep — and publishes the result into the registry's `root_snapshot`. It also
raises `in_blocked_region`, so `ThreadRegistry::fold_pointer_map_into_blocked`
keeps that snapshot remapped through every collection the continuation sleeps
through, and records exact `slot_origins` for write-back.

Conservative scanning enters only through two doors, and **neither is open for
a virtual thread**:

* `memory/roots.rs::conservative_locals_enabled()` requires
  `CRATONVM_REAL_FORKJOINPOOL`, off by default.
* `jit::conservative_roots::scan_active_jit_frames` — a no-op here, because
  virtual threads never execute compiled code. `interpreter.rs:5682` forces
  `env_disable_jit` for `ThreadKind::Virtual`, `:7764` skips OSR, and `:23616`
  / `:32296` / `:40062` gate the tier-up and direct-entry paths. The claim in
  `docs/internal/continuation-backed-virtual-threads.md` about a "per-thread
  continuation gate" is one of the few doc claims here that **checks out**.

So parked virtual-thread stacks are **not** a second instance of the JIT
imprecision hole. They are precise. The price is that virtual threads are
permanently interpreted.

### The real hole: the remount path never applied the accumulated fixup

> **RESOLVED at wave-1 integration** by the owner of `vm_exec.rs`, as cross-owner
> request 7.1 below. `resume_virtual_continuation` now calls
> `check_post_block_gc()` (gated on `resumed`, since a first mount was never in a
> blocked region and would otherwise arrive twice at the same handshake), after
> the registry publishes rather than before. See
> `docs/internal/arch-2026-07-26/vt-resume-gc-fixup.md`. The analysis below is
> kept because it is the reasoning that found it.


`resume_virtual_continuation` (`vm_exec.rs:2009`) does:

```rust
thread.gc_block_state.in_blocked_region.store(false, Release);
thread.gc_block_state.java_state.store(0, Release);
```

and then goes straight to `resume_continuation`. It never calls
`check_post_block_gc()`.

That matters because the blocked-thread protocol is two-sided.
`fold_pointer_map_into_blocked` only remaps the **snapshot**; the thread's
actual frame slots keep the addresses they had when it blocked, and the moves
are accumulated in `gc_block_state.fixup` (chained across every missed
collection) plus `slot_origins`. The *only* code that drains and applies them
is `check_post_block_gc_refs` (`vm_exec.rs:2597`), which walks every frame's
locals, operand stack and `monitor_on_exit`.

Nothing else does. `safepoint_check` explicitly does not — its own comment
(`interpreter.rs:3638`) names "nothing ever applies its accumulated
blocked-fixup" as the bug it is trying to detect.

Therefore: **a virtual thread that is parked across a moving collection resumes
with stale `ObjectRef`s in its frames.** With the default Generational
collector, any young copying cycle while virtual threads are parked is enough.
The failure signature is this repo's most familiar one — zeroed headers,
`class_id=0`, NoSuchMethodError / ClassCastException on a receiver read out of a
resumed frame local.

Two smaller defects ride along on the same two lines:

* the raw `store(false)` bypasses `GcBarrier::leave_blocked_region_flagged`,
  which exists precisely so the flag-clear happens under the barrier lock that
  confirmed no pause is active. The raw store re-opens the
  "excluded-but-running mutator" window that `check_post_block_gc_refs`'
  comment calls "finding 1's corruption family".
* `slot_origins` (the exact per-slot tracker deposited on the way in) is never
  written back, so even the slots the `fixup` chain would have missed stay
  stale.

The fix is a one-line-shaped change in a file I do not own — see cross-owner
requests. I did not make it.

---

## 4. Scheduling fairness and wakeup cost

**Per-unpark allocation:** none. `submit()` pushes a `u64` into a `VecDeque`.
Timed waits cost one `WakeupEntry` heap insertion on a single shared
`VirtualThread-wakeup-timer` thread (the per-`Thread.sleep` OS-thread spawn is
long gone) plus one `Arc<(Mutex<bool>, Condvar)>` per registration.

**Thundering herd:** no. `submit` uses `notify_one`; `notify_all` is only for
shutdown and for nudging a freshly spawned compensating carrier.

**Single global lock:** yes, and it is the main fairness limit. Every submit
takes `submission_queue`, and every carrier poll takes it too. The per-carrier
`work_queues` are allocated and stolen from but **never pushed to** — nothing
in the tree enqueues into a carrier-local queue, so the work-stealing half of
the "work-stealing scheduler" is inert and 100% of traffic goes through the one
global deque. At high virtual-thread counts that deque is the bottleneck. Not
fixed here (it needs a submit-side routing decision that touches the mount
path); recorded as a known gap.

**Lost wakeup on submit — fixed.** `submit` used to `notify_one` *outside*
`task_signal`, the mutex `wait_for_task` waits on. A carrier that had just
polled the queues and found nothing, but had not yet reached `wait_for`, missed
the notification. The 100 ms poll timeout inside the wait loop disguised this
as latency rather than a hang: every unpark that lost the race added up to
100 ms. `submit` and `notify_all_carriers` now hold `task_signal` across the
notify. Lock order is `task_signal` → queues, matching `wait_for_task_until`.

**Lost unpark — fixed, and this one could hang forever.** `unpark_virtual`
tested `state == Started` *after* `vt.unpark()`:

* If the continuation had returned `ContinuationYield` but `suspend_runtime`
  had not run yet, the state was still `Running`; `vt.unpark()` does not change
  a `Running` state, the test failed, and **nothing was submitted**. The permit
  it set (`unpark_permit`) was read by no live code, because the only consumer
  is the dead `VirtualThread::park`. An untimed `LockSupport.park()` yields with
  `wake_after_nanos == 0`, so `suspend_runtime` arms no timer either — the
  virtual thread was parked forever. That window is exactly the one every real
  park/unpark handoff races through.
* If the state was already `Started` (queued, not yet mounted), the test
  *passed* and enqueued a duplicate id.

`unpark_virtual` now resubmits only for `Parked` + runtime-deposited, and
otherwise leaves a sticky permit; `suspend_runtime` consumes that permit after
depositing. The consumption is gated on `wake_after.is_zero()` on purpose: an
untimed park is the only yield that can hang without it, and honouring a stray
permit on a timed yield would let it truncate a real `Thread.sleep`. Residual:
a `parkNanos` racing an unpark waits out its timeout instead of returning
promptly — bounded latency, not a hang. Recorded as a known gap.

**I/O-driven wakeups (`event_loop.rs`):** the module is well-built — one
dedicated OS thread per loop, a `WakeableCondvar` with correct
timeout-vs-spurious-vs-woken reporting, `catch_unwind` per task, hard caps
(256 loops / 10k tasks / 1k timers), no lock held across a park.

But it has **no connection to virtual threads whatsoever**, contrary to its own
module doc ("`EventLoopAffinity` — the marker `super::virtual_threads` consults
before routing a task onto the general fork-join pool"). Nothing consults it.
`VirtualThreadManager::set_event_loop_affinity` validates the id and then
literally does `let _ = vt_id; // future: tag VirtualThread with affinity`, and
both it and `schedule_on_event_loop` have zero callers. Its only real consumer
is `native-builtins/src/vertx_eventloop.rs`, via the global
`event_loop_manager()`, not via the virtual-thread manager.

So there is no I/O-driven virtual-thread wakeup path at all: a virtual thread
doing blocking socket I/O goes through the ordinary blocking native and holds
its carrier. I left the doc comment in place rather than rewriting a sibling
subsystem's header, but it should not be believed.

---

## 5. Reconciliation with the monitor rework

These were written as assumptions before
`docs/internal/arch-2026-07-26/monitor-and-registry-contention.md` existed;
re-checked against it after the wave-1 integration merge.

1. **Monitor ownership is keyed by `ThreadId`, not by carrier / OS thread.**
   A virtual thread that acquires a monitor, unmounts, and remounts on a
   different carrier must still be seen as the owner. Anything that keys
   ownership or reentrancy on an OS thread id breaks virtual threads silently.
   **Confirmed holds.** The rework moved inflated-monitor lookup from the
   global table to the mark word, but ownership is still a VM `ThreadId` — the
   thin lock stores it as a u32 (hence that doc's `ThreadId > u32::MAX` legacy
   fallback), and `holds` / `current_owner` are still `ThreadId`-keyed. Nothing
   became carrier-keyed.
2. **`monitors.release_monitors_held_by_except(tid, ..)` runs only at real
   virtual-thread termination** (`resume_virtual_continuation`, `vm_exec.rs:2170`),
   never on unmount. If the rework adds any "release on carrier exit" sweep, it
   must not fire on a continuation yield.
3. **Correctness of my changes does not depend on the current global
   `MonitorTable` mutex.** Nothing in `virtual_threads.rs` takes a monitor lock;
   the compensation watchdog reads only its own atomics, and the
   unpark/suspend handshake is serialized by the manager's own `threads` mutex.
   Removing the process-wide monitor lock does not invalidate any argument here.
4. **My starvation compensation assumes contended monitor entry still blocks
   the carrier.** If the rework makes contended entry yield a continuation
   instead, compensation becomes a no-op safety net rather than the thing
   standing between the VM and a deadlock — strictly better, no conflict.
5. **I assume `ThreadRegistry` keeps publishing `root_snapshot`,
   `gc_block_state` and `jvm_thread_addr` per `ThreadId`,** and that a parked
   virtual thread (registered, alive, `in_blocked_region` raised, no OS tid
   currently running it) stays a legal registry state. `suspend_runtime` leaves
   exactly that state behind.

---

## 6. Changes made in this pass

All in `vm/src/threading/virtual_threads.rs`.

| Change | Kind |
|---|---|
| `unpark_virtual` rewritten: submit only when `Parked` + runtime deposited; sticky permit otherwise | bug fix (hang) |
| `suspend_runtime` consumes the sticky permit for untimed yields | bug fix (hang) |
| `submit` / `notify_all_carriers` hold `task_signal` across the notify | bug fix (100 ms latency) |
| `next_task` uses `work_queues.get(idx)` | bug fix (panic on compensating index) |
| `shutdown` drains handles before joining | bug fix (deadlock vs. watchdog) |
| Starvation watchdog + compensating carriers, cap 256, 30 s idle retirement | new, default on, no gate |
| `busy_carriers` / `live_carriers` / `dispatch_count` / `queued_len` accounting | supporting |
| `wait_for_task_until` with idle retirement | supporting |
| Carrier counters released by `Drop` guards, so a panic in a task body cannot ratchet the pool to its cap | supporting |

Tests added: `unpark_before_unmount_is_not_lost` (the hang regression),
`unpark_after_unmount_resubmits_once`,
`unpark_of_queued_thread_does_not_duplicate_submission`,
`unpark_permit_does_not_truncate_timed_yield`,
`unpark_of_terminated_thread_is_a_noop`,
`unpark_of_parked_thread_without_runtime_still_resubmits`,
`stall_detector_requires_queued_work_saturation_and_no_progress`,
`next_task_tolerates_compensating_carrier_index`,
`blocked_carrier_pool_is_grown_by_the_watchdog` (a task body that never returns,
asserting queued work still runs — the carrier-release-under-blocking case),
`compensating_carrier_respects_the_pool_cap`.

### 6.1 Correction after the first execution

Agents were barred from building, so the first run of this code was the
orchestrator's. Three tests failed. Two causes, one in the implementation and
one in the tests — recorded rather than quietly patched, because the split is
the useful part.

**Implementation was wrong: `manager_park_and_unpark` (pre-existing).**
I wrote the resubmit condition as `state == Parked && vt.runtime.is_some()`,
copied from `wake_waiters`. That test parks a virtual thread that has no
deposited runtime, so unpark left it `Parked` instead of `Started`.

The pre-existing expectation is correct and I made it fail. On the live path the
two halves of that conjunct are equivalent — `suspend_runtime` writes
`state = Parked` and `runtime = Some(..)` under a single hold of the `threads`
mutex, so no observer can see them disagree. The conjunct could therefore only
ever change behaviour when the invariant is *already* broken, and there it fails
in the dangerous direction: refusing to submit a `Parked` thread is a permanent
hang — the precise defect this rework exists to remove — whereas submitting one
that has no runtime is a no-op (`take_runtime_for_mount` returns `None` and
`resume_virtual_continuation` returns immediately). Fail open.

Condition is now `state == Parked` in both `unpark_virtual` and `wake_waiters`.
`wake_waiters` had the identical latent loss: for an already-parked thread its
`else` branch sets `wake_pending`, which only `suspend_runtime` consumes, and
`suspend_runtime` will never run again for a thread that is already parked.
Added `unpark_of_parked_thread_without_runtime_still_resubmits` so the reasoning
survives even if `manager_park_and_unpark` is ever rewritten.

**My tests were wrong: `unpark_after_unmount_resubmits_once` and
`unpark_permit_does_not_truncate_timed_yield`.** Both call `mgr.start(id)`,
which submits, and then neither drained that submission before asserting
`next_task(..) == None`. The implementation behaved exactly as designed; the
tests were reading `start`'s own leftover queue entry.

Worth being explicit that adding the drain *strengthens* these rather than
relaxing them. With an undrained entry sitting in the queue, an assertion of the
form "nothing was resubmitted" cannot distinguish `start`'s entry from a
wrongly-resubmitted one — it would have passed whether or not the bug it names
was present. Draining first is what makes them able to fail.

Not built or tested by me (nine concurrent builds OOM the host, per standing
instruction). `rustfmt --check` is clean.

---

## 7. Cross-owner requests

### 7.1 `vm/src/vm/vm_exec.rs` — `resume_virtual_continuation` must apply the blocked-region fixup (HIGH, heap corruption) — **DONE**

Landed by the `vm_exec.rs` owner at wave-1 integration, with one correction to
what I specified: the call is gated on `resumed`, because a first mount was
never in a blocked region and calling it unconditionally makes that mount arrive
a second time at the same safepoint handshake. The ordering note below (publish
the registry addresses first, drain second) was kept. See
`docs/internal/arch-2026-07-26/vt-resume-gc-fixup.md`. Original request:


**File:** `vm/src/vm/vm_exec.rs`
**Function:** `resume_virtual_continuation`, the block at ~lines 2009–2016.

Replace the two raw stores

```rust
thread.gc_block_state.in_blocked_region.store(false, Release);
thread.gc_block_state.java_state.store(0, Release);
```

with the same wake protocol every other blocking site uses:

```rust
NativeContextImpl { shared: &shared, thread: &mut thread }.check_post_block_gc();
thread.gc_block_state.java_state.store(0, Release);
```

`check_post_block_gc` calls `GcBarrier::leave_blocked_region_flagged` (clearing
the flag under the barrier lock instead of racily), drains
`gc_block_state.fixup`, applies it to every frame's locals, operand stack and
`monitor_on_exit`, writes back `slot_origins`, and refreshes the snapshot.

**Rationale:** `suspend_runtime`'s caller raises `in_blocked_region` via
`deposit_root_snapshot()`, so every moving collection while the continuation is
parked folds its pointer map into `fixup` and remaps only the *snapshot*. The
frames still hold pre-move addresses. Today nothing applies the fixup on
remount, so a virtual thread parked across a young copying cycle resumes on
vacated addresses. `safepoint_check` does not cover this
(`interpreter.rs:3636-3641` says so explicitly). Ordering note: the fixup must
be applied **after** `set_jvm_thread_addr` / `set_tlab_addr` publish the new
`JvmThread` address, and before `resume_continuation`.

The same two-line pattern exists in the platform-spawn yield path
(`vm_exec.rs:7120-7136`), but that path deposits and suspends; it is the *remount*
that is missing the counterpart, and there is only one remount site.

### 7.2 `vm/src/vm/vm_exec.rs` + `vm/src/runtime/interpreter.rs` — contended monitor entry should unmount, not block a carrier (HIGH, deadlock)

**Files/functions:** `vm_exec.rs::monitor_enter_blocking`,
`vm_exec.rs::monitor_enter_synchronized_method`,
`vm_exec.rs::NativeContextImpl::monitor_wait`,
`interpreter.rs::Instruction::Monitorenter` (~17182).

For `ThreadKind::Virtual` with `pin_count == 0`, the contended path should
return `VmError::ContinuationYield` and register the virtual thread on the
monitor's wait set (the `wait_on_key` / `wake_waiters` machinery in
`VirtualThreadManager` already provides the deposit-then-check handshake and is
race-free against a wake that beats the unmount), rather than calling
`m.block_enter(tid)`.

**Rationale:** with `synchronized` not pinning, virtual threads unmount while
holding monitors; with contended entry blocking the carrier, `parallelism`
contenders wedge the entire pool while the owner sits unmountable in the queue.
The compensation watchdog I added turns that from a deadlock into a slowdown,
but it is a safety net, not a design.

### 7.3 `vm/src/vm/vm_exec.rs` — remove or wire the vestigial `virtual_scheduler` (LOW, clarity) — **DONE, and it was worse than I said**

All eight call sites in `vm_exec.rs` are gone (only explanatory comments remain
at `:7064`, `:7067`, `:7982`, `:8735`). I filed this as a clarity issue; the
`vm_exec.rs` owner found the `NativeContextImpl::park` pair was a **live hang
risk**, not merely misleading: nothing ever releases into that permit pool on
the virtual-thread path, so once more virtual threads park concurrently than
`carrier_count`, the surplus post-park `acquire()` calls block a real carrier OS
thread forever. My §1 characterisation ("releasing one frees nothing") was
right about the release side and missed that the *acquire* side is what bites.

### 7.4 `vm/src/vm/vm_exec.rs` — decide whether `synchronized` should pin (MEDIUM, semantics)

`interpreter.rs::Monitorenter` does not raise `JvmThread::pin_count`, while
`NativeContextImpl::monitor_enter` does. So the same `synchronized` region pins
or not depending on whether it was entered from bytecode or from a native. Pick
one. If the intent is JEP 491 (no pinning), 7.2 becomes mandatory rather than
merely desirable, and `NativeContextImpl::monitor_enter`'s pin should go.

---

## 8. Known gaps left open

* **Work-stealing is inert.** `work_queues` are stolen from but never pushed
  to; the global `submission_queue` mutex carries 100% of scheduling traffic.
* **`parkNanos` racing an unpark** waits out its timeout instead of returning
  promptly (see §4). Bounded latency, never a hang.
* **Virtual threads never JIT.** This is what makes their parked stacks
  precisely scannable (§3); it is also a standing throughput cost, and it means
  virtual-thread numbers in any benchmark are interpreter numbers.
* **No I/O-driven virtual-thread wakeup.** `event_loop.rs` and
  `virtual_threads.rs` are not connected despite the doc comment claiming they
  are (§4).
* **`ForkJoinScheduler::mount` / `unmount` / `carriers` / `active_count` /
  `SchedulerStats::peak_active`** are only reached from tests; the live mount
  bookkeeping is `VirtualThread::mount` via `take_runtime_for_mount`.
