# CDI/Weld cluster (14+ classes) — `ForkJoinPool.commonPool().invokeAll()` `RejectedExecutionException` during Weld bootstrap

**Status:** OPEN (2026-08-04). Confirmed root cause, confirmed workaround
(`CRATONVM_SYNTHETIC_FORKJOINPOOL=1`). Not a new mechanism — it's a coverage
gap in an existing real-ForkJoinPool bridge allow-list — but it currently
FAILs the **entire** `org.hibernate.orm.test.cdi.*` + `jpa.cdi.*` cluster,
plus at least one class outside that package hierarchy (added 2026-08-04,
see below), and it silently reopened three previously-"FIXED" docs' PASS
claims (see Corrections below) when `CRATONVM_REAL_FORKJOINPOOL` became the
**default** on `dev` (`16ec5d7ad` "wip: deep-audit agent handoff snapshot",
2026-07-30).

**2026-08-04 addendum:** `org.hibernate.orm.test.filter.FilterParameterTests`
is a 15th witness of this exact mechanism, missed by the original 14-class
survey because that survey was scoped to the `cdi.*`/`jpa.cdi.*` package
hierarchy rather than a full-log signature search. Only 2 of the class's 7
`@ParameterizedTest` methods touch CDI (`testAutoEnableWithResolver` and
`testAutoEnableWithoutResolver`, both of which construct a `Weld`
container to resolve a dynamic filter parameter) — the other 5 pass cleanly.
Today's fresh run (`run-20260804-113511-custom/on-real/shard-3/raw.log`)
shows `found=10 ok=6 failed=4` (2 methods x 2 parameterized instances each),
every failure the identical
`RejectedExecutionException` at `ForkJoinPool.submissionQueue` reached via
`Weld.initialize` -> `WeldStartup.startInitialization` ->
`ConcurrentBeanDeployer.addClasses` -> `AbstractExecutorServices
.invokeAllAndCheckForExceptions` -> `ForkJoinPool.invokeAll`, byte-for-byte
the same five bottom frames as the `CdiSmokeTests` example below. No
separate doc filed for it — same root cause, same fix, same workaround; this
addendum is the record. A full-log search of today's run for
`RejectedExecutionException` found no other classes outside the original 14
plus this one.

## Symptom

All 14 classes in the fresh 2026-08-04 residual run
(`apps/hib-suite-runner/runs/run-20260804-113511-custom/on-real/`, dev tip
`a43a74ded`) FAIL with the same signature — `WeldStartup.startInitialization`
throws (or wraps) `java.util.concurrent.RejectedExecutionException`:

```
org.hibernate.orm.test.cdi.converters.standard.CdiHostedConverterTest
org.hibernate.orm.test.cdi.converters.delayed.DelayedCdiHostedConverterTest
org.hibernate.orm.test.cdi.events.standard.StandardCdiSupportTest
org.hibernate.orm.test.cdi.events.delayed.DelayedCdiSupportTest
org.hibernate.orm.test.cdi.events.extended.ValidExtendedCdiSupportTest
org.hibernate.orm.test.cdi.general.hibernatesearch.standard.HibernateSearchStandardCdiSupportTest
org.hibernate.orm.test.cdi.general.hibernatesearch.delayed.HibernateSearchDelayedCdiSupportTest
org.hibernate.orm.test.cdi.general.hibernatesearch.extended.HibernateSearchExtendedCdiSupportTest
org.hibernate.orm.test.cdi.general.mixed.DelayedMixedAccessTest
org.hibernate.orm.test.cdi.general.mixed.ExtendedMixedAccessTest
org.hibernate.orm.test.cdi.general.mixed.ImmediateMixedAccessTests
org.hibernate.orm.test.cdi.type.CdiSmokeTests
org.hibernate.orm.test.cdi.type.SimpleTests
org.hibernate.orm.test.jpa.cdi.BasicCdiTest
```

`shard-2/raw.log` (`CdiSmokeTests`), representative direct form:

```
Failures (1):
  JUnit Jupiter:CdiSmokeTests:testCdiOperations()
    => java.util.concurrent.RejectedExecutionException
       java.util.concurrent.ForkJoinPool.submissionQueue(ForkJoinPool.java:2611)
       java.util.concurrent.ForkJoinPool.poolSubmit(ForkJoinPool.java:2623)
       java.util.concurrent.ForkJoinPool.invokeAll(ForkJoinPool.java:3373)
       java.util.concurrent.ForkJoinPool.invokeAll(ForkJoinPool.java:3389)
       org.jboss.weld.executor.AbstractExecutorServices.invokeAllAndCheckForExceptions(AbstractExecutorServices.java:60)
       org.jboss.weld.executor.AbstractExecutorServices.invokeAllAndCheckForExceptions(AbstractExecutorServices.java:68)
       org.jboss.weld.bootstrap.ConcurrentBeanDeployer.addClasses(ConcurrentBeanDeployer.java:52)
       org.jboss.weld.bootstrap.BeanDeployment.createClasses(BeanDeployment.java:208)
       org.jboss.weld.bootstrap.WeldStartup.startInitialization(WeldStartup.java:414)
```

`shard-0/raw.log` (`CdiHostedConverterTest`), wrapped form — same
`RejectedExecutionException` `Caused by:`, reached via Hibernate's own
`ServiceRegistryExtension` resolving the CDI-backed registry instead of via
`CdiContainerExtension` directly:

```
java.lang.RuntimeException: Could not configure StandardServiceRegistryBuilder
	at org.hibernate.testing.orm.junit.ServiceRegistryExtension.configureServices(...)
     Caused by: java.util.concurrent.RejectedExecutionException
       java.util.concurrent.ForkJoinPool.submissionQueue(ForkJoinPool.java:2611)
       ...
       org.jboss.weld.bootstrap.ConcurrentBeanDeployer.addClasses(ConcurrentBeanDeployer.java:52)
```

Every one of the 14 classes bottoms out at the identical five frames
(`ConcurrentBeanDeployer.addClasses` → `AbstractExecutorServices
.invokeAllAndCheckForExceptions` → `ForkJoinPool.invokeAll` → `poolSubmit`
→ `submissionQueue` → `RejectedExecutionException`), confirmed individually
for all 14 by grepping `@@TESTFAIL`/`Failures (` blocks in
`shard-{0,1,2,3}/raw.log` — this is one shared bootstrap defect, not 14
independent failures (the assignment's working hypothesis is confirmed, not
assumed).

## Root cause

`ForkJoinPool.commonPool()` under CratonVM's `CRATONVM_REAL_FORKJOINPOOL`
mode is a VM **Bridge** shortcut (`is_forkjoin_native_override` /
`keep_real_forkjoinpool_bridge` in
`vm/src/runtime/interpreter/native_override.rs:1608` and
`native-api/src/registry.rs:5047`) — it does not run the real JDK
`ForkJoinPool` constructor, so the returned instance never populates the
real `queues`/`runState`/`mode` fields that `submissionQueue()` reads.
Every `ForkJoinPool` method Weld or the app might call therefore has to be
on an explicit allow-list of VM-implemented bridge methods; anything **not**
listed falls through to real JDK bytecode running against that
under-initialized instance and throws `RejectedExecutionException` at
`submissionQueue()`.

The allow-list currently covers `commonPool`, `getFactory`,
`getParallelism`, `getCommonPoolParallelism`, `invoke(ForkJoinTask)`,
`submit(ForkJoinTask)`, `externalSubmit`, `execute(Runnable|ForkJoinTask)`,
`awaitQuiescence`, and — per the `fix(vm): cover
ForkJoinPool.submit(Callable/Runnable) under real-FJP gate` commit
(`7f68e07bd`, 2026-07-21) — `submit(Callable)`, `submit(Runnable)`,
`submit(Runnable, T)`. That July 21 commit's own message describes **the
exact same failure mode** for `submit`: *"That overload was missing from the
real-JDK Bridge allow-list ... so it fell through to real JDK bytecode
running against a pool whose commonPool() shortcut never populates the real
queues/runState/mode fields — poolSubmit()/submissionQueue() threw
RejectedExecutionException."*

**`invokeAll(Collection<? extends Callable<T>>)` — the overload Weld's
`ConcurrentBeanDeployer`/`AbstractExecutorServices.invokeAllAndCheckForExceptions`
actually calls — was never added to either allow-list.** Confirmed by
searching both `is_forkjoin_native_override` and `keep_real_forkjoinpool_bridge`
match arms end-to-end: neither lists `invokeAll` under any descriptor, and a
repo-wide `grep -rn "\"invokeAll\""` across `native-api/src` and `vm/src`
returns nothing. So `invokeAll` always falls through to real bytecode, always
reaches the same under-initialized `commonPool()` bridge object, and always
throws — this is a **coverage gap**, structurally identical to the
`submit(Callable/Runnable)` gap fixed on 2026-07-21, just for one more
overload that wasn't in scope of that fix.

## Why this wasn't visible before 2026-07-30

Until commit `16ec5d7ad` ("wip: deep-audit agent handoff snapshot",
2026-07-30), `CRATONVM_REAL_FORKJOINPOOL` was **opt-in**
(`types/src/flags.rs`, `real_forkjoinpool: present(src,
"CRATONVM_REAL_FORKJOINPOOL")`) and the default path additionally seeded
`org.jboss.weld.executor.threadPoolType=NONE` (`vm/src/vm/vm_init.rs:2812`,
`HIB-CV-20`) so Weld's real `SimpleBeanDeployer` (single-threaded, no
`ForkJoinPool` at all) ran instead of `ConcurrentBeanDeployer`. That
`16ec5d7ad` commit changed the resolution to
`real_forkjoinpool: !present(src, "CRATONVM_SYNTHETIC_FORKJOINPOOL") ||
present(src, "CRATONVM_REAL_FORKJOINPOOL")` — i.e. real-FJP-by-default,
synthetic-opt-out — and `vm_init.rs`'s `threadPoolType=NONE` seed is
conditioned on `!flags().natives.real_forkjoinpool`, so it stopped firing by
default too. That flip is what routes every CDI class through
`ConcurrentBeanDeployer` → `invokeAll` → the uncovered overload, by default,
today.

## Confirmed workaround: `CRATONVM_SYNTHETIC_FORKJOINPOOL=1`

Single-class repro from `apps/hib-suite-runner`, same binary as the fresh
run (`C:/craton/CratonVM-hib-local-0712-v3/target/release/cratonvm.exe`, dev
tip `a43a74ded`):

```
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 CRATONVM_SYNTHETIC_FORKJOINPOOL=1 \
  cratonvm.exe --java-home "<jdk25>" --Xmx 1500m @common.args \
  -Dcraton.batch=1 CratonRunner org.hibernate.orm.test.cdi.type.CdiSmokeTests
```

Result: `@@RESULT org.hibernate.orm.test.cdi.type.CdiSmokeTests found=1
started=1 ok=1 failed=0 aborted=0 skipped=0 ms=5435` — PASS. Setting
`CRATONVM_SYNTHETIC_FORKJOINPOOL=1` restores the pre-07-30 behavior
(`real_forkjoinpool=false` → `threadPoolType=NONE` seeded →
`SimpleBeanDeployer`, no `invokeAll` call at all), which confirms the causal
chain end to end: the default-real-FJP flip is both necessary and sufficient
to reproduce the failure, and reverting just that one flag for this class
clears it without touching anything else.

This is a real, narrowly-scoped workaround (`CRATONVM_SYNTHETIC_FORKJOINPOOL=1`
env var), not a fix — it forces the whole CDI cluster back onto the
already-closed `HIB-CV-25`/`HIB-CV-25b` code paths (see Corrections below)
rather than exercising the real concurrent pool the 07-30 change intended to
default on. The actual fix is adding `invokeAll(Collection)` (and, to be
safe, auditing for other missing `ForkJoinPool`/`ForkJoinTask` overloads
Weld or other CDI/Jakarta libraries might call — `invokeAny` was not checked
here) to `is_forkjoin_native_override` / `keep_real_forkjoinpool_bridge`,
mirroring the `submit(Callable)` fix's eager-inline synchronous-execution
model (run each `Callable` synchronously, collect `Future`s, mark done).

## Not tested

`--nojit`: not run — this failure is 100% in Weld/JDK bootstrap bytecode
before any application hot loop runs, and the exception's own stack trace
(interpreter-driven `ForkJoinPool.submissionQueue`/`poolSubmit` JDK
bytecode calling a VM bridge object) gives no reason to expect JIT
involvement; skipped as low-value given the workaround above already
isolates the variable that matters (`CRATONVM_SYNTHETIC_FORKJOINPOOL`).
HotSpot: not run here, but is obviously unaffected — `commonPool()` on a
real JDK is a fully-initialized real pool, so `invokeAll` never reaches an
under-initialized bridge object.

## Repro (any of the 14 classes, default config — reproduces the failure)

```
cd apps/hib-suite-runner
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 cratonvm.exe --java-home "<jdk25>" \
  --Xmx 1500m @common.args -Dcraton.batch=1 CratonRunner \
  org.hibernate.orm.test.cdi.type.CdiSmokeTests
```

## Corrections to existing docs

This bug silently invalidated the PASS claims in three previously-closed
docs once `16ec5d7ad` (2026-07-30) flipped `CRATONVM_REAL_FORKJOINPOOL` to
default-on. Each is corrected in place (dated correction section added, not
deleted) rather than reopened wholesale, because their actual fixes (generic
interfaces, `containsAll` foreign-collection, `DataOutputStream.written`
slot) are all still present and still correct — they're just unreachable
today because bootstrap now fails one step earlier, before Weld ever reaches
the code paths those fixes touch:

- `HIB-CV-20-weld-observer-beanmanager-param-reflection.md` —
  its own "Layer 2" analysis is the historical version of this exact
  mechanism (synthetic pool's `RejectedExecutionException`) and its fix was
  the `threadPoolType=NONE` default seed that the 07-30 flag-default flip
  silently disabled for the (now-default) real-FJP path. Added a 2026-08-04
  correction section there pointing back to this doc.
- `HIB-CV-25-cdi-weld-qualifier-containsall-foreign-collection.md` —
  claimed "9/11 sampled cdi.* classes PASS == HotSpot"; today all 14 FAIL,
  but earlier in bootstrap (`WeldStartup.startInitialization`, before
  `BeanAttributesFactory.initQualifiers` ever runs) — a strictly prior
  blocker, not a regression of the `containsAll` fix itself. Added a
  correction section.
- `HIB-CV-25b-weld-clientproxy-dataoutputstream-written-slot.md` —
  claimed "All 12 sampled cdi.* tests PASS == HotSpot"; same situation,
  the `DataOutputStream`/proxy-generation code this doc fixed is only
  reached after `ConcurrentBeanDeployer.addClasses` succeeds, which no
  longer happens by default. Added a correction section.

`hib-delayedcdisupporttest-weld-bootstrap-hang-NOT-A-BUG.md`
is **not** corrected — its claim ("the 2026-07-06 hang was shared-host
load, not a VM bug") is about a *hang* symptom under the pre-07-30
single-threaded-by-default config, which is orthogonal to and unaffected by
this doc's finding. `DelayedCdiSupportTest` today fails fast and
deterministically (~3.8s, `RejectedExecutionException`), not a hang; a
cross-reference note was added there pointing forward to this doc so a
future reader doesn't conflate the two symptoms.
