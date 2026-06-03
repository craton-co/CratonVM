# Continue: Keycloak SD-JWT — EC public key is a bare interface → ClassCastException (~95 tests)

**Severity:** high — a single `<clinit>` cause zeroes the entire Keycloak SD-JWT test cluster (~95 methods across 8 classes, all green on HotSpot). Self-contained session.

## Symptom / repro
Keycloak `core` is JUnit-4; run a concrete SD-JWT test under `--nojit` (a known JIT-only `LambdaForm.<clinit>` bug blocks JUnit-4 under JIT — use `CRATONVM_DISABLE_JIT=1`). The abstract base classes (`org.keycloak.sdjwt.SdJwsTest`, etc.) can't be instantiated by JUnitCore — run the **concrete `org.keycloak.crypto.def.test.sdjwt.DefaultCrypto*Test`** subclasses, and the supplied `apps/keycloak/core/cratonvm-core-cp.txt` is **missing the crypto provider**, so build the classpath as:
```
DEPS=$(cat apps/keycloak/crypto/default/cratonvm-crypto-cp.txt)   # has BouncyCastle/provider
CP="apps/keycloak/core/target/classes;apps/keycloak/core/target/test-classes;apps/keycloak/crypto/default/target/classes;apps/keycloak/crypto/default/target/test-classes;$DEPS"
CRATONVM_DISABLE_JIT=1 MSYS_NO_PATHCONV=1 target/release/cratonvm.exe --java-home "C:/Program Files/Java/jdk-25" --stack-dump-on-timeout 0 -Xmx1g -cp "$CP" org.junit.runner.JUnitCore org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwsTest
```
HotSpot: `OK (14 tests)`. CratonVM: 0/14 — every class dies at `<clinit>`:
```
ClassCastException (TestSettings.generateEcdsaKeySpec, TestSettings.java:195)
  -> RuntimeException: Error obtaining ECParameterSpec for P-256 curve
  -> ExceptionInInitializerError on SdJwsTest.<clinit>
```

## Root cause (fully traced)
`TestSettings.generateEcdsaKeySpec` casts `keyPair.getPublic()` to `java.security.interfaces.ECPublicKey` and calls `.getParams()`. Under CratonVM:

| | HotSpot | CratonVM |
|---|---|---|
| keyGen.getProvider() | `SunEC` | **null** (synthetic) |
| public key class | `sun.security.ec.ECPublicKeyImpl` | **`java.security.PublicKey`** (bare interface!) |
| `pub instanceof ECPublicKey` | true | **false** → CCE |

CratonVM intercepts `KeyPairGenerator.getInstance("EC")`/`generateKeyPair()` with a synthetic native that allocates the public key as the **bare interface class** `"java/security/PublicKey"` (which implements only `AsymmetricKey`, never `ECPublicKey`):
- `native-builtins/src/crypto.rs:855-882` — EC `generateKeyPair` (`alg_idx==7`); the defect is **`crypto.rs:866`**: `alloc_concurrent_synthetic(ctx, "java/security/PublicKey", 4)`. Same anti-pattern for RSA (`:837`) and Ed25519 (`:888`).
- `native-builtins/src/lib.rs:17857` `alloc_concurrent_synthetic` — allocates with the literal class name, so runtime type is the interface.

The whole `crypto` module is gated behind feature `legacy-synthetic-crypto` (`native-builtins/Cargo.toml:32`, registered at `crypto.rs:1936`/`lib.rs:457`), default `[]` (OFF) in source — **but the shipped binary was built WITH it** (970 synthetic stubs per `--dump-native-registry`). See memory `reference_jca_synthetic_crypto_layers`, `feedback_no_synthetic_stubs`.

## Two fix routes
1. **Clean / project-direction (preferred long-term).** Build the binary **without** `legacy-synthetic-crypto` (+ `app-stubs`) so real SunEC bytecode runs and returns a true `ECPublicKeyImpl`. SunEC is in the JDK image (`sun/security/ec/ECPublicKeyImpl.class` verified in `lib/modules`). **Caveat:** flipping the feature off changes the whole binary — must re-validate the regression pool + all apps for stub-dependence (some apps may currently lean on the stubs; that's exactly what the synthetic-stub-removal project is working through). This is a binary-wide decision, not a local fix.
2. **Narrow (user-selected; bigger than it sounds).** Keep stubs on, but make the synthetic EC keypair return a *real* `ECPublicKey`. At `crypto.rs:866`, allocate the **concrete** `sun/security/ec/ECPublicKeyImpl` (so `instanceof ECPublicKey` is true) **and** make `getParams()` return a fully-built P-256 `java.security.spec.ECParameterSpec` and `getW()` the public `ECPoint`. Building `ECParameterSpec` from a native means constructing `EllipticCurve` (`ECFieldFp` over the P-256 prime, `a`, `b`), the generator `ECPoint(gx,gy)`, the order `n`, and cofactor `h` — i.e. several JCA value objects via natives, or a Java-side helper invoked from the native. Do the same for RSA (`RSAPublicKeyImpl` + `getModulus`/`getPublicExponent`) and Ed25519 if those clusters matter. Verify `instanceof` and `getParams()` round-trip against HotSpot.

## Verification
- `DefaultCryptoSdJwsTest` → `OK (14)`, then sweep the 8 `DefaultCrypto*` SD-JWT classes (`SdJwtVerificationTest` 16, `SdJwtVPVerificationTest` 24, `JwtVcMetadataTrustedSdJwtIssuerTest` 16, `SdJwtVPTest` 14, `SdJwtKeyBindingTest` 7, `SdJwtCreationAndSigningTest` 2, `SdJwtPresentationConsumerTest` 2) — target ~95 green, matching HotSpot.
- Regression pool stays green; for route 1, re-run the full app gauntlet to catch stub-dependence regressions.
- NOTE separate, smaller bug: `org.keycloak.SkeletonKeyTokenTest` 4/5 fail with `cannot assign instance of java.lang.Object to field KeycloakPrincipal.context` — a Java object-**deserialization** typing gap, NOT fixed by the EC-key change.

## Key files
`native-builtins/src/crypto.rs:740` (getInstance), `:820-904` (RSA/EC/Ed25519 generateKeyPair; defects at `:837/:866/:888`), `native-builtins/src/lib.rs:17857` (alloc_concurrent_synthetic), feature gate `native-builtins/Cargo.toml:32` + `lib.rs:457`. Test trigger `apps/keycloak/core/src/test/java/org/keycloak/sdjwt/TestSettings.java:188,195`; JCA call site `apps/keycloak/common/src/main/java/org/keycloak/common/util/KeyUtils.java:86`.
