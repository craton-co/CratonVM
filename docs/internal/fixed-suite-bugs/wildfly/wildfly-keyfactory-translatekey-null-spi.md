# KeyFactory.translateKey() / getKeySpec() NPE on synthetic KeyFactory's null `spi` field

Status: RESOLVED — fixed 2026-07-07
Severity: was Medium (broke self-signed X.509 certificate generation via Elytron/WildFly-security helpers; affected any code calling `KeyFactory.translateKey` or `KeyFactory.getKeySpec`)
First confirmed: 2026-07-07, Azure worktree `test/wildfly-full-suite-20260707`
Fixed: 2026-07-07, branch `fix/keyfactory-translatekey-20260707`

## Symptom

Any code path that calls `java.security.KeyFactory.translateKey(Key)` on a CratonVM-backed `KeyFactory` threw:

```text
java.lang.NullPointerException: Cannot invoke "java.security.KeyFactorySpi.engineTranslateKey(java.security.Key)" because "this.spi" is null
	at java.security.KeyFactory.translateKey(KeyFactory.java:475)
```

Concretely this broke `org.wildfly.security.x500.cert.X509CertificateBuilder.getTBSBytes()` →
`SelfSignedX509CertificateAndSigningKey.Builder.build()`, i.e. **generating a self-signed X.509
certificate failed outright** under CratonVM. This is a pure client-side crypto operation with
no server/network/container involved.

## Confirmed CratonVM-specific via HotSpot A/B (same class, same harness, same command)

`org.jboss.as.test.manualmode.ejb.client.outbound.connection.security.ElytronRemoteOutboundConnectionTestCase`
(module `testsuite/integration/manualmode`), which calls
`org.jboss.as.test.integration.security.common.Utils.createKeyStoreTrustStore()` in its `@Before` setup
(`prepareSSLFiles`, line 207), building an RSA-1024 / SHA256withRSA self-signed cert via
`SelfSignedX509CertificateAndSigningKey.builder()...build()`:

- **Real HotSpot**: `OK` — 22/22 test methods pass, wall time 79s.
- **CratonVM** (jit-real mode, identical class/command), pre-fix: `FAIL` — `Errors: 2`, both failing in
  the `@Before` setup with the `KeyFactory.translateKey` NPE above, before any test method body ran.

## Root cause

`../../../../native-builtins/src/jca/key_factory.rs` intercepts `KeyFactory`/`KeyPairGenerator` with a native
"3-field synthetic" object layout (`algo_idx`, `key_size`, `state` — see the module doc comment at the
top of the file) instead of a real JDK `Provider`/`Spi`-backed object, to avoid an unrelated real-bytecode
NPE in `sun.security.jca.GetInstance.getServices()`. This shim registers `getInstance`,
`initialize`, `generateKeyPair`, `getAlgorithm`, etc., but did **not** register a native override for
`KeyFactory.translateKey` (nor, it turned out, for the sibling method `KeyFactory.getKeySpec`).

Because the shim never sets the real JDK `KeyFactory.spi` field, any caller that reached either
*unshimmed* method fell through to the real JDK bytecode, which unconditionally does
`return spi.engineTranslateKey(key);` / `return spi.engineGetKeySpec(key, keySpec);` — and `spi` is
`null` on a synthetic-shimmed `KeyFactory`, so both NPE'd.

This shim is the DEFAULT path (active unless `CRATONVM_REAL_JCA` is set — `real_jca_mode()` in
`native-builtins/src/lib.rs:1205`), so this affected normal/default usage.

## Fix

Added two native overrides in `../../../../native-builtins/src/jca/key_factory.rs`, registered alongside the
existing `KeyFactory` natives (still gated by the same `if crate::real_jca_mode() { return; }` at the
top of `register()`, so nothing changes when real-JCA mode is opted into):

- **`kf_translate_key`** (`KeyFactory.translateKey(Key)`): compares the passed key's
  `getAlgorithm()` against the `KeyFactory`'s own algorithm. Same algorithm → pass the key through
  unchanged (our `generateKeyPair`/`generatePublic`/`generatePrivate` already hand back real,
  provider-native key objects — `RSAPrivate/PublicKeyImpl`, `EC*Impl`, BouncyCastle's own classes —
  so there is no foreign representation to re-derive, matching the "already this provider's own impl
  class" fast path every real `KeyFactorySpi.engineTranslateKey` takes). Mismatched algorithm, or a
  null key → throws `InvalidKeyException`, matching real JDK's contract exactly (verified: real JDK
  also throws `InvalidKeyException` for both cases, only the message text differs).
- **`kf_get_key_spec`** (`KeyFactory.getKeySpec(Key, Class)`): discovered as a second instance of the
  *same* gap while verifying the `translateKey` fix against the real WildFly Elytron code path (see
  Verification below) — `X509CertificateBuilder.getTBSBytes()` calls
  `keyFactory.getKeySpec(publicKey, X509EncodedKeySpec.class)` right after the `translateKey` call, and
  it NPE'd identically. Supports `X509EncodedKeySpec` / `PKCS8EncodedKeySpec` (from the key's own
  encoding), `RSAPublicKeySpec` / `RSAPrivateKeySpec` (from the key's own BigInteger accessors), and
  `ECPublicKeySpec` / `ECPrivateKeySpec` (from the key's own `getW()`/`getS()` + `getParams()`).
  Anything else throws `InvalidKeySpecException`, matching real JDK's fallback for an unsupported spec
  class.

## Verification

1. **Minimal repro from this doc** (RSA `translateKey`) — fixed; returns a real
   `sun.security.rsa.RSAPrivateKeyImpl`, matching real HotSpot's return type family.
2. **EC `translateKey`**, **cross-algorithm mismatch** (RSA `KeyFactory.translateKey(ecKey)`), and
   **null-key** — all throw `InvalidKeyException`, matching real HotSpot's exception type (message text
   differs, which is not part of the API contract).
3. **`getKeySpec`** with `RSAPublicKeySpec`, `X509EncodedKeySpec` (RSA), `ECPublicKeySpec` (EC), and an
   unsupported spec class (`DSAPublicKeySpec`, expected to throw `InvalidKeySpecException`) — all match
   real HotSpot output.
4. **Real Elytron `SelfSignedX509CertificateAndSigningKey.Builder.build()`**, driven directly against
   the actual `wildfly-elytron-x500-cert-2.4.2.Final.jar` (extracted from the WildFly 32.0.1.Final
   distribution, no WildFly server/checkout needed) — was throwing `IllegalArgumentException:
   ELY10018: Failed to generate self-signed X.509 certificate` (caused by the `translateKey` NPE, then
   after that fix alone, by the `getKeySpec` NPE); now succeeds, output matches real HotSpot exactly
   (`OK built self-signed cert: CN=test key=RSA`).
5. **The actual WildFly test class**, `ElytronRemoteOutboundConnectionTestCase`, run through the real
   Maven/Surefire harness via `apps/wildfly-suite-runner`'s Linux driver
   (`run-suite-linux.sh run --category all --jit on --jdk real --only
   'ElytronRemoteOutboundConnectionTestCase'`) against the fixed binary: the `@Before` setup (where the
   2 errors previously occurred, before any test method ran) now completes successfully and the test
   proceeds deep into actual test-method execution (management-client connections, elytron subsystem
   operations) — a categorically later failure point than before. See
   [[wildfly-elytron-remoting-segfault-post-keyfactory-fix]] for what happens next (a **new, distinct**
   bug this fix uncovered, now that the class gets far enough to reach it — not a regression from this
   fix, and out of scope for it).

## Related

Unrelated to `wildfly-domain-heap-corrupt-value-timeout`, `wildfly-domain-managed-servers-timeout`, and
`wildfly-surefire-empty-class-goodbye` — no domain-mode, no zero-test-class handshake involved here.
See [[wildfly-elytron-remoting-segfault-post-keyfactory-fix]] for the new issue this fix's own
verification run uncovered one layer deeper in the same test class.
