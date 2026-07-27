# Kotlin-reflect calls threw `IllegalStateException: root` with an empty stack trace — 30 Spring Kotlin test classes

| | |
|---|---|
| **Status** | **FIXED 2026-07-26.** Not a kotlin-reflect defect at all — a JIT miscompile of `java.lang.String.length()` and related intrinsics. All 30 classes now pass (the last one needed a second, unrelated fix — see "Verification"). |
| **Category** | VM-CORRECTNESS (JIT codegen / String field layout) |
| **Discovered** | 2026-07-26, full-suite run, dev `346c74b71`, Azure host `20.83.144.174`, worktree `/data/data/wt-springsuite8-20260725`, `apps/spring-suite-runner`, real JDK 25, jit-real. |
| **Root-caused / fixed** | 2026-07-26, worktree `/data/data/wt-testngnpe-20260726`. Fix landed on `dev` as **BUG-STRING-CODER-COMPACT-20260726** (`jit/src/lib.rs` `StringFieldLayout`, `jit/src/x64.rs` `emit_load_string_*`; commits `13055f75c` + `234a45b98`). |
| **CratonVM** | was FAIL — `java.lang.IllegalStateException: root` |
| **HotSpot** | OK |
| **Re-verified** | 2026-07-26, worktree `/data/data/wt-kotlinreflectdoc-20260726` on the same Azure host, against dev tip `73ab60c70` (already carries both fixes). All 30 classes rerun individually with correct per-module classpaths — 30/30 `status=OK`, `BindingReflectionHintsRegistrarKotlinTests` 4/4. See "Verification". |

## Symptom (as originally filed)

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
```

The original filing noted the failures carried **no stack trace at all** and
suspected either a missing `fillInStackTrace()` or a VM-synthesised exception.
Both leads were wrong — see below.

### Affected classes (30, all `FAIL` in the original run, across 11 Spring Framework modules)

```
aop.support.AopUtilsKotlinTests                                                          (spring-aop)
beans.BeanUtilsKotlinTests                                                                (spring-beans)
beans.factory.BeanFactoryExtensionsTests                                                  (spring-beans)
beans.factory.ListableBeanFactoryExtensionsTests                                          (spring-beans)
context.annotation.ConfigurationClassKotlinTests                                          (spring-context)
aot.hint.BindingReflectionHintsRegistrarKotlinTests                                        (spring-core)
aot.hint.JdkProxyHintExtensionsTests                                                       (spring-core)
aot.hint.ProxyHintsExtensionsTests                                                         (spring-core)
aot.hint.ResourceHintsExtensionsTests                                                      (spring-core)
aot.hint.TypeHintExtensionsTests                                                           (spring-core)
core.KotlinReflectionParameterNameDiscovererTests                                          (spring-core)
core.MethodParameterKotlinTests                                                            (spring-core)
core.convert.converter.ConverterFactoryNullnessTests                                       (spring-core)
core.env.PropertyResolverExtensionsKotlinTests                                             (spring-core)
core.io.support.SpringFactoriesLoaderKotlinTests                                           (spring-core)
expression.spel.SpelReproKotlinTests                                                       (spring-expression)
jdbc.core.JdbcOperationsExtensionsTests                                                    (spring-jdbc)
messaging.converter.KotlinSerializationJsonMessageConverterTests                           (spring-messaging)
test.context.bean.override.mockito.MockitoBeanByTypeLookupForConstructorParametersIntegrationKotlinTests (spring-test)
test.web.servlet.MockMvcExtensionsTests                                                    (spring-test)
test.web.servlet.client.RestTestClientExtensionsTests                                      (spring-test)
http.converter.cbor.KotlinSerializationCborHttpMessageConverterTests                        (spring-web)
http.converter.json.Jackson2ObjectMapperFactoryBeanTests                                   (spring-web)
http.converter.protobuf.KotlinSerializationProtobufHttpMessageConverterTests               (spring-web)
web.bind.annotation.ControllerMappingReflectiveProcessorKotlinTests                        (spring-web)
web.method.support.InvocableHandlerMethodKotlinTests                                       (spring-web)
web.service.invoker.HttpServiceProxyFactoryExtensionsTests                                 (spring-web)
web.reactive.result.method.annotation.CoroutinesIntegrationTests                           (spring-webflux)
web.reactive.result.method.annotation.RequestParamMethodArgumentResolverKotlinTests        (spring-webflux)
web.servlet.function.ServerRequestExtensionsTests                                          (spring-webmvc)
```

(all under `org.springframework.`; every class name either ends in
`KotlinTests`/`ExtensionsTests` or is otherwise known to use Kotlin
reflection — a consistent, coherent set, not a random scatter. Note some
class *simple names* recur across modules with different packages, e.g.
`InvocableHandlerMethodKotlinTests` and `RequestParamMethodArgumentResolverKotlinTests`
each exist once under `spring-web` and once under `spring-webflux` — these are
different classes; the doc's 30-class list disambiguates by full package.)

## Root cause

`kotlin.reflect.jvm.internal.impl.name.FqNameUnsafe.isRoot()` compiles to

```
getfield        fqName : Ljava/lang/String;
checkcast       java/lang/CharSequence
invokeinterface java/lang/CharSequence.length:()I
ifne  -> false
```

i.e. "this fully-qualified name is the root package iff its backing string is
empty". CratonVM's JIT inlines that `length()` as the `StringLength` intrinsic,
which evaluates `value.length >> coder`.

`StringFieldLayout` carried ONE byte offset per String field and let `x64.rs`
derive both physical addresses from it — `cell + payload` for a
`GC_FLAG_COMPACT` object, `cell + payload + 8` for a legacy 16-byte-cell one.
That derivation is only valid when a field's compact body offset equals
`field_index * SLOT_SIZE`. It does for `value` (index 0, body offset 0). It
does **not** for `coder`: real JDK 25 `String` packs `value@0, coder@8,
hash@12, hashIsZero@16`, so the compact `coder` read landed on **`hash`**.

`length()` therefore computed `value.length >> (hash & 31)`. `hash` is lazily
cached and zero until something calls `hashCode()`, so the shift was 0 and the
answer correct — right up until the string was used as a map key or interned
into a hash-based structure. After that, `length()` returned
`value.length >> (hash & 31)`, which is 0 for most hashes.

kotlin-reflect puts every `FqName` string into hash-based structures during
module-descriptor construction, so by the time `isRoot()` ran, a non-empty
package name reported `length() == 0` and therefore `isRoot() == true`. The
throw site is `FqNameUnsafe.parent()`'s `check(!isRoot) { "root" }`, reached
from `FqNamesUtilKt.parentOrNull` → `isChildOf` → `findValueForMostSpecificFqname`
while building `JavaTypeEnhancementState` — which is why every affected class
died in the same place with the same one-word message.

`hashCode()` had the mirror defect (it read `hashIsZero` as the cached hash),
and `hash`'s own LEGACY address was wrong too. `equals`, `compareTo`, `charAt`,
`isEmpty` and `indexOf` all decode through `coder` and were wrong the same way.

### Both of the doc's original leads were wrong

* **"Multiple kotlin-reflect versions in the Gradle cache"** — irrelevant. The
  same jar passes under `--nojit`.
* **"Synthetic exception constructed VM-side, no `fillInStackTrace`"** — no.
  The exception is thrown by ordinary kotlin-reflect bytecode and has a full
  ~40-frame stack trace; the original run simply did not set `KRUN_STACK=1`
  (`KRun.java` only prints a trace for a `FAILCAUSE` when that env var is set).
  Re-running with `KRUN_STACK=1` produced the complete trace immediately and is
  what made this a one-hour investigation instead of an open question.

## How it was isolated

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
```

Minimal standalone reproducer (no Spring, no Kotlin): take any String, call
`hashCode()` on it, then call `length()` from JIT-compiled code — it returns
`value.length >> (hash & 31)`.

## Verification

All 30 classes rerun on the fix, real JDK 25, jit-real, `KRun`:

* **30 OK.** 29 of them from the String-intrinsic fix alone.

The 30th, `aot.hint.BindingReflectionHintsRegistrarKotlinTests`, needed a
second and unrelated fix. CratonVM's native `Assertions.assertThat` shim
(`native_assertj_lightweight_comparable_assert`) builds `AbstractAssert` by
hand rather than running its constructor, and left the `WritableAssertionInfo`
it allocates with a null `representation` — a state the real constructor makes
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
should have passed. Fixed (commit `fdc852f55`); that class is now 4/4.

Standalone repro on any classpath carrying assertj-core:
`Assertions.assertThat("actualValue").isEqualTo("expectedValue")` threw
`NullPointerException` on CratonVM where HotSpot throws the expected
`AssertionError`. Worth remembering that this silently degraded the failure
message of every AssertJ assertion in every suite.

**Independent re-verification (2026-07-26, closing this doc):** this file was
corrupted by a bad manual merge of two concurrently-written revisions (one
that had already root-caused and fixed the bug, one that re-filed the same
symptom as freshly-discovered and OPEN because that session hadn't picked up
the fix yet). Before reconciling the doc, re-built dev tip `73ab60c70` (which
carries both `234a45b98`/`13055f75c` and `fdc852f55`) in a fresh worktree and
re-ran all 30 classes individually with correct per-module classpaths (the
30-class list spans 11 different Spring Framework modules, each with its own
`build/cratonvm-testcp.txt` — a single module's classpath only covers the
subset of the 30 that live in that module). Result: 30/30 `status=OK`,
including `BindingReflectionHintsRegistrarKotlinTests` at 4/4 with no AssertJ
NPE. Confirms the fix is real and complete, not just claimed by commit
messages.

## Related

[`docs/internal/gaps/spring-bug-02-kotlin-reflect-builtins-null.md`](../gaps/spring-bug-02-kotlin-reflect-builtins-null.md)
documents an older (2026-06-15) kotlin-reflect defect over 8 classes, 3 of
which overlap this doc's 30. That doc's primary symptom
(`@NotNull method getBuiltInClassByFqName must not return null`) was fixed by
`cf58ea48`; its `spring-bug-02b` residual (generic-type-argument dropping at
nesting depth ≥2) is a genuinely different symptom and is **not** addressed
here — it remains a separate open item tracked in that doc, not this one. The
suspicion recorded in the original version of this file — that the two might
be the same defect resurfacing — is now settled: they are unrelated, and
neither is a kotlin-reflect version-resolution problem.

The same JIT defect (found independently, from an H2 investigation) is
documented in
[`h2-suite-bugs/h2-jitban-schema-not-found-on-reconnect-FIXED.md`](h2-suite-bugs/h2-jitban-schema-not-found-on-reconnect-FIXED.md).
