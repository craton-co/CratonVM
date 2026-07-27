# KeyStore (PKCS12/JKS) + ML-DSA / ML-KEM Real-Provider Coverage

Status: implemented. M. Close the `KeyStore.getInstance` gap and route
post-quantum ML-DSA / ML-KEM to a real provider — `java_security` is the lowest
JCK-compliance row, and these are its named gaps.

**Implementation status (2026-06-19):**
- **Part A — KeyStore** load + read accessors + `engineStore` write path: done
  (`native-builtins/src/keystore.rs`, JKS round-trip via `write_jks`).
- **Part B — ML-DSA** keygen/keyfactory + `Signature` sign/verify: done, routed
  to `sun.security.provider.ML_DSA_Impls$KPG*/$KF*/$SIG*`
  (`jca::key_factory`, `jca::signature`).
- **Part B — ML-KEM** keygen/keyfactory + **`javax.crypto.KEM` encaps/decaps**:
  done, routed to `com.sun.crypto.provider.ML_KEM_Impls$KPG*/$KF*/$K*`
  (`jca::key_factory`, new `jca::kem`). Validated end-to-end:
  `KEM.getInstance(...).newEncapsulator(pub).encapsulate()` and
  `newDecapsulator(priv).decapsulate(ct)` agree on the shared secret for
  ML-KEM-512/768/1024 (sizes match HotSpot: secret=32, ct=768/1088/1568).
  Required a sibling fix to the `MessageDigest` shim — registering the
  `digest(byte[],int,int)` overload that ML-KEM's FIPS-203 keygen hashes
  through (`jca::message_digest::md_digest_into`).
- **TCK rows** (`vm/src/runtime/tck.rs`) corrected to honest `Partial` (routed,
  not native) for both ML-KEM (496) and ML-DSA (497).
- Gated behind `route_pqc_to_real()` (default on; `CRATONVM_SYNTHETIC_PQC=1`
  fails closed with `NoSuchAlgorithmException`).

## Goal

Two related crypto gaps, both fixed by the proven "route to the real provider"
pattern already used for EC:

1. **`KeyStore.getInstance("PKCS12")` / `"JKS"`** load/store works end-to-end
   (the JCK row's named gap), not just the underlying parse.
2. **ML-DSA (post-quantum signatures) and ML-KEM (post-quantum KEM)** produce
   real keys and real sign/verify / encaps/decaps via a real provider, instead
   of name-only stubs that fail closed.

## Current state (cited)

- **`java_security` is the lowest JCK row and names both gaps.**
  `docs/jck-compliance.md` (`api/java_security`, **68%**): "SHA-2,
  SHA-3, HMAC, AES-GCM/CBC/CTR, ChaCha20-Poly1305, Ed25519 all shipped. RSA +
  ECDSA partial. `KeyStore.getInstance("PKCS12")` is a known gap."
- **KeyStore parsing exists; the `getInstance` routing is the gap.**
  `native-builtins/src/crypto_impl.rs:3547` ("G60 — KeyStore real loading (JKS +
  PKCS12)") defines `KeyStoreEntry` (`:3554`), `KeyStoreData` (`:3570`), a global
  `KEYSTORE_STORE` (`:3576`), and `KeyStoreData::parse` for JKS (`:3897`) and
  PKCS12 (`crypto_impl.rs:4024` dispatch: `"PKCS12"|"pkcs12"|"p12" =>
  Self::load_pkcs12`). `vm/src/vm.rs:38081`/`:38106`/`:38144` reference a
  `"PKCS12"` store type. So the parser is present — the gap (per the JCK note) is
  the `KeyStore.getInstance(type)` → real-load/store/get-entry surface being
  wired and complete (aliases, `getKey`/`getCertificate`/`store` round-trip,
  password handling — cf. `MEMORY.md` "Tomcat JSSE TLS chain" JKS HMAC null-pw
  fix).
- **ML-DSA / ML-KEM are name-mapped but not really provided.**
  `native-builtins/src/jca/key_factory.rs:779`–`800`: `algo_idx`/`algo_name`
  recognize `ML-KEM-512/768/1024` and `ML-DSA-44/65/87`, but these are name
  tables. The TCK self-report claims them compliant
  (`vm/src/runtime/tck.rs:2040` "ML-KEM … Post-quantum KEM implemented",
  `:2041` "ML-DSA … implemented") — **but `MEMORY.md` "keycloak suite vs
  HotSpot" records the truth**: "JCA fail-closed only honestly-exposes
  unimplemented ML-DSA (false-pass before); real fix = route ML-DSA/ML-KEM to BC
  like EC." So the name plumbing exists; the cryptography does not, and the TCK
  rows over-claim.
- **The proven routing template exists: `route_ec_to_real` (default ON).**
  `native-builtins/src/lib.rs:537` `route_ec_to_real()`; the EC-scoped real-SunEC
  routing (`key_factory.rs:185` "EC-scoped real-SunEC routing
  (crate::route_ec_to_real, default ON)") and the Cipher path's
  `route_ec_to_real()` gate (`jca/cipher.rs:773`) show the pattern: a default-on
  scoped flag that hands EC operations to the real provider's CipherSpi /
  KeyFactory / Signature while keeping CratonVM's fast paths for everything else.
  `MEMORY.md` "keycloak full-suite fixes" documents `route_rsa_to_real` and the
  EC cert `getPublicKey`-via-`BouncyCastleProvider.getPublicKey` intercept as the
  same approach.

Net: KeyStore needs the `getInstance` surface completed over an existing parser;
ML-DSA/ML-KEM need the *cryptography* routed to a real provider behind the same
`route_*_to_real` pattern EC/RSA already use, and the over-claiming TCK rows
corrected.

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

## Implementation steps (ordered)

1. **KeyStore `getInstance` + `load` + read accessors** over the existing
   parser (Part A 1–3); real key types out.
2. **Validate read path** against the TLS suites (Tomcat JSSE / keystore-loading
   tests); flip default-on.
3. **KeyStore `store` (write/round-trip)** including PKCS12 PBE-MAC (Part A 4–5).
4. **`route_pqc_to_real` flag** + route ML-DSA/ML-KEM KeyPairGenerator/KeyFactory
   to the real provider (Part B 1–2).
5. **ML-DSA Signature + ML-KEM KEM** through the real SPI (Part B 3).
6. **Fix the TCK rows** to match actual coverage (Part B 4).
7. **Soak** against the keycloak crypto suite (`test-infra/run-keycloak-suite.sh`,
   `MEMORY.md` "keycloak suite vs HotSpot") and flip defaults when green.

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

## Effort

M. Part A read path (steps 1–2) is S–M (parser exists). Part A write path
(step 3) is M (PKCS12 PBE). Part B (steps 4–6) is M and follows the EC/RSA
routing template closely — the main work is provider detection + honest
fail-closed, not new cryptography on our side.
