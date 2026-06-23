# Bug W — native methods hold raw oops across `monitor_wait` (stale-ref SIGSEGV)

**Severity:** High (hard VM SIGSEGV). Surfaced by
`org.apache.catalina.core.TestSwallowAbortedUploads` (`testAbortedPOSTOKSwallow`).
**Status:** Crash #1 (Condition.await) **FIXED**; LBQ.take/poll **FIXED**;
sibling blocking collections **PENDING** (follow-up sweep); a separate
downstream crash (#2) remains open. Branch `fix/tomcat-hardcrashes` (worktree
`C:/craton/CratonVM-tcbugs`), commits `046c0d04` + `8f87748c`.

## The systemic bug

A native method captures a heap object as a raw `ObjectRef` (a Rust local /
copied `Value::Object`) and **holds it across `ctx.monitor_wait`**, then uses it
afterward. `monitor_wait` parks the thread — a **GC safepoint**. A relocating
collection (the moving collector, or the non-moving young sweep's selective
promotion, default-on) that runs while the thread is blocked moves the object;
the Rust-local copy still points at the old, now-reused address. The subsequent
`monitor_exit(stale)` / `get_field(stale)` / next-loop `monitor_enter(stale)`
does `header_of(dead addr)` → `EXCEPTION_ACCESS_VIOLATION` (typically in
`MonitorTable::exit`, `monitor.rs`, `rax=0`).

The VM ships the intended remedy: `NativeContext::pin_native_root(obj) -> handle`
/ `read_native_pin(handle, fallback)` / `unpin_native_roots(base)`. `pin_native_root`
adds the oop to `thread.native_pin_roots`, which the collector **remaps**
(`vm/src/memory/gc.rs` `update_all_roots`, both GC paths), so reading it back
after the wait yields the post-GC address. The blocking-collection / Condition
natives simply never used it across their waits.

## Fixed (verified the first SIGSEGV moves on)

- `native_cond_await` / `_timeout` / `_nanos` and `reacquire_lock_after_await`
  (`java.util.concurrent.locks.Condition.await`, `ReentrantLock` re-acquire) —
  pinned `this`/`lock_ref` across each `monitor_wait`. **This was crash #1**
  (`MonitorTable::exit` via `native_cond_await`, `monitor.rs:1046`). Commit
  `046c0d04`. `rl_key` uses `identity_hash_code` (stable across GC), so the
  lock-state key needs no pinning. (Reusable helper `monitor_wait_keepalive`.)
- `LinkedBlockingQueue.take` / `poll(timeout)` — Tomcat's executor `TaskQueue`
  extends `LinkedBlockingQueue`, so its worker threads hit this. Commit
  `8f87748c`, via `monitor_wait_keepalive`.

`monitor_wait` itself is safe: `MonitorTable::wait` holds a stable Rust-heap
`Arc<Monitor>` across the condvar wait and never re-reads the object header
afterward — the staleness is purely in the **callers'** retained oops.

## PENDING — the same pattern in sibling blocking collections

The identical `ctx.monitor_wait(this, …)` + post-wait `this` use exists in
`native-builtins/src/lib.rs` at (line numbers approximate, pre-merge):
`ArrayBlockingQueue` put/take/poll (~20661/20729/20775) and ~9 more sites
(~21275, 21354, 21373, 21564, 21668, 22158, 22192, 22330, 22450, 22661, 27376 —
other blocking queues/stacks). Each is the same mechanical fix: make `this`
`mut` and route the wait through `monitor_wait_keepalive`. Not yet swept because
they can't be exercised by this test; do them as a batch and re-verify.

## RESOLVED — crash #2 = stale refs across a young-arena `grow()` (UAF)

With crash #1 fixed, the test ran further and SIGSEGV'd; the symbolized
faulting access was a **WRITE to a freed address** (not `rax=0` null — that was a
release-build artifact), first inside `gc_alloc_object`'s `init_object_header`
(allocation), and after the first round of fixes inside
`MonitorTable::enter_or_contend`'s `try_thin_lock` CAS. All faults wrote into the
same `0x2E…` region.

**Root cause:** the young-GC moving collector's `young_to.grow()`
([gen_heap.rs:2888]) **reallocates the arena `Vec`**, freeing the old backing
buffer. Any raw pointer still aimed into that buffer then dangles. Confirmed
decisively: running with `--Xmx 6g` (large young semi → no GC during the short
test → no `grow`) produces **zero crashes** (the test instead runs until the
interpreter-throughput watchdog). Three distinct dangling-pointer holders, each
fixed:

1. **Parked-thread TLAB** — in a multi-threaded STW GC the initiator retired its
   own TLAB but threads parked in `safepoint_check` did not, so after a `grow`
   their TLAB `[cursor,end)` pointed into the freed buffer and the next fast-path
   bump wrote the object header there. Fix: retire the TLAB in `safepoint_check`
   before parking ([interpreter.rs], commit `796ad9fa`).
2. **GC-blocked threads' TLABs** — threads that go GC-blocked (so STW proceeds
   without them) never retired: `monitor_wait` (Object.wait / Condition.await /
   blocking-queue `take` — the dominant path), contended `synchronized` acquire,
   `Thread.join`, `LockSupport.park`, `ReferenceQueue` `begin_blocking_region`.
   Fix: retire the TLAB at each blocking-entry point, while the arena is still
   valid (commit `aa7e1d55`).
3. **Thread-death join-wakeup** — the spawn closure captured the Thread object as
   a raw `ObjectRef` (`thread_obj_for_spawn`) and reused it at *death* to wake
   `Thread.join` waiters; never remapped, it dangled after a lifetime of GCs, so
   `enter_or_contend`'s header CAS wrote into freed memory (observed on a dying
   `TaskThread` worker). Fix: read the current Thread object from the registry's
   remapped `java_thread_obj(tid)` instead (commit `c43bf59c`).

**Result:** `TestSwallowAbortedUploads` CRASH → clean. It now starts the embedded
server, serves, stops, and advances through its test methods; verified 300 s with
no SIGSEGV (it ends as an interpreter-throughput HANG, the same class as the
other embedded-server tests — perf, not a defect). Regression: `bt18 = 68332206`,
`bt14 = 3222190`, the BUG-V `MonV` repro clean.

> General lesson: a raw `ObjectRef` / TLAB pointer held across a GC by a thread
> that is *blocked* (not at an interpreter safepoint) is a UAF the moment a young
> GC `grow`s (reallocs) the arena. Every GC-blocked-entry path must retire its
> TLAB before blocking, and any long-lived captured oop must be re-read from a
> remapped root (registry / `java_thread_obj`) rather than the stale capture.

## Reproduce / diagnose

```
.tooling/run-one.ps1 -Class org.apache.catalina.core.TestSwallowAbortedUploads `
  -CratonExe <binary>
```
Symbolize with a `release-with-debug` worktree build (`build-wt-rwd.bat`) +
`CRATONVM_SYMBOLIZE=0x<rva>,…` (raw hex RVAs from the hs_err, comma-separated).
Crash #1's deterministic micro-repro: `.tooling/drv/MonV.java` is for BUG-V; for
the await path the Tomcat test is currently the most reliable trigger.
