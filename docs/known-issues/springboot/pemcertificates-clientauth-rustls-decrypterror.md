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

## Affected classes

| Module | Class | Test method |
|---|---|---|
| `module/spring-boot-reactor-netty` | `org.springframework.boot.reactor.netty.NettyReactiveWebServerFactoryTests` | `sslWithPemCertificates` |
