# `SpringBootContextLoaderAotTests` — Groovy `GroovySystem.<clinit>` bootstrap NPE (`CachedClass.getFields()` returns null)

**Status: OPEN — found 2026-07-23 (craton-rerun-20260723). New symptom, not the one covered by the now-fully-fixed parent doc.**

## Symptom

`core/spring-boot-test`, `SpringBootContextLoaderAotTests.loadContextForAotProcessingAndAotRuntime()`:

```
Caused by: java.lang.ExceptionInInitializerError
    at org.springframework.beans.factory.groovy.GroovyBeanDefinitionReader.<init>(GroovyBeanDefinitionReader.java:152)
    at org.springframework.boot.BeanDefinitionLoader.<init>(BeanDefinitionLoader.java:90)
    at org.springframework.boot.SpringApplication.createBeanDefinitionLoader(SpringApplication.java:748)
    ...
Caused by: java.lang.NullPointerException: Cannot read the array length because "<local2>" is null
    at groovy.lang.MetaClassImpl.addFields(MetaClassImpl.java:2504)
    at groovy.lang.MetaClassImpl.inheritFields(MetaClassImpl.java:2492)
    at groovy.lang.MetaClassImpl.setUpProperties(MetaClassImpl.java:2374)
    at groovy.lang.MetaClassImpl.addProperties(MetaClassImpl.java:3375)
    at groovy.lang.MetaClassImpl.reinitialize(MetaClassImpl.java:3349)
    at groovy.lang.MetaClassImpl.initialize(MetaClassImpl.java:3342)
    at org.codehaus.groovy.runtime.metaclass.MetaClassRegistryImpl.<init>(MetaClassRegistryImpl.java:143)
    at org.codehaus.groovy.runtime.metaclass.MetaClassRegistryImpl.<init>(MetaClassRegistryImpl.java:95)
    at groovy.lang.GroovySystem.<clinit>(GroovySystem.java:37)
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard4/logs/core_spring-boot-test.org.springframework.boot.test.context.SpringBootContextLoaderAotTests.out.log`
(`SBRUNNER_RESULT tests=1 failed=1`).

This is a **different** failure from the one investigated (and, as of
2026-07-21, "FULLY FIXED... `SpringBootContextLoaderAotTests` PASSES
end-to-end") in
[`core-spring-boot-test-config-data-and-classpath-scan-cluster-FIXED.md`](../../internal/fixed-suite-bugs/springboot/core-spring-boot-test-config-data-and-classpath-scan-cluster-FIXED.md).
That doc's Cluster C investigated (and fixed) `invokeinterface` dispatch,
annotation-`Enum` loader-blind resolution, and a JIT `checkcast`
name-vs-id mismatch under `@CompileWithForkedClassLoader` — none of which
match this NPE's shape or call site. This is a regression on top of that
fix, not a recurrence of it: `GroovySystem.<clinit>` (the JVM-wide,
one-time-only static initializer for Groovy's whole metaclass system) is
failing during its *very first* invocation in the process, before any
`@CompileWithForkedClassLoader`-specific machinery would even be relevant —
`GroovyBeanDefinitionReader` is real, unmodified Spring bytecode reached via
`SpringApplication.createBeanDefinitionLoader()`, nothing isolated-loader-specific
about the call site itself.

## Root cause — narrowed to a specific call via bytecode decompilation, not fully pinned to a CratonVM file:line

Decompiled `groovy-5.0.6.jar` (`org/apache/groovy:groovy:5.0.6`, resolved from
this worktree's Gradle cache) with `javap -p -c -l` to get past-the-name
line/bytecode detail `MetaClassImpl.java`'s public source doesn't show
(class was built with `-g` debug info: full local-variable tables).

`MetaClassImpl.addFields(CachedClass, Map)` (line 2504):

```java
private static void addFields(CachedClass klass, Map<String, MetaProperty> index) {
    CachedField[] fields = klass.getFields();   // line 2504 -- NPE here, fields == null
    for (CachedField field : fields) {          // line 2504 (arraylength on null)
        index.put(field.getName(), field);      // line 2505
    }
}
```

`CachedClass.getFields()` is a memoized (`LazyReference<CachedField[]>`)
accessor. Its `initValue()` (decompiled from the `CachedClass$1` anonymous
inner class, `org/codehaus/groovy/reflection/CachedClass.java:72-79`):

```java
public CachedField[] initValue() {
    return (CachedField[]) CachedClass.doPrivileged(() ->
        (CachedField[]) Arrays.stream(this$0.getTheClass().getDeclaredFields())
            .filter(CachedClass::isAccessibleOrCanSetAccessible)
            .map(...)                              // Field -> new CachedField(...)
            .toArray(CachedField[]::new));
}
```

i.e. `klass.getFields()` should be structurally incapable of returning
`null` from ordinary Java semantics — worst case (a class with zero
declared fields, or every field filtered out by
`isAccessibleOrCanSetAccessible`) is a **zero-length** array, never `null`.
For `getFields()` (and therefore its backing `LazyReference.get()`) to
actually hand back `null`, one of these calls inside the
`AccessController.doPrivileged(PrivilegedAction)` lambda must be behaving
incorrectly on CratonVM:

1. `Class.getDeclaredFields()` itself — checked directly:
   `native-builtins/src/lang_class.rs::native_class_get_declared_fields`
   (~line 5490-5535) always constructs and returns a real (possibly
   zero-length) mirror array via `build_mirror_array_comp`; there is no
   path in that function that returns Java `null`. This rules out the
   simplest explanation.
2. `AccessController.doPrivileged(PrivilegedAction)` swallowing/discarding
   the lambda's return value instead of propagating it — not checked this
   session (native registrations for `doPrivileged` live in
   `native-builtins/src/lang_system.rs`, `security_manager.rs`,
   `security_manager/policy.rs`, `deprecated_verify.rs`; which one is live
   for this call signature under the default real-JDK build was not
   determined).
3. The `Stream.filter(...).map(...).toArray(CachedField[]::new)` pipeline
   itself — specifically `Stream.toArray(IntFunction)` combined with an
   array-constructor method reference (`CachedField[]::new`) synthesized via
   `invokedynamic`/`LambdaMetafactory` — mis-dispatching or short-circuiting
   to a null result instead of the generated array. This class of
   lambda/`invokedynamic` + array-constructor-reference interaction has been
   a recurring source of CratonVM gaps elsewhere in this suite (see
   `springsuite-0620-toarray-referencepipeline-recursion.md` in
   `docs/internal/fixed-suite-bugs/` for a *different* but related
   `Stream.toArray` bug, not confirmed to share this mechanism).

**Not confirmed**: which of these three is the actual culprit. No debugger
attach or temporary trace was added this session — narrowing further
requires either instrumenting `doPrivileged`'s native registrations with a
one-off trace gated on the lambda class name, or writing a minimal
standalone repro (`AccessController.doPrivileged(() -> Arrays.stream(X.class.getDeclaredFields()).filter(...).map(...).toArray(Y[]::new))`)
outside Groovy/Spring entirely to bisect which layer drops the result. Given
`GroovySystem.<clinit>` only runs once per process and this is the *first*
class it processes, it's also not established which concrete `Class` object
(`this$0.getTheClass()`) is being processed when the failure occurs — likely
one of Groovy's own early-bootstrap classes (`GroovyObjectSupport`,
`groovy.lang.MetaClassRegistryImpl`, or similar), not user code.

## Affected classes

| Module | Class |
|---|---|
| `core/spring-boot-test` | `org.springframework.boot.test.context.SpringBootContextLoaderAotTests` (`loadContextForAotProcessingAndAotRuntime`, 1/1) |

Only 1 class in this session's batch, but the trigger (`GroovyBeanDefinitionReader`
being on the classpath, reached via `SpringApplication.createBeanDefinitionLoader()`)
is generic Spring Boot machinery, not specific to AOT processing or
`@CompileWithForkedClassLoader` — any class whose classpath includes Groovy
and whose `SpringApplication.run()` is the first thing in the process to
touch `GroovySystem` would be equally affected. Worth a broader search if
picked up for a fix.
