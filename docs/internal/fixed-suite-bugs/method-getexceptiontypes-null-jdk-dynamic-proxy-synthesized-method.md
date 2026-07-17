# `Method.getExceptionTypes()` returns `null` (not an empty array) for the synthetic `Method` object CratonVM builds for JDK dynamic-proxy `InvocationHandler.invoke()` calls — 3 classes (4 test methods)

**Status: FIXED (2026-07-17).** Both live JDK-proxy dispatch paths now set
`Method.exceptionTypes` to a non-null, correctly typed `Class[]`, populated
from the declaring interface method's JVMS `Exceptions` attribute. This closes
the Spring Data NPE and preserves declared checked exceptions for every
`InvocationHandler.invoke()` callback.

## Resolution and verification

`proxy_invoke_handler` and its shared-interpreter counterpart
`proxy_invoke_handler_shared` both synthesize the `Method` passed to an
`InvocationHandler`. Both now use a common helper that allocates a `Class[]`
even for no-throws methods and fills it from the exact declaring interface's
`Exceptions` attribute. The legacy synthetic-layout fallback writes its
`exceptionTypes` slot as well.

Verified on the Linux build host with a uniquely named release binary
`probes/cratonvm-proxy-getexceptiontypes-20260717` using the committed
`vm/tests/resources/cratonvm/ProxyMethodExceptionTypes.java` probe:

- normal execution: `OK proxy-method-exception-types`;
- interpreter-only execution (`CRATONVM_DISABLE_JIT=1`): the same `OK` result;
- the probe checks both a zero-length, non-null `Class[]` and an accurately
  preserved `IOException` throws declaration.

Found while re-running the Spring Boot 4.1.0-SNAPSHOT suite in worktree
`CratonVM-spring-boot-crashfail-20260714` (`rerun-20260716`, shard3). All 3
occurrences are `spring-data-*` autoconfiguration integration tests whose
repository call goes through a JDK dynamic proxy
(`org.springframework.aop.framework.JdkDynamicAopProxy`):

- `module/spring-boot-data-commons`:
  `DataRepositoryMetricsAutoConfigurationIntegrationTests.repositoryMethodCallRecordsMetrics()`
- `module/spring-boot-data-jdbc-test`:
  `DataJdbcTestIntegrationTests.testRepository()`
- `module/spring-boot-data-jdbc`:
  `DataJdbcRepositoriesAutoConfigurationTests.basicAutoConfiguration()` and
  `.honoursUsersEnableJdbcRepositoriesConfiguration()`

(4 failing test methods total across 3 classes.)

## Symptom

Identical `NullPointerException` shape in all 3 logs, e.g.
`apps/spring-boot-suite-runner/.suite/results/rerun-20260716/shard3/logs/module_spring-boot-data-commons.org.springframework.boot.data.autoconfigure.metrics.DataRepositor-3d579b8670ec.out.log`:

```
Failures (1):
  JUnit Jupiter:DataRepositoryMetricsAutoConfigurationIntegrationTests:repositoryMethodCallRecordsMetrics()
    MethodSource [className = 'org.springframework.boot.data.autoconfigure.metrics.DataRepositoryMetricsAutoConfigurationIntegrationTests', methodName = 'repositoryMethodCallRecordsMetrics', methodParameterTypes = '']
    => java.lang.NullPointerException: Cannot read the array length because "<local3>" is null
       org.springframework.util.ReflectionUtils.declaresException(ReflectionUtils.java:301)
       org.springframework.dao.support.PersistenceExceptionTranslationInterceptor.invoke(PersistenceExceptionTranslationInterceptor.java:139)
       org.springframework.aop.framework.ReflectiveMethodInvocation.proceed(ReflectiveMethodInvocation.java:179)
       org.springframework.data.util.NullnessMethodInvocationValidator.invoke(NullnessMethodInvocationValidator.java:96)
       org.springframework.aop.framework.ReflectiveMethodInvocation.proceed(ReflectiveMethodInvocation.java:179)
       org.springframework.aop.framework.JdkDynamicAopProxy.invoke(JdkDynamicAopProxy.java:222)
       org.springframework.boot.data.autoconfigure.metrics.DataRepositoryMetricsAutoConfigurationIntegrationTests.lambda$repositoryMethodCallRecordsMetrics$0(DataRepositoryMetricsAutoConfigurationIntegrationTests.java:67)
       ...
```

`module/spring-boot-data-jdbc-test`'s `DataJdbcTestIntegrationTests` shows the
exact same first 6 frames (`declaresException` →
`PersistenceExceptionTranslationInterceptor.invoke` → `proceed` →
`NullnessMethodInvocationValidator.invoke` → `proceed` →
`JdkDynamicAopProxy.invoke`), just with a plain (non-lambda) call site above
it. `module/spring-boot-data-jdbc`'s `DataJdbcRepositoriesAutoConfigurationTests`
shows it twice (once per failing test method), again with the identical
6-frame AOP-proxy core. In every case the outermost frame is
`JdkDynamicAopProxy.invoke(JdkDynamicAopProxy.java:222)` — i.e. the `Method`
object involved is always the one JDK-proxy dispatch hands to
`InvocationHandler.invoke(Object proxy, Method method, Object[] args)`, never
a `Method` obtained directly via `Class.getMethod`/`getDeclaredMethod` on a
concrete class.

`ReflectionUtils.declaresException(Method method, Class<?> exceptionType)`
(real Spring Framework code) is:

```java
public static boolean declaresException(Method method, Class<?> exceptionType) {
    Assert.notNull(method, "Method must not be null");
    Class<?>[] declaredExceptions = method.getExceptionTypes();
    for (Class<?> declaredException : declaredExceptions) {
        if (declaredException.isAssignableFrom(exceptionType)) {
            return true;
        }
    }
    return false;
}
```

`"<local3>" is null` is the decompiled name of `declaredExceptions` — so
`method.getExceptionTypes()` returned `null`. Per the JDK spec,
`Method.getExceptionTypes()` **never** returns `null`; a method with no
`throws` clause returns a zero-length `Class[]`. A `null` return here is a
CratonVM-side contract violation, not a Spring bug — `declaresException` is
heavily-exercised, unmodified upstream Spring code.

## Root cause

CratonVM has two independent code paths that build a `java.lang.reflect.Method`
object, and only one of them populates the `exceptionTypes` field.

**Path 1 (correct) — `create_method_object`,
`native-builtins/src/lang_class.rs:5220-5322`.** For a real declared method
on a concrete/interface class (the normal `Class.getMethod`/
`getDeclaredMethod`/`getMethods` path), this function always allocates a
non-null `exceptionTypes` array (line 5321:
`ctx.set_field_by_name(obj, "exceptionTypes", Value::Object(Some(exception_arr)));`),
populated from the class file's JVMS §4.7.5 `Exceptions` attribute via
`ctx.method_exceptions(...)` (line 5287-5288), falling back to an empty
(never null) array when the method declares no checked exceptions
(`build_mirror_array_comp` over `exception_names.len()`, which is `0` for a
no-throws method — line 5293-5298). The extensive comment at lines
5273-5286 ("G2 ... Always allocate non-null array fields ... If these are
left as the default `null`, any caller doing `arr.length` ... will NPE")
shows this exact class of bug was already fixed for this path.

**Path 2 (buggy) — `proxy_invoke_handler`,
`vm/src/vm/vm_exec.rs:10238` (the function that "intercepts [a call] on a
`Proxy$Instance` object ... and forwards to `InvocationHandler.invoke()`",
per its doc comment at lines 10227-10237).** When any interface method is
called on a JDK dynamic proxy, this function synthesizes a **fresh**
`java.lang.reflect.Method` object (`alloc_object` at line 10296-10299) to
pass as the `method` argument of `InvocationHandler.invoke(Object, Method,
Object[])`. It explicitly sets, by field name (`proxy_method_set_field_by_name`,
lines 10331-10362): `clazz`, `name`, `returnType`, `parameterTypes`,
`modifiers`, `signature`, `slot` — but **never `exceptionTypes`** (nor
`annotations`/`parameterAnnotations`/`annotationDefault`, which
`create_method_object` also explicitly sets to `null` on purpose, matching
HotSpot — but `exceptionTypes` must be non-null per spec, unlike those). Since
`alloc_object` zero-initializes reference fields to `null`, the synthesized
proxy `Method`'s `exceptionTypes` field is left `Value::Object(None)`.

`native_method_get_exception_types`
(`native-builtins/src/lang_reflect.rs:806-817`) — the registered native for
`Method.getExceptionTypes()` (registered at
`native-builtins/src/lang_reflect.rs:1471-1476`) — calls
`method_exception_types_value` (`native-builtins/src/lang_class.rs:5728-5738`),
which does a bare `method_object_field_value_or_legacy(ctx, method_obj,
"exceptionTypes", METHOD_LEGACY_SLOT_EXCEPTION_TYPES)` field read with **no
null-check or empty-array fallback** — the comment right above the native at
lines 810-812 explicitly assumes "The field is always non-null (set in
`create_method_object`)", which is true for Path 1 but false for Path 2. The
field read therefore faithfully returns the `null` that `proxy_invoke_handler`
left behind, and Spring's `declaresException` NPEs on `.length`.

**Why all 3 affected classes share the exact same 6-frame core:** Spring
Data repositories are implemented as JDK dynamic proxies
(`JdkDynamicAopProxy`) wrapping the repository interface. Every repository
method call is dispatched through `proxy_invoke_handler`, which builds the
`Method` object handed to `JdkDynamicAopProxy.invoke`. That `Method` then
flows unchanged through `ReflectiveMethodInvocation.proceed()` into every
advisor in the chain, including
`PersistenceExceptionTranslationInterceptor`, whose very first `invoke()`
line (139) checks `ReflectionUtils.declaresException(invocation.getMethod(),
...)` — so *any* Spring Data repository call through this advisor hits this
gap, independent of the specific repository method being called.

## Repro

Full crashfail rerun already reproduced this; to isolate one class, build a
1-row (or 4-row) TSV under `.suite\` with `module<TAB>class` and pass it via
`-ClassList`:

```powershell
"module`tclass" | Out-File -Encoding utf8 .suite\repro-getexceptiontypes.tsv
"module/spring-boot-data-commons`torg.springframework.boot.data.autoconfigure.metrics.DataRepositoryMetricsAutoConfigurationIntegrationTests" |
  Out-File -Encoding utf8 -Append .suite\repro-getexceptiontypes.tsv

powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 `
  -ClassList apps\spring-boot-suite-runner\.suite\repro-getexceptiontypes.tsv `
  -Start 1 -Count 1 -RunName repro-getexceptiontypes-20260716 `
  -Exe C:\craton\CratonVM-spring-boot-crashfail-20260714\target\release\cratonvm-spring-boot-rerun-20260716.exe
```

The other two affected classes:
`module/spring-boot-data-jdbc-test\torg.springframework.boot.data.jdbc.test.autoconfigure.DataJdbcTestIntegrationTests`
and
`module/spring-boot-data-jdbc\torg.springframework.boot.data.jdbc.autoconfigure.DataJdbcRepositoriesAutoConfigurationTests`.

A minimal non-Spring standalone repro (once a JDK/toolchain is at hand)
would be simpler and faster to iterate on than the Spring Boot suite:

```java
interface Greeter { String greet(); }
Greeter g = (Greeter) java.lang.reflect.Proxy.newProxyInstance(
    Greeter.class.getClassLoader(), new Class<?>[]{Greeter.class},
    (proxy, method, args) -> {
        // On CratonVM: method.getExceptionTypes() == null (should be Class[0]).
        System.out.println(method.getExceptionTypes());
        return "hi";
    });
g.greet();
```

## Suggested fix

In `vm/src/vm/vm_exec.rs::proxy_invoke_handler`, after the existing
`proxy_method_set_field_by_name(ctx.shared, method_obj, "slot", ...)` call
(around line 10362), allocate and set a (possibly zero-length) `Class[]`
`exceptionTypes` field the same way `create_method_object` does — the
interface method being proxied is a real declared method (on the proxied
interface), so its actual `Exceptions` attribute is available via the same
`ctx.method_exceptions(...)`-style lookup `create_method_object` uses, rather
than always synthesizing an empty array; either is a strict improvement over
the current `null`. `Constructor`'s synthesized-proxy analogue (if any) should
be audited too, though proxies are only ever constructed via `Proxy`'s own
generated constructor, not user-visible `Constructor` reflection, so this may
not apply there.

## Related

- The comment block at `native-builtins/src/lang_class.rs:5273-5286` ("G2 fix")
  documents the *exact same class* of bug — `Method`/`Constructor` array
  fields left `null` instead of empty — already fixed once for
  `create_method_object` and `create_constructor_object`
  (`native-builtins/src/lang_reflect.rs:819-826`, the CGLib
  `Constructor.getExceptionTypes` fix). This finding is a second, parallel
  occurrence of the same underlying lesson in the proxy-dispatch code path,
  which the original G2 fix did not cover because it lives in a different
  crate/file (`vm/src/vm/vm_exec.rs` vs `native-builtins/src/lang_class.rs`).
- `docs/internal/fixed-suite-bugs/proxy-real-classfile.md` and
  `docs/internal/fixed-suite-bugs/mergedannotationstests-proxy-class-identity-reflection-vs-synthesize.md`
  are other previously-fixed bugs in the same JDK-dynamic-proxy
  synthesized-`Method`/synthesized-class-identity area — worth checking
  before starting a fix, in case they touched this same `proxy_invoke_handler`
  function and left useful context.
- No existing `docs/known-issues` or `docs/internal` doc mentions
  `getExceptionTypes`, `declaresException`, or `proxy_invoke_handler`
  specifically — this is a new, previously-unfiled gap.
