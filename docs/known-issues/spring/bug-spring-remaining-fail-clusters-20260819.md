# Spring Framework: remaining FAIL clusters not explained by the CHM/MethodHandle bugs

## Status
**OPEN, mixed confidence, root causes NOT identified** — found 2026-08-19
running the full Spring Framework suite under CratonVM on Azure. This is a
triage/pointer doc covering the FAIL classes (common to all three GC
variants) left over after accounting for:
* `bug-spring-concurrenthashmap-entrysetview-removeif-npe-20260819.md` (66
  classes — `ConcurrentHashMap.entrySet().removeIf()` NPE, root-caused);
* `MethodHandle.asSpreader` (11 classes matched this shape in the sweep) —
  turned out to be **already fixed** on `dev` before this sweep even
  finished (commit `d766af065`, merged 2026-08-19 23:16, `dev` tip
  `b97c40d97`) — see
  `fixed-suite-bugs/spring/bug-spring-methodhandle-asspreader-groovy-invocation-cluster-20260819-FIXED-20260820.md`.
  Independently reverified 2026-08-20 against a fresh `dev`-tip build: fixed.

That leaves roughly 10 classes (one more, `FileNativeConfigurationWriterTests`,
was confirmed NOT a CratonVM bug on 2026-08-20 and moved out — see
`not-cratonvm-bugs-consolidated.md`) plus whichever of the "plausibly related"
Groovy/JRuby classes below turn out to share the (now-fixed) `asSpreader`
root cause — reverify those against `dev` tip before investigating them as a
separate open issue. Each item below is a distinct signature, not
individually root-caused — grouped only where the evidence directly suggests
it.

## Note (2026-08-20): some Groovy classes not listed here may already be fixed
`GroovyAspectTests`, `GroovyAspectIntegrationTests`, `GroovyScriptFactoryTests`,
and both `JRubyScriptTemplateTests` classes were filed as "plausibly related"
to the `MethodHandle.asSpreader` bug in the (now-superseded) doc referenced
above — that bug turned out to already be fixed on `dev`. Recheck those
classes against `dev` tip before investigating them as a separate open
issue; see the FIXED doc's "Independent verification" section for the full
list.

## Groovy template **compile**-time failures (2 classes) — different failure stage from the MethodHandle cluster
```
GroovyMarkupViewTests: renderI18nTemplate/renderLayoutTemplate/renderMarkupTemplate
  org.codehaus.groovy.control.MultipleCompilationErrorsException: startup failed:
ViewResolutionIntegrationTests.groovyMarkup():
  jakarta.servlet.ServletException: Request processing failed:
    org.codehaus.groovy.control.MultipleCompilationErrorsException: startup failed:
```
Both fail while **compiling** a `.tpl` Groovy markup template, not while
invoking a compiled script through `MethodHandle.asSpreader` (the runtime
adapter bug documented separately). This could be an unrelated, genuine
Groovy/markup-template compiler issue (version skew, a missing compiler
flag/classpath entry the test harness doesn't reproduce) rather than a
CratonVM defect — not distinguished in this pass. Needs the full compiler
error text (truncated to "startup failed:" in the pooled failcause summary
used for this triage) to make any further call.

## `AnnotationTransactionAttributeSourceTests.serializable()` — `InvalidObjectException: invalid object`
```
java.io.InvalidObjectException: invalid object
```
A Java serialization round-trip test (`serializable()` — almost certainly
serializes then deserializes an `AnnotationTransactionAttributeSource` and
asserts equality) fails during deserialization with a generic
"invalid object" message. `InvalidObjectException` with this exact generic
text is what `readResolve()`/`ObjectInputValidation`/`readObject`
validation throws when a custom check rejects the reconstructed object — the
specific validation that's failing isn't visible from the one-line failcause;
needs the full stack trace.

## `FileNativeConfigurationWriterTests` — RESOLVED: confirmed NOT a CratonVM bug (2026-08-20)
Rechecked under stock HotSpot 25 on the same Azure host as part of the
2026-08-20 88-class HotSpot baseline: fails identically, all 5 methods,
`java.lang.AssertionError: \nUnexpected: comment` — a JSON-fixture/parser
issue (a comment token in a fixture a strict JSON parser rejects), not
VM-specific. Moved to
`not-cratonvm-bugs-consolidated.md`.

## `WebClientIntegrationTests` — Reactor `VerifySubscriber` timeout
```
[4] HttpComponents :: java.lang.AssertionError: VerifySubscriber timed out on reactor.core.publisher.MonoFlatMap$FlatMapMain@...
```
A reactive-stream test's `StepVerifier`-style assertion times out waiting for
a `Mono` to complete, specifically on the HttpComponents client variant of a
parameterized test (other client variants of the same test presumably pass,
not confirmed). Could be a genuine hang/deadlock in CratonVM's handling of
that specific HTTP client's reactive adapter, or a flake under host load
(6-way parallel sharding was in effect) — not distinguished; would need an
isolated single-shard rerun to rule out load-induced flakiness before
treating as a defect.

## `InvocableHandlerMethodKotlinTests.genericParameter()` — Kotlin reflection NPE
```
java.lang.NullPointerException: Parameter specified as non-null is null: method kotlin.reflect.jvm.internal.impl.types.SimpleTypeImpl.<init>, parameter arguments
```
A `kotlin-reflect` internal precondition (`Intrinsics.checkNotNullParameter`)
fires because a null was passed where Kotlin's reflection metadata expects a
non-null `arguments` list when constructing a `SimpleTypeImpl`. This is deep
inside `kotlin-reflect`'s own internals, reached via CratonVM's Java
reflection surface (`InvocableHandlerMethod` uses Java reflection to inspect
a Kotlin method's generic parameter type, and `kotlin-reflect` layers its own
reflection on top of that). Plausible that this is a gap in CratonVM's
`java.lang.reflect.Type`/generic-signature support surfacing through
Kotlin's reflection bridge, but not isolated to a minimal repro in this pass.

## Next steps
* `FileNativeConfigurationWriterTests` and `AnnotationTransactionAttributeSourceTests`
  are the most promising to pick up next — both are single-class, narrowly-scoped
  failures where pulling the full (non-truncated) test output should make the
  actual defect obvious quickly, the same way the CHM and MethodHandle bugs were
  found by going one level past the one-line summary.
* `WebClientIntegrationTests` should be reran alone (not under 6-way parallel
  sharding) before concluding anything, given the timeout-shaped failure.
* The two Groovy-template compile failures should be checked against
  HotSpot's exact Groovy/templating classpath version to rule out a
  classpath/version mismatch before assuming a CratonVM defect.

## Repro
```bash
cd apps/spring-suite-runner
JDK25=/data/toolchain/jdk-25 SPRING=/data/cratonvm/apps/spring-framework \
CRATONVM_BIN=<cratonvm-bin> \
./run-suite.sh run --category all --only 'FileNativeConfigurationWriterTests|AnnotationTransactionAttributeSourceTests|WebClientIntegrationTests|InvocableHandlerMethodKotlinTests|GroovyMarkupViewTests|ViewResolutionIntegrationTests' --tag remaining-triage
```
