# crypto/fips1402: `KeyPairGenerator.getInstance("ECDSA", "BCFIPS")` resolves to SunEC, not BC-FIPS, and SunEC's curve table is missing P-384

Status: open — newly exposed by fixing
[keycloak-crypto-fips1402-provec-algorithmparametersspi-classnotfound-FIXED.md](../../internal/fixed-suite-bugs/keycloak-crypto-fips1402-provec-algorithmparametersspi-classnotfound-FIXED.md)
(that fix let EC crypto actually run instead of crashing the process before reaching
this point).

Date observed: 2026-07-07 (branch fix/keycloak-fips1402-provec-classname-20260707,
local host verification of the ProvEC fix)

## Summary

Three `crypto/fips1402` tests that exercise 384-bit EC (`ES384`) key generation all fail
identically:

```
java.security.InvalidParameterException: No EC parameters available for key size 384 bits
    at java.security.InvalidParameterException.<init>(InvalidParameterException.java:59)
    at sun.security.ec.ECKeyPairGenerator.initialize(ECKeyPairGenerator.java:78)
    at sun.security.ec.ECKeyPairGenerator.<init>(ECKeyPairGenerator.java:68)
```

- `BCFIPSECDSACryptoProviderTest` (1/3 parameterized cases)
- `BCFIPSEcdhEsAlgorithmProviderTest` (1/2 tests)
- `FIPS1402SdJwtCreationAndSigningTest` (1/2 tests)

Every other case (256-bit/512-bit and the non-EC-keysize tests) passes.

## Root cause hypothesis (not yet confirmed)

The constructed `KeyPairGenerator` is `sun.security.ec.ECKeyPairGenerator` —
**SunEC's own implementation**, not any BouncyCastle-FIPS class — even though the test
explicitly requests `KeyPairGenerator.getInstance("ECDSA", BouncyIntegration.PROVIDER)`
(provider name `"BCFIPS"`, confirmed via `javap` that BC-FIPS legitimately registers
`KeyPairGenerator.ECDSA` → `KeyPairGeneratorSpi$ECDSA` with its own `EngineCreator`).

This means some existing, pre-`this`-investigation CratonVM mechanism substitutes SunEC
for the requested BC-FIPS engine for the `KeyPairGenerator` type specifically — most
likely the EC-scoped real-JCA bridge documented in
`native-builtins/src/jca/key_factory.rs`
(`route_ec_to_real`/`seed_sunec_services` — see `reference_jca_synthetic_crypto_layers`
session notes: "route_ec_to_real ... default ON" pre-dates this session and was built
for `crypto/default`'s plain-BC EC bring-up). If that shortcut unconditionally routes
any `KeyPairGenerator.getInstance("EC"/"ECDSA", *)` call to a hardcoded real-SunEC path
regardless of the requested provider name, it would explain both: (a) why BC-FIPS's own
`ProvEC$ECKeyPairGenerator` never gets constructed, and (b) why the resulting SunEC
`ECKeyPairGenerator`'s internal named-curve-by-size table doesn't happen to include
whatever curve `AlgorithmParameters.getInstance("EC")` (now correctly resolving to
BC-FIPS's `ProvEC$ECAlgParams` per the sibling fix) reports for the P-384 case —
SunEC and BC-FIPS may format/report the curve OID or name differently, and SunEC's
`initialize(int keysize, ...)` fallback path only recognizes its own known set.

## Next steps

1. Read `native-builtins/src/jca/key_factory.rs`'s `KeyPairGenerator.getInstance`
   interception to confirm whether it ignores the requested provider name for EC/ECDSA
   algorithms specifically.
2. If confirmed, decide whether to (a) route BC-FIPS's own `KeyPairGenerator.ECDSA`
   through the same `try_engine_creator_instantiate` bridge added for
   `AlgorithmParameters` (letting BC-FIPS's own SPI run for real), or (b) fix whatever
   curve-name/OID mismatch causes SunEC's `ECKeyPairGenerator` to reject 384-bit
   specifically when initialized via a spec derived from BC-FIPS's `AlgorithmParameters`.
3. Confirm real HotSpot's behavior in the same configuration (does it also end up on
   SunEC for `getInstance("ECDSA", "BCFIPS")`, or does the JDK provider chain correctly
   prefer BC-FIPS here) to establish this as a genuine CratonVM divergence rather than
   matching upstream JCA provider-priority semantics.

## Repro

```
cd C:\craton\CratonVM-fips-provec-classname-20260707
$cp = Get-Content .\fullcp_fips1402.txt -Raw   # module classpath: kc-runner + crypto/fips1402 target/{classes,test-classes} + mvn build-classpath + JUnit platform infra jars
.\target\release\cratonvm_fipsprovec.exe --java-home "C:\Program Files\Java\jdk-25" --Xmx 1g -cp $cp KcRunner org.keycloak.crypto.fips.test.BCFIPSECDSACryptoProviderTest
```

## Evidence

Local verification run, 2026-07-07, `cratonvm_fipsprovec.exe` (branch
`fix/keycloak-fips1402-provec-classname-20260707`, commit with the `try_engine_creator_instantiate`
fix applied) — `BCFIPSECDSACryptoProviderTest`: `tests=3 failed=1` (was `tests=0` /
whole-process crash before the sibling fix); `BCFIPSEcdhEsAlgorithmProviderTest`:
`tests=2 failed=1`; `FIPS1402SdJwtCreationAndSigningTest`: `tests=2 failed=1`. All three
failures share the identical `InvalidParameterException: No EC parameters available for
key size 384 bits` stack signature above `sun.security.ec.ECKeyPairGenerator`.
