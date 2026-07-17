# `CertificateMatcherTests` — `KeyPairGenerator.getInstance("DSA")` throws `NoSuchAlgorithmException` on CratonVM

**Status: OPEN — found 2026-07-17**

## Symptom

All 4 parameterized test methods in `CertificateMatcherTests` fail identically
before any of the test's own assertions run — the failure is inside the
`@MethodSource`/`ArgumentsProvider` (`CertificateMatchingTestSource`) that
generates test fixtures, so the class reports `tests=0 failed=0
containersFailed=4` (JUnit treats this as a container-level failure, not an
individual test failure):

```
Failures (4):
  JUnit Jupiter:CertificateMatcherTests:matchesAnyWhenOneMatchesReturnsTrue(CertificateMatchingTestSource)
    => java.security.NoSuchAlgorithmException: DSA KeyPairGenerator not available
       java.security.GeneralSecurityException.<init>(GeneralSecurityException.java:58)
       java.security.NoSuchAlgorithmException.<init>(NoSuchAlgorithmException.java:59)
       org.springframework.boot.autoconfigure.ssl.CertificateMatchingTestSource$Algorithm.generateKeyPair(CertificateMatchingTestSource.java:108)
       org.springframework.boot.autoconfigure.ssl.CertificateMatchingTestSource.create(CertificateMatchingTestSource.java:84)
       java.util.Spliterators$ArraySpliterator.tryAdvance(Spliterators.java:1034)
```

(same trace for all 4 — `matchesWhenNoMatchReturnsFalse`,
`matchesAnyWhenNoneMatchReturnsFalse`, `matchesWhenMatchReturnsTrue`).

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard1/logs/core_spring-boot-autoconfigure.org.springframework.boot.autoconfigure.ssl.CertificateMatcherTests.out.log`

## Root cause (hypothesis)

`CertificateMatchingTestSource$Algorithm.generateKeyPair()` calls
`KeyPairGenerator.getInstance("DSA")` to build one of several key-pair
algorithms it parameterizes over (alongside RSA/EC, which are not reported
as failing here — only DSA). CratonVM's JCA provider does not register a
`KeyPairGenerator` service for `"DSA"`, so the lookup throws
`NoSuchAlgorithmException` where real HotSpot's default `SUN` provider
successfully returns one (DSA `KeyPairGenerator` is part of the JDK's
built-in provider set and this cluster is confirmed CratonVM-only via a
same-scope HotSpot baseline). This is a straightforward JCA
algorithm-registration gap (a missing service, not a broken parser like the
neighboring PEM/PKCS12 cluster in this triage batch) — plausibly the same
general area as `docs/internal/CRATONVM_BUGS/BUG-Y-tls-cluster-jca-factory-layer.md`
and other JCA-registration gaps in this codebase's history, but not
confirmed to be the identical registration table entry; no source-level
lookup of CratonVM's `KeyPairGenerator` provider registration
(`native-builtins`/JCA bootstrap) was done this session to pin the exact
file/line.

## Affected classes

| Module | Class |
|---|---|
| `core/spring-boot-autoconfigure` | `org.springframework.boot.autoconfigure.ssl.CertificateMatcherTests` |
