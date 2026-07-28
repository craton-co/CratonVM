# Group 01 — JSSE/TLS chain: SSLContext-abort → live HTTPS  (FIXED)

**Status:** FIXED, merged to `dev` (`0660fa27`, commit `82565bbe`).
**Affected:** `TestSsl` and the JSSE variants of every HTTPS test class.
**HotSpot:** PASS.

## Symptom

`TestSsl[JSSE]` aborted immediately at connector init — a sequence of distinct
walls, each surfacing only after the previous was cleared:

1. `runtime error: no KeyManagerFactory SunX509 / no KeyStore JKS / no
   SSLContext TLS implementation in any provider`
2. `IllegalArgumentException: Error creating SSLContext` → `IOException: JKS HMAC
   integrity check failed`
3. `AbstractMethodError: java/security/cert/X509Certificate.checkValidity(...)
   has no Code attribute`
4. `runtime error: no CertificateFactory X.509 implementation in any provider`
5. `NullPointerException: monitorenter in java/net/ServerSocket.getImpl pc=14`
6. `IllegalArgumentException: None of the [ciphers] specified are supported`

## Root causes + fixes (all real bytecode / real provider routing, no stubs)

- **JCA factory layer** (`jca/provider_chain.rs` `seed_sunjsse_services`):
  seeded the real SunJSSE/SUN service tables — KeyManagerFactory.SunX509,
  TrustManagerFactory.PKIX, SSLContext.TLS*, KeyStore.JKS/PKCS12. (earlier,
  `e22ec46c`/`0660fa27`.)
- **JKS HMAC null-password** (`keystore.rs` `load_jks`): real `JavaKeyStore`
  only verifies the integrity HMAC when a password is supplied; a null/empty
  password loads the certs without it (how a truststore loads). Wrapped the
  verify in `if !password.is_empty()`.
- **Abstract X509Certificate** (`keystore.rs` `make_x509_mirror`): the keystore
  returned a bare ABSTRACT `java/security/cert/X509Certificate` whose
  `checkValidity(Date)` has no body → `AbstractMethodError` during SunX509
  KeyManager chain validation. Now builds a real `sun.security.x509.X509CertImpl`
  from the entry DER (full cert API); synthetic mirror only as fallback.
- **CertificateFactory X.509** (`jca/provider_chain.rs`): the real X509CertImpl/
  PKIX validator path calls `CertificateFactory.getInstance("X.509")`; seeded
  `SUN → sun.security.provider.X509Factory`.
- **ServerSocket bind NPE** (`native-io/socket_channel.rs` `ssc_socket`):
  `ServerSocketChannel.socket()` allocated a bare `new java/net/ServerSocket`
  WITHOUT `<init>`, so the `socketLock = new Object()` field initializer never
  ran → `setSoTimeout()`→`getImpl()` `monitorenter` on null. Now returns the real
  `sun.nio.ch.ServerSocketAdaptor.create(channel)` under
  `CRATONVM_REAL_NET_SOCKETS` (mirrors the Socket path).
- **SSLEngine.getSupportedCipherSuites** (`t27_tls.rs`): returned 0 suites →
  "None of the ciphers supported". Now returns the rustls-negotiable suites. Plus
  `native-collections` `retainAll`/`removeAll` null the vacated tail (a values-view
  false-positive made `retainAll` empty the list).

## Result

`testSimpleSsl[JSSE]` now stands up a real HTTPS connector (binds a port),
serves, pauses, stops, and destroys every valve cleanly — the TLS path works end
to end. OpenSSL / OpenSSL-FFM variants fail environmentally (no native OpenSSL —
same as HotSpot).

**Caveat:** `TestSsl` is not confirmed all-green — it is throughput-bound
(group 04): each JSSE method's webapp deploy is slow, so the full class does not
finish within practical timeouts. See [04](../../../known-issues/tomcat/04-embedded-server-throughput-wall-OPEN.md).

## Reproduction

```
cratonvm.exe -Xmx2g -cp <cp> org.junit.runner.JUnitCore \
  org.apache.tomcat.util.net.TestSsl
# env: CRATONVM_REAL_NET_SOCKETS=1 CRATONVM_REAL_AQS=1 CRATONVM_DISABLE_DEFAULT_WATCHDOG=1
# CWD: apps/tomcat
```
