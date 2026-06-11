# CratonVM Code Review — `vm` crate threading / debug / jvmti / config

Scope: `vm/src/threading/*`, `vm/src/debug/*`, `vm/src/jvmti/*`,
`vm/src/config.rs`, `vm/src/lib.rs`, plus the remaining loose `vm/src/*.rs`
(`dispatch_trace.rs`, `jck_capture.rs`, `error.rs`). Static review only — no
build/test was run. Reviewer focus: concurrency correctness (lock ordering,
condvar use, data races via raw pointers, GC interaction).

Date: 2026-06-10. Reviewer: Fable (Opus 4.8).

---

## Summary

The threading core (`monitor.rs`, `jvm_thread.rs::ParkState`, `gc_barrier.rs`,
`event_loop.rs`, `thread_registry.rs`) is mostly carefully engineered, with
explicit, mostly-correct race analysis in the comments and good test coverage
on the happy paths. The thin-lock / monitor-inflation state machine is sound,
condvars use the mutex+permit pattern that avoids lost wakeups, and the
event-loop uses a `WakeableCondvar` with a dedup flag.

The most significant findings are GC-soundness gaps rather than classic data
races:

1. **`async_exception_slot` is neither a GC root nor remapped after a moving
   GC** (`thread_registry.rs`) — a posted async exception (e.g. `Thread.stop`)
   can be collected or left dangling before the target consumes it. **High.**
2. **`GcBarrier::arrive_and_wait` over-counts `arrived` for excluded
   (blocked) threads** (`gc_barrier.rs`) — a blocked thread waking mid-STW can
   prematurely release `wait_for_all` while a real mutator is still running,
   letting the moving collector run under a live mutator. **High** (timing
   dependent; static read).
3. A cluster of **unchecked allocations on untrusted JDWP lengths**
   (`debug/protocol.rs`, `debug/commands.rs`) and an **arbitrary-pointer write
   into a frame local via JVMTI `SetLocal`** (`jvmti/mod.rs`). All are behind
   the `experimental-debug` feature (off by default) and assume a trusted
   debugger, lowering severity, but they are real.

There is also a meaningful amount of **dead / synthetic infrastructure**: the
entire `VirtualThreadManager` + continuation model in `virtual_threads.rs`
(~2200 lines) is not wired into native dispatch; `jmm.rs` (~1090 lines, the
Java-Memory-Model / race detector) is referenced only by tests; and several
`AtomicOperations::get_and_*` helpers in `varhandle.rs` operate on throwaway
locals (same defect class as the already-deprecated `compare_and_set_int`).

The `debug` and `jvmti` modules are gated behind `#[cfg(feature =
"experimental-debug")]` (`lib.rs:37-42`), so they are absent from the default
build.

---

## Bugs

### B1 (High) — async exception slot is not a GC root and is never remapped
`vm/src/threading/thread_registry.rs:63, 183-200, 206-219`, remap functions at
`471-496` and `525-588`.

`ThreadEntry.async_exception_slot: Arc<AtomicUsize>` stores a raw
`throwable.as_ptr() as usize` (line 192) and reconstructs it later via
`ObjectRef::from_raw(raw as *mut u8)` (line 217). The slot is:
- **not scanned as a GC root** — `collect_all_root_snapshots` (439) does not
  include it, and a grep shows the field is referenced only in this file; and
- **not remapped after a moving GC** — neither
  `update_thread_objs_after_gc` (471) nor `fold_pointer_map_into_blocked` (525)
  touches it.

The reachable caller is `Thread.stop`-class async-exception posting
(`native-builtins/src/lib.rs:3844` → `vm_exec.rs:3222` →
`post_async_exception`). The throwable is a fresh Java object that generally is
NOT held by the target's frames. Between `post_async_exception` and the
target's next safepoint `take_async_exception`, a young/moving GC can either
**collect** the throwable (no root keeps it alive) or **relocate** it (slot now
dangles), so the target raises a stale/garbage object. The docstring at 178-182
assumes "roots from the target's frames will pick it up once stored", which is
only true in the narrow case where the throwable is already frame-reachable.
Fix: register the slot as a remap target (mirror `java_thread_obj` handling in
both GC functions) and include it in the root snapshot, or pin the throwable
for the post→consume window.

### B2 (High) — `GcBarrier::arrive_and_wait` lets excluded blocked threads inflate `arrived`
`vm/src/threading/gc_barrier.rs:231-247` (with `wait_for_all` at 207-212 and
`request_stw` `expected` computation at 117-124).

`request_stw` deliberately **excludes** threads currently in a blocked region
from `expected` (`expected = alive - 1 - blocked`). But `arrive_and_wait`
increments `inner.arrived` for *any* non-initiator caller while STW is active
(line 238) and signals `all_arrived` once `arrived >= expected` (239-241). A
blocked (excluded) thread that wakes mid-STW runs `check_post_block_gc_refs`
(`vm/src/vm/vm_exec.rs:902-907`) which loops calling `arrive_and_wait` — so it
bumps `arrived` even though it was never in `expected`.

Worked example: 3 alive threads — initiator I, running mutator M (in
`expected`), blocked thread B (excluded). `expected = 3 - 1 - 1 = 1`. If B
wakes before M reaches its safepoint, B's `arrive_and_wait` makes
`arrived == 1 == expected`, `all_arrived.notify_all()` fires, and the
initiator's `wait_for_all` returns **while M is still running with raw
`ObjectRef`s in Rust locals** — exactly the condition the barrier exists to
prevent. The moving collector then relocates objects under a live mutator. The
in-code comment (107-111) claims this over-count is "harmless because
`wait_for_all` uses `<`", but `arrive_and_wait` uses `>=` for the notify, so the
spurious arrival can satisfy the quota early. Fix: gate the `arrived++` /
notify on the caller actually being a counted (non-excluded) thread, e.g. pass
a flag from the safepoint vs. the post-block wake path, or have the wake path
call a distinct "drain without counting" entry.

### B3 (Medium) — `schedule_wakeup` leaks `wakeup_signals` entries on timer fire
`vm/src/threading/virtual_threads.rs:814-828`.

The timer thread resubmits the VT on expiry (825) but never removes its entry
from `self.wakeup_signals` — only `cancel_wakeup` (831) removes. Every fired
`Thread.sleep` on a virtual thread therefore leaks one `(vt_id, Arc<...>)` map
entry permanently. Combined with the already-`TODO`'d per-sleep OS-thread spawn
(802-813), sleep-heavy VT workloads accumulate both threads and map entries.
(This is on the dead `VirtualThreadManager` path — see S1 — so the live blast
radius is currently nil, but it is a real leak if the manager is wired up.)

### B4 (Medium) — `VirtualThreadScheduler::release` can over-shoot `carrier_count`
`vm/src/threading/virtual_scheduler.rs:66-70`.

`release()` does `state.available += 1` unconditionally. An unbalanced or
double `release()` raises `available` above `carrier_count`, permanently
breaking the concurrency bound the semaphore is meant to enforce. This
scheduler *is* wired in (`vm_init.rs`, `vm_exec.rs`, `native-builtins`). Fix:
`state.available = (state.available + 1).min(self.carrier_count)` or assert the
invariant.

### B5 (Low) — `wait_for_non_daemon_threads` can busy-loop at 1s/iter
`vm/src/threading/thread_registry.rs:671-698` together with `join` at 249-280.

If a non-daemon thread is alive but has no `JoinHandle` (lost spawn/`set_join_handle`
race, or main), `join()` spin-waits 1000×1ms then returns `false`;
`wait_for_non_daemon_threads` re-snapshots, finds it still pending, and repeats —
a 1s-per-iteration busy loop. Bounded only when a `deadline` is supplied
(`None` means spin forever). Minor (rare race), but worth a backoff/condvar.

### B6 (Low) — `MemoryModelTracker::happens_before` false positive on join
`vm/src/threading/jmm.rs:316-321`.

The "from_thread finished and to_thread joined it" branch only checks that
`from_thread` has a finish event with `final_clock >= from_clock`; it never
checks that `to_thread` actually joined `from_thread`. Any later thread is
reported as having an HB edge from any finished thread. The race detector is
diagnostic-only and not wired into the interpreter (see S2), so impact is nil,
but the logic is wrong relative to its own doc.

### B7 (Low) — `WakeableCondvar::park` return expression is dead/redundant
`vm/src/threading/event_loop.rs:235`.

`woken && !wait_res.timed_out() || woken` simplifies to `woken`. Harmless but
obscures intent (looks like it was meant to distinguish notify vs. timeout).

---

## Vulnerabilities

All `debug`/`jvmti` items are behind `#[cfg(feature = "experimental-debug")]`
(default-off) and assume the JDWP/JVMTI peer is a trusted local debugger; this
caps real-world severity but the issues are genuine if the feature ships
enabled or is exposed over a socket.

### V1 (Medium) — JVMTI `SetLocal` reconstructs an arbitrary pointer into a GC-root frame local
`vm/src/jvmti/mod.rs:469-497` (object case 483-492).

`set_local` with `type_tag == b'L'` takes a caller-supplied `i64` and does
`ObjectRef::from_raw(value as usize as *mut u8)` (487-489), then stores it into
a live interpreter frame local. There is no validation that `value` points at a
live heap object. Reachable through the JDWP `StackFrame.SetValues` →
JVMTI-style local-set bridge, this is a remote-controlled arbitrary pointer
that the interpreter and GC will subsequently dereference / treat as a root →
memory corruption. `frame_depth`/`thread_id` are validated (470-475); only the
pointer is not. Fix: validate the address lies within a live heap region (the
heap already exposes object-bounds checks for the GC) before reconstructing.

### V2 (Low/Medium) — unchecked allocation on untrusted JDWP packet length
`vm/src/debug/protocol.rs:80-115` (`read_packet`, `data_len` at 91; `vec![0u8;
data_len]` at 95/105) and `read_string` at 183-188 (`vec![0u8; len]` from a u32
read off the wire).

`length`/`len` are 32-bit values read directly from the socket; the
`data_len = length - HEADER_SIZE` allocation (up to ~4 GiB) happens *before*
`read_exact`, so a single malformed header triggers a multi-GB allocation /
OOM-abort DoS. The buffered-payload reader (`PayloadReader`, 264-330) is fine —
it validates `remaining() < len` before allocating — but the stream-level
helpers are not. Fix: cap `length`/`len` against a sane maximum (and/or the
socket's known remaining bytes) before allocating.

### V3 (Low/Medium) — `Vec::with_capacity(arg_count)` from untrusted JDWP payload
`vm/src/debug/commands.rs:933` and `1163` (`InvokeMethod` arg arrays).

`arg_count` is read from the command payload (928-931) and used directly as a
`Vec::with_capacity` size. A hostile `arg_count = 0xFFFF_FFFF` forces a
4-billion-element pre-allocation before the loop would otherwise fail on the
first short `read_tagged_value`. Fix: bound `arg_count` by `reader.remaining()
/ min_tagged_value_size` (or a hard cap) before reserving.

### V4 (Low) — raw-pointer heap iteration trusts caller-supplied object list
`vm/src/jvmti/mod.rs:293-336` (`iterate_over_heap`,
`iterate_over_instances_of_class`).

Both deref `*mut u8` entries as `ObjectHeader` with no validation. Sound only
if the GC heap walker is the sole caller (internal contract). Noted for the
open-source threat model: if the JVMTI heap-iteration command becomes
externally driveable, the object list must be VM-internal, never client-supplied.

---

## Stubs and Unimplemented

### S1 — `VirtualThreadManager` + continuation model is unwired dead code
`vm/src/threading/virtual_threads.rs` (entire `VirtualThreadManager`,
`ForkJoinScheduler`, `Continuation`, `VirtualThread` machinery, ~2200 lines).

`park()`/`park_with_frames()` "freeze" continuations but `park()` stores
`Vec::new()` (line 298), and a grep shows `VirtualThreadManager` /
`create_virtual_thread` / `park_with_frames` are referenced only inside the
threading module + a roadmap doc — never from `native-builtins` (the real
`Thread.ofVirtual` / Continuation natives live there and do not call this).
This is an entire simulation model that does not drive real execution. Either
wire it up or remove it before open-sourcing; as shipped it misleads readers
into thinking virtual threads are scheduler-backed.

### S2 — `jmm.rs` JMM / race detector is test-only
`vm/src/threading/jmm.rs` (~1090 lines). `JavaMemoryModel`,
`MemoryModelTracker`, `DataRaceDetector`, `HappensBefore` are referenced only by
`vm/tests` and the file's own tests; the interpreter's volatile dispatch does
not call `on_volatile_read/write`. The `fence()` mappings are correct but the
tracking layer is dead infrastructure (and contains B6).

### S3 — `AtomicOperations::get_and_*` helpers operate on throwaway locals
`vm/src/threading/varhandle.rs:434-544` (`get_and_set_int`, `get_and_add_int`,
`get_and_or/and/xor_int`, `get_and_set/add/or/and/xor_long`).

Each constructs a stack-local `AtomicI32/I64` from `current`, performs the op
on it, and returns the old value — it never publishes to the heap slot, so the
"atomic" op is meaningless (identical defect to `compare_and_set_int`, which was
already deprecated + `#[cfg(test)]`-gated + panic-guarded at 372-406). Grep
confirms production `Unsafe.getAndAdd*` goes through
`native-builtins::native_unsafe_get_and_add_int` instead, and these helpers are
referenced only by varhandle's own tests. They should be `#[cfg(test)]`-gated
or deleted; as public non-test fns they invite a future caller to use them and
silently get a non-atomic no-op.

### S4 — `ForkJoinPool` workers do no work; `getStealCount()` is a synthetic metric
`vm/src/threading/forkjoin.rs:115-149, 195-201`.

`worker_loop` pops a task and immediately signals completion without running it
(`let _ = task.task_id;`, 136) — the submitter inline-runs `compute()`. This is
documented as an intentional constraint (`NativeContext` is not `Send`), but it
means `steal_count()` returns the submission count as "a proxy because the
single-queue model has no genuine steal events" (197-201), i.e.
`ForkJoinPool.getStealCount()` reports a fabricated value. Per project
no-synthetic-values policy this should be surfaced (return 0, or a clearly
documented approximation) rather than masquerading as a steal count.

### S5 — `ScheduledExecutorService` fires only when control returns to the interpreter
`vm/src/threading/scheduled.rs` (module doc 12-20, `elapsed_ticks` 55-76).

Periodic tasks do not run on the ticker thread; a caller-side pump fires them
"the next time control returns to the interpreter (e.g. inside Thread.sleep)".
This is a documented partial implementation — correct for the probe pattern,
wrong for a program that schedules a task and then does CPU-bound work without
sleeping. Flagged as a known approximation, not a defect.

### S6 — `set_event_loop_affinity` validates but discards affinity
`vm/src/threading/virtual_threads.rs:852-866`. Validates `raw_id` then `let _ =
vt_id;` with a comment "future: tag VirtualThread with affinity" — the affinity
is never stored. No-op beyond validation.

---

## Performance

### P1 — per-`Thread.sleep` OS-thread spawn for VT timers
`vm/src/threading/virtual_threads.rs:814-828` (already `TODO`'d at 802-813).
Spawns a fresh OS thread + stack per VT sleep, defeating the VT premise. The
correct fix (single timer-wheel thread) is described in the TODO.

### P2 — `try_steal` is an O(parallelism) lock-acquire scan per miss
`vm/src/threading/virtual_threads.rs:482-493` and `next_task` 497-508. Each
`next_task` miss locks every other carrier's queue in turn. Fine at small
parallelism; O(n) lock traffic under high carrier counts. (On the dead-code
path per S1.)

### P3 — `wait_for_task` lost-wakeup window forces 100 ms latency
`vm/src/threading/virtual_threads.rs:461-479`. `submit()` (433-441) pushes to
`submission_queue` and `notify_one()`s `task_available` without holding
`task_signal`; a carrier that checked `next_task()` (empty) but hasn't yet
parked misses the notify and only wakes on the 100 ms timeout. Not a hang
(bounded) but adds up-to-100 ms scheduling latency under the race. (Dead-code
path.)

### P4 — `tlab_offset`/`shadow_stack_offset` build a full `JvmThread::default()` to measure an offset
`vm/src/threading/jvm_thread.rs:400-427`. One-time (cached in `OnceLock`), but
constructs an entire thread (TLAB, shadow stack, pools) just to subtract two
addresses. `core::mem::offset_of!` (stable since 1.77) would be zero-cost and
clearer; the comment cites an MSRV that may now be satisfied.

### P5 — `dispatch_trace::record_*` is called on every dispatch even when disabled
`vm/src/dispatch_trace.rs:82-101, 103-...`. Each call does a function call +
relaxed atomic load before early-returning. Cheap, but it is on the hottest
path (every bytecode-method entry + every native dispatch); a compile-time
`cfg`/macro gate would remove it entirely from release builds.

### P6 — `remap_after_gc` drains and rebuilds the whole monitor/cas maps each GC
`vm/src/threading/monitor.rs:1247-1276`. `drain().collect()` then re-insert for
every entry on every moving GC, even entries whose address did not change.
Acceptable while inflated-monitor counts are low; an in-place rekey of only the
moved keys would avoid the full rebuild + temporary `Vec`.

---

## Tests

Inline `#[cfg(test)]` counts (function-definition lines, approximate): monitor
69, virtual_threads 133, thread_registry 58, event_loop 64, jmm 95, varhandle
91, jvm_thread 23, forkjoin 14, scheduled 18, virtual_scheduler 8, stamped 2.
Integration tests touching scope: `vm/tests/monitor_stress.rs`,
`vm/tests/lock_order_smoke.rs`, `vm/tests/vthread_probe_regression.rs`.

**Estimated coverage for this scope: ~62%.** Basis: line-level happy-path
coverage is high in monitor/event_loop/thread_registry/jvm_thread/varhandle
(many focused unit tests, plus a multi-threaded `monitor_stress`), but the
highest-risk concurrency/GC-interaction edges are largely untested, and ~3300
lines (virtual_threads manager + jmm) are dead code whose tests exercise a
model that never runs in production.

Does it plausibly reach 85%? **No, not for the risk-weighted surface.** The
mechanical line coverage may approach it because dead-but-tested modules
inflate the number, but the load-bearing race/GC paths are under-tested.

Most important missing tests:
- **GcBarrier excluded-thread over-count (B2):** a multi-thread test where a
  blocked thread wakes mid-STW and arrives while a counted mutator has not —
  assert `wait_for_all` does NOT return early. There is currently no test that
  mixes `enter_blocked`/`mark_blocked_region_*` with `arrive_and_wait`.
- **async exception under GC (B1):** post an async exception, force a moving GC
  before `take_async_exception`, assert the consumed ref is the relocated (and
  still-live) object.
- **Monitor inflation race:** concurrent `enter` on a thin-locked object from
  the owner (recursive overflow) and a contender (the 844-887 path) — the
  inflation CAS-loop is only indirectly covered.
- **`MonitorTable::remap_after_gc` correctness** beyond the empty-map and
  single-entry cases (`monitor_remap_after_gc`); no test moves an *inflated*
  monitor's object and re-enters via the new key.
- **`VirtualThreadScheduler` over-release (B4):** assert `available` never
  exceeds `carrier_count` after unbalanced releases.
- **JDWP malformed-input fuzz (V2/V3):** oversized `length`, oversized
  `arg_count`, truncated payloads — assert graceful error, not OOM/panic.
- **`ParkState` interrupt-during-park** and spurious-wakeup behavior for the
  untimed `park()` path (136).

---

## Feature Suggestions

1. **Single timer-wheel thread for VT/event-loop timers** (replaces P1 and the
   per-sleep spawn): a process-wide min-heap of `(deadline, vt_id)` driven by
   one timer thread, with cancellation as a flag flip. Already scoped in the
   `schedule_wakeup` TODO.
2. **Wire or delete the `VirtualThreadManager` continuation model.** If
   continuations are intended, integrate real frame capture into the
   interpreter; otherwise remove ~2200 lines of misleading simulation before
   open-sourcing.
3. **Make `gc_barrier` self-checking in debug builds:** a `debug_assert!` in
   `arrive_and_wait` that the caller is in the current `expected` set (tracked
   via a small per-STW arrived-thread set) would have caught B2 and documents
   the invariant.
4. **JDWP hardening pass:** a single `read_bounded_vec(len, max)` helper used by
   all wire-length allocations, plus address-validation for JVMTI `SetLocal`
   object writes — turn the trusted-debugger assumption into an enforced one so
   `experimental-debug` can be shipped safely.
5. **`offset_of!`-based field offsets** for `tlab_offset`/`shadow_stack_offset`
   (P4), removing the `JvmThread::default()` allocation and the lazy `OnceLock`.
6. **Lock-free epoch-per-slot dispatch ring** (noted as out-of-scope in
   `dispatch_trace.rs`) so the postmortem trace never contends on a mutex even
   under heavy multi-thread dispatch.

---

## Files sampled vs fully read

**Fully read (all or substantially all lines):** `threading/monitor.rs`
(structure + thin-lock/inflate/wait/notify/remap regions, ~lines 116-450,
676-1000, 1159-1298), `threading/thread_registry.rs` (181-280, 431-498,
525-700 + structure), `threading/gc_barrier.rs` (whole file), `threading/
jvm_thread.rs` (ParkState + offsets + struct), `threading/virtual_threads.rs`
(structure + 245-342, 433-560, 657-883), `threading/event_loop.rs` (structure +
WakeableCondvar, schedule_task/timer, run loop 805-917),
`threading/virtual_scheduler.rs` (whole), `threading/forkjoin.rs` (whole),
`threading/scheduled.rs` (1-115), `threading/mod.rs` (whole), `lib.rs` (whole),
`debug/protocol.rs` (1-330), `jvmti/mod.rs` (structure + 293-336, 469-497),
`dispatch_trace.rs` (1-101, dump path), `config.rs` (path-normalization 555-674,
defaults 290-330).

**Sampled (structure-grep + targeted regions):** `threading/jmm.rs` (fence
mappings + happens_before 295-349; not every clock-tracking method),
`threading/varhandle.rs` (AtomicOperations region 240-544; not every AccessMode
table), `debug/commands.rs` (InvokeMethod arg parsing 920-958, 1163, plus
PayloadReader contract; not all 2308 lines — bulk is `#[cfg(test)]`),
`debug/mod.rs`, `debug/events.rs`, `debug/transport.rs`, `debug/ids.rs`
(grepped for panics/parsing; not deep-read — feature-gated, lower risk),
`jvmti/agent.rs`, `jvmti/events.rs`, `jvmti/capabilities.rs` (grepped for
stubs/no-ops), `jck_capture.rs` (1-60), `config.rs` (remainder via grep),
`threading/stamped.rs` (header only).

**Cross-referenced outside scope (read-only, to confirm findings):**
`vm/src/vm/vm_exec.rs` (847-969 — blocked-region enter/leave + post-block GC
apply, confirms B1/B2), `vm/src/runtime/interpreter.rs` (safepoint
`arrive_and_wait` call site), `native-builtins/src/lib.rs:3844` (async-exception
post caller), `native-builtins/src/lib.rs:12826` (real Unsafe getAndAdd
registration, confirms S3).
