# `OnBeanCondition$Spec` intermittently sees `@ConditionalOnMissingBean` as absent via `MergedAnnotations.get(Class)`, even though `AnnotatedTypeMetadata.isAnnotated(String)` correctly finds it moments earlier

**Status: RESOLVED — closed 2026-07-27.** The doc's own claimed symptom
(`IllegalStateException: @ConditionalOnMissingBean did not specify a bean
using type, name or annotation`, from a `MergedAnnotations.get(Class)` /
`isAnnotated(String)` disagreement) never reproduced against current `dev`:
`ManagementWebSecurityAutoConfigurationTests` and
`ReactiveManagementWebSecurityAutoConfigurationTests` passed cleanly across
**23 total runs** (13 initial verification runs plus 10 more during residual
work below), with JIT on and off, before any code change — see
[[check-already-fixed-before-setup]]. Not independently root-caused (no
fixing commit identified for the original symptom); most likely already
fixed by unrelated `dev` drift sometime after 2026-07-23, same pattern as the
doc's own linked residual
[`isolated-loader-onbeancondition-type-deduction-bypass-FIXED.md`](isolated-loader-onbeancondition-type-deduction-bypass-FIXED.md).
Kept here (moved from `known-issues`, not deleted) as a symptom record in
case it regresses.

## A different, real residual WAS found and fixed in the same test method

While verifying the doc's repro, `ReactiveManagementWebSecurityAutoConfigurationTests
.securesEverythingElseWhenHealthIsAbsent()` turned out to still be flaky —
**not** with the doc's claimed symptom, but with a completely different one:

```
java.lang.IllegalStateException: Timeout on blocking read for 30000000000 NANOSECONDS
  reactor.core.publisher.BlockingSingleSubscriber.blockingGet(BlockingSingleSubscriber.java:128)
  ...
```

caused by a `boundedElastic` Reactor scheduler worker thread dying with an
uncaught `NoClassDefFoundError` moments earlier:

```
WARN cratonvm_native_builtins::classloader: URLClassLoader.findClass(reactor/core/scheduler/NonBlocking) define failed: Linkage(IncompatibleClassChangeError { message: "class reactor/core/scheduler/NonBlocking already defined by user-defined(4) loader" })
ERROR reactor.core.scheduler.Schedulers -- Scheduler worker in group main failed with an uncaught exception
java.lang.NoClassDefFoundError
```

Reproduced reliably under `--nojit` (interpreter-only) at roughly a 1-in-6 to
1-in-10 rate across three independent un-instrumented baseline batches
(10 runs / 1 failure, 30 runs / 7 failures — the higher rate likely reflects
this being a shared, contended build host); essentially never reproduced
under normal JIT operation in small samples, and — notably — **never
reproduced at all once any diagnostic tracing was added**, even
minimal/narrowed tracing gated on the exact class name (62 total instrumented
runs across 4 separate tracing designs, 0 failures). This is a textbook
Heisenbug: even a cheap, conditionally-skipped `std::env::var()` check on the
hot classloading path shifts thread scheduling enough to close the race
window. The exact live interleaving was never captured; the fix below was
derived from static analysis of the locking design and validated by the
dramatic before/after failure-rate change (10-23% → **0/40** clean, plus
16 more clean runs across both the servlet and reactive variants with JIT
on and off, plus the full `cratonvm-classloading` unit suite — 631/631 — and
a from-scratch verification that 5 unrelated `cratonvm-native-builtins` unit
test failures encountered along the way are pre-existing on unmodified `dev`,
not caused by this change).

### Root cause (structural, high confidence; exact live trace not captured)

`native-builtins/src/classloader.rs`'s `ucl_try_define_local_class`
(`URLClassLoader.findClass` override) already has a per-`(loader_namespace_id,
class_name)` mutex+condvar (`url_classloader_define_locks`) specifically
designed to prevent two THREADS BOTH calling `findClass` for the same
not-yet-loaded class from racing — see that function's own doc comment,
which already documents exactly this class of bug for
`OnClassCondition$ThreadedOutcomesResolver`. That lock only covers callers
that enter through `ucl_try_define_local_class` itself.

`classloading/src/class_manager.rs`'s `ClassManager::define_class` —
while defining ANY class — recursively resolves that class's superclass and
declared interfaces via a `resolve_supertype` closure, which (when the
current loader hasn't already defined the supertype) falls through to the
generic, loader-agnostic `ClassManager::load_class`. This recursive path
runs **entirely independently of `ucl_try_define_local_class`'s lock** — it
is reached only via a normal Rust method call already inside another
`define_class` invocation, never re-entering the native-builtins entry
point or its lock table.

Reactor's `boundedElastic` scheduler creates a worker `Thread` subclass
(`ReactorThreadFactory$NonBlockingThread`) that implements the
`reactor.core.scheduler.NonBlocking` marker interface. When a background
scheduler thread defines that worker class for the first time (recursively
resolving its `NonBlocking` interface via `resolve_supertype`) at the same
moment the main thread independently resolves `NonBlocking` directly
(e.g. via reflection/`Class.forName` on the isolated
`ModifiedClassPathClassLoader`, going through the PROPERLY locked
`ucl_try_define_local_class` path), both can reach
`ClassManager::define_class("reactor/core/scheduler/NonBlocking",
loader_id=4, ...)` without any mutual exclusion between them. The loser
hits the pre-existing "WP2.3: Duplicate-define rejection" check and gets a
hard `IncompatibleClassChangeError` ("class ... already defined by
user-defined(4) loader"), surfaced to Java as `NoClassDefFoundError` — which
silently kills the `boundedElastic` worker thread the reactive pipeline was
depending on, so the `Mono` driving the test's HTTP-filter chain never
completes and `Mono.block()` times out after 30s.

### Fix

Rather than adding a second lock layer at the recursive-resolution site
(risking new deadlocks/lock-ordering issues in a very hot, deeply-recursive
path), made BOTH the recursive path and the top-level path treat "already
defined by user-defined(N) loader" as a **benign, recoverable** outcome —
exactly the guarantee a real JVM's per-`(loader, name)` `SystemDictionary`
placeholder table provides, and exactly what the existing
`url_classloader_define_locks` comment already says is the intended
behavior, just not reachable from every code path that can trigger a
definition:

- `classloading/src/class_manager.rs`: `resolve_supertype`'s fallback now
  catches an `IncompatibleClassChangeError` whose message matches
  `"already defined by"`, re-probes `loaded_classes_probe` for the winner's
  `ClassId`, and returns that instead of propagating the error — but ONLY
  when a winner is actually found (a genuine, differently-caused
  `IncompatibleClassChangeError`, e.g. a sealed-class violation, still
  propagates normally, since the message text and the probe both have to
  agree).
- `native-builtins/src/classloader.rs`: `ucl_try_define_local_class`'s
  `Ok(Err(msg))` branch does the same at the top level — on the specific
  "already defined by" message, look up the existing mirror via
  `find_loaded_class_for_loader` and return it as success; any other
  `define_class_full` failure keeps the original hard-error behavior
  unchanged.

Both changes are narrowly scoped (matched on the exact error-message
substring, not a blanket catch) and purely additive on the failure path —
the success path for every other class, in every other test, is untouched.

## Repro (for the residual, if it regresses)

Same harness as the original doc. On the Azure Linux build host:
`module/spring-boot-security`'s `ReactiveManagementWebSecurityAutoConfigurationTests`
via `apps/spring-boot-suite-runner`, `--nojit`, repeated ~20-40x — a clean
run shows `tests=9 failed=0`; the (pre-fix) failure showed `tests=9 failed=1`
with the `Timeout on blocking read` / `NonBlocking already defined` pair
above.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.actuate.web.servlet.ManagementWebSecurityAutoConfigurationTests` |
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.actuate.web.reactive.ReactiveManagementWebSecurityAutoConfigurationTests` |
