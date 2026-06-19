# Tomcat suite bug 11 — `ScheduledThreadPoolExecutor` teardown NPE (broad catalina.* regression)

**Status:** FIXED (pending full-suite re-census)
**Severity:** dominant regression — ~30 `catalina.*` classes flipped PASS→FAIL between
dev `77620f55` (baseline) and `b2203ae4`.
**Repro VM:** worktree `C:/craton/CratonVM-tcfull`, branch `tomcat-fullsuite-triage`,
default `cratonvm-cli` build (NOT `synthetic-jdk`).

## Symptom

Every `TomcatBaseTest` subclass errors in `tearDown` (so each test method is scored an
ERROR, not a failure). The test bodies pass; the teardown throws:

```
org.apache.catalina.LifecycleException: Failed to stop component [StandardServer[-1]]
  at org.apache.catalina.util.LifecycleBase.stop(LifecycleBase.java:251)
  at org.apache.catalina.startup.Tomcat.stop(Tomcat.java:462)
  at org.apache.catalina.startup.TomcatBaseTest.tearDown(TomcatBaseTest.java:253)
Caused by: java.lang.NullPointerException: Cannot invoke lock on null
  at java.util.concurrent.locks.ReentrantLock.lock(ReentrantLock.java:323)
  at java.util.concurrent.ThreadPoolExecutor.shutdownNow(ThreadPoolExecutor.java:1371)
  at java.util.concurrent.ScheduledThreadPoolExecutor.shutdownNow(...)
  at org.apache.catalina.core.StandardServer.stopInternal(StandardServer.java:921)
```

## Root cause

`StandardServer` holds a utility `ScheduledThreadPoolExecutor` (constructed with
`new ScheduledThreadPoolExecutor(1, daemonThreadFactory)`). On CratonVM that object was
**never really constructed** — but not because of missing init: a *synthetic native
shadowed the real constructor and corrupted the object*.

`native-collections` `register_executors_scheduled_natives` registers
`ScheduledThreadPoolExecutor.<init>` → `native_stpe_init`, which assumes a synthetic
3-field layout `(poolSize=0, shutdown=1, taskList=2)`:

```rust
let task_arr = alloc_ref_array(ctx, 16);
ctx.set_field(this, 0, Int(pool_size));        // slot 0
ctx.set_field(this, 1, Int(0));                // slot 1
ctx.set_field(this, 2, Object(task_arr));      // slot 2 = Object[16]
```

On a **real** STPE those slots are `ctl(0)` / `workQueue(1)` / `mainLock(2)`. So the native:
- writes `Int` into the *reference* fields `ctl` and `workQueue` → descriptor-aware
  coercion turns them into `null`;
- writes the `Object[16]` into `mainLock`.

Reflection on a freshly-constructed STPE confirms exactly this: `ctl == null`,
`workQueue == null`, `mainLock == [Ljava.lang.Object;` (length 16), `workers`/`termination`
null, `corePoolSize == 0` — whereas a plain `new ThreadPoolExecutor(...)` (and any plain
user subclass of TPE) has every field correct. This native always *shadowed* the real,
self-contained STPE constructor bytecode (which works fine on CratonVM).

The corruption was *latent* until the executor worker-reaping fix
(`f157de8a` "real-JDK executor shutdown reaps workers") **dropped the synthetic
`ThreadPoolExecutor.shutdownNow`** so the real inherited bytecode runs. Real
`shutdownNow()` does `mainLock.lock()` first → `mainLock` is the bogus `Object[16]`, so
inside `ReentrantLock.lock()` `sync` is null → `Cannot invoke lock on null`. Before
`f157de8a`, `shutdownNow` was a synthetic no-op that never touched `mainLock`, so teardown
silently succeeded — hence the regression.

The same corrupt object explains the pre-existing
`ScheduledThreadPoolExecutor emulation (Cannot invoke add on null)` note in
`native-builtins/src/lib.rs` (real `schedule()` → `delayedExecute` → `workQueue.add` on the
null `workQueue`).

## Fix

Stop intercepting `ScheduledThreadPoolExecutor` in real-JDK mode and let the
self-contained real bytecode run — exactly the precedent already used for
`register_blocking_queue_natives`, `register_phaser_natives`, and the
`ConcurrentSkipListMap` natives.

- `native-collections/src/lib.rs`: gate `register_executors_scheduled_natives` behind
  `#[cfg(feature = "synthetic-jdk")]` (its `native_stpe_init` / `native_stpe_*` shadow real
  STPE objects).
- `vm/src/vm/vm_init.rs`: remove the leftover synthetic `ScheduledThreadPoolExecutor.<init>`
  `(I,ThreadFactory)V` native in the real-JDK arm (it poked slots 0/1 → `ctl`/`workQueue`
  null, `mainLock` null → same NPE).

With no synthetic STPE natives, `new ScheduledThreadPoolExecutor(...)` runs the real JDK
constructor, which initialises `ctl`/`workQueue`(DelayedWorkQueue)/`mainLock`/`workers`/
`termination` correctly, and the inherited real `shutdownNow`/`shutdown`/`awaitTermination`
plus real `schedule*` all operate on a valid object.

## Verification

- `scratch/tpe/Probe2.java`: real STPE now has `mainLock=ReentrantLock`,
  `ctl=AtomicInteger`, `workQueue=DelayedWorkQueue`, `workers=HashSet`,
  `termination=ConditionObject` (all correct).
- `scratch/tpe/Rep.java`: `schedule()` fires the task, `shutdownNow()` returns cleanly.
- 11 of the 34 regressed classes re-run green directly on the fixed binary (across
  `core`/`loader`/`session`/`valves`/`storeconfig`/`startup`):
  `TestStandardService`, `TestListener`, `TestContextNamingInfoListener`,
  `TestMemoryRealm`, `TestFilterUtil`, `TestStandardContextAliases`,
  `TestWebappClassLoader`, `TestStandardSessionAccessor`, `TestAccessLogValveFile`,
  `TestStoreConfig`, `TestTomcatClassLoader`.
- No regression in controls: plain `ThreadPoolExecutor` unchanged;
  `TestThreadPoolExecutor` fails identically on HotSpot (harness cp skew, not a VM bug);
  `TestStandardContext` was already HANG pre-fix (interpreter throughput, unrelated).
