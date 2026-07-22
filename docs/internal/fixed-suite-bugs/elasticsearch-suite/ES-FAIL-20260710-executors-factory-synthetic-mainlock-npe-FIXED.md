# ES FAIL - Executors.newFixedThreadPool/newCachedThreadPool/newSingleThreadExecutor synthesize half-real objects; real shutdown()/submit() NPE on null mainLock/ctl

Status: FIXED

Found: 2026-07-10, while retiring 3 `findNative`-crash-family docs for
`org.elasticsearch.action.admin.cluster.storedscripts.{GetScriptContextResponseTests,GetStoredScriptResponseTests,ScriptContextInfoSerializingTests}`.
All three classes' `testConcurrentSerialization`/`testConcurrentHashCode`/`testConcurrentEquals`/
`testConcurrentToXContent` tests (inherited from `org.elasticsearch.test.AbstractWireTestCase`,
which spins up `Executors.newFixedThreadPool(n)` and `.shutdown()`s it once the concurrent checks
finish) hit this bug identically.

## Symptom (before fix)

```text
java.lang.NullPointerException: Cannot invoke "java.util.concurrent.locks.ReentrantLock.lock()" because "mainLock" is null
```

thrown from real JDK `java.util.concurrent.ThreadPoolExecutor.shutdown()`/`submit()`/`execute()`
bytecode (inherited, not overridden) on the object `Executors.newFixedThreadPool(int)` (and
siblings) handed back.

## Root cause (two layers, both fixed)

**Layer 1 (fixed 2026-07-10, earlier in this doc's history)**: `native-builtins/src/phases_early.rs`'s
`Executors` factory registrations — `newFixedThreadPool(I)`, `newCachedThreadPool()`,
`newCachedThreadPool(ThreadFactory)`, `newSingleThreadExecutor()` — were copy-pasted from the
`newScheduledThreadPool`/`newSingleThreadScheduledExecutor` registrations immediately above them
and never had their allocated class corrected: each one called
`alloc_concurrent_synthetic(ctx, "java/util/concurrent/ScheduledThreadPoolExecutor", 2)` instead of
`"java/util/concurrent/ThreadPoolExecutor"`. Fixed by correcting all four class-tag strings.

**Layer 2 (fixed 2026-07-10)**: even correctly tagged as `ThreadPoolExecutor`, the object created by
`alloc_concurrent_synthetic(ctx, "java/util/concurrent/ThreadPoolExecutor", 2)` never ran the real
`ThreadPoolExecutor` constructor, so its real fields (`ctl`, `workQueue`, `mainLock`, `workers`,
`termination`, …) were never initialized — only two hand-picked legacy slots (`poolSize`, `shutdown`
flag) were set. `native-api/src/registry.rs`'s `drop_real_layout_synthetic` registration-time gate
drops every synthetic `ThreadPoolExecutor` lifecycle/stat native whenever
`class_name == "java/util/concurrent/ThreadPoolExecutor"`, on the assumption that any
`ThreadPoolExecutor`-tagged object is genuinely real — true for a direct `new ThreadPoolExecutor(...)`
call, but false for these factory shortcuts, so real inherited bytecode NPE'd on the
never-initialized fields.

**Fix**: `native-builtins/src/phases_early.rs` gained `initialize_real_thread_pool_executor`, which
drives the real `ThreadPoolExecutor(int, int, long, TimeUnit, BlockingQueue[, ThreadFactory])V`
constructor via `invoke_special` (with a real `LinkedBlockingQueue`/`SynchronousQueue` and, when no
factory is supplied, the real JDK `Executors.defaultThreadFactory()`/`defaultHandler` internally) —
the exact pattern `initialize_real_scheduled_thread_pool_executor` already used for
`ScheduledThreadPoolExecutor`. All four `Executors.*` factories now call this instead of poking two
legacy slots, so they return genuinely real objects and `drop_real_layout_synthetic`'s assumption
actually holds for them.

**Residual (fixed same session)**: `ThreadFactory.newThread(Runnable)`
(`native-builtins/src/lib.rs`, `register_executor_natives`) had the identical bug one level down —
it allocated a real-shaped `Thread` via `alloc_concurrent_synthetic` but poked 5 legacy slots
instead of running the real `Thread(Runnable)` constructor. A `Thread` built that way never runs
`start()`'s real internal state machine, so a genuinely real `ThreadPoolExecutor`'s worker thread
silently never ran submitted tasks even after the pool itself became real. Fixed by driving the
real `Thread(Runnable)` constructor via `new_object_initialized` instead.

## Verification

Standalone probe (`ExecProbe2.java`, extends the original repro to cover all four factories, a
custom `ThreadFactory`, `submit()`/`Future.get()`, `execute()` across real worker threads,
`shutdown()`/`shutdownNow()`/`isShutdown()`/`isTerminated()`/`awaitTermination()`, and
`shutdownNow()` correctly interrupting a blocked worker) — all pass. Reflection on the returned
objects confirms `ctl`/`workQueue`/`mainLock`/`workers`/`termination` are all real, correctly-typed
fields (`AtomicInteger`, `LinkedBlockingQueue`/`SynchronousQueue`, `ReentrantLock`, `HashSet`,
`ConditionObject`).

ES suite (tested against dev `4b08ffad` + this fix, i.e. before an unrelated later regression —
see "Related, NOT fixed here" below — made suite-wide runs unusable): the `storedscripts` cluster
this doc's repro pointed at now passes 8/9 classes, including the original repro target
`ScriptContextInfoSerializingTests`. The 9th (`GetScriptContextResponseTests`) fails on an
unrelated, pre-existing serialization/equals `AssertionError` — confirmed present in that class's
*non-concurrent* test methods too (`testSerialization`, `testFromXContent`), so it is not this bug.
A 60-class before/after regression sweep (same base commit, with vs. without this fix) showed zero
status changes across any class.

Also resolves `docs/known-issues/threadpoolexecutor-shutdown-npe-on-mainlock-synthetic-executor.md`
(filed independently, same session, alongside a companion `execute()`-dispatch fix in
`fix/tpe-npe-dispatch-20260710`/`306cd352`): since these objects are now genuinely real, that fix's
receiver-aware `workers`-populated check correctly treats them as real too, so `shutdown()`/
`shutdownNow()`/`submit()`/etc. all run real bytecode end-to-end.

## Related, NOT fixed here — two independent regressions discovered on dev during verification

Both confirmed present on dev commit `aa21e334` ("Fix Spring SpEL evaluation edge cases") and
later, and confirmed ABSENT on `4b08ffad` (immediately prior commit) with or without this fix, so
neither is caused by or related to the bug this doc describes:

1. **`ClassCastException: java.util.ArrayList cannot be cast to [Ljava.lang.String;` in
   `java.util.StringJoiner.add` / `java.lang.reflect.Modifier.toString` /
   `com.carrotsearch.randomizedtesting.ClassModel$2.compare`**, thrown from
   `RandomizedRunner.<init>` before any ES test method runs. This currently fails essentially
   every ES test class at bootstrap (confirmed across `client/rest`, `storedscripts`, and other
   unrelated modules) — it is what makes suite-wide verification of this doc's fix impossible on
   current dev tip. Bisected to `aa21e334` (touches `vm/src/runtime/interpreter.rs` and
   `jit/src/x64.rs`).
2. **Real `ThreadPoolExecutor.execute()` no longer dispatches asynchronously** — even a plain
   `new ThreadPoolExecutor(...).execute(task)` (nothing to do with `Executors.*` or this fix) now
   runs the task synchronously on the calling thread (confirmed identically with `--nojit`, so not
   JIT-specific). At dev `aa21e334` alone (before `306cd352`'s dispatch fix), the task was silently
   *never run at all* (worse — no output, no exception). At current dev tip (`aa21e334` +
   `306cd352` + later), `306cd352`'s defense-in-depth inline-execution fallback in
   `native_es_execute` at least runs the task, but synchronously instead of on a real worker
   thread — so `306cd352`'s receiver-aware "is this a genuinely real `ThreadPoolExecutor`" checks
   (in `vm/src/runtime/interpreter.rs`'s `intercept_force_registered_native` and
   `vm/src/vm/vm_exec.rs`'s `invoke_or_native`/`invoke_on_class_shared_inner`) are evidently not
   preventing the forced-native dispatch for real receivers in some code path, despite the
   `workers`-populated check looking correct on inspection. Not root-caused in this session — flagged
   for follow-up given it silently degrades (or, pre-`306cd352`, silently drops) async task
   execution for **every** real `ThreadPoolExecutor` in the VM, not just `Executors.*`-created ones.

## Reproduction

```powershell
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1 -Category all -Jit on -Vm craton -ElasticsearchRoot "<es-checkout>" -WorkDir "<workdir>" -Exe <cratonvm-binary> -JdkHome /usr/lib/jvm/java-21-openjdk-amd64 -TimeoutSec 600 -RunName repro-execnpe -ModeName repro-execnpe -Start 331 -Count 1
```

Collection context:
- Host: `victor@20.83.144.174`
- Worktree: `/data/data/cratonvm-worktrees/20260710-132130-es-executors-factory-real-init`,
  branch `fix/es-executors-factory-real-init-20260710`
- Fixed binary base dev SHA: merged through `8edd57be`
- Standalone probes: `/tmp/execprobe2/ExecProbe2.java`, `/tmp/execprobe2/ExecProbe3.java` on the
  collection host
