# `class_manager` RwLock writer starvation under heavy concurrent read pressure (OPEN)

## Status

**OPEN.** Confirmed via live `gdb -p <pid> -batch -ex 'thread apply all bt'`
captures on branch `fix/httpclient-vtable-classmanager-abba-deadlock-0706c`
(Azure Linux host, worktree `/data/data/wt-hc-vtable-abba-0706c`), AFTER
fixing a separate, previously-documented AB-BA deadlock between
`vtable_manager` and `class_manager` (see the FIXED writeup / commit
`caa4ee65` on the same branch — that fix cut the hang rate on this test from
13/20 (65%) to 5/20 (25%), but did not eliminate it). This doc covers the
remaining 5/20.

## Symptom

`org.springframework.http.client.HttpComponentsClientHttpRequestFactoryTests`
still hangs intermittently (~20-25% of runs, down from ~50-65% before the
AB-BA fix) during `@AfterEach` teardown (`MockWebServer.close()`), even with
the `vtable_manager`/`class_manager` lock-order fix applied. `time` on a
killed run shows near-zero CPU burned across the full timeout window
(consistent with genuine blocking, not a throughput problem).

## Root cause (confirmed via live gdb, two captures 2 seconds apart with
identical thread state — ruling out a transient snapshot artifact)

Two threads deadlocked purely on `SharedVm.class_manager`
(`vm/src/vm/vm_init.rs:314`, a `parking_lot::RwLock<ClassManager>`):

- **main-vm thread**: blocked in `RawRwLock::lock_exclusive_slow` inside
  `SharedVm::load_class_concurrent` (`vm/src/vm/vm_init.rs:3542`,
  `self.class_manager.write()`), reached via
  `ensure_class_initialized` → `alloc_synthetic` → `stream_try_defer` →
  `native_stream_filter` — i.e. a `Stream.filter` lambda dispatch that needs
  to load/initialize a class.
- **A pooled worker thread** ("Thread-35" in the capture): blocked in
  `RawRwLock::lock_shared_slow` inside `try_stackless_invoke` →
  `execute_invokestatic` (`vm/src/runtime/interpreter.rs:20958`,
  `shared.class_manager.read()`).

**No thread in either capture holds `class_manager`** at the snapshot
instant, and the state was IDENTICAL across two captures taken 2 seconds
apart (same PCs, same thread set) — this rules out "caught mid-wakeup, about
to proceed" (a real possibility with futex-based parking that a single
snapshot can't distinguish from genuine blocking, but repeated identical
snapshots over a real time window rule it out).

### Why this is starvation, not a simple lock-order bug

`execute_invokestatic`'s read-guard (`vm/src/runtime/interpreter.rs:20958`,
`let cm = shared.class_manager.read();`) is tightly scoped — it is
constructed and dropped entirely within one `{ }` block (ending at line
~20972) that walks the callee's superclass chain checking for a native
override, and this drop happens BEFORE `load_class_concurrent` (the write
acquisition) is ever called later in the same function. So this is not a
same-thread reentrancy bug, and it is not a second AB-BA cycle with a
different second lock (unlike the `vtable_manager` bug fixed on this
branch) — only `class_manager` itself is involved on both sides.

`parking_lot::RwLock`'s plain (non-`_fair`) `.read()`/`.write()` methods are
**not strictly writer-preferring**. The crate documents an "eventual
fairness" backstop (a periodic mechanism that occasionally hands the lock
directly to a queued waiter) but this is a heuristic, not a hard starvation
bound — the fast-path `.read()` acquisition is a lock-free counter
increment that does not need to check whether a writer is already queued.
Under a steady stream of overlapping short-lived reader acquisitions from
many concurrent threads (exactly what this test produces: a
`ThreadPoolExecutor`/OkHttp `Dispatcher` driving many worker threads, each
doing frequent `invokestatic` dispatch through this same read-guarded
superclass-chain walk), a single queued writer can in principle wait far
longer than the test's timeout, or — per the repeated identical captures —
effectively indefinitely for the lifetime of the process.

This is architecturally consistent with (and likely explains, at least in
part) the ORIGINAL known-issues doc's hypothesis of "a Java thread was
holding the lock when it got torn down non-cooperatively" — that hypothesis
is not needed to explain the symptom; plain non-fair-mode reader/writer
contention on a hot, frequently-acquired lock is sufficient, and no
teardown/interruption mechanism needed to be invoked. (Separately, thread
teardown/interrupt in this VM IS fully cooperative — see the "Ruled out"
section below — so the original "torn down while holding the lock" theory
is now considered unlikely on its own merits too, independent of this
alternative explanation.)

## Ruled out this session

- **Forced/non-cooperative OS thread teardown.** `Thread.stop()`
  (`native-builtins/src/deprecated_lang.rs`), `Thread.interrupt()`
  (`vm/src/vm/vm_exec.rs:4796` `thread_interrupt`), and
  `ExecutorService.shutdownNow()` (`native-builtins/src/lib.rs:45680`
  `interrupt_executor_workers`) are all fully cooperative in this VM: they
  set an atomic flag / post an async exception / call `LockSupport.unpark`
  equivalent, consumed at the target thread's own next safepoint check.
  There is no `pthread_cancel`, no forced `pthread_kill`, no
  `std::process::exit` from within a spawned thread, and no unwind-across-
  `extern "C"` path found in the thread-lifecycle code
  (`vm/src/threading/thread_registry.rs`). A held `RwLock` guard cannot be
  silently dropped by any thread-teardown path found in this codebase.
- **Self-deadlock via same-thread reentrant lock acquisition** in either
  `execute_invokestatic` or `execute_invokevirtual_vtable_fast` — confirmed
  by reading both functions line-by-line; guards are dropped before any
  nested acquisition of the same lock on the same call stack.
- **`mem::forget`/`ManuallyDrop`/`RwLockWriteGuard::leak()`/`unlock_fair()`
  misuse** — grepped the whole `vm/src` and `classloading/src` trees; no
  such calls exist on `class_manager` or `vtable_manager`.
- **A third/different lock in the cycle** — the two live captures each show
  only two contending threads (plus unrelated sleeping/idle threads); no
  third synchronization primitive (e.g. the per-class-name
  `class_loading_locks` mutex/condvar in `load_class_concurrent`) appears in
  either blocked thread's backtrace.

## Not attempted this session (next step for whoever picks this up)

Reducing `execute_invokestatic`'s read-lock hold time/frequency is the most
promising fix, e.g. by caching the "does this call site's target have a
native override higher in its hierarchy" result per `(ClassId, cp_index)`
the same way `execute_invokevirtual_vtable_fast` already does via
`thread.native_shadow_cache` / `remember_vtable_native_shadow`
(`vm/src/runtime/interpreter.rs:26365-26388`). This was deliberately NOT
attempted this session — the codebase has a documented history of
correctness bugs in exactly this kind of native-dispatch caching under
class redefinition/hot-reload (see memory
`duplicate-native-registrations-verify-which-wins` and
`redefine-structural-check-order-sensitive`), so a cache here needs to be
invalidated correctly on `redefine_class` and reviewed carefully rather than
added under time pressure. An alternative, more general fix would apply
`RwLockReadGuard::unlock_fair()` semantics to hot, frequently-contended
`class_manager` read sites — parking_lot exposes this only via manual guard
handling (not the RAII `Drop` path used everywhere today), so that would be
a broader refactor across ~221 existing read/write call sites
(`vm.rs`/`vm_init.rs`/`vm_exec.rs`/`interpreter.rs`), too large to attempt
safely in one session.

## Reproduction

```bash
# Azure host, isolated worktree off dev with the AB-BA fix (commit caa4ee65)
# already applied on top:
cd /data/data/wt-hc-vtable-abba-0706c
CP="$(cat /data/data/spring-framework-shared/spring-web/build/cratonvm-testcp.txt)"
for i in $(seq 1 20); do
  timeout 30 ./target/release/cratonvm --java-home /data/data/jdk25-real \
    -cp "/data/data/spring-suite-runner-shared:$CP" \
    KRun org.springframework.http.client.HttpComponentsClientHttpRequestFactoryTests
done
# ~20-25% of runs hang (down from ~50-65% before the AB-BA fix).
# Capture a live hang with:
#   sudo gdb -p <pid> -batch -ex 'thread apply all bt' > cap1.txt
#   sleep 2
#   sudo gdb -p <pid> -batch -ex 'thread apply all bt' > cap2.txt
#   diff cap1.txt cap2.txt   # identical => genuine stall, not a snapshot artifact
```
