# quarkus/runtime PicocliTest post-CompactValue-fix hang

Status: fixed on this branch. The class-level probe no longer reaches the CratonVM watchdog timeout; it now finishes with ordinary JUnit failures.

Fixed: 2026-07-05

## Summary

`quarkus/runtime :: org.keycloak.quarkus.runtime.cli.PicocliTest` stopped reproducing the raw CompactValue NaN-box `rc=139` crash on `dev`, but the same representative run then timed out after entering Picocli/SmallRye/Keycloak configuration code.

The timeout is fixed by keeping the volatile Keycloak/Picocli/SmallRye configuration packages out of the conservative JIT policy, while still allowing explicit package re-enablement for future bisection with `CRATONVM_JIT_ALLOW_PACKAGES`. The fix also adds native fast paths for the Picocli and Keycloak configuration helpers that the focused repro repeatedly exercised before the timeout.

## Validation

Default CratonVM after rebasing onto current `origin/dev`, patched binary, JDK 25:

```
/data/bin/cratonvm-quarkus-picocli-lambda-20260705-030-rebased
/tmp/kc-picocli-direct-20260705-030-rebased-jdk25.summary
JAVA_HOME=/home/victor/jdk25
RC=1
ELAPSED=174
KCRUNNER_RESULT tests=106 failed=28 aborted=0 skipped=1 containersFailed=0
```

The important regression signal is that the run returns from the JUnit launcher instead of being killed by the 260s CratonVM stack-dump watchdog. The remaining 28 failures in this JDK 25 run are later behavioral assertions or harness/resource issues, not the post-CompactValue hang signature.

Control runs during triage:

```
# no JIT completed before the package policy change
/tmp/kc-picocli-direct-20260705-026-nojit.summary
RC=1
ELAPSED=172
KCRUNNER_RESULT tests=106 failed=27 aborted=0 skipped=1 containersFailed=0

# default JIT timed out before the package policy change
/tmp/kc-picocli-direct-20260705-027-default.summary
RC=134
ELAPSED=265

# denying the volatile packages completed before the permanent policy change
CRATONVM_JIT_DENY=org/keycloak/,picocli/,io/smallrye/
/tmp/kc-picocli-direct-20260705-028-deny-kc-picocli-smallrye.summary
RC=1
ELAPSED=168
KCRUNNER_RESULT tests=106 failed=27 aborted=0 skipped=1 containersFailed=0
```

## Fix

- `../../../../vm/src/jit/skip_list.rs` now conservatively skips `org/keycloak/`, `picocli/`, and `io/smallrye/` packages under the default conservative JIT policy, with `CRATONVM_JIT_ALLOW_PACKAGES` still able to opt selected packages back in for diagnosis.
- `native-builtins` now covers the Picocli/Keycloak/SmallRye helper methods hit in this repro path, including Picocli styled text/usage helpers, Keycloak wildcard/logging/telemetry/configuration helpers, SmallRye exact property lookup helpers, and a few supporting reflection/collection methods.

## Historical evidence

Observed on 2026-07-04 after the raw CompactValue NaN-box SIGSEGV stopped reproducing:

```
[all-jit] HANG 120.1s org.keycloak.quarkus.runtime.cli.PicocliTest
```

The only terminal stderr line before timeout was:

```
WARN [org.keycloak.quarkus.runtime.cli.ExecutionExceptionHandler] Transformer for the 'io.quarkus.vertx.http.runtime.options.TlsUtils' class is overridden
```

Original evidence directory:

```
apps/keycloak-suite-runner/.suite/results/kc0704-compact-picocli-172634/all-jit/
```
