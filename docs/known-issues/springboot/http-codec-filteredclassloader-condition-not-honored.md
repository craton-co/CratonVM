# `spring-boot-http-codec`: `FilteredClassLoader`-hidden Jackson still detected as present

**Status: OPEN — found 2026-07-17. Same mechanism as 3 already-filed sibling docs (see below); not independently root-caused at the VM-source level this session.**

## Symptom

| Module | Class | Failing test |
|---|---|---|
| `module/spring-boot-http-codec` | `CodecsAutoConfigurationTests` | `kotlinSerializationUsesUnrestrictedPredicateWhenNoOtherJsonConverterIsAvailable` (1 of many) |

```
JUnit Jupiter:CodecsAutoConfigurationTests:kotlinSerializationUsesUnrestrictedPredicateWhenNoOtherJsonConverterIsAvailable()
    => org.opentest4j.AssertionFailedError:
Expecting value to be true but was false
       org.springframework.boot.http.codec.autoconfigure.CodecsAutoConfigurationTests.lambda$kotlinSerializationUsesUnrestrictedPredicateWhenNoOtherJsonConverterIsAvailable$0(CodecsAutoConfigurationTests.java:158)
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-http-codec.org.springframework.boot.http.codec.autoconfigure.CodecsAutoConf-8f9a5bcd4b78.out.log`

## What the test does (confirmed from source)

`CodecsAutoConfigurationTests.java:151-159`:
```java
void kotlinSerializationUsesUnrestrictedPredicateWhenNoOtherJsonConverterIsAvailable() {
    FilteredClassLoader classLoader = new FilteredClassLoader(JsonMapper.class.getPackage().getName(),
            ObjectMapper.class.getPackage().getName());
    this.contextRunner.withClassLoader(classLoader)
        .withUserConfiguration(KotlinxJsonConfiguration.class)
        .run((context) -> {
            KotlinSerializationJsonEncoder encoder = findEncoder(context, KotlinSerializationJsonEncoder.class);
            assertThat(encoder.canEncode(ResolvableType.forClass(Map.class), MediaType.APPLICATION_JSON)).isTrue();
        });
}
```
`FilteredClassLoader` (`org.springframework.boot.test.context.FilteredClassLoader`)
hides the Jackson `ObjectMapper`/`JsonMapper` packages so
`CodecsAutoConfiguration` should behave as if Jackson isn't on the
classpath — in which case the Kotlin-serialization JSON encoder is
supposed to switch to an "unrestricted" `canEncode` predicate (it no longer
has to defer to Jackson for generic types like `Map`). The assertion
(`canEncode(Map.class, APPLICATION_JSON)` should be `true`) fails — the
encoder is still behaving as if Jackson is present and restricting what it
will encode.

## Root cause

**Not independently confirmed this session — but this is the same
symptom shape as 3 already-filed 2026-07-17 sibling docs, one of which
(`jdbc-classloader-hide-override-not-honored-cluster.md`) already
root-caused the mechanism at the VM source level:**

- [`jdbc-classloader-hide-override-not-honored-cluster.md`](jdbc-classloader-hide-override-not-honored-cluster.md) —
  confirmed that a `URLClassLoader` subclass overriding `loadClass(String,
  boolean)` (exactly `FilteredClassLoader`'s shape — see below) is not
  consistently honored when reached via `ClassUtils.isPresent`/
  `Class.forName(name, false, loader)`, and traces the native routing
  through `native-builtins/src/classloader.rs::cl_load_class` /
  `receiver_overrides_load_class_resolve` and the `force_native_over_real_jdk_bytecode`
  gate in `vm/src/runtime/interpreter.rs`.
- [`micrometer-tracing-filteredclassloader-condition-not-honored.md`](micrometer-tracing-filteredclassloader-condition-not-honored.md) —
  the exact same `FilteredClassLoader` class, a different
  `@ConditionalOnClass`-driven autoconfiguration guard, same "hidden
  package still detected as present" symptom.
- `r2dbc-filteredclassloader-loadclass-override-bypassed.md` — same
  family, third module.

This module's test uses `org.springframework.boot.test.context.FilteredClassLoader`
directly (verified from source,
`apps/spring-boot/test-support/spring-boot-test-support/.../FilteredClassLoader.java:112-119`):
```java
protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
    for (Predicate<String> filter : this.classesFilters) {
        if (filter.test(name)) throw new ClassNotFoundException();
    }
    return super.loadClass(name, resolve);
}
```
— the identical `protected Class<?> loadClass(String, boolean)` override
shape the `jdbc` doc already traced into `cl_load_class`. This is a fourth
independent occurrence of the same underlying gap (whatever the precise
mechanism turns out to be — the `jdbc` doc itself notes its VM source read
"looks correct on paper" and flags a possible dispatch-cache issue as the
next-most-likely candidate, not yet confirmed by a live repro). Not
re-investigated at the source level in this pass — filing as a fourth data
point for whoever picks up the `jdbc` doc's suggested standalone repro
(construct a `URLClassLoader` subclass overriding `loadClass(String,
boolean)`, call `Class.forName("java.lang.String", false, thatLoader)`,
see whether the override is honored).

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-http-codec` | `org.springframework.boot.http.codec.autoconfigure.CodecsAutoConfigurationTests` (1 failing test: `kotlinSerializationUsesUnrestrictedPredicateWhenNoOtherJsonConverterIsAvailable`) |
