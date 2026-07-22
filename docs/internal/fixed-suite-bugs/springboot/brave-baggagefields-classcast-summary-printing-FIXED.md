# `BraveAutoConfigurationTests` summary-printing `ClassCastException` — fixed

**Status: FIXED and retired 2026-07-15.** This note was moved from
`docs/known-issues/springboot/` after focused validation on the current
`dev` baseline.

## Failure and root cause

The original failure surfaced while JUnit rendered its execution summary:
`brave.internal.baggage.BaggageFields` was cast to `String` through the
`Formatter`/CLDR locale-initialization path. A prior fix on `dev` corrected
the early synthetic-to-real `Collections` upgrade so its real `<clinit>` runs
and initializes the static empty collections.

That exposed one independent residual in Brave's pending-span cleanup.
`WeakConcurrentMap` removes a stored `WeakKey` using its live
`TraceContext` referent. Java's `ConcurrentHashMap` calls equality on the
lookup key, so `TraceContext.equals(WeakKey)` is expected. CratonVM could
reuse a monomorphic `Object.equals(Object)` target from a different bytecode
offset sharing the same constant-pool entry, reversing that comparison and
entering `WeakKey.equals(TraceContext)`. The latter assumes a
`WeakReference`, causing:

```
java.lang.ClassCastException: brave.propagation.TraceContext cannot be cast to java.lang.ref.WeakReference
```

The runtime now keeps this receiver-polymorphic `Object.equals(Object)` call
out of the constant-pool-indexed monomorphic invoke cache. The real-JDK
reference-native bridge also implements Brave `WeakKey.equals(Object)` with
the correct referent-identity fallback, preserving the contract if a reverse
dispatch is encountered during bootstrap.

## Verification

Built the dedicated release executable
`cratonvm-brave-baggagefields-20260715-001.exe`, then ran exactly:

```
module/spring-boot-micrometer-tracing-brave
org.springframework.boot.micrometer.tracing.brave.autoconfigure.BraveAutoConfigurationTests
```

against a clean Spring Boot worktree. Both modes completed normally:

| VM mode | Result |
|---|---|
| CratonVM, JIT off | 26 found, 26 successful, 0 failed |
| CratonVM, JIT on | 26 found, 26 successful, 0 failed |

JUnit summary rendering completed in both runs. The original CLDR/Formatter
`BaggageFields`-to-`String` cast and the discovered
`TraceContext`-to-`WeakReference` residual no longer reproduce.

## Related

- [`onclasscondition-npe-cast-string-array-cluster-FIXED.md`](onclasscondition-npe-cast-string-array-cluster-FIXED.md)
  — the earlier Spring Boot condition fix that made this focused path visible.
