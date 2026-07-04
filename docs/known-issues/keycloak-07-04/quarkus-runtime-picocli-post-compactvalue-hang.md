# quarkus/runtime PicocliTest post-CompactValue-fix hang

Status: open - residual after the raw CompactValue NaN-box SIGSEGV stopped reproducing

Date observed: 2026-07-04

## Summary

`quarkus/runtime :: org.keycloak.quarkus.runtime.cli.PicocliTest` no longer crashes with `rc=139` after the compact-long local-kind fix present on `dev`. With `cratonvm-keycloak-0704-172634`, the same representative repro reaches the suite timeout instead:

```
[all-jit] HANG 120.1s org.keycloak.quarkus.runtime.cli.PicocliTest
```

The only terminal stderr line before timeout is:

```
WARN [org.keycloak.quarkus.runtime.cli.ExecutionExceptionHandler] Transformer for the 'io.quarkus.vertx.http.runtime.options.TlsUtils' class is overridden
```

This should be investigated as a new hang signature, separate from the fixed compact-value raw SIGSEGV.

## Evidence

`apps/keycloak-suite-runner/.suite/results/kc0704-compact-picocli-172634/all-jit/`
