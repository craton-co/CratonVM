# Real `ThreadPoolExecutor.execute()` degraded to synchronous (or, before `306cd352`, silently dropped) — receiver-real check not fully effective

Status: RESOLVED (2026-07-10) — root cause pinned down and fixed by the very next session to touch
this area (branch `fix/wildfly-hib32-gate-20260710`); see the "Root-caused and fixed" update below.
Moved to `docs/internal/`.

## 2026-07-10 update (root-caused and fixed)

This doc's own analysis correctly suspected "a fourth, unpatched dispatch path" — confirmed: it's
`try_stackless_invoke`'s own direct, unconditional native-registry lookup
(`vm/src/runtime/interpreter.rs`), which is NOT one of the three receiver-aware exemptions
`306cd352` patched (`intercept_force_registered_native`, `invoke_or_native`,
`invoke_on_class_shared_inner`). A real `ThreadPoolExecutor.execute()` call reaching dispatch
through that path still finds `native_es_execute` registered (correctly — the registration itself
is receiver-agnostic) and calls it, landing on `306cd352`'s own defense-in-depth branch, which ran
the task inline/synchronously specifically to break the shared async-pool's self-recursion — but
fired for every real receiver reaching native this way, not just that one case.

**Fix**: rather than patching a fifth dispatch point with the same kind of check, the registry-level
drop for `java/util/concurrent/ThreadPoolExecutor` (`native-api/src/registry.rs`) was removed
entirely, and the real-vs-synthetic decision moved fully into the native callbacks via a new
`NativeContext::invoke_virtual_bytecode_only` (calls `interpreter::execute` directly, bypassing
every native check — not just the primary one; `invoke_on_class_shared` was tried first and found
to have its own unconditional native re-check for concrete declaring classes, which would have
reintroduced this exact bug). `native_es_execute`'s old synchronous-inline defense-in-depth branch
was removed as dead code once the earlier check unconditionally routes a real receiver to genuine
bytecode instead. See
`docs/internal/threadpoolexecutor-execute-npe-on-ctl-regression-FIXED.md` for the full writeup.

**Verified** with this doc's own exact repro (`ExecProbe3.java`, plain `new ThreadPoolExecutor(...)`,
3 tasks): all 3 now run on distinct worker threads (`Thread[#1,...]`/`Thread[#2,...]`/`Thread[#3,...]`),
confirmed not the calling thread — matches the "correct" behavior this doc's own bisection
described for `4b08ffad` before either receiver-check fix landed.

## Original report

Found: 2026-07-10, while verifying
`docs/known-issues/elasticsearch-suite/ES-FAIL-20260710-executors-factory-synthetic-mainlock-npe-FIXED.md`
(the "layer 2" fix that makes `Executors.new*ThreadPool()` construct genuinely real
`ThreadPoolExecutor` objects). Confirmed **unrelated** to that fix — reproduces identically for a
plain, directly-constructed `new ThreadPoolExecutor(...)` that never touches `Executors.*` or this
session's fix at all.

## Symptom

```java
ExecutorService es = new ThreadPoolExecutor(4, 4, 0L, TimeUnit.MILLISECONDS, new LinkedBlockingQueue<>());
for (int i = 0; i < 3; i++) {
    int idx = i;
    es.execute(() -> System.out.println("task " + idx + " on " + Thread.currentThread()));
}
```

On dev `4b08ffad`: prints 3 distinct real worker threads (`Thread[#1,Thread,5,main]`,
`Thread[#2,...]`, `Thread[#3,...]`) — correct, matches HotSpot semantics (`execute()` returns
immediately, tasks run async on pool workers).

On dev `aa21e334` alone (no `306cd352`): **nothing prints at all** — the tasks are silently never
run, no exception, no hang, `execute()` returns normally.

On current dev tip (`aa21e334` + `306cd352` "fix: ThreadPoolExecutor.execute() NPE on ctl for
synthetic Executors objects" + later commits, `8edd57be` at time of writing): all 3 tasks print, but
all on the **same** thread (`Thread[#1099511627776,main,5,main]`) — i.e. `Thread.currentThread()`
inside the task is the *calling* thread, not a pool worker. `execute()` is running the task
synchronously/inline instead of asynchronously.

## Analysis

`306cd352` added a receiver-aware exemption (checking the real `workers` field is non-null) at
three independent dispatch points — `native-api/src/registry.rs`'s registration-time drop,
`vm/src/runtime/interpreter.rs`'s `intercept_force_registered_native` /
`threadpool_executor_has_real_workers`, and `vm/src/vm/vm_exec.rs`'s `invoke_or_native` /
`invoke_on_class_shared_inner` — specifically so a genuinely real `ThreadPoolExecutor` (its own
`workers` field populated by a real `<init>`) keeps running its own real `execute()` bytecode
instead of being forced through the registered `native_es_execute` native (which assumes
CratonVM's synthetic 2-field layout). That commit's own message claims this was verified working
for a directly-constructed `new ThreadPoolExecutor(...).execute()` ("still dispatches to real
bytecode").

On the current dev tip tested here, that verification does not hold: `native_es_execute` is
evidently still being reached for a receiver with a populated `workers` field, and its own
defense-in-depth fallback (added in the same commit — "if ever reached for a real receiver anyway,
run the task inline instead of recursing through `spawn_runnable_on_real_thread`") is what's
producing the synchronous-execution symptom observed here. That fallback was intended purely to
break a specific recursion (the shared `async_worker_pool` singleton calling `.execute()` on
itself), not as a general substitute for real async dispatch — but it appears to be firing for
every real `ThreadPoolExecutor.execute()` call, not just that one recursive case.

Confirmed **not** JIT-specific: identical result with `--nojit`. Not root-caused further in this
session (the three-dispatch-point interpreter/vm_exec logic in question is unfamiliar code from a
different, very recently landed commit, not something written or well-understood as part of this
session's own fix) — flagged for follow-up by whoever owns `306cd352`/`fix/tpe-npe-dispatch-20260710`,
since they already have context on which of the three checks is failing (or whether there's a
fourth, unpatched dispatch path).

## Severity

This silently degrades (or, on `aa21e334` alone without `306cd352`, silently drops entirely) the
async-execution contract of `Executor.execute()`/`ExecutorService.execute()` for **every** real
`ThreadPoolExecutor` in the VM — not just ones created via `Executors.*`. Any code that calls
`.execute()` expecting a non-blocking, fire-and-forget dispatch (the documented contract) will
instead block the calling thread for the task's duration. This did not visibly break the specific
verification probe used to confirm the mainLock-NPE fix (results were still correct, just serial
instead of concurrent), but is a correctness-relevant regression for any code with actual
concurrency requirements (e.g. a caller submitting a long-running task and expecting to continue
other work immediately).

## Bisection

- `4b08ffad` + this session's `Executors.*` real-init fix, **no** `306cd352`: correct (distinct
  worker threads, confirmed via both `Executors.newFixedThreadPool(n)` and a plain
  `new ThreadPoolExecutor(...)`).
- `aa21e334` alone, no fix, no `306cd352`: tasks silently never execute.
- Current dev tip (`aa21e334` + `306cd352` + later, with or without this session's `Executors.*`
  fix — confirmed identical either way): tasks execute synchronously on the calling thread.

## Reproduction

Standalone (`/tmp/execprobe2/ExecProbe3.java` on the collection host — plain `new
ThreadPoolExecutor(...)`, no `Executors.*` involved):

```java
import java.util.concurrent.*;
public class ExecProbe3 {
    public static void main(String[] args) throws Exception {
        ExecutorService es = new ThreadPoolExecutor(4, 4, 0L, TimeUnit.MILLISECONDS, new LinkedBlockingQueue<>());
        for (int i = 0; i < 3; i++) {
            final int idx = i;
            es.execute(() -> System.out.println("  execute() task " + idx + " ran on " + Thread.currentThread()));
        }
        Thread.sleep(300);
        es.shutdown();
        es.awaitTermination(5, TimeUnit.SECONDS);
        System.out.println("DIRECT_CTOR_PROBE_OK");
    }
}
```

Collection context:
- Host: `victor@20.83.144.174`
- Worktree: `/data/data/cratonvm-worktrees/20260710-132130-es-executors-factory-real-init`
- Bisection binaries under `/data/data/cratonvm-targets/es-executors-factory-real-init-20260710/release/`:
  `cratonvm-fix-prespel-verify` (correct), `cratonvm-baseline-dev-aa21e334` (silently drops),
  `cratonvm-es-executors-factory-real-init-20260710` (current dev tip + this session's fix,
  synchronous)
