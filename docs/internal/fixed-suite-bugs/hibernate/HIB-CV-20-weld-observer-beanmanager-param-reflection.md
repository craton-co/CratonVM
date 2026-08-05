# HIB-CV-20 — Weld CDI rejects a valid container-lifecycle observer (`WELD-000409`) — `BeanManager` parameter type mis-resolved

**Severity:** Medium — blocks the Weld-based CDI smoke path. Confirmed class:
`org.hibernate.orm.test.cdi.type.CdiSmokeTests` — `found=1 ok=0 failed=1`.

**Status:** ✅ `CdiSmokeTests` **PASSES** with `CRATONVM_REAL_FORKJOINPOOL=1`
(real concurrent CDI bootstrap). Default (no flag): fast-fails at L4 in ~18s
(no hang). All on `dev`.
- **L1** `WELD-000409` (`Class.getGenericInterfaces`) — FIXED, `dev` `dac97936`.
- **L2/L3** `ForkJoinPool.invokeAll` RejectedExecution + Weld-bootstrap hang —
  default `threadPoolType=NONE` single-threaded fallback (`eada9337`), **and**
  the real fix: `CRATONVM_REAL_FORKJOINPOOL=1` runs the real ForkJoinPool
  (`8dca8bcd`).
- **L4** `WELD-001301` (`@Produces` treated as a qualifier) — was an **artifact
  of the single-threaded path**; the real concurrent pool clears it too.
**Mode:** Interpreter (JIT-off census).
**HotSpot:** not affected.

## Real ForkJoinPool fix (the keeper for layer 3) — `CRATONVM_REAL_FORKJOINPOOL`

CratonVM's `ForkJoinPool.commonPool()` is a synthetic 1-field stub returning an
**uninitialised real-class instance** (parallelism 0, no `ctl`/`queues`), so the
real `invokeAll`/`submit` bytecode throws `RejectedExecutionException` and Weld's
concurrent `ConcurrentBeanDeployer` cannot run.

The opt-in gate `CRATONVM_REAL_FORKJOINPOOL=1` (registry, mirroring
`CRATONVM_REAL_NET_SOCKETS`) **drops every synthetic `ForkJoinPool` native** so
the **real JDK pool bytecode** runs: proper init (`parallelism = cpus-1`), real
`ForkJoinWorkerThread`s, work-stealing degrading to caller-runs (functionally
correct). With it, Weld runs its real concurrent deployer and **`CdiSmokeTests`
+ the CDI cluster PASS** — clearing the RejectedExecution, the hang, *and*
`WELD-001301` (no `REAL_AQS` needed). The gate also skips the `threadPoolType=NONE`
default so it is a clean **one-flag** opt-in (Weld then uses its `COMMON` pool).

The gate **keeps the synthetic eager-inline `execute`** (drops everything else).
Under the real pool, work submitted via `execute()` runs on a worker thread, and
CratonVM's cross-worker memory ordering doesn't reliably publish an
object-reference field (e.g. `CompletableFuture.result`) written by one worker to
a dependent task on another worker — so `CompletableFuture.*Async` (every stage
scheduled via `execute`) read a stale-null upstream result (`thenApplyAsync →
got[null]`). Keeping `execute` eager-inline runs those stages caller-side, so
**`CompletableFuture` now works under the gate too** (`CfProbe` DONE, robust over
repeated runs). Real worker threads DO spawn and run `execute()` tasks
(`ForkJoinPool.commonPool-worker-1`), so the pool is genuinely live; `invokeAll`/
`submit` use the real pool's caller-runs helping.

**Why still opt-in, not default.** Forcing real-FJP on globally regressed
`PersistenceXmlParserTest` 4/4 → 2/4: parallel-stream / fork-join work uses
`ForkJoinTask.fork`/`invoke` (NOT `execute`), runs on real workers, and hits the
**same cross-worker object-reference visibility gap** — which the eager-inline
`execute` mitigation does not cover. So the synthetic pool stays the default
(CompletableFuture works, `PersistenceXmlParserTest` 4/4); the real pool is an
opt-in for concurrent-CDI workloads that don't lean on parallel-stream result
passing.

**What "closing it fully" (default-on, no tradeoff) requires:** fixing CratonVM's
cross-worker object-reference publication — a worker thread's writes to an
object's reference fields (incl. via `Unsafe.compareAndSetReference` /
`putReferenceRelease`) must be reliably visible to another worker that later
reads them through the work-stealing deque. This is a deep VM concurrency/GC
correctness item (worker-stack root remap and/or heap-store happens-before), not
a localized native — out of scope here; it is the single remaining blocker to
making the real ForkJoinPool the universal default.

**Verified (`dev` `435898af`):** gate ON → `CdiSmokeTests`, `StandardCdiSupportTest`,
`ValidExtendedCdiSupportTest`, `CdiHostedConverterTest` PASS **and** `CfProbe`
(supplyAsync/thenApplyAsync/thenCombineAsync/allOf/handleAsync) DONE + robust over
3 runs; 4 control classes (`InstantTest`/`ExpressionsTest`/`SessionJdbcBatchTest`/
`QueryTimeOutTest`) PASS. Default (gate OFF) → `CompletableFuture` works,
`PersistenceXmlParserTest` 4/4 (no regression). Forcing it default-on regressed
`PersistenceXmlParserTest` to 2/4 (parallel-stream cross-worker gap) → kept opt-in.

## ACTUAL ROOT CAUSE (the title's "param type mis-resolved" was wrong — see Investigation)

Decompiling `org.jboss.weld.event.ObserverMethodImpl.checkObserverMethod` (the
WELD-000409 throw site, `weld-se-shaded 6.0.4`) shows the real check for a
container-lifecycle observer: each non-`@Observes` parameter must satisfy
`parameter.getTypeClosure().contains(jakarta.enterprise.inject.spi.BeanContainer.class)`.
`BeanManager extends BeanContainer`, so its type closure must include
`BeanContainer`. Weld builds that closure with `HierarchyDiscovery`, which walks
**`Class.getGenericInterfaces()`** (+ `getGenericSuperclass()`).

**Layer 1 — `Class.getGenericInterfaces()` returned `[]` for non-generic interfaces.**
Probe (`BeanMgrProbe`): for `BeanManager` (a *non-generic* `interface BeanManager
extends BeanContainer`), CratonVM `getInterfaces()` = `[BeanContainer]` (correct)
but `getGenericInterfaces()` = **`[]`** (HotSpot: `[BeanContainer]`). The JDK
contract is that `getGenericInterfaces()` falls back to the raw `getInterfaces()`
when there is no generic `Signature`; CratonVM's native returned an empty array in
that fallback. So Weld's type closure for the `BeanManager` parameter was
`{BeanManager}` — missing `BeanContainer` — and `WELD-000409` fired.
Fix: `native_class_get_generic_interfaces` (lang_class.rs) now returns the raw
direct superinterfaces (`ctx.class_interfaces`) in the no-Signature fallback.
**Verified:** `typeClosure.contains(BeanContainer)=true`, WELD-000409 gone.

**Layer 2 — `ForkJoinPool.invokeAll` `RejectedExecutionException` (downstream, OPEN).**
With WELD-000409 cleared (genif build), Weld's `ConcurrentBeanDeployer.addClasses`
reaches `ForkJoinPool.invokeAll(Collection)`; the real bytecode submits via
`submissionQueue` and throws `RejectedExecutionException` on CratonVM's synthetic
pool (which has no real worker/queue machinery). An eager-inline
`native_forkjoin_invoke_all` (runs each Callable synchronously, returns completed
`CompletableFuture`s) + a `force_native_over_real_jdk_bytecode` entry **was tried
and REVERTED** (branch `fix/hibernate-full-suite`, commit `e61091d8`): it cleared
the `RejectedExecutionException` but exposed **layer 3** — turning a fast FAIL into
a worse multi-minute HANG without going green.

**Layer 3 — `WeldStartup.<clinit>` thread/class-init hang (FIXED with L2).**
A `--stack-dump-on-timeout` of the hung VM showed the main thread inside
`org/jboss/weld/bootstrap/WeldStartup.<clinit>` with a live daemon `Thread-1`:
Weld's concurrent-deployment path stands up a `ForkJoinPool`-backed executor and
fans bootstrap work across worker threads CratonVM's synthetic pool runs inline,
deadlocking the handshake.

**L2 + L3 fix (`dev` `eada9337`).** Rather than make the synthetic `ForkJoinPool`
concurrent (a large subsystem change), seed `org.jboss.weld.executor.threadPoolType
= NONE` as a VM default system property (`vm_init.rs`). Weld then constructs **no**
executor and runs its real **single-threaded** `SimpleBeanDeployer` bytecode — no
`ForkJoinPool.invokeAll`, no `RejectedExecutionException`, no hang. Functionally
identical (same beans, no parallelism); the documented Weld config for constrained
environments; an app `-D` overrides it. The earlier eager-inline
`force_native invokeAll` attempt was reverted (branch `e61091d8`) — it turned the
fast FAIL into a worse hang. **Verified:** `CdiSmokeTests` now reaches real bean
processing in ~13s.

**Layer 4 — `WELD-001301`: `@Produces` treated as a qualifier (OPEN, deep).**
With single-threaded bootstrap working, Weld reaches bean processing and throws
`IllegalArgumentException: WELD-001301: Annotation QualifierInstance {Produces} is
not a qualifier`. A probe (`QualProbe`) shows CratonVM's leaf reflection is
**identical to HotSpot** — `Produces.isAnnotationPresent(Qualifier)=false`,
`Default.isAnnotationPresent(Qualifier)=true` — so this is **not** a meta-annotation
reflection bug (same shape as the refuted L1 hypothesis); it is a deeper
Weld-internal qualifier-extraction discrepancy needing in-situ Weld tracing.
Tracked here; not yet fixed.

## Outcome

Layers 1–3 are fixed on `dev`: the documented `WELD-000409` *and* the
`ForkJoinPool.invokeAll` `RejectedExecutionException` + bootstrap hang are gone, and
`CdiSmokeTests` now **fails fast** at a separate deeper layer (`WELD-001301`)
instead of consuming the full per-class timeout — a real win for the whole Weld
cluster's census behavior even though the class is not yet green.

## Symptom

Weld bootstrap aborts during CDI container startup:

```
org.jboss.weld.exceptions.DefinitionException: WELD-000409: Observer method for
  container lifecycle event can only inject BeanManager:
  [BackedAnnotatedMethod] public org.jboss.weld.environment.se.WeldSEBeanRegistrant
    .registerWeldSEContexts(@Observes AfterBeanDiscovery, BeanManager)
```

The flagged method's second parameter **is** `BeanManager`, so on HotSpot this passes — Weld only raises `WELD-000409` when a container-lifecycle observer has a non-event parameter that is not `BeanManager`.

## Investigation — the obvious reflection hypotheses are REFUTED

A direct probe (`WeldParamProbe`) of `WeldSEBeanRegistrant.registerWeldSEContexts` shows CratonVM's reflection of this method is **byte-identical to HotSpot**:

| query | CratonVM | HotSpot |
|---|---|---|
| `getParameterTypes()[1]` | `interface …spi.BeanManager` | same |
| `getGenericParameterTypes()[1]` | `interface …spi.BeanManager` | same |
| `BeanManager.class.isAssignableFrom(param[1])` | **true** | true |
| `param[1] == BeanManager.class` | **true** | true |
| `getParameterAnnotations()[0]` | `{@Observes}` | `{@Observes}` |
| `Parameter[0].getAnnotations()` | `{@Observes}` | `{@Observes}` |
| `getParameterAnnotations()[1]` | `{}` | `{}` |

So the validator's inputs — the `BeanManager` raw/generic type, interface assignability, and the `@Observes` parameter annotation that marks param[0] as the event — are all correct under CratonVM. The defect is **not** in parameter-type or parameter-annotation reflection.

## Root cause (revised — still open)

`WELD-000409` therefore originates deeper inside Weld's bootstrap, not in the leaf reflection my probe exercised. Candidates, in order of likelihood:

1. Weld builds its `BackedAnnotatedType` for `WeldSEBeanRegistrant` by reflecting **all** members; a failure on some *other* member (a different method/field's annotated type, a generic signature, an `AnnotatedType` element CratonVM models differently) corrupts the annotated-type model so the observer's event parameter is no longer recognized as the event during validation.
2. Weld's event-parameter detection runs over its own `EnhancedAnnotatedMethod` view (not the raw `Method`), which is assembled from a CratonVM-provided source that differs only along a path the isolated probe does not hit (e.g. annotation **default-value** materialization, repeating/meta annotations, or `@Observes`-on-`AnnotatedParameter` vs on-`Parameter`).

Either way it requires **in-situ tracing of the live Weld bootstrap** (CdiSmokeTests), not a leaf reflection probe.

## Why it is deep

Localizing it means instrumenting Weld's `BackedAnnotatedType`/observer-validation while the container boots and finding the single reflective answer CratonVM gives differently — across the full CDI-API + Weld annotated-type surface — without disturbing the reflection the 590+ passing classes depend on.

## Next steps

Trace the live bootstrap: enable Weld debug logging during `CdiSmokeTests` and diff the constructed `AnnotatedType`/observer model element-by-element against HotSpot, focusing on which member of `WeldSEBeanRegistrant` (or which `AnnotatedParameter` view) Weld actually feeds into the `WELD-000409` check.

## 2026-08-04 correction — the whole 14-class CDI cluster FAILs again, by default, on fresh `dev`

This doc's "Layer 2" fix (seed `org.jboss.weld.executor.threadPoolType=NONE`
by default so Weld avoids `ForkJoinPool.invokeAll` entirely) was itself
conditioned on `!flags().natives.real_forkjoinpool` (`vm/src/vm/vm_init.rs`).
Commit `16ec5d7ad` ("wip: deep-audit agent handoff snapshot", 2026-07-30)
changed `real_forkjoinpool`'s resolution from opt-in
(`present(CRATONVM_REAL_FORKJOINPOOL)`) to **default-on**
(`!present(CRATONVM_SYNTHETIC_FORKJOINPOOL) || present(CRATONVM_REAL_FORKJOINPOOL)`)
— so the `threadPoolType=NONE` seed silently stopped firing by default, and
every CDI class now goes through Weld's `ConcurrentBeanDeployer` again.

That alone should be fine per this doc's "Real ForkJoinPool fix" section
(gate ON was verified to make `CdiSmokeTests` etc. PASS) — except the real
FJP bridge's method allow-list never covered
`ForkJoinPool.invokeAll(Collection)`, the exact overload
`AbstractExecutorServices.invokeAllAndCheckForExceptions` calls. That
overload falls through to real JDK bytecode against the bridge's
under-initialized `commonPool()` object and throws
`RejectedExecutionException` at `submissionQueue()` — the identical
mechanism this doc's own "Layer 2" section describes for the *synthetic*
pool, just for real-pool mode's uncovered overload instead. Confirmed by a
fresh 2026-08-04 run: all 14 `cdi.*`/`jpa.cdi.*` classes FAIL with this
exact stack. Full analysis, evidence, and a confirmed workaround
(`CRATONVM_SYNTHETIC_FORKJOINPOOL=1`, which restores the `threadPoolType=NONE`
path and makes `CdiSmokeTests` PASS again) are in
[`docs/internal/fixed-suite-bugs/hibernate/cdi-cluster-forkjoinpool-invokeall-rejectedexecution-FIXED-20260804.md`](cdi-cluster-forkjoinpool-invokeall-rejectedexecution-FIXED-20260804.md).
This doc's Layers 1/2/3 fixes are all still present and correct — the
regression is a *new* coverage gap in the real-FJP allow-list exposed only
once real-FJP became the default, not a reversion of anything fixed here.

## 2026-08-04 update — the coverage gap is FIXED; the workaround is retired

`ForkJoinPool.invokeAll(Collection)` (plus `invokeAny`, `invokeAllUninterruptibly`,
`lazySubmit` and `awaitQuiescence`) are now on both real-FJP bridge allow-lists,
and all 14 `cdi.*` / `jpa.cdi.*` classes PASS in the **default** configuration
with per-class `found`/`ok` counts identical to HotSpot.

Two consequences for this doc:

* The `CRATONVM_SYNTHETIC_FORKJOINPOOL=1` workaround quoted above is no longer
  needed for this cluster and should not be used — it forces Weld back onto
  `SimpleBeanDeployer` and hides whether the concurrent path works.
* This doc's "Layer 2" fix (the `org.jboss.weld.executor.threadPoolType=NONE`
  default seed, conditioned on `!flags().natives.real_forkjoinpool`) is now
  genuinely dormant on the default path rather than accidentally so: Weld runs
  its real `ConcurrentBeanDeployer` and the cluster is green that way. The seed
  still guards the `CRATONVM_SYNTHETIC_FORKJOINPOOL` path.
