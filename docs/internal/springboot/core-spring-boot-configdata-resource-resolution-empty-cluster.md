# Spring Boot ConfigData resource-resolution empty cluster — fixed

**Resolved: 2026-07-18**

## Root cause

`native-builtins/src/spring_startup_bootstrap.rs` registered synthetic native
implementations for both public `StandardConfigDataLocationResolver` methods:

- `resolve(ConfigDataLocationResolverContext, ConfigDataLocation)`
- `resolveProfileSpecific(ConfigDataLocationResolverContext, ConfigDataLocation, Profiles)`

Each implementation constructed and returned an empty `ArrayList`. That
silently bypassed Spring Boot's real resolver bytecode, so existing classpath
and file resources were presented to every ConfigData caller as if no location
had resolved. The compatibility shim had been added for an old, unverified
bootstrap-null hypothesis and was no longer valid for the real fixture.

## Fix and regression guard

The two native registrations and their empty-list implementation were removed.
Spring Boot now executes its own ordinary and profile-specific resolver
bytecode. A native-registry regression test asserts that neither method can be
registered as a CratonVM native again.

## Validation

- `cargo test -p cratonvm-native-builtins standard_config_data_resolver_uses_real_bytecode --lib` passed.
- A focused probe on the real Spring Boot test classpath returned one location
  from the public resolver in both JIT and `--nojit` modes.
- `ConfigDataEnvironmentTests` passed **21/21** under `--nojit` through the
  Spring Boot suite runner (113.3 seconds).

The JIT suite harness timed out in JUnit engine initialization before any
ConfigData result. The resolver-level JIT probe completed successfully, so that
documented JUnit livelock signature is separate from this ConfigData fix.

## Scope

`BeanDefinitionLoaderTests`, `SimpleMainTests.basePackageScan`, and the
Windows trailing-separator assertion in
`ConfigTreeConfigDataLocationResolverTests` use separate code paths and are
not effects of the removed ConfigData resolver override.
