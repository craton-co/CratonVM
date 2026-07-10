# `ReentrantLock`/`BlockingQueue`/`Future` natives leaked the VM monitor on `InterruptedException` — permanent deadlock — FIXED

Status: FIXED (dev, commit `<COMMIT_HASH>`)

Date observed: 2026-07-09
Date fixed: 2026-07-09

## Symptom

Found while continuing the websocket close-delay investigation
(`docs/known-issues/tomcat-08-07/wsremoteendpoint-close-delay-near-deadlock.md`)
after the `EnumSet.of()`/`allOf()` fix
(`docs/internal/fixed-suite-bugs/enumset-synthetic-surface-drop-realmode-FIXED.md`)
let Tomcat start under real-JDK mode and reach real `ThreadPoolExecutor`
contention for the first time. `TestWsRemoteEndpointImplServerDeadlock`'s
`testTemporaryDeadlockOnClientClose` (all 4 param combos) went from the
previously-documented ~19s bounded delay to hanging **indefinitely**
(observed past 180s, would not have terminated on its own — this is a
regression in observed severity relative to the doc's original,
pre-EnumSet-fix evidence, not an improvement).

## Root cause (confirmed live via `gdb -p <pid> -batch -ex 'thread apply all bt'`)

Attaching gdb to the CratonVM process during the stall showed **all ~10
worker threads plus the main VM thread simultaneously blocked** inside
`cratonvm_native_builtins::native_rl_unlock` → `monitor_enter` →
`Monitor::block_enter` → condvar wait — every thread piled up trying to
acquire the *same* wedged VM-level monitor, with nothing ever positioned
to release it. Full stack, one representative thread:

```
#3  cratonvm_vm::vm::vm_exec::NativeContextImpl::monitor_enter
#4  cratonvm_native_builtins::native_rl_unlock
#5  cratonvm_vm::vm::vm_exec::safe_native_call
...
```

Root cause: `native_rl_lock` (`java.util.concurrent.locks.ReentrantLock.lock()`)
and 8 other blocking natives (`ArrayBlockingQueue`/`LinkedBlockingQueue`
`put`/`take`/`poll(timeout)`/`transfer`, `ReentrantLock.tryLock(timeout)`,
a `CompletableFuture`-family timed wait) all followed the same pattern:

```rust
ctx.monitor_enter(this);
...
ctx.monitor_wait(this, Some(N))?;   // <-- `?` returns early on Err
ctx.monitor_exit(this);             // <-- never reached on Err
```

`monitor_wait` throws `InterruptedException` (as `Err`) exactly like real
`Object.wait()` does, both if the calling thread was already interrupted
at entry and if it is interrupted while parked. When that happens here,
the `?` operator returns immediately, **skipping the paired
`monitor_exit(this)`** — permanently leaking `this`'s VM-level monitor as
held by the interrupted thread. Every subsequent `monitor_enter(this)` on
that same object — by any thread, including this native's own retry loop
and (critically) `native_rl_unlock`'s separate `monitor_enter`/`notify`/
`monitor_exit` step used purely to signal waiters — then blocks forever.
Real `ThreadPoolExecutor`/`ScheduledThreadPoolExecutor` worker threads
routinely contend on a small number of shared internal locks and are
routinely interrupted (shutdown, task cancellation, etc.), so once real
executor contention started reaching this code path (via the
`ScheduledThreadPoolExecutor` fix bundled with the EnumSet fix — see the
doc referenced above), the leak became a near-certainty, wedging the
entire Tomcat instance's worker pool.

This exact "release even on error" requirement was already correctly
handled elsewhere in the same file for `Condition.await()`
(`native_cond_await`/`cb_await_inner`, both carry an explicit comment:
*"On error (interrupt) the monitor is still held — release it before
propagating"*) — this fix extends the same discipline to the 9 call sites
that had been missed.

## Fix

`native-builtins/src/lib.rs`:

1. Added a shared helper, `monitor_wait_release`, that calls
   `ctx.monitor_wait(obj, timeout_ms)` and **unconditionally** calls
   `ctx.monitor_exit(obj)` before returning the original `Result` —
   whether `monitor_wait` succeeded or errored. Replaced all 8 simple
   `ctx.monitor_wait(this, N)?; ctx.monitor_exit(this);` call sites
   (`ArrayBlockingQueue`/`LinkedBlockingQueue` `put`/`take`/
   `poll(timeout)`/`transfer`, `ReentrantLock.tryLock(timeout)`, a
   `CompletableFuture`-family timed wait) with
   `monitor_wait_release(ctx, this, N)?;`.
2. `native_rl_lock` had a slightly different shape (the `monitor_wait` call
   is inside an `if still_owned { ... }` block, with the unconditional
   `monitor_exit` after the block covering the `else` branch) — restructured
   by hand to the equivalent leak-safe form.
3. `native_rl_lock` specifically also got an additional, JLS-motivated
   refinement beyond the leak fix: real `ReentrantLock.lock()` (unlike
   `lockInterruptibly()`/`tryLock(timeout, unit)`) is specified to **never
   throw** on interrupt — it defers, per the `Lock#lock()` javadoc: *"If
   the current thread... is interrupted while acquiring the lock... it
   will continue to wait... but upon acquiring the lock its interrupted
   status will be set."* Added `is_interrupted_exception()` to detect the
   specific `InterruptedException` case and, only for that error, restore
   the thread's interrupt flag (via `ctx.thread_interrupt(ctx.current_thread_object())`,
   since `monitor_wait` already consumed it) and retry the loop instead of
   propagating. The other 8 sites are unaffected by this refinement — their
   Java method signatures (`BlockingQueue.put/take/poll`,
   `Future.get(timeout)`) legitimately declare `throws InterruptedException`,
   so propagating it there (now leak-free) is already correct.

## Verification

- Live repro: `gdb -p <pid> -batch -ex 'thread apply all bt'` attached to
  the stalled process during the `TestWsRemoteEndpointImplServerDeadlock`
  hang showed the described pile-up; after the fix, the same test class
  (all 4 param combos) completes in 9-14 seconds instead of hanging past
  180s (`timeout 200` no longer needed to kill it).
- Standalone `RlProbe.java`:
  - High-contention correctness: 8 threads × 2000 `lock()`/`incrementAndGet()`/
    `unlock()` iterations → `counter=16000` (exact), no lost updates, no hang.
  - Interrupt-during-contended-`lock()`: a waiter thread blocked on a
    contended lock is interrupted; before the fix it either hung (leak) or
    threw an uncaught `InterruptedException` from `lock()` (spec
    violation); after the fix it correctly absorbs the interrupt, prints
    `interrupted=true` (flag restored), and the lock remains fully usable
    afterward (`lock2 usable after interrupt scenario: OK`).
- `cargo test -p cratonvm-native-builtins`: 2938 passed, 2 failed — both
  confirmed **pre-existing on `origin/dev` independent of this change**
  (verified by running the same tests against a clean `origin/dev`
  worktree with none of this session's changes):
  `security_manager::policy::tests::wp68_substitution_dollar_escape_preserves_literal`
  (long-known, documented pre-existing) and
  `nio_heap_byte_buffer_tests::bytebuffer_allocate_initializes_real_address_for_bulk_copy`
  (newly pre-existing, from an unrelated `dev` commit that landed the same
  day — not this session's concern). 6 ignored, matching baseline.
- Targeted rerun of all lock/queue/blocking-concurrency-named tests: 127
  passed, 0 failed.

## Residual: this fix does NOT make `TestWsRemoteEndpointImplServerDeadlock` pass

With the deadlock gone, the test runs to completion quickly but still
fails (`Tests run: 4, Failures: 8`) — now on a **different, unrelated,
already-tracked** bug: `org.apache.tomcat.util.net.SocketWrapperBase`'s
`private final ReentrantLock lock` field is `null` when read
(`NullPointerException: Cannot invoke
"java.util.concurrent.locks.ReentrantLock.lock()" because "lock" is null`),
breaking the WebSocket upgrade handshake itself
(`jakarta.websocket.DeploymentException` / `EOFException ... Status Code
[0]`) before the server ever reaches the actual close-handshake logic
under test. This is tracked separately, in detail, with two untested
hypotheses, in
`docs/known-issues/tomcat-08-07/swallowabortedupploads-unexpected-socketexception.md`
("New blocker #3") — not this fix's scope; see that doc for the next
steps on it. See
`docs/known-issues/tomcat-08-07/wsremoteendpoint-close-delay-near-deadlock.md`
for the close-delay investigation's updated status.
