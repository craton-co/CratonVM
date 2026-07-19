# `ThymeleafServletAutoConfigurationTests.createLayoutFromConfigClass` hangs building a Groovy `MetaClass` during real template rendering

**Status: OPEN — found 2026-07-19, newly exposed by the `DecorateProcessor` constructor-mismatch fix**

## Background

`docs/known-issues/springboot/thymeleaf-groovy-layoutdialect-cluster.md`
(now closed — see
`docs/internal/springboot/thymeleaf-groovy-layoutdialect-cluster-FIXED.md`)
tracked a `GroovyRuntimeException: Could not find matching constructor for
...DecorateProcessor(...)` that made `ThymeleafServletAutoConfigurationTests`
fail 7/8 tests and crash before ever reaching real template rendering. Once
that bug was fixed (real root cause: `invokedynamic`'s generic
`MethodHandle.invoke` bridge erased every call-site descriptor to
`([Ljava/lang/Object;)Ljava/lang/Object;`, so a trailing `boolean` argument
in Groovy's constructor-selection call arrived boxed as `Integer` instead of
`Boolean` — see the fix commit in `vm/src/runtime/invokedynamic.rs`
`bootstrap_generic`), the whole test class runs far enough to expose this
**new, previously-unreachable** hang.

## Symptom

`createLayoutFromConfigClass` (the one test in this class that actually
renders a Thymeleaf template through the real `nz.net.ultraq` layout-dialect
`FragmentProcessor`) never completes — the suite runner reports `HANG` at
whatever timeout is configured (confirmed hung past 300s). It is also the
first test JUnit5 selects to run in this class (no `@TestMethodOrder`
declared), so left in, it blocks every other test in
`ThymeleafServletAutoConfigurationTests` from ever running in the same
process.

A `--stack-dump-on-timeout` capture shows the main thread parked in the same
call chain across many successive dumps (not visibly progressing between
dumps taken ~1s apart):

```
nz/net/ultraq/thymeleaf/layoutdialect/fragments/FragmentProcessor.doProcess
  -> nz/net/ultraq/thymeleaf/layoutdialect/extensions/FragmentExtensions.getFragmentCollection
  -> org/codehaus/groovy/vmplugin/v8/IndyInterface.fromCacheHandle/fallback
  -> org/codehaus/groovy/vmplugin/v8/Selector$MethodSelector.getMetaClass
  -> org/codehaus/groovy/runtime/metaclass/MetaClassRegistryImpl.getMetaClass
  -> groovy/lang/MetaClassImpl.initialize/reinitialize/addProperties
  -> java/beans/Introspector.getBeanInfo/getTargetMethodInfo
  -> java/beans/MethodDescriptor.<init> -> java/beans/FeatureDescriptor.getParameterTypes
  -> com/sun/beans/TypeResolver.resolve (self-recursive, 3 levels)
  -> com/sun/beans/WeakCache.get -> java/util/WeakHashMap.get/hash -> java/util/Arrays.hashCode
```

i.e. Groovy is building a `MetaClass` for the first time (via
`java.beans.Introspector`, Groovy's standard mechanism for MOP
introspection) for whatever object is the receiver of the
`getFragmentCollection(ITemplateContext)` extension-method call site.

## What's already ruled out

A standalone repro was built (`ReproIntrospect.java`) that directly calls
`Introspector.getBeanInfo()` on
`nz.net.ultraq.thymeleaf.layoutdialect.extensions.FragmentExtensions` itself
(the class whose static extension methods are being dispatched), using the
same classpath as the failing test:

- Real HotSpot (JDK 25): `methods.length=19`, `getBeanInfo` takes **101ms**.
- CratonVM (this fix's binary, `--java-home` pointed at the same JDK):
  `methods.length=19` (identical), `getBeanInfo` takes **49ms** — also fast,
  no hang.

So introspecting `FragmentExtensions` itself is **not** the slow part in
either runtime. Since Groovy extension-method dispatch resolves the
`MetaClass` of the **receiver** of the call (here, whatever concrete class
implements `ITemplateContext`/backs the template-processing context at that
point — not `FragmentExtensions`), the actual object being introspected when
the hang occurs is a different, not-yet-identified class — very likely a
Thymeleaf-internal context implementation with a larger/more complex
interface and generic-method surface than the plain extension class tested
above. This still needs to be pinned down (e.g. by instrumenting
`Selector$MethodSelector.getMetaClass()`'s receiver, or adding an
`eprintln!` at the `Introspector.getBeanInfo` native/interpreted entry point
to print which `Class` is being processed) before a root cause can be
proposed.

## Reproduction

```
apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 -ClassList <tsv with just this class> \
  -Exe <cratonvm.exe with the DecorateProcessor fix> -JdkHome <real JDK 25 home> \
  -Parallel 1 -TimeoutSec 300 -CratonArgs @('--stack-dump-on-timeout','30')
```

Excluding just this one test method (via a custom JUnit Platform
`selectMethod` launcher, since the shared `SbRunner` only supports whole-class
selection) lets the rest of the class run to completion normally — 26/26
non-hung tests pass except the unrelated
[`path-tostring-indy-stringconcat-dead-dispatch.md`](path-tostring-indy-stringconcat-dead-dispatch.md)
residual (`templateLocationEmpty`). This confirms the hang is isolated to
`createLayoutFromConfigClass` and does not indicate a broader regression
from the `DecorateProcessor` fix.

## Affected classes

- `module/spring-boot-thymeleaf` | `ThymeleafServletAutoConfigurationTests` | `createLayoutFromConfigClass()` (HANG, blocks the rest of the class from running in the same process)
