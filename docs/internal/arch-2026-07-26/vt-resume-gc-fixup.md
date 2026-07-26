# Virtual-thread remount: the blocked-region GC fixup — 2026-07-26

Slug `vt-resume-gc-fixup`. Files touched: `vm/src/vm/vm_exec.rs`,
`vm/src/threading/virtual_scheduler.rs`.

Companion to `virtual-threads.md`; this closes its §7.1 and §7.3.

---

## 1. The bug, re-verified from scratch

`virtual-threads.md` §7.1 was written by a sibling session. It is correct. I
re-derived every step on the merged tree rather than taking it on faith,
because a wrong edit in a GC fixup path is worse than no edit. What follows is
the independent derivation, with the evidence for each link.

### 1.1 The blocked-thread protocol is two-sided

A thread that parks in a native cannot reach an interpreter safepoint, so it
cannot apply a collection's pointer map itself. The protocol compensates:

**Park side.** `NativeContextImpl::deposit_root_snapshot`
(`vm_exec.rs`, `deposit_root_snapshot_inner`) walks `thread.frames`, publishes
a *typed* root snapshot into the shared `root_snapshot`, fills
`gc_block_state.slot_origins` with one entry per object-holding local and
operand-stack slot (`{frame, idx, is_stack, orig, cur}`), and — last, after the
snapshot is complete — raises `gc_block_state.in_blocked_region`. From that
store on, every STW census *excludes* this thread.

**Collector side.** `ThreadRegistry::fold_pointer_map_into_blocked`
(`thread_registry.rs:1593`) runs on the GC initiator under STW, before
`complete_gc`. For each alive thread with the flag raised it does exactly three
things:

1. composes the map into `gc_block_state.fixup`, chaining `orig -> cur -> new`;
2. seeds first-move entries from the pre-remap snapshot;
3. remaps the `root_snapshot` **in place**;
4. advances `slot_origins[*].cur` through the map.

It never touches the thread's frames. That is deliberate and documented — the
thread owns its frames, and the fold is keyed by the addresses those frames
still hold.

**Wake side.** `NativeContextImpl::check_post_block_gc_refs` (`vm_exec.rs`) is
the *only* code in the tree that drains and applies the accumulation. It calls
`GcBarrier::leave_blocked_region_flagged`, `mem::take`s `fixup`, rewrites every
frame's locals / operand stack / `monitor_on_exit`, the thread mirror,
`native_pin_roots`, `native_alloc_pool`, the JIT HashMap cache, scoped values,
the pending async exception and the JNI local handles, then `mem::take`s
`slot_origins` and does the exact per-slot write-back, then refreshes the
snapshot with the no-flag variant.

I grepped for any other consumer of `gc_block_state.fixup`. There is none.
`safepoint_check` (`interpreter.rs:3633`) only *detects* the condition, under
the `CRATONVM_DBG_BLOCKED_ACCESS` gate, and its comment names the exact
failure: "nothing ever applies its accumulated blocked-fixup".

### 1.2 Virtual threads use this protocol, and skipped the wake side

`resume_virtual_continuation` (`vm_exec.rs:2000`) is the **single** remount
site — the only caller of `take_runtime_for_mount` outside
`virtual_threads.rs`'s own tests, invoked from the carrier closure installed by
`start_carriers_once`. It read:

```rust
let tid = thread.thread_id;
thread.gc_block_state.in_blocked_region.store(false, Release);
thread.gc_block_state.java_state.store(0, Release);
shared.threads.thread_registry.set_os_tid_current(tid);
// ... set_jvm_thread_addr / set_tlab_addr ...
```

and then went straight to `resume_continuation`, which does no fixup of its own
(verified: `interpreter.rs:7436`, it just calls `execute_frame_from_index`).

The park side is genuinely engaged. Both `suspend_runtime` call sites
(`vm_exec.rs:2150` in this same function, and the platform-spawn path) call
`deposit_root_snapshot()` — the flag-*raising* variant — immediately before
suspending.

The fold genuinely reaches a parked continuation. This is the link most worth
checking, because the parked `JvmThread` is a `Box` held by
`VirtualThreadManager`, not by any registry entry. It works because
`spawn_thread`'s `is_virtual` arm shares the *same Arcs*:

```
vm_exec.rs:6999  set_root_snapshot(tid, virtual_runtime.root_snapshot.clone())
vm_exec.rs:7011  set_gc_block_state(tid, virtual_runtime.gc_block_state.clone())
```

`ThreadEntry::gc_block_state` is an `Arc<GcBlockState>`
(`thread_registry.rs:161`) and `JvmThread::gc_block_state` is the same type
(`jvm_thread.rs:409`). So the fold's `entry.gc_block_state.fixup.lock()` and the
parked thread's own `fixup` are one object.

**Therefore:** every moving collection during the park accumulated a fixup that
the remount then discarded, and the continuation resumed on vacated addresses.

### 1.3 It is reachable on the default build

This is the part that looks like it should save us and does not.

The default generational collector fails closed to a non-moving sweep whenever
conservative roots are live. In `gc/src/gen_heap.rs`:

```rust
let divert_non_moving = (has_conservative_roots && !moving_young)
    || honor_promotion_oom_risk
    || divert_for_incomplete_moving_coverage
    || explicit_full_gc;
```

`has_conservative_roots` is essentially "some thread holds a live JIT frame".
But **virtual threads never enter JIT**: `interpreter.rs:5682` folds
`ThreadKind::Virtual` into `env_disable_jit`, and `:7764` (OSR), `:23616`,
`:32296`, `:40062` gate the tier-up and direct-entry paths. So in a
virtual-thread workload nothing publishes conservative JIT roots,
`has_conservative_roots` is false, `divert_non_moving` is false, and the moving
Cheney young cycle runs.

That is precisely the workload in which continuations park. The property that
makes parked virtual-thread stacks *precisely* scannable is the same property
that guarantees the collection they sleep through is a *moving* one.

The failure signature is this repo's most familiar one: zeroed headers,
`class_id=0`, `NoSuchMethodError` / `ClassCastException` on a receiver read out
of a resumed frame local.

### 1.4 Two smaller defects on the same two lines

* The raw `store(false)` bypassed `GcBarrier::leave_blocked_region_flagged`,
  which clears the flag under the same lock hold that confirmed no pause is
  active. The raw store re-opened the excluded-but-running-mutator window that
  the barrier's own doc comment calls "finding 1's corruption family".
* Worse in the old ordering: the clear happened *before* the `safepoint_check`
  on the resume path (now `:2105`). So a remount that landed on a requested
  pause participated in it
  with pre-move frame addresses and applied that new map on top, compounding
  the desync rather than repairing it.

---

## 2. The fix, and why the drain sits where it sits

`vm/src/vm/vm_exec.rs`, `resume_virtual_continuation`. The two raw stores are
replaced by a real wake, and **moved below** the registry publishes:

```rust
let tid = thread.thread_id;
shared.threads.thread_registry.set_os_tid_current(tid);
shared.threads.thread_registry.set_jvm_thread_addr(tid, ...);
shared.threads.thread_registry.set_tlab_addr(tid, ...);
if resumed {
    NativeContextImpl { shared: &shared, thread: &mut thread }.check_post_block_gc();
} else {
    thread.gc_block_state.in_blocked_region.store(false, Release);
}
thread.gc_block_state.java_state.store(0, Release);
```

Three placement decisions, each of which is wrong in at least one direction:

**After the publishes, not before.** `check_post_block_gc` turns this thread
back into a *counted* mutator. From that instant a new pause can be requested
that expects it, and the takeover / cross-thread scan paths resolve it through
the registry: `os_tid` drives the counted-set excusal, and
`frozen_peer_thread_addrs` (`thread_registry.rs:423`) maps OS tids to
`jvm_thread_addr` in order to walk frames. A parked continuation is remounted
on *whichever* carrier picked it up, so `os_tid` genuinely differs from the
previous mount. Becoming counted while the registry still names the old carrier
would aim a takeover at the wrong OS thread. (`jvm_thread_addr` and `tlab_addr`
happen to be stable — the `JvmThread` lives in a `Box` that never moves — but
`os_tid` is not, and the ordering rule should not depend on that.)

Leaving the flag raised across the publishes is strictly safer than the old
code, not merely equivalent: while the frames still hold pre-move addresses the
thread *must* stay excluded from the census. The old ordering cleared the flag
first and left a window in which a counted-but-unfixed thread existed.

**Before `resume_continuation` and before the resume-path `safepoint_check`
(`:2105`).**
Later would mean applying a fixup to a stack that has already run bytecode off
vacated addresses, and would leave the `safepoint_check` participating in a
pause with the flag still raised.

**`resumed`-gated.** `resumed` is `vt.execution_started`
(`virtual_threads.rs:1241`), so `false` means the box came straight from
`install_runtime` with a virgin `GcBlockState` — flag down, `fixup` empty,
nothing to drain. Calling the drain there would not merely be pointless, it
would be a correctness bug: the first-mount arm runs its own
`run_if_no_stw_requested` / `arrive_and_wait_excluded` handshake, and
`leave_blocked_region_flagged` classifies participation as
`!inner.excluded_blocked.contains(&tid.0)` — an unexcluded thread, which a
first-mount thread is. It would arrive a *second* time for the same pause,
inflating `arrived` and releasing `wait_for_all` while a counted mutator still
runs. That is the same corruption class the fix exists to close.

### Drained exactly once

`check_post_block_gc_refs` `mem::take`s both `fixup` and `slot_origins`, and
`leave_blocked_region_flagged` clears the flag under the barrier lock, after
which no further fold can select this thread (`fold_pointer_map_into_blocked`
skips threads whose flag is down). So the drain is exactly-once and a repeated
call is inert. Covered by a test.

### Test coverage

`vm/src/vm/vm_exec.rs`, `mod tests`:

* `parked_continuation_resumes_with_remapped_frame_slots` — registers a thread
  with the *same Arc sharing* the virtual mount path uses, parks it via
  `deposit_root_snapshot`, folds a synthetic pointer map, asserts the frames
  are still stale and the accumulation is present (which is the bug), then
  drains and asserts all four slot classes are forwarded. The frame is built so
  that `monitor_on_exit` exercises the `fixup` chain (it is snapshotted
  unconditionally, and the chain is the only thing that heals it) while a
  liveness-dead local exercises the `slot_origins` write-back.
* `blocked_region_drain_is_idempotent` — asserts both structures are emptied,
  not merely read, and that a second wake moves nothing.

Not built or run: nine concurrent agents share this host and cargo builds OOM
it. `rustfmt --check` passes on both edited files.

---

## 3. The vestigial `virtual_scheduler` (closes §7.3)

`VirtualThreadScheduler` is a counting semaphore whose permits are **disjoint
from the real carrier pool**. The real pool is `VirtualThreadManager` /
`ForkJoinScheduler` in `virtual_threads.rs`. Nothing in `virtual_scheduler.rs`
can start, stop, park or free a carrier: `release()` increments a counter and
the carrier OS thread keeps doing whatever it was doing.

All eight call sites in `vm_exec.rs` are removed. Site-by-site, with why each
was safe or necessary to remove:

| Old site | Verdict |
| --- | --- |
| `acquire()` in the spawn closure | Unreachable. The `is_virtual` arm returns at `:7026`, before `std::thread::Builder`, so the closure only ever runs for platform threads. |
| `release()` after `suspend_runtime` in the spawn closure | Unreachable, same reason. |
| `release()` on thread exit in the spawn closure | Unreachable, same reason. |
| `vt_release_carrier` / `vt_acquire_carrier` overrides | Reached only from six sites in `native-builtins/src/lang_system.rs`, all unreachable — see below. Removing the overrides falls back to the `NativeContext` defaults, which are already no-ops (`native-api/src/registry.rs:2168,2172`). |
| `release()` / `acquire()` in `NativeContextImpl::park` | The only genuinely reachable pair, and a live hang risk — see below. |

**Why the `lang_system.rs` callers are unreachable.** All three
`Thread.sleep` natives have the identical shape:

```rust
let pinned  = is_virtual && ctx.vt_pin_count() > 0;
let release = is_virtual && !pinned;                 // == is_virtual && pin_count == 0
if release && ctx.vt_park_for(d) { return ContinuationYield }
if release { ctx.vt_release_carrier(); }
```

and `vt_park_for` returns exactly `is_virtual && pin_count == 0` — the same
predicate as `release`, evaluated with nothing in between that can change it.
So whenever `release` holds, the guarded return always fires first and both
carrier calls are dead.

**Why the `park` pair was worse than cosmetic.** It was not merely
"looks like backpressure, applies none". `release()` before the park was a
clamped no-op, but `acquire()` after the park genuinely takes a permit, and
after the removals above *nothing else in the tree releases into that pool*.
With more virtual threads parked concurrently than `carrier_count`, the surplus
wakers block on a permit nobody will return — a real carrier OS thread wedged
on a bookkeeping counter unrelated to any carrier. Removing it loses no
backpressure, because there was none to lose.

**What is left.** The type itself still exists, because four files *outside
this change's ownership* reference it and the build would break otherwise. Its
module header now says, unmissably, that it has no live callers, that it bounds
nothing, and that it must not be wired up. See the cross-owner request in §4.1.

---

## 4. Cross-owner requests

**I did not make any of these edits.** Each names the exact file and function.

### 4.1 Delete `VirtualThreadScheduler` outright (LOW, clarity)

The four structural references that keep the type alive:

* `vm/src/threading/mod.rs:15` — `pub mod virtual_scheduler;`
* `vm/src/threading/mod.rs:38` — `pub use virtual_scheduler::VirtualThreadScheduler;`
* `vm/src/vm/realms/thread_realm.rs:60` — the `virtual_scheduler` field (and
  its doc comment on the following lines, which contrasts it with
  `virtual_thread_manager`)
* `vm/src/vm/vm_init.rs:2473` — `virtual_scheduler: VirtualThreadScheduler::new_default()`
* `vm/src/vm.rs:54601` `virtual_scheduler_in_shared_vm` and `vm.rs:55905`
  `virtual_scheduler_concurrent_acquire_release` — both delete with the type;
  neither asserts anything about virtual-thread behaviour.

Then `vm/src/threading/virtual_scheduler.rs` can be deleted. There are no other
references anywhere in the tree. I own that file and would have deleted it, but
deleting it without the five lines above breaks the build.

**Rationale:** a dead module is tolerable; a dead module that *reads* as
carrier backpressure is how a reader convinces themselves virtual-thread
starvation is already bounded. It is not — the real compensation is the
watchdog in `virtual_threads.rs`.

### 4.2 The frame-copying continuation implementation is dead (LOW, clarity)

My dispatch brief located this in `vm_exec.rs`. **It is not there.** It lives
entirely in files I do not own, which is why I made no change:

* `vm/src/threading/virtual_threads.rs` — `FrozenFrame` (`:55`),
  `Continuation::freeze` / `thaw` (`:134`, `:142`), `PinReason` (`:207`),
  `VirtualThread::park_with_frames` / `unpark_with_frames` (`:326`, `:343`),
  `VirtualThreadManager::park_virtual` (`:1365`), `pin_thread` / `unpin_thread`
  (`:1439`, `:1455`), `park_with_frames` / `unpark_with_frames` (`:1524`,
  `:1542`), `ThreadBuilder` / `ThreadBuilderKind` (`:1658`, `:1668`).
* `vm/src/runtime/frame.rs:1595` `to_frozen_frame` and `:1623`
  `from_frozen_frame` — the only non-test producers/consumers of `FrozenFrame`,
  themselves reachable only from the above.

The live path freezes nothing: `suspend_runtime` keeps the `Box<JvmThread>`
whole and the frames stay in their heap stack chunks
(`virtual_threads.rs:1261-1266`).

The two tests named in my brief, `p81_synchronized_park_blocks_carrier`
(`virtual_threads.rs:2948`) and `p81_monitor_state_preserved_in_continuation`
(`:2965`), are also in `virtual_threads.rs`. They assert on the dead struct and
therefore prove nothing about the live remount path — the path §1 above shows
was silently corrupting frames while those tests were green. Delete them with
the machinery, or re-point them at `suspend_runtime` /
`resume_virtual_continuation`.

### 4.3 Already-known, restated for completeness

`virtual-threads.md` §7.2 — contended `monitorenter` / `Object.wait` blocks the
carrier OS thread instead of yielding a continuation. Touches
`vm/src/runtime/interpreter.rs` (`Instruction::Monitorenter`), owned by a
running sibling, as well as `monitor_enter_blocking` /
`monitor_enter_synchronized_method` / `NativeContextImpl::monitor_wait` in
`vm_exec.rs`. I did not attempt the `vm_exec.rs` half alone: a half-landed
unmount protocol is worse than the current blocking one, and the interpreter
half is not mine to write.

Note that §4.1's removal of the `park` acquire/release pair is a prerequisite
for §7.2 rather than a conflict with it — the permit round-trip would have to
be deleted anyway before a real unmount could be wired in.

`virtual-threads.md` §7.4 — whether `synchronized` should pin. Unchanged; the
decision belongs with §7.2.
