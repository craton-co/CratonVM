# `Executors.newSingleThreadExecutor()`/`newFixedThreadPool()`/`newCachedThreadPool()` — `.execute()` NPEs on `ctl` (regression)

Status: FIXED — 2026-07-10, branch `fix/tpe-npe-dispatch-20260710`, root-caused live via gdb per this doc's own recommendation
Severity: High (broad blast radius — any code using these three `Executors` factories and calling `.execute()`)
First confirmed: 2026-07-10, while investigating `wildfly-domain-heap-corrupt-value-timeout.md`

## 2026-07-10 update (later same day) — extended to `submit()`/`shutdown()`; the "real pool loses async semantics" oddity explained and fixed

A separate, parallel session (branch `fix/wildfly-hib32-gate-20260710`, working the same gating
regression from `wildfly-domain-heap-corrupt-value-timeout.md`, independently) converged on this
exact same root cause before discovering this fix had already landed, and merged a follow-up on
top of it:

- **Root cause of the "genuinely real `ThreadPoolExecutor` didn't run truly async" oddity flagged
  below**: `try_stackless_invoke`'s own direct native-registry lookup (`vm/src/runtime/
  interpreter.rs`) is a **fourth** dispatch point that reaches `native_es_execute` unconditionally —
  not one of the three this fix's receiver-aware exemptions patched
  (`intercept_force_registered_native`, `invoke_or_native`, `invoke_on_class_shared_inner`). A real
  receiver reaching native via that path hit this fix's "defense in depth" branch in
  `native_es_execute`, which ran the task inline/synchronously instead of via real bytecode —
  exactly the discrepancy this doc's last paragraph observed between a pristine and a fixed build.
- **Fix**: rather than patching a fifth dispatch point, the registry-level drop
  (`../../../native-api/src/registry.rs`) is now removed entirely for `java/util/concurrent/
  ThreadPoolExecutor` (not just `execute(Runnable)`), and the real-vs-synthetic decision moved
  fully into the native callbacks themselves via a new `NativeContext::invoke_virtual_bytecode_only`
  (bypasses every native check, calling `interpreter::execute` directly — `invoke_on_class_shared`
  was tried first and found to have its own unconditional native re-check for concrete declaring
  classes, which reintroduced the recursion this fix's defense-in-depth guarded against). This
  covers `native_es_execute` regardless of which of the (now at least four) dispatch paths reaches
  it, with genuine real-bytecode dispatch instead of a synchronous fallback — and applies the same
  per-instance `executor_has_real_workers` pattern to `submit(Runnable)`/`submit(Callable)`/the
  `shutdown` closures, closing
  `docs/internal/threadpoolexecutor-shutdown-npe-on-mainlock-synthetic-executor-FIXED.md` (this
  doc's sibling) in the same push.
- This fix's own `intercept_force_registered_native`/`invoke_or_native`/
  `invoke_on_class_shared_inner` receiver-exemption checks and the `force_native_over_real_jdk_bytecode`
  allowlist entry for `execute` are left in place — harmless (they just make some paths reach
  bytecode slightly earlier, without ever reaching native at all) and require no changes.
- The now-redundant synchronous-inline defense-in-depth branch inside `native_es_execute` was
  removed (dead code once the earlier `invoke_virtual_bytecode_only` check unconditionally returns
  first for a real receiver).

Re-verified with this doc's own `ExecProbe.java`, an equivalent of the extended repro
(`newFixedThreadPool`/`newCachedThreadPool`/`submit()`), a `submit()`+`shutdown()` repro, and a
genuinely-real `new ThreadPoolExecutor(...)`: `execute()` now runs the task on a real worker thread
(not synchronously on the caller) and `shutdown()`/`awaitTermination()` complete normally.
`cargo test -p cratonvm-native-api --lib` (179/179) and `cargo test -p cratonvm-native-builtins
--lib` (2964/2965 passed — 1 pre-existing, unrelated `ByteBuffer` failure confirmed present on
unmodified `dev` too) both pass.

## Symptom (recap)

```java
ExecutorService es = Executors.newSingleThreadExecutor();
es.execute(() -> System.out.println("ran on " + Thread.currentThread()));
```

threw `NullPointerException: Cannot invoke "java.util.concurrent.atomic.AtomicInteger.get()"
because "this.ctl" is null` from real `ThreadPoolExecutor.execute()` bytecode, because
`Executors.new*ThreadPool()` allocate a synthetic 2-field object stamped with the real class
name `java/util/concurrent/ThreadPoolExecutor` but never run it through the real `<init>`.

## Root cause, actually pinned down this time

A live `gdb` session (per this doc's own "Recommended next steps") breaking on
`cratonvm_vm::runtime::exceptions::create_exception_object` and walking the Rust backtrace
showed the call chain for `es.execute(...)` is:

```
Vm::invoke -> invoke_shared -> invoke_on_class_shared -> invoke_on_class_shared_inner
  -> execute (interpreter) -> execute_frame
    (invokeinterface `ExecutorService.execute`, handled INLINE by execute_frame's dispatch loop)
    -> execute_invoke_kind -> intercept_force_registered_native
       -> should_force_registered_native_over_bytecode
          -> force_native_over_real_jdk_bytecode  [class/method/descriptor allowlist]
```

This confirmed the prior session's diagnosis (`af1f3a45` "Force native ThreadPoolExecutor.execute()
over real bytecode") had the *right* fix location, but it reported "no effect" because of a second,
independent bug that made the fix inert:

1. **`../../../native-api/src/registry.rs`'s `NativeMethodRegistry::register()`** has a blanket
   `self.drop_real_layout_synthetic && class_name == "java/util/concurrent/ThreadPoolExecutor"` guard
   (added 2026-06-17, commit `f157de8a`, intended only for the lifecycle/stat natives —
   `shutdown`/`shutdownNow` — so a *genuinely real* `ThreadPoolExecutor`'s real bytecode can reap its
   own workers). The guard has **no method-name filter**, so it also silently drops `execute`'s
   registration. `drop_real_layout_synthetic` is unconditionally `true` in the **default**
   `cratonvm-cli` build (`../../../vm/src/vm/vm_init.rs`, the `#[cfg(not(feature = "synthetic-jdk"))]` arm —
   "Real JDK mode (the default cratonvm-cli build)") — so even without `--java-home`/`CRATONVM_REAL`,
   `native_es_execute` was **never actually registered**, and forcing it in
   `force_native_over_real_jdk_bytecode` was a no-op: `shared.native_methods.find(...)` returned
   `None` regardless.
2. Even after un-dropping the registration and forcing it, a **second** bug surfaced: forcing native
   unconditionally for *every* `ThreadPoolExecutor.execute()` call — including a genuinely real,
   bytecode-constructed instance — breaks real pools (loses real queue/pool-size semantics) and,
   worse, **infinitely recurses** for `native_es_execute`'s own internal use case:
   `spawn_runnable_on_real_thread`'s shared singleton async worker pool (`async_worker_pool`,
   itself a real `ThreadPoolExecutor` built via `ctx.new_object_initialized(...)`) calls
   `pool.execute(task)` — which, forced unconditionally, calls `native_es_execute` again, which
   calls `spawn_runnable_on_real_thread` again, forever (confirmed via gdb: real SIGSEGV/stack
   overflow, not a hang).

## Fix

Four coordinated changes, all gated on the receiver's real `workers` field (`HashSet<Worker>`,
populated only by the real `<init>`; CratonVM's synthetic factories never set it — the same
"real vs. synthetic instance sharing one real class name" check already used by
`native_es_shutdown`/`executor_has_real_workers`):

1. `../../../native-api/src/registry.rs`: narrow the `drop_real_layout_synthetic` ThreadPoolExecutor guard
   to exempt `execute(Runnable)` specifically — it stays registered even in real-JDK mode.
2. `../../../vm/src/runtime/interpreter.rs`: add the `force_native_over_real_jdk_bytecode` allowlist entry
   for `ThreadPoolExecutor.execute(Runnable)` (this part matches the reverted `af1f3a45` attempt),
   and make `intercept_force_registered_native` (the `execute_invoke_kind` call site) skip forcing
   when the receiver has a populated real `workers` field — a genuinely real pool keeps running its
   own real bytecode.
3. `../../../vm/src/vm/vm_exec.rs`: the SAME receiver-aware exemption, independently, at the two OTHER
   dispatch points that can also route `ThreadPoolExecutor.execute()` through the native registry
   (`invoke_or_native`, used when native Rust code calls back into the VM via
   `NativeContext::invoke_virtual`; and `invoke_on_class_shared_inner`'s own
   `should_force_registered_native_over_bytecode` consult) — both are separate code paths from
   `execute_invoke_kind`/`intercept_force_registered_native` and needed their own copy of the check.
4. `../../../native-builtins/src/lib.rs`'s `native_es_execute`: a defense-in-depth guard — if this native is
   EVER reached for a receiver with real `workers` populated anyway (in case some other, not-yet-found
   dispatch path also forces it), run the task inline instead of recursing through
   `spawn_runnable_on_real_thread` again. This is what actually stops the stack overflow; (2) and (3)
   are what keep a real pool's `execute()` running genuine real bytecode (true async semantics)
   instead of degrading to this synchronous fallback.

Verified (debug AND release builds): `ExecProbe.java` (this doc's original repro) and an extended
repro covering `newFixedThreadPool`/`newCachedThreadPool`/`submit()` on synthetic executors, plus a
plain `new ThreadPoolExecutor(...).execute()` on a genuinely real instance (confirmed dispatched to
real bytecode via the receiver check, not recursing/crashing). `cargo test -p cratonvm-native-api`
(179/179) and `cargo test -p cratonvm-vm --lib` (2190/2192, the 2 failures pre-exist on `dev` tip
unrelated to this change — a stale native-count threshold and an unrelated system-streams test).

**Residual, separately filed:** the stack-trace corruption noted in this doc's original write-up
(the impossible `ScheduledThreadPoolExecutor.execute -> schedule -> delayedExecute -> isShutdown`
frame order) did not reproduce with a fresh debug build in this session — the debug-build stack
trace was internally consistent (`ExecProbe.main` -> `ThreadPoolExecutor.execute`). Likely specific
to a release/JIT build's frame reconstruction; not re-investigated here since the underlying NPE
(which made the trace meaningful in the first place) is fixed. A separate, pre-existing bug
surfaced while verifying this fix (confirmed present on pristine `dev` before this change too, same
root cause — the June-17 `drop_real_layout_synthetic` blanket drop has no method-name filter) and is
filed on its own: `docs/known-issues/threadpoolexecutor-shutdown-npe-on-mainlock-synthetic-executor.md`.

**Observed but NOT filed as a confirmed distinct bug:** a genuinely real `new ThreadPoolExecutor(...)`
instance's `execute()` did not appear to run its task on a truly concurrent worker thread in either
this fix's build or a fresh pristine-`dev` build (both dispatch to the same real bytecode either
way, confirmed via receiver-side tracing) — pristine showed the task never completing inside a 5s
wait, this fix's build showed it completing synchronously on the caller thread. The two builds'
symptoms differed enough (timeout vs. immediate synchronous run) that this looks like a pre-existing,
timing/state-sensitive characteristic of CratonVM's real `Thread.start()`/worker-thread execution
under real bytecode — not something this change's dispatch-routing logic controls (dispatch was
verified identical: real bytecode either way) — but it was not isolated further here. Worth a
dedicated investigation if real (not synthetic-factory) `ThreadPoolExecutor` concurrency semantics
matter for an upcoming suite.
