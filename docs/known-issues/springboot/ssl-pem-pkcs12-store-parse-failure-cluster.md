# `spring-boot-autoconfigure` SSL bundle tests: PEM private-key parse and PKCS12 keystore load both fail against CratonVM-loaded test fixture files

**Status: OPEN — found 2026-07-17**

## Symptom

3 classes, 4 individual test failures, all inside `org.springframework.boot.ssl`
loading real key/trust material from classpath test-fixture files under
`org/springframework/boot/autoconfigure/ssl/`:

| Class | Method | Failure |
|---|---|---|
| `PropertiesSslBundleTests` | `jksPropertiesAreMappedToSslBundle` | `IllegalStateException: Unable to create trust store: Could not load store from 'classpath:.../ssl/keystore.pkcs12'` → `IOException: PKCS#12 MAC verification failed (wrong password?)` |
| `PropertiesSslBundleTests` | `pemPropertiesAreMappedToSslBundle` | `IllegalStateException: Unable to create trust store: Missing private key or unrecognized format` → `PemPrivateKeyParser.parse` |
| `SslAutoConfigurationTests` | `sslBundlesCreatedWithCertificates` | `IllegalStateException: Unable to create key store: Missing private key or unrecognized format` → `PemPrivateKeyParser.parse` |
| `SslPropertiesBundleRegistrarTests` | `shouldUseResourceLoader` | `IllegalStateException: Could not load SSL context: Could not load trust manager factory: Unable to create trust store: Missing private key or unrecognized format` → `PemPrivateKeyParser.parse` |

Representative trace (`SslAutoConfigurationTests`):

```
=> java.lang.IllegalStateException: Unable to create key store: Missing private key or unrecognized format
   org.springframework.boot.ssl.pem.PemSslStoreBundle.createKeyStore(PemSslStoreBundle.java:108)
   org.springframework.boot.ssl.pem.PemSslStoreBundle.lambda$new$0(PemSslStoreBundle.java:70)
   org.springframework.boot.ssl.pem.PemSslStoreBundle.getKeyStore(PemSslStoreBundle.java:76)
   org.springframework.boot.autoconfigure.ssl.SslAutoConfigurationTests.lambda$sslBundlesCreatedWithCertificates$1(SslAutoConfigurationTests.java:95)
 Caused by: java.lang.IllegalStateException: Missing private key or unrecognized format
   org.springframework.boot.ssl.pem.PemPrivateKeyParser.parse(PemPrivateKeyParser.java:220)
   org.springframework.boot.ssl.pem.PemContent.getPrivateKey(PemContent.java:87)
   org.springframework.boot.ssl.pem.LoadedPemSslStore.loadPrivateKey(LoadedPemSslStore.java:79)
```

and the PKCS12 sub-case (`PropertiesSslBundleTests.jksPropertiesAreMappedToSslBundle`):

```
=> java.lang.IllegalStateException: Unable to create trust store: Could not load store from 'classpath:org/springframework/boot/autoconfigure/ssl/keystore.pkcs12'
   org.springframework.boot.ssl.jks.JksSslStoreBundle.createKeyStore(JksSslStoreBundle.java:114)
 Caused by: java.lang.IllegalStateException: Could not load store from 'classpath:org/springframework/boot/autoconfigure/ssl/keystore.pkcs12'
   org.springframework.boot.ssl.jks.JksSslStoreBundle.loadKeyStore(JksSslStoreBundle.java:142)
 Caused by: java.io.IOException: PKCS#12 MAC verification failed (wrong password?)
```

Full logs (relative to repo root):
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/core_spring-boot-autoconfigure.org.springframework.boot.autoconfigure.ssl.PropertiesSslBundleTests.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/core_spring-boot-autoconfigure.org.springframework.boot.autoconfigure.ssl.SslAutoConfigurationTests.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/core_spring-boot-autoconfigure.org.springframework.boot.autoconfigure.ssl.SslPropertiesBundleRegistrarTests.out.log`

## Root cause (hypothesis — grounded in the stack traces, not confirmed against CratonVM native source)

Both failure shapes are 100% real Spring Framework bytecode
(`org.springframework.boot.ssl.pem.PemPrivateKeyParser`,
`org.springframework.boot.ssl.jks.JksSslStoreBundle` — no synthetic/native
CratonVM frame appears anywhere in either stack), so the defect is one
level down, inside whatever JCA primitive these two call into on CratonVM:

- **PEM path (3/4 failures):** `PemPrivateKeyParser.parse()` (real JDK/Spring
  bytecode, line 220) tries a fixed list of `KeyFactory` algorithms
  (RSA/EC/DSA/Ed25519/Ed448, PKCS8-encoded) against the same DER byte
  content in turn and only throws `"Missing private key or unrecognized
  format"` once every one of them has failed. Real HotSpot decodes the same
  test-fixture PEM file's private key without issue (this cluster is
  confirmed CratonVM-only per the assignment's HotSpot-baseline pass), so
  this points at `KeyFactory.generatePrivate(PKCS8EncodedKeySpec)` (or the
  underlying ASN.1/DER decode it depends on) rejecting a key CratonVM's JCA
  provider should be able to parse — the same general "JCA
  crypto-provider gap" area flagged elsewhere in project history (see
  `reference_jca_synthetic_crypto_layers.md` / `reference_rsa_crt_keygen_and_cert_encoding_tls_fixes.md`
  in session memory, and `docs/internal/CRATONVM_BUGS/BUG-Y-tls-cluster-jca-factory-layer.md`
  in this repo) but not confirmed here to be the *same* specific gap — no
  breakpoint or standalone `KeyFactory` probe was run this session to pin
  which algorithm/key-encoding variant is failing.
- **PKCS12 path (1/4 failures):** `KeyStore.getInstance("PKCS12").load(...)`
  throws `IOException: PKCS#12 MAC verification failed (wrong password?)`
  loading `keystore.pkcs12` with the password the test supplies — a MAC
  (HMAC-SHA*) integrity-check failure inside CratonVM's PKCS12 keystore
  parser, distinct from the PEM private-key-parsing mechanism above (this
  is `KeyStore.load`, not `KeyFactory.generatePrivate`) but in the same
  general "loading a real key/trust-store test fixture" area. A prior,
  same-day crash-cluster doc
  (`crashfail-20260717-crash-cluster.md`, Cluster 2 — `SslServerCustomizerTests`
  native crash) independently observed the same `"JKS key integrity check
  failed (wrong password?)"` / `"PKCS#12 MAC verification failed"` warning
  text against a *different* keystore file in a *different* module
  (`spring-boot-jetty`) the same day, suggesting this may not be isolated to
  this one test fixture — worth checking whether both point at the same
  PKCS12 MAC-verification code path.

Neither sub-mechanism was traced to a specific CratonVM source file/line
this session — filed as OPEN with the concrete failing call sites pinned,
root cause left as a grounded hypothesis for the next session with crypto
context to confirm via a standalone `KeyFactory`/`KeyStore` probe against
the exact fixture files under
`apps/spring-boot/core/spring-boot-autoconfigure/src/test/resources/org/springframework/boot/autoconfigure/ssl/`.

**Corroborating evidence (separate triage batch, same rerun):**
`module/spring-boot-tomcat`'s `SslConnectorCustomizerTests` independently
fails 2/8 tests (`sslEnabledProtocolsConfiguration`,
`sslEnabledMultipleProtocolsConfiguration`, both
`AssertionError: Expecting actual not to be null`) after its `.err.log`
shows the exact same `"JKS key integrity check failed (wrong password?)"`
warning text repeating for every one of its 5 SSL-connector sub-configurations,
followed by `ERROR [org.apache.catalina.util.LifecycleBase] Failed to
initialize component [Connector[...]] (org/apache/catalina/LifecycleException:
Protocol handler initialization failed)` for the connectors whose protocol
list the test exercises. Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-tomcat.org.springframework.boot.tomcat.SslConnectorCustomizerTests.err.log`
(and matching `.out.log`). This is a **third** independent module hitting
the identical `"JKS key integrity check failed"` log text the same day
(alongside this doc's own PKCS12 MAC-verification failure and the
`crashfail-20260717-crash-cluster.md` Cluster 2 native crash in
`spring-boot-jetty`) — each against a different keystore test fixture
(`.jks`/`.pkcs12` files are module-local, not shared), which is strong
evidence this is one systemic JKS/PKCS12 keystore-parsing gap in CratonVM's
crypto layer rather than 3 coincidentally-similar, independent bugs. Still
not traced to a specific CratonVM source file/line this session.

## Update 2026-07-17 (large `core/spring-boot` batch triage, 73-class rerun doc pass) — PEM sub-cause pinned more precisely

`core/spring-boot`'s own `PemPrivateKeyParserTests` (12/47 tests) and
`PemSslStoreBundleTests` (1/11 tests) hit the same
`"Missing private key or unrecognized format"`/`PemPrivateKeyParser.parse`
call site as this doc's PEM sub-case, but split cleanly into **two
sub-clusters** that narrow down the "PEM path" hypothesis above
considerably more than "some `KeyFactory` algorithm rejects a valid key":

**Sub-cluster A — unrecognized key algorithm (5 tests, unencrypted keys):**

```
shouldParseXdhPkcs8[1] file="x448.key"       => IllegalStateException: Missing private key or unrecognized format
shouldParseXdhPkcs8[2] file="x25519.key"     => IllegalStateException: Missing private key or unrecognized format
shouldParseEdDsaPkcs8[1] file="ed448.key"    => IllegalStateException: Missing private key or unrecognized format
shouldParseEdDsaPkcs8[2] file="ed25519.key"  => IllegalStateException: Missing private key or unrecognized format
shouldParseTraditionalPkcs8 file="rsa-pss.key" algorithm="RSASSA-PSS" => IllegalStateException: Missing private key or unrecognized format
```

all at `PemPrivateKeyParser.parse(PemPrivateKeyParser.java:220)`. Every
*other* algorithm/curve combination in the same parameterized test
(plain RSA, EC, DSA, traditional PKCS8 RSA/EC) passes — only XDH
(X448/X25519), EdDSA (Ed448/Ed25519), and RSASSA-PSS specifically fail.
This narrows the original doc's "some `KeyFactory.generatePrivate` call
rejects a key it shouldn't" hypothesis to a **specific, small set of
algorithm OIDs** CratonVM's `KeyFactory`/JCA provider does not register or
does not correctly recognize from PKCS8 DER — not a general PEM/DER
decoding defect (plain RSA/EC/DSA all decode fine).

**Sub-cluster B — PBES2 `SecretKeyFactory` not available at all (7 tests, encrypted keys):**

```
shouldParseEncryptedPkcs8[1] file="dsa-aes-128-cbc.key"        algorithm="DSA"    => IllegalStateException: Error loading private key file: PBES2 SecretKeyFactory not available
shouldParseEncryptedPkcs8[2] file="rsa-aes-256-cbc.key"        algorithm="RSA"    => IllegalStateException: Error loading private key file: PBES2 SecretKeyFactory not available
shouldParseEncryptedPkcs8[3] file="prime256v1-aes-256-cbc.key" algorithm="EC"     => IllegalStateException: Error loading private key file: PBES2 SecretKeyFactory not available
shouldParseEncryptedPkcs8[4] file="ed25519-aes-256-cbc.key"    algorithm="EdDSA"  => IllegalStateException: Error loading private key file: PBES2 SecretKeyFactory not available
shouldParseEncryptedPkcs8[5] file="x448-aes-256-cbc.key"       algorithm="XDH"    => IllegalStateException: Error loading private key file: PBES2 SecretKeyFactory not available
shouldNotParseEncryptedPkcs8NotUsingAes()    => AssertionError (expected message "Error decrypting private key", got the PBES2-unavailable message instead)
shouldNotParseEncryptedPkcs8NotUsingPbkdf2() => AssertionError (same)
```

bottoming out at `PemPrivateKeyParser$Pkcs8PrivateKeyDecryptor.decrypt`
(`PemPrivateKeyParser.java:457`) with root cause
`java.lang.SecurityException: PBES2 SecretKeyFactory not available`. This
is a **materially different, more specific** defect than "an algorithm gets
rejected": it fires identically for **every** underlying key
algorithm/curve (DSA/RSA/EC/EdDSA/XDH all hit it the same way once the key
is PBES2-encrypted), which means no PBES2 `SecretKeyFactory` provider is
registered in CratonVM's JCA layer at all — the failure has nothing to do
with which key type is inside the encrypted PKCS8 envelope. `PemSslStoreBundleTests.createWithDetailsWhenHasKeyStoreDetailsCertAndEncryptedKey`
fails the identical way, chained through `PemSslStoreBundle.createKeyStore`
→ `getKeyStore` → the same `PemPrivateKeyParser.parse` →
`Pkcs8PrivateKeyDecryptor.decrypt` call.

This confirms and sharpens (rather than duplicates) this doc's existing PEM
hypothesis: it is not one generic "JCA crypto-provider gap" but at least
**two independent, specific gaps** — (A) missing/unrecognized
XDH/EdDSA/RSASSA-PSS `KeyFactory` support, and (B) no PBES2
`SecretKeyFactory` registered at all — both surfacing through the same
`PemPrivateKeyParser.parse` call site, which is why they were previously
indistinguishable from the outside.

Full logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard1/logs/core_spring-boot.org.springframework.boot.ssl.pem.PemPrivateKeyParserTests.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard1/logs/core_spring-boot.org.springframework.boot.ssl.pem.PemSslStoreBundleTests.out.log`

## Affected classes

| Module | Class |
|---|---|
| `core/spring-boot-autoconfigure` | `org.springframework.boot.autoconfigure.ssl.PropertiesSslBundleTests` |
| `core/spring-boot-autoconfigure` | `org.springframework.boot.autoconfigure.ssl.SslAutoConfigurationTests` |
| `core/spring-boot-autoconfigure` | `org.springframework.boot.autoconfigure.ssl.SslPropertiesBundleRegistrarTests` |
| `core/spring-boot` | `org.springframework.boot.ssl.pem.PemPrivateKeyParserTests` (12/47 tests, added large-batch triage — see sub-clusters A/B above) |
| `core/spring-boot` | `org.springframework.boot.ssl.pem.PemSslStoreBundleTests` (1/11 tests, added large-batch triage — sub-cluster B) |
| `module/spring-boot-tomcat` | `org.springframework.boot.tomcat.SslConnectorCustomizerTests` (2/8 failures — see Corroborating evidence above) |
