# `MethodHandle.asSpreader(Object[].class, n)` broken under CratonVM — FIXED (already landed before this was filed)

| | |
|---|---|
| **Status** | **FIXED.** Landed on `dev` as commit `d766af065` ("fix(invoke): stand-in Class mirrors claimed to be primitive, and asSpreader kept the unspread type"), merged 2026-08-19 23:16 via `fix/jetty-jsp-and-groovy-mh-20260819` — **before** this doc was originally drafted. The original filing (below) was written from a 2026-08-19 19:32 sweep using a binary built before the fix landed; independently re-verified fixed 2026-08-20 against a fresh build of current `dev`. |
| **CratonVM (pre-fix binary)** | FAIL — `WrongMethodTypeException: cannot convert MethodHandle(int,int)int to (Object[])Object` |
| **CratonVM (dev tip, rebuilt 2026-08-20)** | OK — `result: 7`, no exception |
| **HotSpot** | OK (always was) |

## Process note
This is a corrected/retracted filing. A 2026-08-19 differential run of the
Spring Framework suite (3 GC variants, 88 common FAILs) surfaced this exact
`WrongMethodTypeException` shape across ~11 classes and it was independently
root-caused via a minimal, Groovy-free probe — without first checking
whether `dev` had already moved past the binary the sweep used. It had: the
fix landed 1h20m before the sweep's own ZGC leg even finished. The
known-issues doc originally filed for this
(`bug-spring-methodhandle-asspreader-groovy-invocation-cluster-20260819.md`)
has been removed from `docs/known-issues/spring/` and replaced by this one.
Lesson: **check `dev` tip / recent merges for a matching fix before filing a
differential-sweep finding as OPEN**, especially when the sweep's binary is
more than a few hours old relative to when the doc is written.

## Independent verification (2026-08-20)
The same minimal repro used to originally root-cause this was rerun against
a fresh `cargo build --release --features zgc` from current `dev`
(commit `b97c40d97`, which includes `d766af065`):
```java
MethodHandle mh = MethodHandles.lookup().findStatic(Probe.class, "add",
        MethodType.methodType(int.class, int.class, int.class));
MethodHandle spread = mh.asSpreader(Object[].class, 2)
        .asType(MethodType.methodType(Object.class, Object[].class));
Object result = spread.invoke(new Object[]{3, 4});
```
Pre-fix binary: `WrongMethodTypeException: cannot convert MethodHandle(int,int)int to (Object[])Object`.
Post-fix (dev tip): `result: 7`, no exception. Confirmed fixed.

The `GroovyScriptEvaluatorTests` and Groovy-`ApplicationContext`-loading
classes that showed this exact shape in the 2026-08-19 sweep
(`GroovyApplicationContextTests`, `GroovyBeanDefinitionReaderTests`,
`GroovyControlGroupTests`, `AbsolutePathGroovySpringContextTests`,
`DefaultScriptDetectionGroovySpringContextTests`, `GroovySpringContextTests`,
`MixedXmlAndGroovySpringContextTests`, `RelativePathGroovySpringContextTests`,
`BasicGroovyWacTests`) should now pass on a `dev`-tip build — not
individually reconfirmed class-by-class in this pass, only the underlying
mechanism. The "plausibly related, not confirmed" classes noted in the
original filing (`GroovyAspectTests`, `GroovyAspectIntegrationTests`,
`GroovyScriptFactoryTests`, both `JRubyScriptTemplateTests`) should also be
rechecked against `dev` tip before assuming they need separate
investigation — some or all may already be fixed by the same commit.

## Original filing (as first written, 2026-08-19 sweep)
Found running the full Spring Framework suite (2,848 classes) under
CratonVM on Azure across three parallel GC-variant runs
(generational/G1/ZGC). `MethodHandle.asSpreader` failed at the adapter-
construction call itself — before the handle was ever invoked — with
`WrongMethodTypeException`. Root cause per the fix commit's own message:
`asSpreader` never installed the adapted `type()`, so the spreader reported
its target's unspread signature; Groovy's `IndyInterface.fallback` does
`asSpreader(Object[].class, n).asType((Object[])Object)` on every
`invokedynamic` dispatch, and `asType` then correctly refused a conversion
HotSpot never sees. A second, related defect in the same commit:
`get_or_create_primitive_mirror` wrote `primitive=1` for every class name it
was handed as a generic unresolved-class stand-in, which made
`Class.isPrimitive()` lie and broke `MethodTypeForm.canonicalize`'s erasure
logic (`Wrapper.forPrimitiveType` died with `"not primitive: beans"`).
