# Cryptography

This page is the per-algorithm status of CratonVM's `javax.crypto.*` /
`java.security.*` support, advertised through the JCA provider chain. It is the
detailed companion to the [Security Overview](overview.md).

> **Crypto is best-effort and unaudited.** Use it for research, benchmarking,
> and self-contained tooling. For production traffic and real secrets, follow
> the [recommended deployment posture](#recommended-deployment-posture).

Each algorithm is classified as:

- **Implemented** — backed by an audited Rust crate or routed to real JDK
  bytecode; produces correct output and (where applicable) is constant-time.
- **Stub-throws** — deliberately throws `NoSuchAlgorithmException`.
- **Recognised, not backed** — reachable in the provider chain but returns no
  service / fails verification.
- **Not supported** — no provider advertises it.

## Random

`SecureRandom` draws every output byte directly from the **OS CSPRNG**
(`RtlGenRandom`/`BCryptGenRandom` on Windows, `/dev/urandom` on Unix), with a
ChaCha20 software fallback if the OS source is unavailable. `SHA1PRNG`, `DRBG`,
`NativePRNG*`, and `Windows-PRNG` are all implemented on this source. If OS
entropy is *completely* unavailable, the code logs a warning and falls back to a
time/PID-mixed seed — a best-effort degraded mode, **not** a cryptographic
source.

## Message digests

| Algorithm | Status |
|-----------|--------|
| `MD5`, `SHA-1` | Implemented |
| `SHA-224`, `SHA-256`, `SHA-384`, `SHA-512`, `SHA-512/224`, `SHA-512/256` | Implemented |
| `SHA3-224` / `-256` / `-384` / `-512` | Implemented |
| `BLAKE2*` | Not supported |

## Symmetric ciphers

| Transformation | Status |
|----------------|--------|
| `AES/ECB/NoPadding` | Implemented (constant-time, AES-NI) |
| `AES/CBC/NoPadding`, `AES/CBC/PKCS5Padding` | Implemented |
| `AES/CTR/NoPadding` | Implemented |
| `AES/GCM/NoPadding`, `AES_128/192/256/GCM/NoPadding` | Implemented |
| `DES`, `DES/CBC*` | Implemented (routed to real `DESCipher`) |
| `DESede` / `TripleDES`, `DESede/CBC*` | Implemented (routed to real `DESedeCipher`) |
| `RC4` / `ARCFOUR`, `Blowfish*` | Not supported (insecure / absent) |
| `ChaCha20*`, `ChaCha20-Poly1305` | Not supported (planned) |

**AES and AES-GCM are constant-time**, backed by audited Rust crates (no
hand-rolled S-box or bit-loop GHASH).

## Asymmetric ciphers

| Transformation | Status |
|----------------|--------|
| `RSA/ECB/PKCS1Padding` | Implemented |
| `RSA/ECB/OAEPWithSHA-*AndMGF1Padding` | Implemented |
| `RSA/NONE/NoPadding` (raw) | Implemented |

> **RSA private-key operations use base blinding but are not fully
> constant-time.** Blinding removes the message-dependent timing/branch channel,
> but the underlying big-integer `modpow` is variable-time. Adequate for
> functional compatibility; do **not** rely on it as fully side-channel
> resistant. For hardened RSA/ECDSA, terminate it in audited native crypto
> outside the VM.

## MAC

`HmacMD5`, `HmacSHA1`, `HmacSHA224`, `HmacSHA256`, `HmacSHA384`, and `HmacSHA512`
are all **implemented**.

## Signatures (JCA `Signature` API)

| Algorithm | Status |
|-----------|--------|
| `SHA1withRSA`, `SHA256withRSA`, `SHA384withRSA`, `SHA512withRSA` | Implemented |
| `SHA256withECDSA` (P-256), `SHA384withECDSA` (P-384) | Implemented |
| `Ed25519` / `EdDSA` | Implemented |
| `SHA1withDSA` / `SHA256withDSA` | Implemented (routed to the JDK 25 SUN DSA SPIs) |
| `SHA224withRSA`, `RSASSA-PSS`, `NONEwithRSA` | Not supported |
| `Ed448`, `SHA512withECDSA` | Not supported |

Unrecognised signature algorithm names fail closed (`sign` returns empty,
`verify` returns `false`).

## Key factories & generators

| Algorithm | Status |
|-----------|--------|
| `RSA` | Implemented (PKCS#8 / X.509 encoding) |
| `EC` (P-256 / P-384) | Implemented |
| `DSA` | Implemented (KeyPairGenerator / KeyFactory routed to JDK 25 SUN) |
| `DH`, `X25519`, `X448` | Not supported |

## Key derivation

| Algorithm | Status |
|-----------|--------|
| `PBKDF2WithHmacSHA1` / `SHA224` / `SHA256` | Implemented (real PKCS#5 v2.0) |
| `PBKDF2WithHmacSHA512` | Stub-throws |
| `HKDF` / `HKDFWithHmacSHA*` | Stub-throws |
| `Scrypt`, `Argon2` | Not supported |

## Post-quantum (JEP 496 / 497)

CratonVM has no in-tree lattice crypto; instead these families are **routed to
the real JDK SPIs** by default (opt out with `CRATONVM_SYNTHETIC_PQC=1`, which
makes them throw):

| Algorithm | Status |
|-----------|--------|
| `ML-KEM-512` / `-768` / `-1024` | Implemented (routed to the real KEM SPI) |
| `ML-DSA-44` / `-65` / `-87` | Implemented (routed to the real signature SPI) |
| `SLH-DSA-*` | Not supported |

These run interpreted (correct, but slower than a native crypto provider).

## TLS / SSL

| Component | Status |
|-----------|--------|
| `SSLContext`, `SSLEngine`, `KeyManagerFactory`, `TrustManagerFactory`, `HttpsURLConnection` | Not supported |

Terminate TLS in front of the JVM (see below). Note that CratonVM *does* perform
real X.509 trust-chain validation for the trust-manager check path — see
[Sandboxing & Hardening](sandboxing.md#tls-trust-validation).

## JAR signature verification

Signed-JAR trust decisions are handled by a **separate, self-contained**
implementation (not the JCA `Signature` provider). It is **fail-closed**: any
parse error, digest mismatch, unsupported algorithm, or missing trust path
rejects the JAR.

| Signature algorithm | Status |
|---------------------|--------|
| RSA PKCS#1 v1.5, SHA-1/256/384/512 | Implemented |
| ECDSA (`SHA*withECDSA`) | Fail-closed unsupported (JAR is rejected) |
| DSA | Fail-closed unsupported |
| RSA-PSS | Not handled |

> ECDSA-signed JARs are *rejected* even though the standalone `Signature` API
> can verify ECDSA on caller-supplied keys — the two subsystems do not share
> code. Sign JARs with RSA, or do not depend on
> `getCodeSource().getCertificates()`.

## Recommended deployment posture

For any deployment touching real user data or network traffic:

1. **Terminate TLS in front of the JVM** (nginx, Envoy, or a service-mesh
   sidecar).
2. **Source long-lived secrets from a dedicated KMS** (cloud HSM, Vault) — don't
   rely on `KeyStore` round-trips through CratonVM.
3. **Handle the failure surfaces:** `KeyAgreement` (DH/ECDH), `HKDF`, and
   `PBKDF2WithHmacSHA512` throw `NoSuchAlgorithmException` — your code must
   handle it.
4. **Don't rely on signed-JAR provenance for ECDSA/DSA-signed JARs** — only RSA
   PKCS#1 v1.5 verifies; everything else fail-closes.
5. For research/benchmarking/tooling needing only digests + AES-GCM + HMAC + RSA
   (plus the standalone `Signature` API's ECDSA/Ed25519), the in-tree crypto is
   correct and constant-time where it matters.
