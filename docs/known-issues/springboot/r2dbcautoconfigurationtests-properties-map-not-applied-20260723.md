# `spring.r2dbc.properties.*` relaxed-bound map entries never reach the built `ConnectionFactoryOptions`

**Status: OPEN — found 2026-07-23 (craton-rerun-20260723), root cause not fully pinned down**

## Symptom

`module/spring-boot-r2dbc`, `R2dbcAutoConfigurationTests.configureWithPoolShouldApplyAdditionalProperties()`:

```
io.r2dbc.spi.NoSuchOptionException: No value found for test
       io.r2dbc.spi.NoSuchOptionException.<init>(NoSuchOptionException.java:36)
       io.r2dbc.spi.ConnectionFactoryOptions.getRequiredValue(ConnectionFactoryOptions.java:165)
       org.springframework.boot.r2dbc.autoconfigure.R2dbcAutoConfigurationTests.lambda$configureWithPoolShouldApplyAdditionalProperties$1(R2dbcAutoConfigurationTests.java:259)
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard3/logs/module_spring-boot-r2dbc.org.springframework.boot.r2dbc.autoconfigure.R2dbcAutoConfigurationTests.out.log`
(24/25 other tests in the class pass).

Test source
(`apps/spring-boot/module/spring-boot-r2dbc/src/test/java/org/springframework/boot/r2dbc/autoconfigure/R2dbcAutoConfigurationTests.java:249-263`):

```java
@Test
void configureWithPoolShouldApplyAdditionalProperties() {
    this.contextRunner
        .withPropertyValues("spring.r2dbc.url:r2dbc:simple://foo", "spring.r2dbc.properties.test=value",
                "spring.r2dbc.properties.another=2")
        .run((context) -> {
            ...
            ConnectionFactory connectionFactory = context.getBean(ConnectionPool.class).unwrap();
            assertThat(connectionFactory).asInstanceOf(type(OptionsCapableConnectionFactory.class))
                .extracting(OptionsCapableConnectionFactory::getOptions)
                .satisfies((options) -> {
                    assertThat(options.getRequiredValue(Option.<String>valueOf("test"))).isEqualTo("value");
                    assertThat(options.getRequiredValue(Option.<String>valueOf("another"))).isEqualTo("2");
                });
        });
}
```

`spring.r2dbc.properties.test`/`spring.r2dbc.properties.another` are
supposed to relaxed-bind into `R2dbcProperties.properties` (a plain
`Map<String, String>` field,
`apps/spring-boot/module/spring-boot-r2dbc/src/main/java/org/springframework/boot/r2dbc/autoconfigure/R2dbcProperties.java:72,118-120`)
and then get copied one-for-one onto the built `ConnectionFactoryOptions` by
`R2dbcAutoConfiguration.PropertiesR2dbcConnectionDetails.getConnectionFactoryOptions()`
(`apps/spring-boot/module/spring-boot-r2dbc/src/main/java/org/springframework/boot/r2dbc/autoconfigure/R2dbcAutoConfiguration.java:82`):

```java
this.properties.getProperties().forEach((key, value) -> optionsBuilder.option(Option.valueOf(key), value));
```

`getRequiredValue(Option.valueOf("test"))` failing with
`NoSuchOptionException` at the assertion means whatever `Option` keys
actually landed in the built `ConnectionFactoryOptions`, `"test"` (and
presumably `"another"`, untested since the assertion throws on the first
`getRequiredValue`) isn't one of them.

## Root cause — narrowed but not confirmed

Two live hypotheses, neither confirmed by a debugger attach or standalone
probe this session:

1. **The relaxed `Map<String, String>` binding of `spring.r2dbc.properties.test`/
   `.another` into `R2dbcProperties.properties` never populates those
   entries** — i.e. `this.properties.getProperties()` returns an empty (or
   incomplete) map at `R2dbcAutoConfiguration.java:82`, so the `forEach`
   lambda's body never runs for `"test"`/`"another"` and `optionsBuilder`
   never receives them. This would point at CratonVM's `Binder`/relaxed
   property-name matching for a bare `Map<String,String>` `@ConfigurationProperties`
   field specifically. Against this: plain `Map<String,String>`
   `@ConfigurationProperties` binding is extremely common across the wider
   Spring Boot suite and would be expected to break far more broadly than
   this one class if it were generally wrong — nothing else in this
   session's batch points at map-property binding, so if this is the cause
   it's likely narrower than "all Map<String,String> binding" (e.g.
   specific to a `Map` field combined with `Option`/reactive
   `@ConfigurationProperties("spring.r2dbc")`'s particular shape, or
   specific to this property-source layering under
   `ApplicationContextRunner.withPropertyValues`).
2. **The map *is* populated correctly but `Option.valueOf(key)`/`ConnectionFactoryOptions.Builder.option()`/`getRequiredValue()`
   disagree on key identity.** Checked against this: decompiled
   `io.r2dbc.spi.Option` (`r2dbc-spi-1.0.0.RELEASE.jar`, this worktree's
   Gradle cache) — `Option.equals()` compares by `getClass()` +
   name-string equality (not reference identity), and `Option.valueOf()`
   goes through a `ConstantPool`-based interning cache, so two
   `Option.valueOf("test")` calls should be `.equals()` (and likely
   reference-`==`) regardless of which layer of CratonVM's dispatch handles
   the call. This makes an `Option`-identity mismatch look less likely than
   hypothesis 1, but wasn't ruled out with a direct probe.

## Suggested next step

A standalone probe binding `spring.r2dbc.properties.test=value` via
`Binder.get(environment).bind("spring.r2dbc", R2dbcProperties.class)` (no
R2DBC driver, no `ApplicationContextRunner`) and inspecting
`.getProperties()` directly would immediately confirm or refute hypothesis
1 without needing a debugger attach.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-r2dbc` | `org.springframework.boot.r2dbc.autoconfigure.R2dbcAutoConfigurationTests` (`configureWithPoolShouldApplyAdditionalProperties`, 1/25) |
