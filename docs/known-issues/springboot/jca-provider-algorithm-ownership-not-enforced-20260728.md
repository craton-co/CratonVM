# `getInstance(algorithm, registeredProvider)` succeeds even when that provider does not offer the algorithm

**Status: OPEN — found 2026-07-28, root-caused, deliberately not fixed in place**

## Symptom

No suite test currently fails on this. It was found while closing
`../../internal/fixed-suite-bugs/springboot/core-spring-boot-keystore-provider-name-swallowed-20260723-FIXED.md`,
by diffing a JCA lookup probe against real HotSpot 25:

```
KeyFactory.getInstance("RSA", "SUN")
  HotSpot : java.security.NoSuchAlgorithmException: no such algorithm: RSA for provider SUN
  CratonVM: OK

Cipher.getInstance("AES/CBC/PKCS5Padding", "SUN")
  HotSpot : java.security.NoSuchAlgorithmException: No such algorithm: AES/CBC/PKCS5Padding
  CratonVM: OK
```

`SUN` supplies neither `RSA` key factories (that is `SunRsaSign`) nor AES
ciphers (that is `SunJCE`), so real JDK rejects both. These are the only two
remaining divergences in
`docs/internal/fixed-suite-bugs/repros/jca-provider-lookup-parity/ProviderLookupProbe.java`
(19 cases), under both JIT and `--nojit`.

## Root cause

Confirmed at file:line. `KeyFactory`, `Signature`, `SecureRandom` and `Cipher`
are served by CratonVM natives that dispatch purely on the **algorithm** —
`kf_get_instance` (`native-builtins/src/jca/key_factory.rs`),
`sig_get_instance` (`native-builtins/src/jca/signature.rs`),
`native_secure_random_get_instance` (`native-builtins/src/securerandom.rs`),
and the `Cipher.getInstance` closures
(`native-builtins/src/jca/cipher.rs`, mirrored in
`native-builtins/src/phases_early.rs`). They never consult the provider-chain
service table, so once the provider NAME resolves (which it now does — see the
FIXED doc above) nothing checks whether that provider actually owns the
algorithm.

## Why it was not fixed alongside the name-resolution defect

Enforcing ownership means routing these engines through
`provider_chain::get_service_entry`, which requires the service tables to be
complete for every engine × provider pair. They are deliberately **not**:
`provider_chain.rs`'s seed list marks `SunEC`, `SunJSSE`, `SunJGSS`, `SunSASL`,
`XMLDSig`, `SunPCSC`, `JdkLDAP`, `JdkSASL`, `SunMSCAPI` and `SunPKCS11` as
`COVERAGE_UNBACKED` — no `Service` entries are ever registered under those
names. Turning "no entry" into a rejection would break every call that names
one of them and works today, across TLS, JAAS and signing paths.

So this is a permissiveness gap that trades one class of wrong answer for
another unless the tables are filled in first. Fixing it is a service-table
completeness project, not a one-line check, and needs a full suite pass to
validate.

## What would confirm/refute

Run the probe above; the two lines are stable and reproduce in both execution
modes. To scope a fix, first inventory which `(engine, provider)` pairs the
suites actually request — instrument `get_service_entry` misses on a full
Spring Boot + Tomcat + Keycloak run — then decide per provider whether to seed
real entries or keep the permissive fallback.

## Affected classes

None currently failing. Tracked for JCA fidelity, not for a red test.
