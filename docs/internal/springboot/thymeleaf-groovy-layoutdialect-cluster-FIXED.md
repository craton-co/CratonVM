# Thymeleaf autoconfigure — Groovy layout-dialect constructor mismatch + missing `CapturedOutput` warning — FIXED

**Status: FIXED — 2026-07-19** (originally filed 2026-07-17 as
`docs/known-issues/springboot/thymeleaf-groovy-layoutdialect-cluster.md`)

## Cluster A — Groovy `DecorateProcessor` constructor mismatch

### Symptom (original)

```
groovy.lang.GroovyRuntimeException: Could not find matching constructor for: nz.net.ultraq.thymeleaf.layoutdialect.decorators.DecorateProcessor(org.thymeleaf.templatemode.TemplateMode, String, nz.net.ultraq.thymeleaf.layoutdialect.decorators.strategies.AppendingStrategy, Integer, Integer)
   at groovy.lang.MetaClassImpl.invokeConstructor(MetaClassImpl.java:1816/1591)
   at nz.net.ultraq.thymeleaf.layoutdialect.LayoutDialect.getProcessors(LayoutDialect.groovy:119)
```

`ThymeleafReactiveAutoConfigurationTests` (5/6) and
`ThymeleafServletAutoConfigurationTests` (7/8) failed identically.

### Root cause

**Not the reflection-metadata mismatch originally hypothesized.** The real
constructor Groovy needed is
`DecorateProcessor(TemplateMode, String, AppendingStrategy, Integer, Integer,
boolean, boolean)` — a **7-arg** constructor with two trailing `boolean`
flags that the original symptom log line simply didn't print (Groovy's error
message elides the tail).

Groovy's constructor selection for this call site goes through a generic
`invokedynamic` bootstrap (`vm/src/runtime/invokedynamic.rs`
`bootstrap_generic`), which ultimately invokes the resolved target through
CratonVM's generic `MethodHandle.invoke([Ljava/lang/Object;)Ljava/lang/Object;`
bridge — **erasing every call-site argument's real type to `Object`**
regardless of the actual `invokedynamic` descriptor. A `boolean` call-site
argument, erased this way, arrives as `Value::Int` and gets boxed as
`Integer` by the generic bridge instead of `Boolean`. Groovy's own
constructor-overload matching (`MetaClassImpl.invokeConstructor`) then
rejects the 7-arg constructor because the last two supplied arguments are
`Integer`, not `Boolean` — so it reports "no matching constructor" for what
is, from Groovy's perspective, a completely different (and nonexistent)
`Integer, Integer`-tailed 5-arg signature.

## Cluster B — missing `CapturedOutput` warning for a nonexistent template location

### Symptom (original)

`templateLocationDoesNotExist()` (same two classes):

```
java.lang.AssertionError:
Expecting actual: ""
to contain: "Cannot find template location"
```

### Root cause

Not a Thymeleaf-specific bug at all — a member of the much larger
`docs/internal/springboot/conditionevaluationreport-capturedoutput-empty-cluster-FIXED.md`
/ `docs/known-issues/springboot/capturedoutput-empty-console-cluster.md`
family (Logger/LogFactory synthetic native stubs never delivering formatted
log text to `System.out`/`System.err`, so `CapturedOutput` saw nothing
regardless of how many matching log lines were actually emitted). Fixed
project-wide by that cluster's real-bytecode fix (removing the native
`Logger`/`LogFactory` overrides so real Logback/commons-logging bytecode
runs), not by anything specific to this doc.

## Fix

- `vm/src/runtime/invokedynamic.rs` (`bootstrap_generic`): preserve the
  actual `invokedynamic` target descriptor (`info.target_descriptor`) when
  invoking the resolved `MethodHandle`, instead of always using the erased
  `([Ljava/lang/Object;)Ljava/lang/Object;` signature; explicitly box any
  `Z` (boolean)-typed call-site argument as `Boolean` (via
  `Boolean.valueOf`) before it enters the generic bridge, so a dynamic
  Groovy target observes the correct runtime type either way.
- `native-builtins/src/lang_class.rs` (`native_class_get_constructors`):
  incidental hardening found while investigating this cluster — root the
  destination `Constructor[]` array before the loop that allocates each
  element's mirror object (`create_constructor_object` can itself allocate
  and trigger class-loading), matching `getDeclaredConstructors()`'s existing
  pattern; also fixed by `dev` in parallel to type the array's component as
  `java/lang/reflect/Constructor` rather than `Object` (`build_mirror_array_comp`
  + `reflection_component_id`).
- Cluster B required no Thymeleaf-specific change — see the `CapturedOutput`
  cluster's own FIXED doc.

## Verification

Binary: `target/release/cratonvm.exe` built from worktree
`CratonVM-thymeleaf-groovy-layoutdialect-20260718-019f768d`, branch
`codex/fix-thymeleaf-groovy-layoutdialect-20260718-019f768d`, merged to
current `dev` tip (`686d86361`) before verification.

- `ThymeleafReactiveAutoConfigurationTests`: **20/21 passing** (up from
  16/21 — 5 Cluster A + the `templateLocationDoesNotExist` Cluster B test all
  now pass). The 1 remaining failure (`templateLocationEmpty`) is a
  different, unrelated bug — see
  [`../../known-issues/springboot/path-tostring-indy-stringconcat-dead-dispatch.md`](../../known-issues/springboot/path-tostring-indy-stringconcat-dead-dispatch.md).
- `ThymeleafServletAutoConfigurationTests`: one test in this class
  (`createLayoutFromConfigClass`) now runs far enough to hit a **different,
  newly-exposed hang** (Groovy `MetaClass`/`java.beans.Introspector`
  building for a template-rendering context) — see
  [`../../known-issues/springboot/thymeleaf-groovy-layoutdialect-metaclass-introspection-hang.md`](../../known-issues/springboot/thymeleaf-groovy-layoutdialect-metaclass-introspection-hang.md).
  Excluding just that one test method (JUnit Platform `selectMethod`,
  bypassing the shared `SbRunner`'s whole-class-only selection), the other
  **26/26 non-hung tests pass except the same `templateLocationEmpty`**
  Path-dead-dispatch residual noted above — confirming both Cluster A (the
  constructor mismatch, including `createFromConfigClass` and the other
  6 previously-failing tests) and Cluster B
  (`templateLocationDoesNotExist`) are genuinely fixed for this class too,
  and that the hang is an isolated, unrelated new finding rather than a
  regression from this fix.
- A differential check against the `dev`-tip binary **without** this fix
  reproduced the original `GroovyRuntimeException` in
  `ThymeleafServletAutoConfigurationTests` (confirming the bug was real and
  unrelated fixes on `dev` hadn't already resolved it) and, notably, did
  **not** hang — it failed fast on the constructor mismatch before ever
  reaching the newly-exposed `MetaClass`-introspection code path, which is
  why that hang was never previously observed.
- A standalone repro (`Introspector.getBeanInfo()` called directly on
  `nz.net.ultraq.thymeleaf.layoutdialect.extensions.FragmentExtensions`)
  ran in 49ms under this fix's CratonVM binary (vs 101ms on real HotSpot
  JDK 25) — ruling out that specific class/call as the source of the new
  hang; the actual slow/stuck introspection target is a different,
  not-yet-identified class (see the hang doc for detail).

## Affected classes (now passing, modulo the two residuals above)

- `module/spring-boot-thymeleaf` | `ThymeleafReactiveAutoConfigurationTests`
- `module/spring-boot-thymeleaf` | `ThymeleafServletAutoConfigurationTests`
