# Kotlin-reflect calls throw `IllegalStateException: root` with an empty stack trace — 30 Spring Kotlin test classes, likely a THIRD distinct kotlin-reflect defect

| | |
|---|---|
| **Status** | OPEN — newly surfaced, not root-caused. Needs a minimal standalone repro before further diagnosis. |
| **Category** | VM-CORRECTNESS (reflection / Kotlin interop) |
| **Discovered** | 2026-07-26, full-suite run, dev `346c74b71`, Azure host `20.83.144.174`, worktree `/data/data/wt-springsuite8-20260725`, `apps/spring-suite-runner`, real JDK 25, jit-real. |
| **CratonVM** | FAIL — `java.lang.IllegalStateException: root`, **zero-frame stack trace** (see below) |
| **HotSpot** | not independently re-verified this session, but every affected class is HotSpot-clean in the Jul22-24 baseline (`fullsuite_lf.tsv`) used for regression comparison |

## Symptom

94 individual test-method failures across 30 distinct classes, every one
throwing the exact same exception with the exact same one-word message:

```
java.lang.IllegalStateException: root
```

Example (`MethodParameterKotlinTests`):
```
FAILCAUSE org.springframework.core.MethodParameterKotlinTests :: Inner class constructor() :: java.lang.IllegalStateException: root
FAILCAUSE org.springframework.core.MethodParameterKotlinTests :: Method parameter with default value() :: java.lang.IllegalStateException: root
FAILCAUSE org.springframework.core.MethodParameterKotlinTests :: Parameter name for suspending function() :: java.lang.IllegalStateException: root
FAILCAUSE org.springframework.core.MethodParameterKotlinTests :: Regular class constructor() :: java.lang.IllegalStateException: root
FAILCAUSE org.springframework.core.MethodParameterKotlinTests :: Method return type nullability() :: java.lang.IllegalStateException: root
```

**Notable: no stack trace at all.** `KRun.java` calls `t.printStackTrace(out)`
right after logging each `FAILCAUSE` line (see `KRun.java:74-77`), and for
every other failure type in the same run (e.g. `SerializationUtilsTests`,
`BeanFactoryUtilsTests`) a normal multi-frame stack trace appears
immediately after in `raw.log`. For every one of these 94 `IllegalStateException:
root` failures, **nothing follows** — the next line in `raw.log` is the next
`BEGIN <class>` marker. A real, bytecode-thrown `IllegalStateException`
from Kotlin/JDK library code always carries a populated stack trace; an
empty one strongly suggests this exception is either being raised without
`fillInStackTrace()` running, or is a synthetic exception constructed
VM-side (native code signaling failure via a bare Java exception object)
rather than one thrown by real interpreted/JIT-compiled bytecode. This is
the most actionable lead for whoever picks this up — find where CratonVM
constructs an `IllegalStateException` with the literal message `"root"` and
without a captured stack trace.

## Affected classes (30, all `FAIL`, this run)

```
aop.support.AopUtilsKotlinTests
aot.hint.BindingReflectionHintsRegistrarKotlinTests
aot.hint.JdkProxyHintExtensionsTests
aot.hint.ProxyHintsExtensionsTests
aot.hint.ResourceHintsExtensionsTests
aot.hint.TypeHintExtensionsTests
beans.BeanUtilsKotlinTests
beans.factory.BeanFactoryExtensionsTests
beans.factory.ListableBeanFactoryExtensionsTests
context.annotation.ConfigurationClassKotlinTests
core.KotlinReflectionParameterNameDiscovererTests
core.MethodParameterKotlinTests
core.convert.converter.ConverterFactoryNullnessTests
core.env.PropertyResolverExtensionsKotlinTests
core.io.support.SpringFactoriesLoaderKotlinTests
expression.spel.SpelReproKotlinTests
http.converter.cbor.KotlinSerializationCborHttpMessageConverterTests
http.converter.json.Jackson2ObjectMapperFactoryBeanTests
http.converter.protobuf.KotlinSerializationProtobufHttpMessageConverterTests
jdbc.core.JdbcOperationsExtensionsTests
messaging.converter.KotlinSerializationJsonMessageConverterTests
test.context.bean.override.mockito.MockitoBeanByTypeLookupForConstructorParametersIntegrationKotlinTests
test.web.servlet.MockMvcExtensionsTests
test.web.servlet.client.RestTestClientExtensionsTests
web.bind.annotation.ControllerMappingReflectiveProcessorKotlinTests
web.method.support.InvocableHandlerMethodKotlinTests
web.reactive.result.method.annotation.CoroutinesIntegrationTests
web.reactive.result.method.annotation.RequestParamMethodArgumentResolverKotlinTests
web.service.invoker.HttpServiceProxyFactoryExtensionsTests
web.servlet.function.ServerRequestExtensionsTests
```

(all under `org.springframework.`; every class name either ends in
`KotlinTests`/`ExtensionsTests` or is otherwise known to use Kotlin
reflection — a consistent, coherent set, not a random scatter.)

## Ruled out this session

- **Not a missing-jar/classpath gap** (the environmental issue this same
  suite run separately found and fixed for `spring-websocket`/`spring-oxm`/
  etc. — see the jar-fix note in the suite-runner session log). Confirmed
  `kotlin-reflect-2.3.20.jar` IS present and resolvable in every affected
  module's `cratonvm-testcp.txt`.

## Open question / suspicious lead not yet chased down

**Multiple kotlin-reflect versions are present in the Gradle cache used to
build this testcp**: `1.6.10`, `2.1.20`, and `2.3.20`, all under
`gradle-home/caches/modules-2/files-2.1/org.jetbrains.kotlin/kotlin-reflect/`.
This testcp was regenerated from scratch via a fresh Gradle dependency
resolution on a brand-new Linux checkout (not the original checkout that
produced the Jul22-24 baseline `fullsuite_lf.tsv`, where these 30 classes
were presumably passing or at least not failing this way — not directly
confirmed, since that baseline predates this investigation). It's
plausible Gradle resolved a *different* kotlin-reflect version into these
modules' actual runtime classpath than whatever produced the earlier
baseline, and this exception is a version-mismatch artifact rather than a
CratonVM code defect. **Not confirmed either way** — needs someone to
check which exact `kotlin-reflect` jar path appears in one of the affected
module's `cratonvm-testcp.txt` and compare Kotlin metadata-version
handling across versions before concluding this is VM-side.

## Related (same general area, likely NOT the same bug — different message, different stack-trace shape)

[`docs/internal/gaps/spring-bug-02-kotlin-reflect-builtins-null.md`](../internal/gaps/spring-bug-02-kotlin-reflect-builtins-null.md)
documents an OLDER (2026-06-15, dev `c5644da4`/`ce325da5`) kotlin-reflect
defect against 8 classes (`NullnessKotlinTests`, `MethodParameterKotlinTests`,
`GenericTypeResolverKotlinTests`, `BridgeMethodResolverKotlinTests`,
`DefaultParameterNameDiscovererKotlinTests`,
`KotlinReflectionParameterNameDiscovererTests`, `PropagationContextElementTests`,
`aot.hint.BindingReflectionHintsRegistrarKotlinTests`) — overlapping 3 of
this doc's 30 classes (`MethodParameterKotlinTests`,
`KotlinReflectionParameterNameDiscovererTests`,
`BindingReflectionHintsRegistrarKotlinTests`). That doc's primary symptom
(`@NotNull method getBuiltInClassByFqName must not return null`, always
with a full multi-frame stack trace through
`JavaToKotlinClassMapper.mapJavaToKotlin`) was fixed by `cf58ea48`; its
documented residual (`spring-bug-02b`, generic-type-argument-dropping at
nesting depth ≥2, `AssertionFailedError`, also always with a real stack
trace) is a still-open but DIFFERENT symptom. Neither of that doc's two
symptoms matches this one (`IllegalStateException: root`, zero-frame
trace) — **do not assume this is the same bug reappearing**; the message,
exception type's stack-trace shape, and affected-class set (30 vs 8, only
3 overlapping) are all different enough that this needs its own
root-causing, not a reopen of `spring-bug-02b`.

## Reproduce

```bash
cd apps/spring-suite-runner
CRATONVM_BIN=<path to cratonvm> ./run-suite.sh run --category all --jit on --jdk real \
  --only 'MethodParameterKotlinTests$' --batch 1
```

## Next steps for whoever continues

1. Find where CratonVM constructs an `IllegalStateException` with literal
   message `"root"` and no captured stack trace — likely native Rust code
   (`native-builtins/`) raising a Java exception object directly rather
   than via normal bytecode `athrow`, given the missing trace. Search for
   the literal string `"root"` near any `IllegalStateException`
   construction site.
2. Settle the kotlin-reflect-version question above — pin one version
   across the whole classpath and see if the symptom changes or
   disappears, before spending time on VM-side tracing.
3. Get a `KRUN_STACK=1` (or equivalent live-trace) capture on one minimal
   case (`ControllerMappingReflectiveProcessorKotlinTests`, single test
   method, smallest class in the affected set) to see what CratonVM code
   path is executing at the moment the exception is thrown, since the
   exception object itself carries no trace.
