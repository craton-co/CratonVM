# `NettyReactiveWebServerFactoryTests.sslWithPemCertificates` — rustls `DecryptError` fatal alert during mutual-TLS handshake

**Status: OPEN — found 2026-07-20 (residual of the reactor-netty hang fix; see
[`reactor-netty-server-startup-hang-FIXED.md`](../../internal/springboot/reactor-netty-server-startup-hang-FIXED.md)).
Hypothesis narrowed but not confirmed.**

## Symptom

`sslWithPemCertificates()` configures the server with client-auth `NEED`, a
PEM certificate+key (`Ssl.setCertificate("classpath:test-cert.pem")` /
`setCertificatePrivateKey("classpath:test-key.pem")`), and a PEM trust
certificate (`ssl.setTrustCertificate("classpath:test-cert.pem")` — same
file). The client presents a PKCS12 identity
(`buildTrustAllSslWithClientKeyConnector("test.p12", "secret")`). The
handshake fails:

```
org.springframework.web.reactive.function.client.WebClientRequestException:
java.io.IOException: rustls process_new_packets: received fatal alert: DecryptError
```

The alert is *received* by the client, i.e. sent by the server-side rustls
stack.

## What's been ruled out (verified this session)

- **Client PKCS12 key material is correct.** `test.p12` contains two
  ambiguous client identities (`spring-boot`, `test-alias`) sharing the same
  subject/issuer DN. Extracted both private keys via
  `X509KeyManager.getPrivateKey(alias)`/`KeyStore.getKey(alias,...)` under
  CratonVM and compared their RSA moduli byte-for-byte against `openssl
  rsa -modulus` on the same file: **both match exactly.** The PKCS12 PBES2
  decryption itself is not the bug.
- **Client alias *selection* is now correct** (see Fix 4 in the FIXED
  doc above) — CratonVM's `chooseClientAlias` now picks `test-alias` first,
  matching live HotSpot JDK 25 behavior on this exact keystore. This *did
  not* fix the failure — the same `DecryptError` reproduces identically
  after the fix, confirming alias order was not (or not solely) the cause.
- **`test-key.pem` and `test-cert.pem` are a matched, valid pair.**
  `test-key.pem` (as delivered in the jar) is a multi-block PEM file — a
  `-----BEGIN PRIVATE KEY-----` block followed by a
  `-----BEGIN CERTIFICATE-----` block, both prefixed with `openssl
  pkcs12`-style `Bag Attributes` comment headers. Its embedded private key
  is (byte-for-byte, confirmed via base64 diff) the *same* key as `test.p12`'s
  `test-alias` entry, and `test-cert.pem` is (byte-for-byte, confirmed via
  `diff`) the *same* certificate as `test.p12`'s `test-alias-cert`/leaf.
  There is no key/cert mismatch in the fixture.
- **Not a JIT bug** — not applicable here (this is a crypto/handshake
  logic question, not re-tested with `--nojit`, but Fix 1-3 above were all
  confirmed JIT-independent and this failure is unrelated to those).

## Leading hypothesis — not confirmed

The server's PEM-sourced credentials are loaded via Spring Boot's
`PemSslStoreBundle.createKeyStore()`, which builds a keystore *purely
in-memory*: `KeyStore.getInstance(...).load(null)` then
`keyStore.setKeyEntry(alias, privateKey, password, chain)` /
`setCertificateEntry(alias, cert)` — **not** a file-based `load(InputStream,
password)`. CratonVM's real-JDK-mode native for this
(`engine_set_key_entry` in `native-builtins/src/keystore.rs`) does two
things: (1) registers the entry into the normal per-`KeyStore`-object
`entries` side-table (the same one `KeyManagerFactory.init()`/
`x509_manager::build_key_manager_state` reads — this path looks correct and
is exercised successfully by other tests), and (2) — per its own doc
comment, citing exactly this in-memory-keystore pattern — additionally
installs the key/chain into `t27_tls::RUNTIME_TLS_IDENTITY`, a **single
process-wide `OnceLock<Mutex<Option<RuntimeTlsIdentity>>>` global**, as a
fallback the native TLS listener consults for "a keystore built purely
in-memory" scenarios (added for an earlier, different bug — see
`docs/known-issues/http-server-cluster-residuals.md`).

This class runs 36 test methods **in one process**, most of which start
their own embedded server with their own (different) certificate. A
single, unscoped, process-wide "the current server identity" singleton is
inherently suspect in that environment — the risk is that some other
test's server setup (or a *later* stage of this same test's own setup: the
trust-store `setCertificateEntry` call happens in a *separate*
`createKeyStore("trust", ...)` invocation right after the key's
`createKeyStore("key", ...)`) races with or overwrites this global between
when it's set and when the rustls listener actually consults it for this
test's `bindNow()`. **Not confirmed** — an isolated single-process repro using plain blocking
`SSLServerSocket`/`SSLSocket`
([`repros/pemcertificates-clientauth/PemClientAuthRepro.java`](../repros/pemcertificates-clientauth/PemClientAuthRepro.java),
bypassing Netty/rustls' async path entirely) hit a *different* failure (a
plain connection timeout, not `DecryptError`), suggesting the bug is
specific to the Netty/`SslProvider.JDK`/`SslHandler` async handshake path
this test actually uses, not to PEM-in-memory-keystore credential loading
in general — that async-specific repro was not completed this session. Run
with `java PemClientAuthRepro <dir-containing-test-cert.pem-test-key.pem-test.p12>`
(extract those three files from
`spring-boot-web-server-4.1.0-SNAPSHOT-test-fixtures.jar`'s
`org/springframework/boot/web/server/reactive/` first).

## Next steps for whoever picks this up

1. Build a **Netty-based** (not plain `SSLServerSocket`) minimal repro
   matching the real test exactly: `reactor.netty.http.server.HttpServer`
   with an in-memory PEM-populated `KeyStore` (via `setKeyEntry`), client
   auth `NEED`, against a `reactor.netty.http.client.HttpClient` using
   `SslProvider.JDK` + a PKCS12 `KeyManagerFactory` — this session's plain
   `SSLServerSocket` repro (`PemClientAuthRepro.java`, left in this
   investigation's scratch notes) hit an unrelated timeout, not the actual
   bug, so it needs redoing with the real transport.
2. Run with `CRATONVM_DBG_TLS_HS=1` (existing debug gate in
   `native-builtins/src/t27_tls.rs`) to trace exactly which identity
   `install_identity_from_der` records and when, versus what the rustls
   listener actually presents at handshake time.
3. If the process-wide-singleton race is confirmed, the real fix is
   scoping `RUNTIME_TLS_IDENTITY` per-`SSLContext`/per-listener instead of
   process-wide — a larger change than this session's fixes, likely
   touching every call site listed at `install_identity_from_der`'s
   definition.
4. Rule in/out cross-test pollution definitively by running
   `sslWithPemCertificates` **alone** (`-ClassList` with just this one
   test method, if the suite runner supports single-method selection, or a
   `@Test`-only extracted copy) versus as part of the full 36-method class
   — this session tested the whole class only.

## 2026-07-20 session: two real bugs found, neither is the whole story

Picked back up as a residual of the h2c HPACK fix
(`h2c-priorknowledge-hpack-headerblock-decode-failure-FIXED.md`). Re-traced
with `CRATONVM_DBG_TLS_AUTH=1 CRATONVM_DBG_TLS_HS=1` end to end. Findings:

- **The `BadSignature`/`DecryptError` alert is genuinely a cryptographic
  signature failure, not a trust-chain rejection** — confirmed by tracing
  engine ids through the full handshake: the SERVER engine (id=15 in one
  captured run) hits `process_new_packets -> Err(Some(InvalidCertificate(
  BadSignature)))` twice while unwrapping the CLIENT's Certificate+
  CertificateVerify flight, and the CLIENT engine (id=14) then receives
  `AlertReceived(DecryptError)` — TLS 1.3's `decrypt_error` alert is exactly
  what rustls sends for a `CertificateVerify` whose signature doesn't
  validate against the just-presented certificate's public key.

- **Found and FIXED a real, separate, definitely-genuine bug**: `KeyStore.
  getKey(alias, password)`-sourced `PrivateKey` objects (the synthetic
  4-field mirror `keystore.rs::engine_get_key` allocates) were never
  registered in `crypto_impl`'s `RSA_KEY_STORE`/`rsa_realkey_map`. `jca/
  signature.rs::extract_key_id_from_key` falls through to reading the
  mirror's field slot 3 as a `crypto_impl` key_id when the identity-hash
  lookup misses — but slot 3 on this mirror is actually a `(store_id,
  alias_hash)` composite for a COMPLETELY different consumer
  (`private_key_der_from_proxy`, used by the native TLS layer/`Key.
  getEncoded()`). Any `Signature.sign()` call using a `KeyStore.getKey()`-
  sourced key therefore signed with whatever unrelated key happened to
  occupy that same numeric id in `RSA_KEY_STORE` (or nothing), producing a
  signature that fails verification even against the CORRECT public key.
  Reproduced in complete isolation, no TLS/sockets/Netty at all
  (`Pkcs12AliasConsistencyProbe.java`: sign with an alias's own key,
  verify with that SAME alias's own certificate's public key — failed on
  CratonVM, passed on real HotSpot, for BOTH of `test.p12`'s aliases).
  **Fixed** in `native-builtins/src/{crypto_impl,keystore}.rs` — added
  `crypto_impl::parse_rsa_private_key_pkcs8` and register the parsed key
  under the mirror's `identityHashCode` in `engine_get_key`, the same
  pattern `jca/key_factory.rs::register_rsa_priv_sign_material` already
  uses for `KeyFactory.generatePrivate` imports.
  **This fix did NOT resolve `sslWithPemCertificates`** — confirmed by
  rerunning the real test after the fix landed (identical `DecryptError`).
  Root cause: the actual TLS handshake signing goes through rustls's own
  native `ring`-backed signer (`rustls::crypto::ring::sign::
  any_supported_type`, fed PEM strings via `t27_tls.rs`), **never** through
  `java.security.Signature`/`crypto_impl` at all. The two code paths are
  completely disjoint; the `Signature` bug was real but irrelevant to this
  test. Worth keeping the fix regardless — it's a live correctness bug for
  any code that does `KeyStore.getKey()` then `Signature.sign()` (JWT
  signing, mTLS via `SSLSocket`/`HttpsURLConnection` rather than
  `SSLEngine`, keycloak-style key verification, etc).

- **Found, attempted, and REVERTED a second, architecturally-real but
  functionally-unsafe fix.** `engine_begin`'s CLIENT branch
  (`t27_tls.rs`, the `SSLEngine`-based path `SSLContext.createSSLEngine()`
  takes — what reactor-netty's `SslProvider.JDK` actually uses) builds its
  `ClientAuthMode` **exclusively** from `state.identity_override`: the ONE
  `(cert_pem, key_pem)` pair `KeyManagerFactory.init`/`engineLoad` collapses
  an entire keystore down to (`keystore.rs`'s `first_key_identity`, always
  the alias that happens to be first in the keystore's own `IndexMap`
  insertion order — for `test.p12`, that's `spring-boot`, NOT the
  `test-alias` identity the server's trust store (`test-cert.pem` ==
  `test.p12`'s `test-alias-cert`, byte-for-byte, per this doc's earlier
  verification) actually recognizes). A **different, already-correct**
  client path (`client_config_for_ssl_context`, used by
  `SSLSocketFactory.createSocket`-based clients) instead prefers a
  `JavaKeyManagerResolver` — a real `KeyManager.chooseClientAlias` call made
  synchronously mid-handshake — whenever the `SSLContext` actually attached
  real `KeyManager[]` objects at `.init()`, via a `km_ctx_key` field
  (`build_engine_client_config_with_identity`'s doc comment spells out
  exactly this priority). `EngineState` (the `SSLEngine` struct) had no
  equivalent field or wiring at all. Added one (`km_ctx_key`, populated
  alongside the existing `trust_managers_ctx_key` in
  `set_engine_trust_ctx_key`) and made `engine_begin`'s client branch prefer
  the resolver when available, mirroring the SSLSocket path's logic exactly.
  **This introduced a regression**: `sslNeedsClientAuthenticationSucceedsWithClientCertificate`
  (previously passing, a single-unambiguous-alias keystore) started failing
  with `CertificateRequired` (server got NO certificate at all) after this
  change — `JavaKeyManagerResolver::resolve()` returned `None` for a case
  the `Fixed` identity path handled correctly. **Reverted** rather than ship
  a fix that trades one broken test for two. The resolver path clearly has
  its own gap/precondition (untested for the `SSLEngine` call context,
  possibly related to `with_active_native_context`'s window not being
  established the same way `createSSLEngine`'s synchronous call site
  expects it, or `root_hint_subjects`/acceptable-issuer filtering rejecting
  a self-signed test cert that the `Fixed` path never checked at all) that
  needs its own investigation before it's safe to prefer over `Fixed` in the
  `SSLEngine` path.

**Net assessment**: the original "process-wide `RUNTIME_TLS_IDENTITY` race"
hypothesis is very likely NOT the actual mechanism (that global is only
consulted as a last-resort fallback; this test's engines both resolved a
real per-`SSLContext` `identity_override` via `ctx_identity`, confirmed via
the `CRATONVM_DBG_TLS_AUTH` trace). The much more likely mechanism is the
alias-ambiguity gap just described: the `SSLEngine` client path presents
`spring-boot`'s (self-consistent, correctly-signed — post-fix — but
untrusted-by-the-server) identity instead of `test-alias`'s. **Still not
proven** — a definitive confirmation would mean instrumenting
`JavaKeyManagerResolver::resolve()`'s actual failure mode for THIS test
(why does it return `None` for `sslNeedsClientAuthenticationSucceedsWithClientCertificate`'s
single-alias keystore, which should be the easy case?) before it's safe to
re-attempt wiring it into the `SSLEngine` path. That diagnostic — not
another guess at the mechanism — is the concrete next step.

## Affected classes

| Module | Class | Test method |
|---|---|---|
| `module/spring-boot-reactor-netty` | `org.springframework.boot.reactor.netty.NettyReactiveWebServerFactoryTests` | `sslWithPemCertificates` |
