# `Executors.new*ThreadPool()` — `.shutdown()`/`.shutdownNow()` NPEs on `mainLock` (same bug class as `execute()`)

Status: FIXED

Severity: Medium (any code that calls `.shutdown()`/`.shutdownNow()` on an `Executors`-factory
executor rather than just letting it be GC'd)
First confirmed: 2026-07-10, while verifying the fix in
`docs/internal/threadpoolexecutor-execute-npe-on-ctl-regression-FIXED.md`

## Symptom (before fix)

```java
ExecutorService fixed = Executors.newFixedThreadPool(2);
fixed.execute(() -> {});
fixed.shutdown();
```

threw immediately on `.shutdown()`:

```text
Exception in thread "main" java/lang/NullPointerException: Cannot invoke "java.util.concurrent.locks.ReentrantLock.lock()" because "mainLock" is null
	at java/util/concurrent/ThreadPoolExecutor.shutdown(ThreadPoolExecutor.java:1392)
```

## Root cause and fix

Same root cause as
`docs/known-issues/elasticsearch-suite/ES-FAIL-20260710-executors-factory-synthetic-mainlock-npe.md`
(now `.../ES-FAIL-20260710-executors-factory-synthetic-mainlock-npe-FIXED.md`), filed and fixed
independently and concurrently in this session: `Executors.new*ThreadPool()` allocated a
real-shaped `ThreadPoolExecutor` (`alloc_concurrent_synthetic`) but only ever populated a 2-field
legacy layout, leaving `mainLock`/`ctl`/`workQueue`/`workers`/`termination` null.

This doc's own "fix sketch" proposed narrowing `native-api/src/registry.rs`'s
`drop_real_layout_synthetic` drop clause to a method-name-scoped exemption (the same two-layer
dispatch-time pattern `306cd352`/`fix/tpe-npe-dispatch-20260710` used for `execute()`). The fix
that actually landed instead took the other direction sketched in the companion doc: make
`Executors.newFixedThreadPool`/`newCachedThreadPool`/`newSingleThreadExecutor` drive the real
`ThreadPoolExecutor(...)` constructor via `invoke_special`
(`initialize_real_thread_pool_executor` in `native-builtins/src/phases_early.rs`), so these
objects are genuinely real from construction. That makes `306cd352`'s existing receiver-aware
`workers`-populated check (already used by `native_es_shutdown`/`native_es_shutdown_now`, per this
doc's own root-cause section) correctly identify them as real without any registry.rs/interpreter.rs
changes needed — `shutdown()`/`shutdownNow()`/`isShutdown()`/`isTerminated()`/`awaitTermination()`
now all run real bytecode end-to-end and reap real workers correctly.

## Verification

See the verification section in the companion doc
(`ES-FAIL-20260710-executors-factory-synthetic-mainlock-npe-FIXED.md`) — the same `ExecProbe2.java`
standalone probe and ES `storedscripts` suite run cover `shutdown()`/`shutdownNow()`/
`awaitTermination()`/`isShutdown()`/`isTerminated()` directly, including `shutdownNow()` correctly
interrupting a blocked worker thread (confirming real worker reaping, not just a flag flip).

`submit`/`invokeAll`/`invokeAny` (this doc's open question about whether they're "equally exposed")
are also covered by the same fix and verified working via `ExecProbe2`'s `submit()`/`Future.get()`
checks.
