# `JacksonAutoConfigurationTests` — JUnit5 `DisabledCondition` NPE — RESOLVED

**Status: FIXED (2026-08-05).** Not an annotation-proxy native returning a raw
`null`. It is the recycled-`JitInvokeInfo` dispatch aliasing fixed by
`383e7f5cf`; see
`flywayautoconfigurationtests-timeout-jit-site-cache-aliasing-FIXED-20260805.md`
for the mechanism and the bisection.

## Original symptom (as filed)

`tests=69 failed=22 containersFailed=32` in the 2026-08-05 Azure full-suite
run — 32 containers never enumerated at all, which is why only 69 of the
class's 162 tests started:

```
org.junit.jupiter.engine.execution.ConditionEvaluationException: Failed to
  evaluate condition [org.junit.jupiter.engine.extension.DisabledCondition]:
  Cannot invoke "java.util.Optional.isPresent()" because "metaAnnotation" is null
Caused by: java.lang.NullPointerException: Cannot invoke "java.util.Optional.isPresent()"
  because the return value of
  "org.junit.platform.commons.util.AnnotationUtils.findAnnotation(AnnotatedElement, Class)"
  is null
```

The page's reasoning up to this point was correct and worth keeping:
`AnnotationUtils.findAnnotation` is declared to return `Optional<A>` and is
ordinary Java bytecode, so a bare `null` coming out of it cannot be JUnit
malfunctioning — something underneath it returned the wrong thing.

## Root cause

The filed hypothesis named the *right layer* and the *wrong mechanism*: it
expected one of the reflective annotation-lookup natives in
`native-builtins/src/lang_class.rs` to be returning a raw `null` array for
some `AnnotatedElement` shape peculiar to this class's `@ParameterizedTest`
methods.

No native returns `null` here. The **call site** returned another site's
value. `vm/src/jit/helpers.rs` keys its per-thread dispatch memos on
`(vm_identity, JitInvokeInfo pointer)`; those boxes are freed with their
`CompiledMethod`, and after `836631dcc` (`NATIVE_SITE_CACHE` holds a resolved
native callback for *every* native call from compiled code) a recycled
address makes a compiled site CALL the previous site's native and return
whatever that returns. A reference-returning reflective site that inherits a
different native's answer yields exactly a bare `null` where an `Optional`
belongs.

The doc's closing instinct — "it plausibly affects any class whose
`@Disabled`/meta-annotation scanning hits the same reflective shape" — was
right, but not because of a shape: because the aliasing is site-lifetime
driven and hits whatever is compiled, dropped and recompiled most. The same
defect produced `MergedAnnotation.isPresent() because "annotation" is null`
in `WebMvcObservationAutoConfigurationTests` (see
`classfile-annotation-metadata-corruption-FIXED-20260805.md`) — the same NPE
shape, a different library.

## Not the 08-02 failure

This class was **already broken before the aliasing regression landed**, and
differently: Azure recorded `08-02 HANG 300.140s` (zero tests started), then
`08-05 FAIL 22/69`. So unlike Flyway/Rabbit/`TomcatServletWebServer`, which
regressed cleanly inside the 08-02→08-05 window, Jackson's history is
hang-then-NPE. Both are now gone; the hang belonged to the throughput family
closed by `jacksonautoconfigurationtests-severe-slowdown-FIXED-20260722.md`
and the surrounding 08-01→08-05 GC/JIT work.

## Validation

Local Windows, one process per class, runner env vars, `--Xmx 2g`:

| Binary | Result | Wall |
|---|---|---:|
| 08-05 full-suite binary (no `383e7f5cf`) | **22 failed / 32 containersFailed**, only 69 of 162 started | 70.7s (Azure) |
| current dev `96acd76ed` (has `383e7f5cf`) | **162/162 PASS**, 0 containersFailed | 308.0s |
| HotSpot 25.0.3+9 control | 162/162 PASS | 18.9s |

Neither `metaAnnotation`, `DisabledCondition` nor `ConditionEvaluationException`
appears anywhere in the fixed run's stdout or stderr. The container count is
the load-bearing number here: 162 tests **started**, not 69.

Azure Linux (`/data/sbrun.sh`, `--Xmx 4g`, worktree
`/data/data/wt-flywayfix-20260805` at `origin/dev`):

```
[1/2] jit JacksonAutoConfigurationTests rc=0 PASS 336s tests=162 failed=0 aborted=0 containersFailed=0
[2/2] jit JacksonAutoConfigurationTests rc=0 PASS 343s tests=162 failed=0 aborted=0 containersFailed=0
```

(load average ~30 on the 16-core host.)

## This class does NOT fit the base 300s budget — carve-out added

The correctness bug is closed, but **336s and 343s are over the standard 300s
shard timeout**, so without further action the suite would keep reporting this
class as a `HANG` — which is exactly what the `08-02 HANG 300.140s` row is,
and what the 2026-07-22 severe-slowdown page was before it. It is not stuck;
it runs to natural completion with all 162 tests every time.

Handled the way this repo already handles finite-but-slow classes
(`RabbitAutoConfigurationTests` 900, `OriginTrackedYamlLoaderTests` 1100,
`ConfigurationPropertySourcesTests`): a documented carve-out in
`run-spring-boot-suite.ps1`, tagged `JACKSON-BUDGET.1`, set to 900s with the
measured times recorded beside it.

**The carve-out is not the fix for the ratio.** 336s against a 22.4s HotSpot
baseline is 15x, and the retired 07-22 page recorded ~225s for the same 162
tests, so the class has also gotten slower since. That belongs to the
suite-wide Spring-bootstrap throughput work, not to this page. What the
carve-out buys is an honest verdict: a `PASS` with a real number instead of a
`HANG` that hides one.

If this class does fail again, the two causes are trivial to tell apart in
`results.tsv`: this NPE always showed `containersFailed>0` (32 of them), a
budget overrun shows `tests=0`.

## Affected classes

- `module/spring-boot-jackson` — `org.springframework.boot.jackson.autoconfigure.JacksonAutoConfigurationTests`

Original log:
`craton-fullsuite-azure-20260805-s5/all-jit/logs/module_spring-boot-jackson.org.springframework.boot.jackson.autoconfigure.JacksonAutoConfigurationTests.{out,err}.log`
