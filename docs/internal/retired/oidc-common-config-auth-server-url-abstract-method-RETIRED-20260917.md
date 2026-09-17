# Retired 2026-09-17: OIDC `authServerUrl()` `AbstractMethodError`

Branch `claude/quarkus-lookup-oidc-kir-20260916`. Retires
`docs/known-issues/quarkus/oidc-common-config-auth-server-url-abstract-method.md`.

**The defect was not in interface-method dispatch or `Lookup.defineClass`.**
It was a one-line default: a CratonVM native that derives a config-mapping's
SmallRye Config `prefix` from the `@ConfigMapping` annotation returned `null`
instead of `""` when the annotation was absent, and that `null` propagated
into a real SmallRye Config `Assert.checkNotNullParam` null-check three calls
later, which the caller (also a CratonVM native, `cm_construct_via_context`)
was silently swallowing and treating as "could not build the real impl" —
falling back to a bare synthetic instance of the mapping **interface itself**
(no method bodies at all), so the first accessor called on it threw
`AbstractMethodError`.

## Repro chain (measured, `OidcClientConfigBuilderTest.testCredentialsBuilder`)

`OidcClientCommonConfigBuilder.CredentialsBuilder.getConfigBuilderWithDefaults()`
does:

```java
new SmallRyeConfigBuilder()
    .addDiscoveredConverters()
    .withMapping(OidcClientCommonConfig.class)   // a @ConfigGroup, not @ConfigMapping
    .build()
    .getConfigMapping(OidcClientCommonConfig.class);
```

`OidcClientCommonConfig` carries no `@ConfigMapping` annotation (only
`@ConfigGroup` — it's normally reached as a *nested* group under
`OidcClientConfig`/`OidcClientsConfig`, never as its own mapping root). Real
SmallRye Config's default prefix handler
(`ConfigMappingHandler$Handlers$ConfigMappingInterfaceHandler.getPrefix`) is:

```java
ConfigMapping ann = cls.getAnnotation(ConfigMapping.class);
return ann != null ? ann.prefix() : "";
```

`native-builtins/src/phases_late.rs::config_mapping_prefix` (the CratonVM
native backing `SmallRyeConfig.getConfigMapping(Class)`'s one-arg overload)
reimplemented only the `ann != null` half and fell through every failure arm
— annotation absent, `Class.forName` miss, `prefix()` invoke miss — to
`Value::Object(None)` (Java `null`), not `""`.

That `null` reached `ConfigMappings$ConfigClass`'s constructor as its `path`
argument three calls later
(`cm_construct_via_context` → `ConfigMappings.ConfigClass.configClass(cls,
prefix)` → `new ConfigMappings$ConfigClass(cls, path)`), which the real JDK
class null-checks via `Assert.checkNotNullParam("path", path)`, throwing
`IllegalArgumentException: Parameter 'path' may not be null` — a real,
correctly-thrown exception that `cm_construct_via_context`'s `_ => None` arm
swallowed without a trace. `native_smallrye_get_config_mapping` then fell to
`cm_fallback_alloc`, minting a bare synthetic instance of the
**`OidcClientCommonConfig` interface itself** (`JVMS 6.5` — an interface
"instance" no real bytecode could ever produce), which is why the census line
```
JVMS 6.5 uninstantiable-receiver census: ... io/quarkus/oidc/common/runtime/config/OidcClientCommonConfig (interface, requester=native-builtins/src/phases_late.rs:...)
```
was the tell. The very first accessor called on it —
`OidcCommonConfigBuilder`'s copy-constructor reading `authServerUrl()` — threw
`AbstractMethodError: method ... authServerUrl()Ljava/util/Optional; has no
Code attribute`, because an interface's own abstract declaration has none.

## The fix

`config_mapping_prefix` now returns `Value::Object(Some(ctx.create_string("")))`
— the empty string, matching HotSpot's own default — on every fallback arm
instead of `Value::Object(None)`. Three lines changed
(`native-builtins/src/phases_late.rs`).

While tracing the swallowed exception, `cm_construct_via_context`'s silent
`_ => None` arms were also given an opt-in diagnostic
(`CRATONVM_DBG_CMCTX=1`) that decodes the actual thrown Java exception class
+ message + cause instead of discarding it — this is what surfaced the real
`IllegalArgumentException` in the first place, and is kept for the next
mapping interface that hits this fallback path for a different reason.

## Verification

`OidcClientConfigBuilderTest` — the full class, real JDK bytecode, real
SmallRye Config, no JIT-off/synthetic-JDK crutches — via
`apps/quarkus-suite-runner/run-quarkus-suite.sh` against a release CratonVM
build with the fix:

```
idx  class                                       status  found  ok  failed
1    io.quarkus.oidc.client.OidcClientConfigBuilderTest  PASS    9   9   0
```

9/9, including `testCredentialsBuilder`. Before the fix: 8/9 (only
`testCredentialsBuilder` failed — every other test in the class happens to
build its `OidcClientConfig` from a manually-materialized `Impl` class or a
root `@ConfigMapping` interface, so this was the only place this exact prefix
gap fired).

`cargo test --release -p cratonvm-native-builtins --lib`: 4280 passed, 0
failed. `cargo test --release -p cratonvm-native-builtins --test
registrar_drift`: 7 passed, 0 failed (no registration conflicts from the
diagnostic addition).
