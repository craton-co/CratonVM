# `Executors.new*ThreadPool()` — `.shutdown()`/`.shutdownNow()` NPEs on `mainLock` (same bug class as `execute()`)

Status: OPEN — confirmed on `dev` (both before and after `fix/tpe-npe-dispatch-20260710`), not fixed
Severity: Medium (any code that calls `.shutdown()`/`.shutdownNow()` on an `Executors`-factory
executor rather than just letting it be GC'd)
First confirmed: 2026-07-10, while verifying the fix in
`docs/internal/threadpoolexecutor-execute-npe-on-ctl-regression-FIXED.md`

## Symptom

```java
ExecutorService fixed = Executors.newFixedThreadPool(2);
fixed.execute(() -> {});
fixed.shutdown();
```

throws immediately on `.shutdown()`:

```text
Exception in thread "main" java/lang/NullPointerException: Cannot invoke "java.util.concurrent.locks.ReentrantLock.lock()" because "mainLock" is null
	at ExecProbe2.main(ExecProbe2.java:8)
	at java/util/concurrent/ThreadPoolExecutor.shutdown(ThreadPoolExecutor.java:1392)
```

Same shape as the `execute()`/`ctl` NPE this doc's companion fix addressed: `Executors.new*ThreadPool()`
(`native_new_single_thread`/`native_new_fixed_pool`/`native_new_cached_pool` in
`native-builtins/src/lib.rs`) stamp their return value with the real class name
`java/util/concurrent/ThreadPoolExecutor` but only ever populate a 2-field synthetic layout
(`poolSize`, `isShutdown`). Real `shutdown()` bytecode reads the never-initialized `mainLock`
`ReentrantLock` field and NPEs.

## Root cause

`native-api/src/registry.rs`'s `NativeMethodRegistry::register()` has, since 2026-06-17
(`f157de8a`, "real-JDK executor shutdown reaps workers"):

```rust
if self.drop_real_layout_synthetic
    && class_name == "java/util/concurrent/ThreadPoolExecutor"
{
    return;
}
```

This unconditionally drops **every** native registered on `java/util/concurrent/ThreadPoolExecutor`
in real-JDK mode (which is the default `cratonvm-cli` build regardless of `--java-home` — see the
companion fix doc) — not just the `shutdown`/`shutdownNow`/`isShutdown`/`isTerminated`/
`awaitTermination` lifecycle natives the commit's own message and comments describe. It has no
method-name filter, so it also drops `submit(Runnable)`/`submit(Callable)`/`submit(Runnable,Object)`/
`invokeAll`/`invokeAny` (registered on the same `tp` variable in
`register_executor_natives`) — `execute` was the one already confirmed broken and fixed by the
companion doc; the others registered on `tp` are likely equally exposed for any code path that
reaches them on a synthetic-layout receiver, though not individually reproduced here.

The June-17 commit's INTENT was correct — a genuinely real `ThreadPoolExecutor`'s `shutdown()` must
run real bytecode so it actually reaps its worker threads (the synthetic native only set a flag,
never interrupting real workers). The bug is that the drop is class-scoped, not
class+method-scoped, so it collaterally starves CratonVM's own synthetic executors (which
categorically never have `mainLock`/`workQueue`/etc.) of the native they need.

## Fix sketch (not applied — separate from the `execute()` fix, out of scope for that change)

Mirror the `execute()` fix in the companion doc:
1. Narrow the `native-api/src/registry.rs` drop clause to exclude `shutdown`/`shutdownNow` (or
   whichever of the surface above is actually exercised) so the native stays registered.
2. `native_es_shutdown`/`native_es_shutdown_now` already call `executor_has_real_workers` internally
   (see `native-builtins/src/lib.rs`) to decide whether to touch the synthetic slot vs. reap real
   workers — so unlike `execute()`, these natives are ALREADY receiver-aware. The missing piece is
   purely making sure the native is reachable at all (getting past the registration-time drop and
   whatever dispatch-time force/no-force decision applies) for the synthetic case, while still
   preferring real bytecode for a genuinely real instance — same two-layer pattern as the `execute()`
   fix (registry.rs registration + interpreter.rs force-list + receiver check).
2. Verify `submit`/`invokeAll`/`invokeAny` similarly, and check whether AbstractExecutorService's
   real bytecode `submit()`/`invokeAll()`/`invokeAny()` (which call `this.execute(...)` internally)
   might already work transitively now that `execute()` correctly dispatches to native for synthetic
   receivers — not confirmed either way in this session.
