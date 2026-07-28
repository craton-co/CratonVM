# `SSLContext.getDefault()` implicit-default caching

**Status: FIXED — originally reported 2026-07-23; implementation landed 2026-07-24; revalidated 2026-07-28.**

## Root cause

The live real-JDK `SSLContext.getDefault()` native in
`native-builtins/src/net_phase_e.rs` already returned an object installed by
`SSLContext.setDefault()`, but when no explicit default had been installed it
allocated a new synthetic `SSLContext` for every call. That violated the JDK
singleton-default contract and made independent calls referentially unequal.

This surfaced in
`module/spring-boot-micrometer-metrics`,
`OtlpMetricsExportAutoConfigurationTests.whenNoSslBundleDefaultHttpSenderHasDefaultSslContext()`:
the HTTP client captured one implicit default during construction while the
assertion read a different object from a second `SSLContext.getDefault()` call.

## Resolution

Commit `b155ef12abb7189bc4ceb96dfd2584085cd0c196`
(`fix(springboot,assertj): micrometer-metrics SLF4J binder, SSLContext.getDefault, AssertJ array equality`)
stores the lazily allocated context through
`crate::t27_tls::set_runtime_default_ssl_context(obj)` before returning it.
The same GC-rooted slot is used by `SSLContext.setDefault()`, so subsequent
implicit calls return the identical object and a later explicit `setDefault()`
still replaces it as required.

## Validation

Using a fresh release binary built from current `dev` source, the complete
affected Spring Boot class passed in both modes:

| Mode | Tests | Failures | Aborted | Skipped | Containers failed |
| --- | ---: | ---: | ---: | ---: | ---: |
| JIT | 21 | 0 | 0 | 0 | 0 |
| `--nojit` | 21 | 0 | 0 | 0 | 0 |

The class includes both the original implicit-default identity assertion and
the companion assertion that an explicitly configured SSL-bundle context is
not the process default. The focused call-site audit found no additional
referential-default residuals in the supplied Spring Boot checkout.
