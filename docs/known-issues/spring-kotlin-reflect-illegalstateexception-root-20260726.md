# Kotlin-reflect calls throw `IllegalStateException: root` with an empty stack trace — 30 Spring Kotlin test classes, likely a THIRD distinct kotlin-reflect defect

# Kotlin-reflect calls throw `IllegalStateException: root` with an empty stack trace — 30 Spring Kotlin test classes

| | |
|---|---|
| **Status** | **FIXED 2026-07-26.** Not a kotlin-reflect defect at all — a JIT miscompile of `java.lang.String.length()`. All 30 classes now pass (the last one needed a second, unrelated fix -- see "Verification"). |
| **Category** | VM-CORRECTNESS (JIT codegen / String field layout) |
| **Status** | OPEN — newly surfaced, not root-caused. Needs a minimal standalone repro before further diagnosis. |
| **Category** | VM-CORRECTNESS (reflection / Kotlin interop) |
| **Discovered** | 2026-07-26, full-suite run, dev `346c74b71`, Azure host `20.83.144.174`, worktree `/data/data/wt-springsuite8-20260725`, `apps/spring-suite-runner`, real JDK 25, jit-real. |
| **Root-caused / fixed** | 2026-07-26, worktree `/data/data/wt-testngnpe-20260726`. Fix landed on `dev` as **BUG-STRING-CODER-COMPACT-20260726** (`jit/src/lib.rs` `StringFieldLayout`, `jit/src/x64.rs` `emit_load_string_*`). |
| **CratonVM** | was FAIL — `java.lang.IllegalStateException: root` |
| **HotSpot** | OK |
| **CratonVM** | FAIL — `java.lang.IllegalStateException: root`, **zero-frame stack trace** (see below) |
| **HotSpot** | not independently re-verified this session, but every affected class is HotSpot-clean in the Jul22-24 baseline (`fullsuite_lf.tsv`) used for regression comparison |

## Symptom (as originally filed)
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
## Root cause

## Affected classes (30, all `FAIL`, this run)
`kotlin.reflect.jvm.internal.impl.name.FqNameUnsafe.isRoot()` compiles to

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
getfield        fqName : Ljava/lang/String;
checkcast       java/lang/CharSequence
invokeinterface java/lang/CharSequence.length:()I
ifne  -> false
```

i.e. "this fully-qualified name is the root package iff its backing string is
empty". CratonVM's JIT inlines that `length()` as the `StringLength` intrinsic,
which evaluates `value.length >> coder`.
(all under `org.springframework.`; every class name either ends in
`KotlinTests`/`ExtensionsTests` or is otherwise known to use Kotlin
reflection — a consistent, coherent set, not a random scatter.)

`StringFieldLayout` carried ONE byte offset per String field and let `x64.rs`
derive both physical addresses from it — `cell + payload` for a
`GC_FLAG_COMPACT` object, `cell + payload + 8` for a legacy 16-byte-cell one.
That derivation is only valid when a field's compact body offset equals
`field_index * SLOT_SIZE`. It does for `value` (index 0, body offset 0). It
does **not** for `coder`: real JDK 25 `String` packs `value@0, coder@8,
hash@12, hashIsZero@16`, so the compact `coder` read landed on **`hash`**.
## Ruled out this session

`length()` therefore computed `value.length >> (hash & 31)`. `hash` is lazily
cached and zero until something calls `hashCode()`, so the shift was 0 and the
answer correct — right up until the string was used as a map key or interned
into a hash-based structure. After that, `length()` returned
`value.length >> (hash & 31)`, which is 0 for most hashes.
- **Not a missing-jar/classpath gap** (the environmental issue this same
  suite run separately found and fixed for `spring-websocket`/`spring-oxm`/
  etc. — see the jar-fix note in the suite-runner session log). Confirmed
  `kotlin-reflect-2.3.20.jar` IS present and resolvable in every affected
  module's `cratonvm-testcp.txt`.

kotlin-reflect puts every `FqName` string into hash-based structures during
module-descriptor construction, so by the time `isRoot()` ran, a non-empty
package name reported `length() == 0` and therefore `isRoot() == true`. The
throw site is `FqNameUnsafe.parent()`'s `check(!isRoot) { "root" }`, reached
from `FqNamesUtilKt.parentOrNull` → `isChildOf` → `findValueForMostSpecificFqname`
while building `JavaTypeEnhancementState` — which is why every affected class
died in the same place with the same one-word message.
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
`hashCode()` had the mirror defect (it read `hashIsZero` as the cached hash),
and `hash`'s own LEGACY address was wrong too. `equals`, `compareTo`, `charAt`,
`isEmpty` and `indexOf` all decode through `coder` and were wrong the same way.

### Both of the doc's original leads were wrong
## Related (same general area, likely NOT the same bug — different message, different stack-trace shape)

* **"Multiple kotlin-reflect versions in the Gradle cache"** — irrelevant. The
  same jar passes under `--nojit`.
* **"Synthetic exception constructed VM-side, no `fillInStackTrace`"** — no.
  The exception is thrown by ordinary kotlin-reflect bytecode and has a full
  ~40-frame stack trace; the original run simply did not set `KRUN_STACK=1`
  (`KRun.java` only prints a trace for a `FAILCAUSE` when that env var is set).
  Re-running with `KRUN_STACK=1` produced the complete trace immediately and is
  what made this a one-hour investigation instead of an open question.
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

## How it was isolated
## Reproduce

```bash
cd apps/spring-suite-runner
CP="$PWD:$(tr -d '\r' < ../spring-framework/spring-core/build/cratonvm-testcp.txt)"
printf -- '-cp\n%s\n' "$CP" > /tmp/af.txt

# 1. JIT or not?
cratonvm --nojit --java-home $JDK25 @/tmp/af.txt KRun \
    org.springframework.core.MethodParameterKotlinTests          # 11/11 OK
# 2. which class?
CRATONVM_JIT_DENY=name/FqNameUnsafe   ... # OK   -> FqNameUnsafe
# 3. which method?
CRATONVM_JIT_DENY=FqNameUnsafe.isRoot ... # OK   -> isRoot()
# 4. which intrinsic? (CRATONVM_DISABLE_INTRINSICS does NOT cover these --
#    it is an interpreter-side kill switch only)
CRATONVM_JIT_NO_STRING_INTRINSICS=length ... # OK -> StringLength
CRATONVM_BIN=<path to cratonvm> ./run-suite.sh run --category all --jit on --jdk real \
  --only 'MethodParameterKotlinTests$' --batch 1
```

## Next steps for whoever continues
Minimal standalone reproducer (no Spring, no Kotlin): take any String, call
`hashCode()` on it, then call `length()` from JIT-compiled code — it returns
`value.length >> (hash & 31)`.

## Verification

All 30 classes rerun on the fix, real JDK 25, jit-real, `KRun`:

* **30 OK.** 29 of them from the String-intrinsic fix alone.

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
The 30th, `aot.hint.BindingReflectionHintsRegistrarKotlinTests`, needed a
second and unrelated fix. CratonVM's native `Assertions.assertThat` shim
(`native_assertj_lightweight_comparable_assert`) builds `AbstractAssert` by
hand rather than running its constructor, and left the `WritableAssertionInfo`
it allocates with a null `representation` -- a state the real constructor makes
impossible (`useRepresentation` starts with `Objects.requireNonNull`). Every
FAILING AssertJ assertion therefore died with

```
NullPointerException: Cannot invoke
"org.assertj.core.presentation.Representation.toStringOf(Object)"
because "this.representation" is null
```

instead of reporting the assertion. It also changed outcomes rather than just
messages: `satisfiesExactlyInAnyOrder` decides which elements matched by
catching `AssertionError` inside `Iterables.byPassingAssertions`, and an NPE is
not an `AssertionError`, so it escaped that filter and failed a test that
should have passed. Fixed; that class is now 4/4.

Standalone repro on any classpath carrying assertj-core:
`Assertions.assertThat("actualValue").isEqualTo("expectedValue")` threw
`NullPointerException` on CratonVM where HotSpot throws the expected
`AssertionError`. Worth remembering that this silently degraded the failure
message of every AssertJ assertion in every suite.

## Related

[`docs/internal/gaps/spring-bug-02-kotlin-reflect-builtins-null.md`](../internal/gaps/spring-bug-02-kotlin-reflect-builtins-null.md)
documents an older (2026-06-15) kotlin-reflect defect over 8 classes, 3 of
which overlap this doc's 30. That doc's primary symptom
(`@NotNull method getBuiltInClassByFqName must not return null`) was fixed by
`cf58ea48`; its `spring-bug-02b` residual (generic-type-argument dropping at
nesting depth ≥2) is a genuinely different symptom and is **not** addressed
here. The suspicion recorded in the original version of this file — that the
two might be the same defect resurfacing — is now settled: they are unrelated,
and neither is a kotlin-reflect version-resolution problem.
