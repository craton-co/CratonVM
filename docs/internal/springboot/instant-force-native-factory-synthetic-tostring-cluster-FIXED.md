# Fixed: real-JDK `Instant` factories must not retain synthetic-stub dispatch

**Resolved: 2026-07-18**

## Impact

In real-JDK mode, Spring Boot actuator's Jackson configuration serializes an
`Instant` as an ISO-8601 string. A forced synthetic `Instant` factory instead
produced values whose fallback `toString()` rendered `Instant(sec.nano)`, which
broke both actuator endpoint Jackson configuration tests.

Affected acceptance classes:

| Module | Class |
|---|---|
| `module/spring-boot-actuator-autoconfigure` | `org.springframework.boot.actuate.autoconfigure.endpoint.jackson.Jackson2EndpointAutoConfigurationTests` |
| `module/spring-boot-actuator-autoconfigure` | `org.springframework.boot.actuate.autoconfigure.endpoint.jackson.JacksonEndpointAutoConfigurationTests` |

## Root cause and closure

The old `is_time_native_override` force-routed `Instant.now()`,
`ofEpochSecond(long)`, `ofEpochSecond(long, long)`, and `ofEpochMilli(long)` to
synthetic-stub callbacks even when the real JDK class was available. Removing
that override alone exposed two residual dispatch paths:

1. `execute_invokestatic` considered a registered synthetic callback enough to
   skip loading the target class, so the first factory call still created a
   stub-backed value.
2. A native invoke-cache entry created before an in-place class upgrade could
   continue serving the synthetic callback after real bytecode was loaded.

`Instant` is now a real-protected synthetic-stub class. Its fallback callback
is retained only for a genuinely synthetic bootstrap class; real-JDK static
calls load and initialize the owner before dispatch. Cached static and virtual
native entries also re-check synthetic-stub precedence and evict themselves
when the real method bytecode becomes authoritative.

## Regression coverage

`vm/tests/instant_real_jdk_factory_tostring.rs` compiles a Java probe and runs
both JIT and `--nojit`. It asserts exact ISO-8601 output for `now`, all three
factory signatures, and sub-second precision.

Focused Spring Boot acceptance on JDK 25 passed in both modes:

| Mode | Classes | Tests | Failures |
|---|---:|---:|---:|
| JIT | 2 | 13 | 0 |
| `--nojit` | 2 | 13 | 0 |
