# GraphQL security auto-configuration early hang: fixed

**Status: FIXED - 2026-07-18**

## What changed

The original rerun from 2026-07-17 recorded both GraphQL security
auto-configuration classes as silent 300-second timeouts.  That observation
is no longer reproducible on the current `dev` baseline: each process reaches
the test runner and finishes.  The baseline does, however, expose the real
residual at the same early Spring GraphQL path:

```
Cannot invoke "java.lang.Package.getName()" because the return value of
"java.lang.Class.getPackage()" is null
```

`ContextDataFetcherDecorator` asks a generated lambda class for its package.
Those VM-created lambda classes intentionally have no class-store record, so
the previous `Class.getPackage()` native implementation had no name from which
to construct a `Package` and returned `null`.

`native_class_get_package` now follows the same defining-host lookup already
used by `Class.getPackageName()`: when the mirror is a generated lambda, it
derives the package from `lambda_proxy_host`. Ordinary classes and arrays
retain their existing behavior. The related GraphQL resource residual is also
closed: `Class.forName` preserves a general user-defined URL class loader's
`ClassNotFoundException` (including Spring's `FilteredClassLoader`) instead
of bypassing its filtered resource view through a global fallback; only the
Spring Boot launcher retains its nested-JAR rescue path.

## Regression coverage

The native-builtin regression test creates a lambda mirror with no class-store
name and verifies that it returns a non-null `Package` whose name is derived
from `org/springframework/graphql/execution/ContextDataFetcherDecorator`.

The real Spring Boot fixture was run through `SbRunner` for both affected
classes, with JIT enabled and with `--nojit`:

| Class | JIT | `--nojit` |
|---|---|---|
| `GraphQlWebFluxSecurityAutoConfigurationTests` | pass | pass |
| `GraphQlWebMvcSecurityAutoConfigurationTests` | pass | pass |

Each run completed with `SBRUNNER_RESULT` and no `Class.getPackage()` null
failure.  This validates the native path in both execution modes rather than
reclassifying the old timeout by log shape alone.

## Residual disposition

The historical CGLIB/OOB-warning signature was stale fixture evidence, not a
current CGLIB lock-order recurrence. The observed current failures were the
generated-lambda package contract and the filtered resource-loader fallback;
both are covered by native and real GraphQL validation. No remaining failure
from this issue remains open.
