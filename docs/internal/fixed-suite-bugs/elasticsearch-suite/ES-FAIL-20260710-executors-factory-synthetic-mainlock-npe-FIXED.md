# ES FAIL - Executors.newFixedThreadPool/newCachedThreadPool/newSingleThreadExecutor synthesize half-real objects; real shutdown()/submit() NPE on null mainLock/ctl

Status: RESOLVED (2026-07-10) — Layer 2 fixed. Moved to `docs/internal/fixed-suite-bugs/`.

## 2026-07-10 update (later same day) — Layer 2 fixed, via this doc's own suggested option 2

A separate, parallel investigation (originally chasing
`docs/known-issues/wildfly-domain-heap-corrupt-value-timeout.md`'s gating
`ThreadPoolExecutor.execute()` NPE) independently converged on this exact
same root cause and this doc's own "Suggested fix direction" option 2: move
the `drop_real_layout_synthetic` real-vs-synthetic distinction from
registration time (a per-*class* gate that can't see per-*object* state) to
dispatch time. A narrower, `execute()`-only fix for the same bug had already
landed on `dev` independently by the time this was found (branch
`fix/tpe-npe-dispatch-20260710`) — this generalizes it to cover
`submit()`/`shutdown()` too and fixes a real-executor async-semantics gap
that fix's own doc had flagged as unexplained.

**Fix**, on branch `fix/wildfly-hib32-gate-20260710` (see
`docs/internal/threadpoolexecutor-execute-npe-on-ctl-regression-FIXED.md`
for the full writeup, including both fixes' reconciliation):
1. Removed the `class_name == "java/util/concurrent/ThreadPoolExecutor"`
   registration-time drop from `NativeMethodRegistry::register()` entirely.
2. Added `NativeContext::invoke_virtual_bytecode_only` — a real
   bytecode-only dispatch escape hatch (calls `interpreter::execute`
   directly, bypassing every native check, not just the primary one) — so
   `native_es_execute`/`native_es_submit_*`/the `shutdown` closures can check
   `executor_has_real_workers()` **per instance** at call time and forward a
   genuinely-real receiver to real bytecode instead of relying on the
   registration-time gate.

**Verified** with this doc's own exact repro (`newFixedThreadPool(4)`,
`submit(Runnable)`, `shutdown()`) against
`cratonvm-wildfly-hib32-gate-20260710.exe`: no NPE, task runs, `getClass()`
still correctly reports `java.util.concurrent.ThreadPoolExecutor`, clean
exit. Also verified a genuinely-real `new ThreadPoolExecutor(...)` still
gets real bytecode end-to-end (`execute()` on a real worker thread,
`shutdown()`/`awaitTermination()` complete normally) — the original
`drop_real_layout_synthetic` intent this doc's Layer 2 conflicted with.

Old status (superseded, kept for history):

Status: OPEN (partially fixed — see "What was fixed" below)

Found: 2026-07-10, while retiring 3 `findNative`-crash-family docs for
`org.elasticsearch.action.admin.cluster.storedscripts.{GetScriptContextResponseTests,GetStoredScriptResponseTests,ScriptContextInfoSerializingTests}`.
All three classes' `testConcurrentSerialization`/`testConcurrentHashCode`/`testConcurrentEquals`/
`testConcurrentToXContent` tests (inherited from `org.elasticsearch.test.AbstractWireTestCase`,
which spins up `Executors.newFixedThreadPool(n)` and `.shutdown()`s it once the concurrent checks
finish) hit this bug identically.

## Symptom

```text
java.lang.NullPointerException: Cannot invoke "java.util.concurrent.locks.ReentrantLock.lock()" because "mainLock" is null
```

thrown from real JDK `java.util.concurrent.ThreadPoolExecutor.shutdown()` bytecode (inherited,
not overridden, by the object `Executors.newFixedThreadPool(int)` hands back). A minimal
standalone repro (no ES/JUnit involved) shows the same family of NPE one step earlier, on
`submit()`/`execute()`:

```text
java.lang.NullPointerException: Cannot invoke "java.util.concurrent.atomic.AtomicInteger.get()" because "this.ctl" is null
	at ExecProbe.main(ExecProbe.java:9)
	at java/util/concurrent/AbstractExecutorService.submit(AbstractExecutorService.java:123)
	at java/util/concurrent/ThreadPoolExecutor.execute(ThreadPoolExecutor.java:1362)
```

Repro (`--java-home /usr/lib/jvm/java-21-openjdk-amd64`, `-cp .`, real-JDK mode, reproduces with
JIT on or `-Xint`):

```java
import java.util.concurrent.*;
public class ExecProbe {
    public static void main(String[] args) throws Exception {
        ExecutorService es = Executors.newFixedThreadPool(4);
        es.submit(() -> System.out.println("task ran"));   // NPEs here now
        es.shutdown();                                      // NPE'd here before the class-tag fix below
    }
}
```

## Root cause (two layers)

**Layer 1 (fixed this session)**: `native-builtins/src/phases_early.rs`'s `Executors` factory
registrations — `newFixedThreadPool(I)`, `newCachedThreadPool()`, `newCachedThreadPool(ThreadFactory)`,
`newSingleThreadExecutor()` — were copy-pasted from the `newScheduledThreadPool`/
`newSingleThreadScheduledExecutor` registrations immediately above them in the same function, and
never had their allocated class corrected: each one called
`alloc_concurrent_synthetic(ctx, "java/util/concurrent/ScheduledThreadPoolExecutor", 2)` instead
of `"java/util/concurrent/ThreadPoolExecutor"`. Real JDK `newFixedThreadPool()`/
`newCachedThreadPool()`/`newSingleThreadExecutor()` never return an STPE. Fixed by correcting all
four class-tag strings to `java/util/concurrent/ThreadPoolExecutor` (matching the sibling,
currently-dead registration `native_new_fixed_pool` in `native-builtins/src/lib.rs`). Verified via
the standalone probe: `es.getClass()` now correctly reports
`class java.util.concurrent.ThreadPoolExecutor`, and failure stack traces now show a sane real
call chain instead of a nonsensical `ScheduledThreadPoolExecutor.submit -> schedule ->
delayedExecute -> isShutdown` one.

This alone did not fix the NPE (see Layer 2) but is a genuine, narrow, low-risk correctness fix
in its own right (anything doing `instanceof ScheduledThreadPoolExecutor` or calling an
STPE-specific method like `.schedule()` on a plain `newFixedThreadPool()` result was previously
succeeding nonsensically instead of failing the way real JDK code would expect).

**Layer 2 (still OPEN)**: even correctly tagged as `ThreadPoolExecutor`, the object created by
`alloc_concurrent_synthetic(ctx, "java/util/concurrent/ThreadPoolExecutor", 2)` never runs the
real `ThreadPoolExecutor` constructor, so its real fields (`ctl`, `workQueue`, `mainLock`,
`workers`, `termination`, …) are never initialized — only two hand-picked slots (`poolSize`,
`shutdown` flag) are set, matching a *fake* 2-field layout that has nothing to do with the real
class's actual field layout.

Under normal circumstances this would be handled gracefully: `native-builtins/src/lib.rs`
registers `tp.shutdown()`/`tp.shutdownNow()`/`tp.isShutdown()`/etc. natives that call
`executor_has_real_workers()` (checks whether `get_field_by_name(this, "workers")` is a real,
non-null object) and take a synthetic-safe branch when it is not — see
`native-builtins/src/lib.rs:57229` (`executor_has_real_workers`) and its callers around
`native-builtins/src/lib.rs:56737` / `:56779` (`es`/`tp` `.shutdown()` registrations).

However, `native-api/src/registry.rs`'s `NativeMethodRegistry::register()` has a *registration-time*
gate, `drop_real_layout_synthetic`, that unconditionally drops **every** synthetic
`ThreadPoolExecutor` lifecycle/stat native (`shutdown`, `shutdownNow`, `isShutdown`,
`isTerminated`, `awaitTermination`, `getPoolSize`, `getActiveCount`, …) whenever
`class_name == "java/util/concurrent/ThreadPoolExecutor"`, on the assumption that *any*
`ThreadPoolExecutor`-tagged object is a genuinely real one whose real bytecode can run safely
end-to-end — see `native-api/src/registry.rs`, the `if self.drop_real_layout_synthetic &&
class_name == "java/util/concurrent/ThreadPoolExecutor" { return; }` block (search for
`drop_real_layout_synthetic` in that file). That assumption is correct for a real
`new ThreadPoolExecutor(...)` call (which DOES run the real constructor, and is the case the gate
was added for — see its own comment about `shutdownNow` not interrupting real workers), but is
**false** for CratonVM's own `Executors.*` factory shortcuts, which hand back a same-named but
field-uninitialized object. Because the gate drops these natives at registration time (not
per-object at dispatch time), the `executor_has_real_workers()` synthetic-safe branch in
`lib.rs` can never run for a `ThreadPoolExecutor`-tagged receiver at all once
`drop_real_layout_synthetic` is enabled — dispatch always falls through to real inherited
bytecode, which unconditionally NPEs on the never-initialized `ctl`/`mainLock`.

This is a **pre-existing** conflict between two independently-reasonable fixes
(`drop_real_layout_synthetic`, added so real `new ThreadPoolExecutor(...)` objects get real
`shutdownNow()` worker-interruption semantics — see its comment for the `@Timeout(SEPARATE_THREAD)`
false-hang rationale — and the `Executors.*` factory shortcuts, added independently) that never
worked together correctly; it is not something the Layer-1 class-tag fix introduced or could fix
by itself. (Confirmed: with the WRONG `ScheduledThreadPoolExecutor` tag from before the Layer-1
fix, the same class of NPE occurred via a different mechanism — the `synthetic-jdk`-gated
`register_executors_scheduled_natives` in `native-collections/src/lib.rs` is compiled out of
real-JDK-mode builds entirely, so no STPE-specific native existed either, and real STPE bytecode
ran against the same uninitialized fields.)

## Suggested fix direction (not attempted this session)

Either:
1. Make `Executors.newFixedThreadPool`/`newCachedThreadPool`/`newSingleThreadExecutor` actually
   construct a real `ThreadPoolExecutor` by invoking its real `<init>` (with a real
   `LinkedBlockingQueue`/`SynchronousQueue` work queue and a real `ThreadFactory`), the same
   fix pattern already used for `ScheduledThreadPoolExecutor` construction via `new
   ScheduledThreadPoolExecutor(...)` bytecode (see
   `docs/internal/tomcat-suite-bugs/11-stpe-mainlock-npe-teardown-regression.md`) — this is the
   most faithful fix and would make `drop_real_layout_synthetic`'s assumption actually hold; or
2. Give `alloc_concurrent_synthetic`-created "fake-real" objects a side-table/marker that
   `drop_real_layout_synthetic`'s per-class registration-time gate cannot see (since it runs once
   at startup, not per-object) — this would require moving the gate from registration-time to
   dispatch-time (i.e. deleting it and instead threading `executor_has_real_workers()`-style
   checks into the interpreter's native-dispatch fast paths), a larger change with a wider blast
   radius across `vm/src/runtime/interpreter.rs`.

Not attempted in this session: both directions are substantially larger and riskier than the
narrow Layer-1 class-tag fix, and `Executors.newFixedThreadPool`/`newCachedThreadPool`/
`newSingleThreadExecutor` are extremely widely used across the whole test suite (and presumably
production code paths), so a wrong fix here has a large blast radius. Filing this as its own OPEN
doc rather than guessing.

## Reproduction

```powershell
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1 -Category all -Jit on -Vm craton -ElasticsearchRoot "<es-checkout>" -WorkDir "<workdir>" -Exe <cratonvm-binary> -JdkHome /usr/lib/jvm/java-21-openjdk-amd64 -TimeoutSec 600 -RunName repro-execnpe -ModeName repro-execnpe -Start 331 -Count 1
```

(`-Start 331` selects `ScriptContextInfoSerializingTests` in a freshly-generated `-Category all`
list built from the ES checkout at
`/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch` as of
2026-07-10 — any of the three `storedscripts` classes retired alongside this doc reproduces it
identically, as would essentially any `AbstractWireTestCase` subclass's `testConcurrentX` tests.)

Collection context:
- Host: `victor@20.83.144.174`
- Worktree: `/data/data/wt-es-storedscripts-retire-20260710`, branch `fix/es-storedscripts-retire-20260710`
- Binary base dev SHA: `5f80719eb11c9c1843fb6baba7c39863c7f3ce75`
- Binary (Layer-1-fixed): `/data/data/cratonvm-targets/es-storedscripts-retire-20260710/release/cratonvm-es-storedscripts-retire-20260710`
- Standalone probe: `/tmp/execprobe/ExecProbe.java` on the collection host
