# CratonVM 0.3.0 — Cryptographic Implementation Status

This document is the per-algorithm companion to the
[Cryptographic Implementation Status](../SECURITY.md#cryptographic-implementation-status)
section of `SECURITY.md`. It enumerates every algorithm CratonVM's
`javax.crypto.*` / `java.security.*` natives advertise via the JCA
provider chain, and classifies each one as:

- **Implemented** — backed by an audited RustCrypto crate or equivalent;
  produces correct output and (where applicable) is constant-time.
- **Stub-throws** — `Service.newInstance(...)` deliberately throws
  `NoSuchAlgorithmException`. Earlier prototype code may have returned
  bogus constants; that code has been removed.
- **Provider-advertised, not backed** — the provider name appears in the
  `Security.getProviders()` chain but `Provider.getService(type, alg)`
  returns null. Callers fall through to the next provider.
- **Not supported** — no provider in the chain advertises the algorithm.

## Provider chain coverage

| Provider           | Status                          | Notes                                              |
|--------------------|---------------------------------|----------------------------------------------------|
| `SUN`              | Real Service map                | Digests, SecureRandom (SHA1PRNG, DRBG)             |
| `SunJCE`           | Real Service map                | AES, AES-GCM, HMAC; EC KeyFactory; ECDSA/Ed25519 Signature; PBKDF2 rejected |
| `SunRsaSign`       | Real Service map                | RSA key factory, RSA signatures                    |
| `SunEC`            | Advertised, not backed          | The `SunEC` *provider object* registers no Service map, but EC/ECDSA algorithms are reachable because `KeyPairGenerator`/`KeyFactory`/`Signature.getInstance` short-circuit algorithm resolution (see Signature / KeyFactory tables) |
| `SunJSSE`          | Advertised, not backed          | TLS endpoints not supported                        |
| `SunJSSL`          | Advertised, not backed          | TLS endpoints not supported                        |
| `SunSASL`          | Advertised, not backed          | No SASL mechanisms                                 |
| `XMLDSig`          | Advertised, not backed          | No XML-DSig transforms                             |
| `SunPCSC`          | Advertised, not backed          | Smartcard I/O absent                               |
| `SunMSCAPI`        | Advertised, not backed          | Windows CSP bridge absent                          |
| `SunPKCS11`        | Advertised, not backed          | PKCS#11 bridge absent                              |
| `JdkLDAP`          | Advertised, not backed          | LDAPS unsupported                                  |
| `JdkSASL`          | Advertised, not backed          | Duplicate of `SunSASL`                             |

## MessageDigest

| Algorithm                  | Status        | Backing                            |
|----------------------------|---------------|------------------------------------|
| `MD5`                      | Implemented   | `md-5` crate                       |
| `SHA-1` / `SHA1`           | Implemented   | `sha1` crate                       |
| `SHA-224`                  | Implemented   | `sha2` crate                       |
| `SHA-256`                  | Implemented   | `sha2` crate                       |
| `SHA-384`                  | Implemented   | `sha2` crate                       |
| `SHA-512`                  | Implemented   | `sha2` crate                       |
| `SHA-512/224`              | Implemented   | `sha2` crate                       |
| `SHA-512/256`              | Implemented   | `sha2` crate                       |
| `SHA3-224` / -256/-384/-512| Implemented   | `sha3` crate                       |
| `BLAKE2*`                  | Not supported | —                                  |

## SecureRandom

All variants draw every output byte directly from the OS CSPRNG
(`RtlGenRandom`/`SystemFunction036` on Windows, `/dev/urandom` elsewhere),
with a ChaCha20 software fallback if the OS source is unavailable.

| Algorithm        | Status        | Backing                                       |
|------------------|---------------|-----------------------------------------------|
| `SHA1PRNG`       | Implemented   | OS RNG (`os_random_bytes`); ChaCha20 fallback |
| `DRBG`           | Implemented   | OS RNG; algorithm parameter ignored           |
| `NativePRNG*`    | Implemented   | OS RNG (`os_random_bytes`)                     |
| `Windows-PRNG`   | Implemented   | OS RNG via `RtlGenRandom` (`SystemFunction036`)|

## Cipher (symmetric)

| Transformation                       | Status        | Backing                                  |
|--------------------------------------|---------------|------------------------------------------|
| `AES/ECB/NoPadding`                  | Implemented   | `aes` crate (constant-time, AES-NI)      |
| `AES/CBC/NoPadding`                  | Implemented   | `aes` + `cbc`                            |
| `AES/CBC/PKCS5Padding`               | Implemented   | `aes` + `cbc` + PKCS#7 padding           |
| `AES/CTR/NoPadding`                  | Implemented   | `aes` + `ctr`                            |
| `AES/GCM/NoPadding`                  | Implemented   | `aes-gcm` crate                          |
| `AES_128/GCM/NoPadding`              | Implemented   | `aes-gcm` crate                          |
| `AES_192/GCM/NoPadding`              | Implemented   | `aes-gcm` crate                          |
| `AES_256/GCM/NoPadding`              | Implemented   | `aes-gcm` crate                          |
| `DES` / `DES/CBC*`                   | Implemented   | Routed to real SunJCE `DESCipher` (CBC, NoPadding/PKCS5) |
| `DESede` / `DESede/CBC*` / `TripleDES` | Implemented | Routed to real SunJCE `DESedeCipher` (CBC, NoPadding/PKCS5) |
| `RC4` / `ARCFOUR`                    | Not supported | Insecure; intentionally absent           |
| `Blowfish*`                          | Not supported | —                                        |
| `ChaCha20*` / `ChaCha20-Poly1305`    | Not supported | Planned post-0.3                         |

## Cipher (asymmetric)

| Transformation                  | Status      | Backing                          |
|---------------------------------|-------------|----------------------------------|
| `RSA/ECB/PKCS1Padding`          | Implemented | `rsa` crate                      |
| `RSA/ECB/OAEPWithSHA-*AndMGF1Padding` | Implemented | `rsa` crate                |
| `RSA/NONE/NoPadding`            | Implemented | `rsa` crate (raw)                |

## Mac

| Algorithm           | Status      | Backing                  |
|---------------------|-------------|--------------------------|
| `HmacSHA1`          | Implemented | `hmac` + `sha1`          |
| `HmacSHA224`        | Implemented | `hmac` + `sha2`          |
| `HmacSHA256`        | Implemented | `hmac` + `sha2`          |
| `HmacSHA384`        | Implemented | `hmac` + `sha2`          |
| `HmacSHA512`        | Implemented | `hmac` + `sha2`          |
| `HmacMD5`           | Implemented | `hmac` + `md-5`          |

## Signature

This table covers the `java.security.Signature` JCA API (sign/verify on
caller-supplied keys), backed by `native-builtins/src/jca/signature.rs`
dispatching to `crypto_impl`.  Algorithm resolution is done by
`algo_idx`; an unrecognised algorithm name maps to index `-1`, whose
`sign()` returns an empty byte array and whose `verify()` returns
`false` (fail-closed).  **JAR signer-block verification is a separate
subsystem with different coverage — see "JAR signature verification"
below.**

| Algorithm                       | Status        | Backing                              |
|---------------------------------|---------------|--------------------------------------|
| `SHA1withRSA`                   | Implemented   | `rsa` + `sha1`                       |
| `SHA256withRSA`                 | Implemented   | `rsa` + `sha2`                       |
| `SHA384withRSA`                 | Implemented   | `rsa` + `sha2`                       |
| `SHA512withRSA`                 | Implemented   | `rsa` + `sha2`                       |
| `SHA256withECDSA`               | Implemented   | `crypto_impl::ecdsa_*_sha256` (P-256) |
| `SHA384withECDSA`               | Implemented   | `crypto_impl::ecdsa_*` (P-384)       |
| `Ed25519` / `EdDSA`             | Implemented   | `ed25519_dalek`                      |
| `SHA1withDSA` / `SHA256withDSA` | Implemented   | Routed to the real JDK 25 SUN DSA SPIs |
| `SHA224withRSA`                 | Not supported | Not mapped by `algo_idx`             |
| `RSASSA-PSS`                    | Not supported | Not mapped by `algo_idx` (PKCS#1 v1.5 only) |
| `NONEwithRSA`                   | Not supported | Not mapped by `algo_idx`             |
| `Ed448` / `SHA512withECDSA`     | Not supported | Not mapped by `algo_idx`             |

## KeyFactory / KeyPairGenerator

| Algorithm            | Status        | Notes                                          |
|----------------------|---------------|------------------------------------------------|
| `RSA`                | Implemented   | PKCS#8 / X.509 encoding via `rsa` + `pkcs8`    |
| `EC`                 | Implemented   | P-256 / P-384 via `jca::key_factory` (`EC` / `ECDSA`) |
| `DSA`                | Implemented   | KeyPairGenerator and KeyFactory route to JDK 25 SUN DSA SPIs |
| `DH`                 | Not supported | —                                              |
| `X25519` / `X448`    | Not supported | —                                              |

## JAR signature verification

Signed-JAR trust decisions are handled by a **separate, self-contained**
implementation in `classloading/src/jar_signer.rs` — not the JCA
`Signature` provider above.  It parses the PKCS#7 / CMS SignedData
signer block (`../apps/META-INF/*.RSA|.DSA|.EC`), verifies the `messageDigest`
authenticated attribute against the `.SF` digest, verifies the
SignerInfo signature over the SignedAttributes against the leaf
certificate, and walks the X.509 chain to a trust anchor.  It is
**fail-closed**: any parse error, digest mismatch, unsupported
algorithm, or missing trust path rejects the JAR.

| Signature algorithm             | Status        | Backing                              |
|---------------------------------|---------------|--------------------------------------|
| RSA PKCS#1 v1.5, SHA-1/256/384/512 | Implemented | In-module `BigUint::modpow` + DigestInfo compare (`verify_signature_with_spki`) |
| ECDSA (`SHA*withECDSA`, P-256/384/512) | Fail-closed unsupported | Recognised but returns `SigVerify::Unsupported` / `TrustError::NotImplemented`; no EC point arithmetic reachable from `classloading` |
| DSA                             | Fail-closed unsupported | Same as ECDSA — recognised, not verifiable |
| RSA-PSS                         | Not handled   | Only PKCS#1 v1.5 DigestInfo is recognised |

> **Note:** ECDSA-signed JARs are *rejected* (fail-closed), even though
> the JCA `Signature` API can verify standalone ECDSA signatures on
> caller-supplied keys.  The two subsystems do not share code: the
> `native-builtins` EC primitives are unreachable from `classloading`
> (a build-cycle constraint), so JAR-signer EC support is a documented
> follow-up.

## Key Derivation

| Algorithm                              | Status            | Behaviour                                            |
|----------------------------------------|-------------------|------------------------------------------------------|
| `PBKDF2WithHmacSHA1`                   | Implemented       | Real PKCS#5 v2.0 derivation via `SecretKeyFactory` (`phases_early::pbkdf2_*`, HMAC over `sha1`/`sha2`) |
| `PBKDF2WithHmacSHA224`                 | Implemented       | Real PKCS#5 v2.0 derivation (HMAC-SHA-224)           |
| `PBKDF2WithHmacSHA256`                 | Implemented       | Real PKCS#5 v2.0 derivation (HMAC-SHA-256)           |
| `PBKDF2WithHmacSHA512`                 | **Stub-throws**   | `NoSuchAlgorithmException` (not mapped by `pbkdf2_prf_code`) |
| `HKDF` / `HKDFWithHmacSHA*`            | **Stub-throws**   | `NoSuchAlgorithmException` — the `javax.crypto.KDF` SPI rejects HKDF; an in-tree `crypto_impl::Hkdf` primitive exists but is not advertised |
| `Scrypt` / `Argon2`                    | Not supported     | Never advertised                                     |

## Post-Quantum (JEP 496 / JEP 497)

CratonVM has no in-tree lattice crypto; instead, when `route_pqc_to_real()` is
on (the default — opt out with `CRATONVM_SYNTHETIC_PQC=1`), these families are
routed to the **real JDK 25 SPIs**: ML-KEM via SunJCE
(`com.sun.crypto.provider.ML_KEM_Impls`, the `javax.crypto.KEM` SPI in
`native-builtins/src/jca/kem.rs`) and ML-DSA via the SUN provider
(`sun.security.provider.ML_DSA_Impls`). The keygen / KEM encaps-decaps /
sign-verify run interpreted (slow but correct, matching HotSpot).

| Algorithm          | Status          | Behaviour                                                       |
|--------------------|-----------------|-----------------------------------------------------------------|
| `ML-KEM-512`       | Implemented     | KeyPairGenerator / KeyFactory / KEM routed to real SunJCE `ML_KEM_Impls` |
| `ML-KEM-768`       | Implemented     | Routed to real SunJCE `ML_KEM_Impls`                            |
| `ML-KEM-1024`      | Implemented     | Routed to real SunJCE `ML_KEM_Impls`                            |
| `ML-DSA-44`        | Implemented     | KeyPairGenerator / KeyFactory / Signature routed to real SUN `ML_DSA_Impls$SIG{2,3,5}` |
| `ML-DSA-65`        | Implemented     | Routed to real SUN `ML_DSA_Impls`                              |
| `ML-DSA-87`        | Implemented     | Routed to real SUN `ML_DSA_Impls`                              |
| `SLH-DSA-*`        | Not supported   | Never advertised                                                |

With `CRATONVM_SYNTHETIC_PQC=1`, the synthetic stubs are restored and every
algorithm above instead throws `NoSuchAlgorithmException` / `InvalidKeySpecException`.

## TLS / SSL

| Component                   | Status        | Notes                                       |
|-----------------------------|---------------|---------------------------------------------|
| `SSLContext` / `SSLEngine`  | Not supported | `SunJSSE` is provider-advertised, not backed |
| `KeyManagerFactory`         | Not supported | —                                           |
| `TrustManagerFactory`       | Not supported | —                                           |
| `HttpsURLConnection`        | Not supported | Use external TLS terminator                 |

## Recommended deployment posture

For any deployment touching real user data or network traffic:

1. Terminate TLS in front of the JVM with `nginx`, `Envoy`, or a
   service-mesh sidecar.
2. Source long-lived secrets from a dedicated KMS (cloud HSM, Vault) —
   do not rely on `KeyStore` round-trips through CratonVM.
3. `SecretKeyFactory` PBKDF2 (HmacSHA1/224/256) and the ML-KEM/ML-DSA
   KEM/Signature paths now run real derivations/operations by default;
   however, `KeyAgreement` (DH/ECDH), `javax.crypto.KDF` HKDF, and
   `PBKDF2WithHmacSHA512` remain failure surfaces that throw
   `NoSuchAlgorithmException` — your code must handle it.
4. Do not rely on signed-JAR provenance for ECDSA/DSA-signed JARs:
   JAR signer-block verification only accepts RSA PKCS#1 v1.5 and
   fail-closes (rejects) every other signature algorithm. Sign with
   RSA, or do not depend on `getCodeSource().getCertificates()`.
5. For research, benchmarking, or self-contained tooling that only
   needs digest + AES-GCM + HMAC + RSA (plus the standalone
   `Signature` API's ECDSA/Ed25519), the in-tree crypto is correct
   and constant-time where it matters.
