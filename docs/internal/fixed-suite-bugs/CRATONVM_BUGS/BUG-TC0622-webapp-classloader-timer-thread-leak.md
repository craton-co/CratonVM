# Bug TC0622 — `Thread` context-classloader not inherited parent→child → Tomcat's `clearReferencesThreads` skips the leaked Timer thread → "Timer thread still running"

> **One-line root cause:** A child `Thread` created on CratonVM does **not inherit
> the spawning (parent) thread's `contextClassLoader`** — the child's
> `contextClassLoader` field is left null and `Thread.getContextClassLoader()`
> falls back to the app/system class loader. Tomcat's leak-prevention
> (`WebappClassLoaderBase.clearReferencesThreads`) only acts on a thread whose
> `getContextClassLoader() == this` (the webapp loader). Because the leaked
> `java.util.TimerThread`'s CCL resolves to the **app loader** instead of the
> webapp loader, the `if (ccl == this)` guard is false, so
> `clearReferencesStopTimerThread(thread)` is **never invoked** — the Timer
> thread is never reflectively stopped and stays alive, and the test's final
> `Assert.fail("Timer thread still running")` fires. The reflective stop
> machinery itself is **not** the bug (it works — see Repro). This is **NOT** the
> known `Thread.getState() always NEW` bug and **NOT** a monitor wait/notify
> desync.

**Severity:** Medium (breaks Tomcat's webapp-classloader thread-leak prevention:
any lingering app-spawned thread escapes `clearReferencesThreads` because its CCL
never matches the webapp loader; correctness gap that can leak threads/loaders on
real undeploy, not just a test assertion).
**Status on CratonVM:** **FIXED 2026-09-05** (was FAIL). **HotSpot:** PASS.
**Run date:** 2026-06-23

> ## 2026-09-05 — closed, in two parts
>
> **Part 1, the defect this page diagnosed, is fixed and the fix works.** The
> recommendation's option 1 landed the same day as this page:
> `native_thread_start0` (`native-builtins/src/lang_system.rs`) copies the
> parent's `contextClassLoader` into the child, behind
> `CRATONVM_INHERIT_THREAD_CCL` (default ON). Re-measured at dev tip
> `355659d00` with this page's own minimal probe: `timer_CCL == cl` is now
> **true**, and the Azure suite log shows
> `clearReferencesStopTimerThread` being **entered** — which it can only be
> when `thread.getContextClassLoader() == webappLoader`. The `if (ccl == this)`
> gate this page is about now passes.
>
> **Part 2 was a second defect hidden behind the first, and is why both classes
> stayed red for ten weeks after the fix.** Once the gate passes, the
> reflective stop throws
> `InaccessibleObjectException: module java.base does not "opens java.util" to
> org.apache.tomcat.catalina` — despite `--add-opens
> java.base/java.util=ALL-UNNAMED` being on the command line. Cause: a modular
> jar reached through the CLASS path was being given its declared module name
> instead of the unnamed module, so an `ALL-UNNAMED`-qualified open could not
> reach it. Fixed the same day; see
> `modular-jar-on-the-class-path-was-given-its-declared-module-name-FIXED-20260905`
> for the full write-up, the classpath-shape reason this reproduced on Azure
> and not on Windows, and the A/B (both classes FAIL → PASS).
>
> One residual is left open there and is NOT this page's defect: this VM
> inherits the CCL at `start()`, HotSpot at CONSTRUCTION. Nothing measured
> depends on the difference. The preferred long-term shape remains this page's
> **option 2** — stop shadowing the multi-arg `Thread` constructors.

**Binary:** dev `df11ac00` (worktree `C:\craton\CratonVM-tctest`, exe
`cratonvm-tcfull-0622.exe`).

## Affected classes

- `org.apache.catalina.loader.TestWebappClassLoaderMemoryLeak` (`testTimerThreadLeak`)

Almost certainly the same root cause (sibling in the same run, same CCL-match
gate in `clearReferencesThreads`):
- `org.apache.catalina.loader.TestWebappClassLoaderExecutorMemoryLeak`
  (its `.log.err` shows the analogous failure; both rely on
  `thread.getContextClassLoader() == webappLoader`).

## Symptom

```
1) testTimerThreadLeak(org.apache.catalina.loader.TestWebappClassLoaderMemoryLeak)
java.lang.AssertionError: Timer thread still running
    at org.apache.catalina.loader.TestWebappClassLoaderMemoryLeak.testTimerThreadLeak(TestWebappClassLoaderMemoryLeak.java:65)
FAILURES!!!
Tests run: 1,  Failures: 1
```

Test shape (`test/.../TestWebappClassLoaderMemoryLeak.java`): a servlet starts a
`new Timer("leaked-thread")` on `doGet`; the test `setClearReferencesStopTimerThreads(true)`,
hits the URL, then `ctx.stop()` (which calls `clearReferencesThreads()`), then
enumerates threads and `join(5000)`s any still-alive thread named `leaked-thread`.
On CratonVM that thread is still alive after 5 s → fail.

**Tell-tale log evidence:** the `clearReferencesStopTimerThread` path logs
`webappClassLoader.warnTimerThread` on success and `…stopTimerThreadFail` on a
reflective error. **Neither message appears** in
`org.apache.catalina.loader.TestWebappClassLoaderMemoryLeak.log[.err]` — i.e. the
stop method was **never entered** for the leaked thread, and it did **not** throw.
That alone localizes the defect to the *gate* in front of it, not the stop logic.

## Root cause (pinned)

`impl: java/org/apache/catalina/loader/WebappClassLoaderBase.java`,
`clearReferencesThreads()` (≈ line 1550) iterates all threads and only acts when:

```java
ClassLoader ccl = thread.getContextClassLoader();
if (ccl == this) {                       // <-- line 1558, the gate
    ...
    if (thread.getClass().getName().startsWith("java.util.Timer")
            && clearReferencesStopTimerThreads) {
        clearReferencesStopTimerThread(thread);   // line 1588 — never reached
        ...
```

On HotSpot the leaked `TimerThread`'s CCL **is** the webapp loader, so the gate
passes. On CratonVM the leaked thread's CCL is the **app/system** loader, so the
gate is false and the timer thread is left running.

**Empirical proof** (two standalone probes, since removed; both compiled with
JDK 25 and run on `cratonvm-tcfull-0622.exe` vs HotSpot):

1. Mimic Tomcat exactly — spawn a `Timer("leaked-thread")` from a worker thread
   whose CCL was set to a custom `URLClassLoader`, then enumerate + reflectively
   reproduce `clearReferencesStopTimerThread`:

   | Observation                         | HotSpot                    | CratonVM                                  |
   |-------------------------------------|----------------------------|-------------------------------------------|
   | `TimerThread` found / alive         | yes / yes                  | yes / yes                                 |
   | `getClass().startsWith("java.util.Timer")` | true               | true                                      |
   | **`timerCCL == webappCCL`**         | **true**                   | **false** (app loader `AppClassLoader`, not the `URLClassLoader`) |
   | reflective stop threw?              | no                         | no (`reflectiveStop=OK`)                  |
   | thread terminated after stop?       | yes                        | **yes** (`afterStop_isAlive=false`)       |

   So the reflective stop (`newTasksMayBeScheduled=false` + `queue.clear()` +
   `queue.notifyAll()` waking `TimerThread.mainLoop`'s `queue.wait()`) **works on
   CratonVM** — the queue-monitor wait/notify path is fine. The *only* divergence
   is the CCL, which gates whether Tomcat ever calls the stop at all.

2. Minimal CCL-inheritance probe — set a worker's CCL to a `URLClassLoader`, then
   inside the worker create `new Thread(...)` and read the child's CCL:

   ```
   currentThread==worker?       HotSpot=true   CratonVM=true   (thread identity OK)
   worker_self_CCL == set CCL?  HotSpot=yes    CratonVM=yes    (set/getCCL field path OK)
   child_inherited_CCL == parent CCL?  HotSpot=YES   CratonVM=NO (child gets app loader)
   ```

   The child thread does **not** inherit the parent's `contextClassLoader`.

**VM-side mechanism.** `java.util.TimerThread extends Thread` calls `super(name)`
→ `Thread.<init>(String)`. In real JDK 25 the private all-args ctor assigns
`this.contextClassLoader = parent.getContextClassLoader()` (parent =
`Thread.currentThread()`), which is how the child inherits. On CratonVM this
assignment does not take effect for app-created threads:

- `native-builtins/src/lib.rs` registers a full set of **synthetic `Thread`
  ctor overrides** (`register_synthetic_overrides`, ≈ line 10444+):
  `<init>(Ljava/lang/String;)V`, `(Ljava/lang/Runnable;Ljava/lang/String;)V`,
  `(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;Ljava/lang/String;)V`, etc. Each
  sets only `name`/`priority`/`target`/`group` by fixed slot and **never reads
  the parent thread nor assigns `contextClassLoader`**. Any active ctor on this
  path drops CCL inheritance.
- `native-builtins/src/lib.rs` `getContextClassLoader` (≈ line 6478) returns the
  thread's `contextClassLoader` field **only if non-null**, otherwise falls back
  to `get_or_create_app_loader(ctx)` — which is exactly the observed app-loader
  result for the un-inherited child.
- Grepping `native-builtins/src` + `vm/src` for `contextClassLoader` /
  `inheritedAccessControlContext` shows the field is touched **only** by
  explicit `set/getContextClassLoader` and by the bootstrap current-thread
  synthesis in `vm/src/vm/vm_exec.rs` (≈ line 4099, which hard-codes the
  *system* CL). There is **no parent→child CCL propagation** anywhere in the
  thread create/start path.

## Relation to known Thread issues — ruled out

- **NOT** "`Thread.getState()` always NEW" (memory `reference_thread_getstate_new_bug`,
  fixed `16d23e7b`). The test never calls `getState()`; it gates on
  `thread.isAlive()`, which is **registry-backed and accurate** here
  (`vm/src/vm/vm_exec.rs::thread_is_alive` → `thread_registry.is_alive`). The
  probe confirms `isAlive` correctly reads `true` while the thread runs and
  `false` after it terminates. The thread is genuinely still alive — not a state
  misreport.
- **NOT** the reactor-worker-thread-leak / GC-timing race
  (`gc-rscache-reactor-shutdown-timing-race.md`): single thread, deterministic,
  GC-independent, no socket involvement.
- **NOT** a monitor wait/notify or join-monitor-owner desync
  (`mt-stw-join-monitor-desync.md`, `BUG-W-stale-oop-across-monitor-wait`): the
  probe shows `queue.notifyAll()` *does* wake `TimerThread.mainLoop`'s
  `queue.wait()` and the thread terminates cleanly when the stop is actually
  invoked.
- **NOT** GC/JIT root corruption: deterministic 1/1, no `--nojit` dependence
  expected (pure classloader-field semantics).

## Reproduction

Full test (reproduces the assertion failure):

```powershell
cd C:\craton\CratonVM\apps\tomcat
$CP = (Get-Content .tooling\cp.txt -Raw).Trim()
$env:CRATONVM_REAL_NET_SOCKETS=1; $env:CRATONVM_REAL_AQS=1; $env:CRATONVM_DISABLE_DEFAULT_WATCHDOG=1
C:\craton\CratonVM-tctest\target\release\cratonvm-tcfull-0622.exe `
  -Xmx2g -Dfile.encoding=UTF-8 -Djava.net.preferIPv4Stack=true `
  "-Dtomcat.test.basedir=$PWD\output\build" "-Dtomcat.test.temp=$PWD\output\test-tmp" `
  "-Dtomcat.test.tomcatbuild=$PWD\output\build" -Dtomcat.test.relaxTiming=true `
  --add-opens java.base/java.lang=ALL-UNNAMED --add-opens java.base/java.io=ALL-UNNAMED `
  --add-opens java.base/java.util=ALL-UNNAMED --add-opens java.base/java.util.concurrent=ALL-UNNAMED `
  -cp $CP org.junit.runner.JUnitCore `
  org.apache.catalina.loader.TestWebappClassLoaderMemoryLeak
```

Minimal standalone repro (the load-bearing fact — no Tomcat needed):

```java
ClassLoader cl = new java.net.URLClassLoader(new java.net.URL[0],
        Probe.class.getClassLoader());
Thread w = new Thread(() -> {
    Thread child = new Thread(() -> {}, "child");
    // HotSpot: child.getContextClassLoader() == cl
    // CratonVM: child.getContextClassLoader() == app/system loader (NOT cl)
    System.out.println(child.getContextClassLoader() == cl);   // true on HotSpot, false on CratonVM
});
w.setContextClassLoader(cl);
w.start(); w.join();
```

## Recommendation

**FIX (bounded, VM-side).** On `Thread` construction, inherit the parent's
context classloader the way real JDK does: set the new thread's
`contextClassLoader` field to `Thread.currentThread().getContextClassLoader()`
(falling back to the system CL only when the parent's is genuinely null). Two
viable spots:

1. In the synthetic `Thread` ctor overrides (`native-builtins/src/lib.rs`
   `register_synthetic_overrides`, the `<init>(…String…)` / `(Runnable,String)` /
   `(ThreadGroup,Runnable,String)` family, ≈ line 10444+): after setting
   name/priority/target, also copy the current thread's CCL into the child's
   `contextClassLoader` field by name. (Mirror the real ctor — don't overwrite a
   value the app later sets via `setContextClassLoader`.)
2. Or, preferably, stop shadowing the multi-arg `Thread` ctors so the **real JDK
   `Thread.<init>` bytecode** runs end-to-end (it already does the parent-CCL
   inheritance); keep only the genuinely-needed native (e.g. `start0`,
   `registerNatives`). This also closes any other latent gaps from the synthetic
   ctors quietly diverging from JDK 25 field semantics.

Either fix makes `TimerThread`'s CCL equal the webapp loader, so Tomcat's
`clearReferencesThreads` reaches `clearReferencesStopTimerThread` (already proven
to work) and the timer thread is stopped — turning this test (and the Executor
sibling) FAIL→PASS. Low blast radius, but verify no test depends on the current
"child CCL = app loader" fallback.
