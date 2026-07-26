# crypto/fips1402: BC-FIPS and SunEC P-384 KeyPairGenerator routing -- FIXED

Status: fixed on 2026-07-08 (branch `fix/crypto-fips1402-sunec-p384-20260708`).

This was exposed after the sibling BC-FIPS `AlgorithmParameters` EngineCreator fix let EC crypto run for real instead of aborting earlier. See [keycloak-crypto-fips1402-provec-algorithmparametersspi-classnotfound-FIXED.md](keycloak-crypto-fips1402-provec-algorithmparametersspi-classnotfound-FIXED.md).

## Summary

Three `crypto/fips1402` tests that exercise 384-bit EC (`ES384`) key generation failed identically:

```text
java.security.InvalidParameterException: No EC parameters available for key size 384 bits
    at sun.security.ec.ECKeyPairGenerator.initialize(ECKeyPairGenerator.java:78)
    at sun.security.ec.ECKeyPairGenerator.<init>(ECKeyPairGenerator.java:68)
```

Affected classes:

- `BCFIPSECDSACryptoProviderTest` (1/3 parameterized cases)
- `BCFIPSEcdhEsAlgorithmProviderTest` (1/2 tests)
- `FIPS1402SdJwtCreationAndSigningTest` (1/2 tests)

## Root Cause

There were two related EC keygen routing failures:

1. Explicit `KeyPairGenerator.getInstance("ECDSA", "BCFIPS")` requests were classified as non-BC because `is_bc_provider` only recognized `"BC"` or provider names containing `"BouncyCastle"`. The synthetic KeyPairGenerator therefore kept the EC algorithm but did not remember that BC-FIPS was requested, so `generateKeyPair()` drove the default SunEC path instead of BC-FIPS's provider-created key generator.
2. The no-provider Keycloak path (`KeyUtils.generateEcKeyPair("secp384r1")`) legitimately uses `KeyPairGenerator.getInstance("EC")`, so it should still use SunEC. CratonVM instantiated SunEC via `ECKeyPairGenerator()`, whose JDK 25 no-arg constructor immediately calls `initialize(SecurityProviderConstants.DEF_EC_KEY_SIZE, null)`. On this JDK that default is 384, and CratonVM's partial SunEC provider map could not resolve `ECUtil.getECParameterSpec(null, 384)`, so the constructor threw before Keycloak's explicit `initialize(ECGenParameterSpec, SecureRandom)` could run.

## Fix

Code changes in `../../../../native-builtins/src/jca/key_factory.rs` and `../../../../native-builtins/src/jca/provider_chain.rs`:

- Factored `provider_chain::build_jca_impl(...)` from `build_jca_instance(...)` so callers can reuse the existing BC-FIPS `EngineCreator` path and get the provider-created implementation object directly.
- `KeyPairGenerator.getInstance` now recognizes `BCFIPS` as a BouncyCastle-family provider and, for EC/ECDSA, returns the real BC-FIPS provider-created `KeyPairGenerator` object instead of a synthetic receiver that later falls through to SunEC.
- The SunEC EC keygen drive now allocates `sun.security.ec.ECKeyPairGenerator` with `NativeContext::allocate_instance` and then invokes the requested `initialize(...)` method explicitly. That avoids the constructor's default-size initialize while preserving the SunEC default path for no-provider EC.

## Validation

Remote Linux host `20.83.144.174`, worktree `/data/data/wt-crypto-fips1402-sunec-p384-20260708`, unique binary:

`/data/data/target-crypto-fips1402-sunec-p384-20260708/release/cratonvm-crypto-fips1402-sunec-p384-20260708`

Focused Rust validation:

```text
cargo test -p cratonvm-native-builtins --features synthetic-jdk jca::key_factory
# 7 passed

cargo test -p cratonvm-native-builtins --features synthetic-jdk jca::provider_chain -- --test-threads=1
# 22 passed
```

Keycloak `crypto/fips1402` validation with `KcRunner`, JIT on, JDK 25 at `/data/data/jdk25-real`:

```text
BCFIPSECDSACryptoProviderTest: KCRUNNER_RESULT tests=3 failed=0 aborted=0 skipped=0 containersFailed=0
BCFIPSEcdhEsAlgorithmProviderTest: KCRUNNER_RESULT tests=2 failed=0 aborted=0 skipped=0 containersFailed=0
FIPS1402SdJwtCreationAndSigningTest: KCRUNNER_RESULT tests=2 failed=0 aborted=0 skipped=0 containersFailed=0
```

The original `InvalidParameterException: No EC parameters available for key size 384 bits` no longer reproduces in any of the three documented classes.
