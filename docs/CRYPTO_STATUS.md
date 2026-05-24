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
| `SunJCE`           | Real Service map                | AES, AES-GCM, HMAC; PBKDF2 rejected                |
| `SunRsaSign`       | Real Service map                | RSA key factory, RSA signatures                    |
| `SunEC`            | Advertised, not backed          | EC / ECDSA fall through; not supported             |
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

| Algorithm        | Status        | Backing                                |
|------------------|---------------|----------------------------------------|
| `SHA1PRNG`       | Implemented   | OS RNG (`getrandom`) for seed and fill |
| `DRBG`           | Implemented   | OS RNG; algorithm parameter ignored    |
| `NativePRNG*`    | Implemented   | OS RNG via `getrandom`                 |
| `Windows-PRNG`   | Implemented   | OS RNG via `BCryptGenRandom`           |

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
| `DES*` / `DESede*`                   | Not supported | Legacy 3DES intentionally absent         |
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

| Algorithm                       | Status        | Backing                              |
|---------------------------------|---------------|--------------------------------------|
| `SHA1withRSA`                   | Implemented   | `rsa` + `sha1`                       |
| `SHA224withRSA`                 | Implemented   | `rsa` + `sha2`                       |
| `SHA256withRSA`                 | Implemented   | `rsa` + `sha2`                       |
| `SHA384withRSA`                 | Implemented   | `rsa` + `sha2`                       |
| `SHA512withRSA`                 | Implemented   | `rsa` + `sha2`                       |
| `RSASSA-PSS`                    | Implemented   | `rsa` crate                          |
| `NONEwithRSA`                   | Implemented   | `rsa` crate                          |
| `SHA*withECDSA`                 | Not supported | No EC provider backing               |
| `Ed25519` / `Ed448`             | Not supported | No EdDSA provider backing            |

## KeyFactory / KeyPairGenerator

| Algorithm            | Status        | Notes                                          |
|----------------------|---------------|------------------------------------------------|
| `RSA`                | Implemented   | PKCS#8 / X.509 encoding via `rsa` + `pkcs8`    |
| `EC`                 | Not supported | —                                              |
| `DSA`                | Not supported | —                                              |
| `DH`                 | Not supported | —                                              |
| `X25519` / `X448`    | Not supported | —                                              |

## Key Derivation

| Algorithm                              | Status            | Behaviour                                            |
|----------------------------------------|-------------------|------------------------------------------------------|
| `PBKDF2WithHmacSHA1`                   | **Stub-throws**   | `NoSuchAlgorithmException` (was: fixed-salt prototype) |
| `PBKDF2WithHmacSHA256`                 | **Stub-throws**   | `NoSuchAlgorithmException`                           |
| `PBKDF2WithHmacSHA512`                 | **Stub-throws**   | `NoSuchAlgorithmException`                           |
| `HKDF` / `HKDFWithHmacSHA*`            | **Stub-throws**   | `NoSuchAlgorithmException`                           |
| `Scrypt` / `Argon2`                    | Not supported     | Never advertised                                     |

## Post-Quantum (JEP 496 / JEP 497)

| Algorithm          | Status          | Behaviour                                                       |
|--------------------|-----------------|-----------------------------------------------------------------|
| `ML-KEM-512`       | **Stub-throws** | `NoSuchAlgorithmException` (was: zero-filled "key" prototype)   |
| `ML-KEM-768`       | **Stub-throws** | `NoSuchAlgorithmException`                                      |
| `ML-KEM-1024`      | **Stub-throws** | `NoSuchAlgorithmException`                                      |
| `ML-DSA-44`        | **Stub-throws** | `NoSuchAlgorithmException`                                      |
| `ML-DSA-65`        | **Stub-throws** | `NoSuchAlgorithmException`                                      |
| `ML-DSA-87`        | **Stub-throws** | `NoSuchAlgorithmException`                                      |
| `SLH-DSA-*`        | Not supported   | Never advertised                                                |

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
3. Treat any code path that calls `KeyAgreement`, `KEM`, or PBKDF
   factories as an immediate failure surface: those throw
   `NoSuchAlgorithmException` and your code must handle it.
4. For research, benchmarking, or self-contained tooling that only
   needs digest + AES-GCM + HMAC + RSA, the in-tree crypto is
   correct and constant-time where it matters.
