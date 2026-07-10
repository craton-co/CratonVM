# `Executors.newSingleThreadExecutor()`/`newFixedThreadPool()`/`newCachedThreadPool()` — `.execute()` NPEs on `ctl` (regression)

Status: RESOLVED (2026-07-10) — see "Root cause found and fixed" below. Moved to
`docs/internal/fixed-suite-bugs/`.
Severity: High (broad blast radius — any code using these three `Executors` factories and calling `.execute()`)
First confirmed: 2026-07-10, while investigating `wildfly-domain-heap-corrupt-value-timeout.md`

## 2026-07-10 update — root cause found and fixed; the earlier bisection to `f28d6ae6` was a red herring

The actual root cause has nothing to do with `f28d6ae6` (the EnumSet/STPE fix the
earlier bisection in this same doc blamed). It's a registration-time gate in
`native-api/src/registry.rs::NativeMethodRegistry::register()`, added by
`f157de8a` (2026-06-17, "real-JDK executor shutdown reaps workers"), that
unconditionally dropped **every** native registered on class name
`java/util/concurrent/ThreadPoolExecutor` in real-JDK mode — including
`execute`/`submit`/`shutdown`, not just the lifecycle/stat methods its own
comment named. The gate's intent was sound (a genuinely-real
`new ThreadPoolExecutor(...)` object should run real bytecode so
`shutdownNow()` actually interrupts workers), but it can't distinguish a real
object from CratonVM's own synthetic 2-field placeholder — `Executors.
newSingleThreadExecutor()`/`newFixedThreadPool()`/`newCachedThreadPool()`
(`native-builtins/src/lib.rs` AND a duplicate registration in
`native-builtins/src/phases_early.rs::register_scheduled_executor_natives`,
which wins the registration race) stamp their placeholder with the exact
same class name via `alloc_concurrent_synthetic`. Because the gate runs once
at registration time, not per-object, it silently starved the synthetic
placeholder's own `execute()`/`submit()`/`shutdown()` overrides too — every
call fell through to real inherited bytecode dereferencing a never-initialized
`ctl` (`AtomicInteger`) field.

This is the exact same bug an independent, parallel Elasticsearch-suite
investigation found and documented the same day (before this fix landed):
`docs/known-issues/elasticsearch-suite/ES-FAIL-20260710-executors-factory-synthetic-mainlock-npe.md`
("Layer 2"), including the same two candidate fix directions. This fix
implements that doc's suggested option 2 (move the check from
registration-time to dispatch-time) — see that doc for the independent
confirmation and its own repro.

**Fix** (branch `fix/wildfly-hib32-gate-20260710`):
1. Removed the blanket `class_name == "java/util/concurrent/ThreadPoolExecutor"`
   drop from `NativeMethodRegistry::register()` — `execute`/`submit`/`shutdown`
   are registered again in real-JDK mode, for every receiver regardless of
   real-vs-synthetic.
2. Added `NativeContext::invoke_virtual_bytecode_only` (default impl falls
   back to `invoke_virtual`, so every other context — mocks, tests — is
   unaffected). The `NativeContextImpl` override calls
   `interpreter::execute` directly — **not** `invoke_on_class_shared`, which
   turned out to have its own unconditional native-registry re-check
   (`override_cb` in `invoke_on_class_shared_inner`, vm_exec.rs) for any
   concrete (non-interface) declaring class, independent of the
   `check_override` gate — routing through it reintroduced the exact same
   infinite recursion this fix set out to remove (confirmed via a
   depth-counter probe: `native_es_execute` called itself on the same real
   receiver until the native stack overflowed, `EXCEPTION_STACK_OVERFLOW`).
   `interpreter::execute` is the actual "just run this bytecode, no native
   check" primitive `invoke_on_class_shared_inner` itself falls back to —
   confirmed correct with the same depth probe (depth never exceeds 1: one
   call on the synthetic placeholder, one on the real async pool it hands
   work to).
3. `native_es_execute`/`native_es_submit_runnable`/`native_es_submit_callable`
   and both `shutdown` closures (registered on `es` and `tp`) now check
   `executor_has_real_workers(ctx, this)` per-instance: real → forward to
   `invoke_virtual_bytecode_only`; synthetic → unchanged existing behavior.

**Verified** (standalone probes, no WildFly involved, real-JDK mode,
`cratonvm-wildfly-hib32-gate-20260710.exe`):
- This doc's own `ExecProbe.java` repro: no NPE, prints `DONE`, clean exit.
- The ES doc's repro (`newFixedThreadPool(4)` + `submit()` + `shutdown()`):
  no NPE, task runs, clean exit.
- A genuinely-real `new ThreadPoolExecutor(...)` (2 core threads, real
  `LinkedBlockingQueue`): `execute()` runs the task on a real worker thread,
  `shutdown()`/`awaitTermination()` complete normally — confirms the
  original `f157de8a` intent (real objects get real bytecode) still holds.
- `cargo test -p cratonvm-native-api` (registry unit tests, including the
  pre-existing STPE-drop coverage this change didn't touch) — see the merge
  commit for the exact pass count.

**Not fixed by this change (separate, pre-existing, out of scope):** a
synthetic `newFixedThreadPool()`'s `submit(Runnable)` still hands back a
synthetic 2-field `FutureTask` placeholder whose `get(timeout, unit)` — if a
caller actually blocks on it — runs real `FutureTask` bytecode against fields
the synthetic path never set, timing out instead of returning immediately.
Unrelated to the `execute()`/`ctl` NPE this doc tracks; not reproduced by
either this doc's or the ES doc's own repro (neither calls `future.get()`).

## Symptom (original report, still accurate for what used to fail)

A minimal, completely standalone repro (no WildFly/app-server involved):

```java
import java.util.concurrent.*;
public class ExecProbe {
    public static void main(String[] a) throws Exception {
        ExecutorService es = Executors.newSingleThreadExecutor();
        es.execute(() -> System.out.println("ran on " + Thread.currentThread()));
    }
}
```

throws immediately on current `dev`:

```text
Exception in thread "main" java/lang/NullPointerException: Cannot invoke "java.util.concurrent.atomic.AtomicInteger.get()" because "this.ctl" is null
	at ExecProbe.main(ExecProbe.java:5)
	at java/util/concurrent/ScheduledThreadPoolExecutor.execute(ScheduledThreadPoolExecutor.java:692)
	at java/util/concurrent/ScheduledThreadPoolExecutor.schedule(ScheduledThreadPoolExecutor.java:549)
	at java/util/concurrent/ScheduledThreadPoolExecutor.delayedExecute(ScheduledThreadPoolExecutor.java:344)
	at java/util/concurrent/ThreadPoolExecutor.isShutdown(ThreadPoolExecutor.java:1384)
```

**Note the stack trace is internally inconsistent** — `ExecProbe.main` is listed
as though it were the innermost frame, "calling" `ScheduledThreadPoolExecutor.
execute` → `schedule` → `delayedExecute` → `ThreadPoolExecutor.isShutdown`, an
order that cannot happen in real code (`isShutdown()` does not call
`delayedExecute()`; `ExecProbe` never references `ScheduledThreadPoolExecutor`
at all — the repro only calls `newSingleThreadExecutor()`). This is itself
either a **separate stack-trace-construction bug** or a strong clue that the
crash site is not what it appears to be. Treat the printed trace as unreliable
for root-causing; it was not resolved in this session (see "Not resolved"
below).

## Why this matters here

This is what blocks `docs/known-issues/wildfly-domain-heap-corrupt-value-timeout.md`
from being re-verified live: WildFly's process-controller reads its Host
Controller child's stdout/stderr/greeting via a background thread spawned as
`readExecutor.execute(this)` (`org.jboss.as.process.protocol.ConnectionImpl$2`,
`readExecutor` being a plain `Executors`-backed pool). That thread now dies
with this exact uncaught `NullPointerException` (confirmed via
`CRATONVM_DBG_UNCAUGHT=1`, which the fix in this same push adds — see
`logmanager.rs`'s `native_level_parse` commit series) before the greeting is
ever processed, so the outer process-controller VM's `main()` blocks forever
waiting on it — surfacing as the pre-existing "T19.H1 watchdog: main thread is
in native (Rust) code" hang.

## Root cause (mechanism)

`native_new_single_thread`/`native_new_fixed_pool`/`native_new_cached_pool`
(`native-builtins/src/lib.rs`, `register_executor_natives`) allocate their
return value via `alloc_concurrent_synthetic(ctx, "java/util/concurrent/
ThreadPoolExecutor", 2)` — i.e. an object stamped with the **real** JDK class
name `java.util.concurrent.ThreadPoolExecutor`, but with only a 2-field
synthetic layout (`poolSize`, `isShutdown`). Real fields the JDK's own
`execute()` bytecode depends on — `ctl` (`AtomicInteger`), `workQueue`,
`mainLock`, `workers` — are never initialized, because the object is never
run through the real constructor.

As long as `.execute(Runnable)` on such an object is dispatched to the
registered native (`native_es_execute`, registered both on
`java/util/concurrent/ExecutorService` and directly on
`java/util/concurrent/ThreadPoolExecutor`), this is harmless — the native
just runs the task immediately on the calling thread, ignoring the unused
real fields. The bug is that dispatch is now landing on the REAL
`ThreadPoolExecutor.execute()` bytecode instead, which immediately does
`int c = ctl.get();` and NPEs on the null `ctl`.

## Bisection (confirmed via fresh rebuilds, not just cached binaries)

- `be605560` (2026-07-09, "Fix WildFly process-controller lifecycle
  blocking" — the last commit that had touched the corrupt-value-timeout
  doc before this session) — **GOOD**. Fresh `cargo build --release`
  directly at this commit: `es.execute(...)` runs the task synchronously on
  the calling thread (`"ran on Thread[...,main,...]"`), matching the
  intended synthetic-executor semantics.
- `f28d6ae6` (2026-07-09 later same day, "Fix EnumSet.of()/allOf() broken
  for non-JDK enums + ScheduledThreadPoolExecutor field corruption") — fresh
  rebuild directly at this commit reproduces the `ctl`-NPE **exactly**.
- `f28d6ae6`'s direct parent is `ba49357e`, a documentation-only merge
  commit — so `f28d6ae6` is the *only* commit in the whole range that could
  have introduced the regression, and it is the only commit in
  `be605560..5f80719e` whose diff contains the literal string
  `ThreadPoolExecutor` (checked via `git log -S'ThreadPoolExecutor'`).
- However — **the actual causal line was not identified.** `f28d6ae6`'s
  real diff only (a) drops the entire `java/util/EnumSet` native surface in
  real-JDK mode via `drop_real_layout_synthetic`, and (b) removes a
  synthetic `ScheduledThreadPoolExecutor.<init>(I,ThreadFactory)V`
  override in `vm_init.rs` (a different constructor overload than the one
  this bug's repro exercises — `newSingleThreadExecutor()` doesn't touch
  `ScheduledThreadPoolExecutor` at all). Neither change is obviously
  connected to `newSingleThreadExecutor().execute()`'s dispatch.

## Not resolved — dispatch path never located

Three separate attempts to force native dispatch back on were made and
reverted after none of them changed observed behavior:

1. Added `("java/util/concurrent/ThreadPoolExecutor", "execute",
   "(Ljava/lang/Runnable;)V")` to `force_native_over_real_jdk_bytecode`
   (`vm/src/runtime/interpreter.rs`) — the mechanism that
   `execute_invokevirtual_vtable_fast`'s Step 4 and
   `invoke_on_class_shared_inner` both consult. **No effect.**
2. Added `CRATONVM_DBG_TPEXEC=1`-gated tracing inside
   `invoke_on_class_shared_inner` (`vm/src/vm/vm_exec.rs`) — **never
   printed**, meaning that function is never reached for this call.
3. Added the same tracing at `execute_invokevirtual_vtable_fast`'s entry,
   its `direct_native_shadow` check, and its Step-4 native-vs-bytecode
   decision (`vm/src/runtime/interpreter.rs`) — **none of these printed
   either**, meaning that function is *also* never reached.

So neither of the two dispatch functions this investigation could find are
actually responsible for resolving `es.execute(...)` in the failing repro.
Combined with the corrupted/impossible stack trace above, the likely
explanation is a **third, not-yet-located dispatch path** (possibly
JIT-related, or a vtable-cache population bug that installs the wrong
resolved method/declaring-class for this exact (class, method, descriptor)
independent of the per-call dispatch checks) — but this was not confirmed.
All three speculative changes were reverted (see
`fix/wildfly-hib32-residuals-20260710`'s "Revert unproven..." commit) since
they had no verified effect and forcing native unconditionally for
`ThreadPoolExecutor.execute()` carries real risk for genuinely-real
`ThreadPoolExecutor` objects (would make their execution synchronous instead
of async) without evidence it fixes anything.

## Recommended next steps

1. **Use a live debugger, not print-tracing candidate call sites.** Attach
   `gdb`/`lldb` to a paused `ExecProbe` process (or set a breakpoint on the
   `panic`/exception-construction site) to get a real Rust-level backtrace
   at the moment `RuntimeError::NullPointerException` (or whatever
   constructs the "Cannot invoke ... because ... is null" message) is
   raised. That will show definitively which Rust function actually handles
   this call, without needing to guess which of the many invoke/dispatch
   functions in `interpreter.rs`/`vm_exec.rs` is involved.
2. Separately investigate the stack-trace corruption itself — reproduce
   with `CRATONVM_DBG_JITC=1` / deopt tracing to see whether the JIT is
   involved (a single call in a brand-new process seems too cold to be
   JIT-compiled, but the frame order being backwards specifically hints at
   deopt/JIT frame reconstruction, not the plain interpreter).
3. Once the actual dispatch path is identified, either (a) fix it to
   correctly prefer the native for these specific synthetic-layout
   `ThreadPoolExecutor` objects (ideally in a way that's still object-aware,
   e.g. checking a marker so a *real* `new ThreadPoolExecutor(...)` still
   gets real bytecode), or (b) — the more thorough, higher-effort fix — make
   `native_new_single_thread`/`native_new_fixed_pool`/`native_new_cached_pool`
   construct genuinely real `ThreadPoolExecutor` objects via
   `ctx.new_object_initialized` (mirroring the real `Executors` factory
   method bodies), the same pattern `f28d6ae6` itself already used
   successfully for `ScheduledThreadPoolExecutor`. This eliminates the
   native/bytecode split entirely but is a bigger, riskier change (moves
   these executors from synchronous/immediate execution to genuine
   multi-threaded async execution) that needs a full suite regression pass
   before landing, since it changes runtime semantics for very widely-used
   factories.
4. Re-run this doc's `ExecProbe.java` repro once a fix lands, then return to
   `wildfly-domain-heap-corrupt-value-timeout.md` to re-verify the domain.sh
   boot now that `java.util.logging.Level.parse()` (fixed in the same push,
   see `java-util-logging-level-parse-throws-for-all-names-FIXED.md`) no
   longer blocks `host.xml`/`domain.xml` parsing.
