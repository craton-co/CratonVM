# KeyStore (PKCS12/JKS) and ML-DSA / ML-KEM

**Status:** Shipped (default on) — but only in real-JDK mode; see the limit
below.

## What it does today

**KeyStore.** `native-builtins/src/keystore.rs` parses real PKCS#12 PFX
(shrouded key bags, MAC verification) and hand-rolls JKS (`0xFEEDFEED` magic,
SHA-1 HMAC), with a `write_jks` store path. It is registered unconditionally
from `native-builtins/src/lib.rs`. `KeyStore.getInstance` routing is in
`native-builtins/src/jca/provider_chain.rs`, which registers the service rows
(`PKCS12` → `sun.security.pkcs12.PKCS12KeyStore`, alias `PKCS#12`), intercepts
`java/security/KeyStore.getInstance(String, Provider)` and builds the SPI
through `build_jca_impl`.

**ML-DSA** keygen, key factory and `Signature` sign/verify route to
`sun.security.provider.ML_DSA_Impls$*` via `jca::key_factory` and
`jca::signature`.

**ML-KEM** keygen, key factory and `javax.crypto.KEM` encapsulate/decapsulate
route to `com.sun.crypto.provider.ML_KEM_Impls$*` via `jca::key_factory` and
`jca::kem`, OID-aware over `2.16.840.1.101.3.4.4.{1,2,3}`.
`KEM.getInstance(...).newEncapsulator(pub).encapsulate()` and
`newDecapsulator(priv).decapsulate(ct)` agree on the shared secret for
ML-KEM-512/768/1024, at HotSpot's sizes (secret 32; ciphertext 768/1088/1568).

## The limit that matters

**There is no native lattice cryptography here.** Post-quantum support works by
delegating to the real JDK's pure-Java implementations, so it requires
real-JDK mode. A build without a real JDK image has no ML-KEM and no ML-DSA.
The TCK rows in `vm/src/runtime/tck.rs` record this honestly as `Partial`
(routed, not native).

## Goal

Two related crypto gaps, both fixed by the proven "route to the real provider"
pattern already used for EC:

1. **`KeyStore.getInstance("PKCS12")` / `"JKS"`** load/store works end-to-end
   (the JCK row's named gap), not just the underlying parse.
2. **ML-DSA (post-quantum signatures) and ML-KEM (post-quantum KEM)** produce
   real keys and real sign/verify / encaps/decaps via a real provider, instead
   of name-only stubs that fail closed.

## Design

### Part A — KeyStore.getInstance("PKCS12"/"JKS") end-to-end

The parser exists; finish the surface:

1. **`KeyStore.getInstance(type)`** returns a store backed by `KeyStoreData`
   (`crypto_impl.rs:3570`), keyed by a `KEYSTORE_STORE` id.
2. **`load(InputStream, char[])`** → `KeyStoreData::parse` (JKS `:3897` / PKCS12
   `:4024`), honoring the password (and the JKS null-password integrity-skip from
   `MEMORY.md` Tomcat JSSE).
3. **Accessors**: `getKey`, `getCertificate`, `getCertificateChain`, `aliases`,
   `containsAlias`, `isKeyEntry`/`isCertificateEntry`, `size` — reading from
   `KeyStoreEntry`. Keys returned must be the *real* key types (so downstream
   `Signature`/`Cipher` work), reusing the RSA/EC real-key bridging
   (`register_rsa_priv_sign_material`, `key_factory.rs:~723`; EC
   `getPublicKey` intercept).
4. **`store(OutputStream, char[])`** (write path) for round-trip; PKCS12 write is
   the harder half (PBE-MAC, bag encryption — reuse the SunJCE Cipher routing
   from `MEMORY.md` "PEMFile PBE crypto").
5. **Default-on** once round-trip is green; the TLS suites
   (`MEMORY.md` Tomcat JSSE) are the regression gate.

### Part B — ML-DSA / ML-KEM via a real provider

Mirror EC exactly:

1. **Add a scoped routing flag** `route_pqc_to_real()` (default-on once
   validated), parallel to `route_ec_to_real` (`lib.rs:537`).
2. **Route `KeyPairGenerator`/`KeyFactory` for ML-DSA/ML-KEM** to the real
   provider (the JDK's Sun=PQC provider where available, else BouncyCastle's
   `MLDSA`/`MLKEM` — the same BC route EC/RSA use per `MEMORY.md` keycloak
   fixes). The name tables in `key_factory.rs:779`–`800` already classify the
   algorithm; the work is dispatching the actual keygen/sign/encaps to the real
   SPI instead of returning a synthetic key.
3. **`Signature` (ML-DSA)**: sign/verify through the real `SignatureSpi`.
   **`KEM` (ML-KEM)**: `encapsulate`/`decapsulate` through the real `KEMSpi`
   (Java 21+ `javax.crypto.KEM`).
4. **Correct the TCK self-report**: `tck.rs:2040`/`:2041` must reflect reality —
   `Compliant` only when routed to a real provider and exercised; otherwise
   `Partial`/`NotImplemented`. The `MEMORY.md` keycloak entry's principle ("JCA
   fail-closed honestly exposes the gap; a false-pass is worse") governs: do not
   claim compliance the crypto can't back.

## Risks

- **Provider availability**: ML-DSA/ML-KEM real SPIs require either a recent JDK
  (SunPQC) or BouncyCastle on the classpath. The route must detect the provider
  and **fail closed honestly** (not silently return a stub) when absent — the
  whole point per `MEMORY.md`.
- **Over-claiming compliance**: the current TCK rows already over-state ML-DSA/
  ML-KEM. Any change must make the report *truthful*, since a false pass hides
  the gap from the gauntlet.
- **KeyStore key-type fidelity**: a `getKey` that returns a bare-interface key
  (not a real `RSAPrivateKey`/`ECPrivateKey`) causes the CCE class documented in
  `MEMORY.md` "JCA synthetic crypto layers". Reuse the proven real-key bridging.
- **PKCS12 write** is genuinely involved (PBE, MAC, bag structure); scope it as a
  separate phase after the read path lands.
- **Password / integrity handling** edge cases (null password, wrong password,
  integrity-check skip) are a known source of suite divergence (Tomcat JSSE).

