# Named JCA provider algorithm ownership enforced

**Status: FIXED 2026-07-28**

## Defect

The direct native factories for `KeyFactory`, `Signature`, `SecureRandom`, and
`Cipher` validated a supplied provider name only for existence. They then
selected their implementation solely from the requested algorithm. As a result
`KeyFactory.getInstance("RSA", "SUN")` and
`Cipher.getInstance("AES/CBC/PKCS5Padding", "SUN")` incorrectly succeeded.

## Fix

`provider_chain` now seeds JDK 25 ownership entries for the direct-native JCA
engine surface and exports one `check_provider_ownership` gate. The gate is
called after the existing provider-name validation by all four native factories,
including the `Provider`-object overloads. It resolves aliases through the same
service table as `Provider.getService`; Cipher additionally recognizes the JDK
generic `Cipher.AES` service for standard AES transformations. It preserves the
JDK-specific `NoSuchAlgorithmException` message spelling for Cipher.

The table includes the standard SUN, SunRsaSign, SunEC, SunJCE, SunJSSE, and
host-default SunMSCAPI ownership relevant to these direct routes. Placeholder
providers are no longer treated as implicit owners.

## Validation

- `cargo check -p cratonvm-native-builtins`
- `cargo test -p cratonvm-native-builtins direct_native_engine_seed_tracks_real_provider_ownership --lib`
- `ProviderLookupProbe.java`, JIT and `--nojit`: all 19 cases match the JDK 25
  reference exactly, including the two former divergences.

`JksSslStoreBundleTests` has no JCA-ownership residual. A fresh runner attempt
was blocked before launch because the shared Spring Boot fixture intentionally
lacks `build-plugin/spring-boot-antlib` and its generated test classpath; the
issue document itself lists no failing suite classes, so the two-mode parity
probe is the closure authority for this fidelity-only defect.
