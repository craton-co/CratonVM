# `SslConnectorCustomizerTests` — CBC-mode TLS cipher suites unavailable (rustls backend limitation)

**Status: OPEN — found 2026-07-19**, while investigating (and disproving) the
"corroborating evidence" hypothesis in
`docs/internal/springboot/ssl-pem-pkcs12-store-parse-failure-cluster-FIXED.md`.

## Symptom

`module/spring-boot-tomcat`'s `SslConnectorCustomizerTests` fails 2/8:
`sslEnabledProtocolsConfiguration`, `sslEnabledMultipleProtocolsConfiguration`
— both `AssertionError: Expecting actual not to be null` on
`sslHostConfig.getEnabledProtocols()`.

## Root cause (confirmed via direct reproduction)

Both tests request cipher suites including
`TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA256`. This surfaced as a total
`sslHostConfig.getEnabledProtocols() == null` because Tomcat's own
`LifecycleBase`/JUL logging swallows the real cause down to a one-line,
stack-trace-free summary
(`ERROR [...] Failed to initialize component [...] (org/apache/catalina/
LifecycleException: Protocol handler initialization failed)`); a from-scratch
reproduction that replicates `Connector.initInternal()`'s exact
adapter-setup sequence (`CoyoteAdapter` + `protocolHandler.setAdapter`) and
calls `protocolHandler.init()` directly, catching + printing the full cause
chain, surfaced the actual exception:

```
java.lang.IllegalArgumentException: None of the [ciphers] specified are supported by the SSL engine : [[TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA256]]
    at org.apache.tomcat.util.net.SSLUtilBase.getEnabled(SSLUtilBase.java:164)
```

CratonVM's TLS engine is backed by `rustls` (`native-builtins/Cargo.toml`:
`rustls = { version = "0.23", features = ["ring", "std", "tls12", "logging"] }`).
rustls **never implements CBC-mode cipher suites** — a permanent, documented,
intentional upstream design decision (only modern AEAD suites: AES-GCM and
ChaCha20-Poly1305). `SSLEngine.getSupportedCipherSuites()`/`getEnabled()`
therefore can never report `TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA256` (or any
other CBC suite) as supported, so Tomcat's `SSLUtilBase.getEnabled()` — which
intersects the requested list against the engine's supported set and throws
if the intersection is empty — always fails for a cipher list containing
*only* CBC suites.

This was investigated because the original doc's "Corroborating evidence"
section speculated this shared the same root cause as the PKCS#12 MAC/PBES2
keystore-parsing bugs fixed there (all three logged the visually-similar
`"JKS key integrity check failed"`/`"PKCS#12 MAC verification failed"`
warning text). It does not: `KeyStore`/`KeyManagerFactory`/`SSLContext`
construction all work correctly in isolation for this exact fixture
(`test.jks`) once replicated faithfully (`ks.getKey`, `ksUsed.setKeyEntry`,
`kmf.init`, `sslContext.getSupportedSSLParameters()` all succeed and return
the right identity/certs) — the JKS integrity-check warnings logged during
these tests are harmless noise from CratonVM's per-entry eager-decrypt-at-
load-time attempt (using the *store* password) against a private key
protected with a *different*, per-entry key password; the real failure is
several layers downstream, in cipher-suite negotiation.

## Why this isn't trivially fixable

Implementing CBC-mode TLS cipher suites would require either forking rustls
or switching TLS backends entirely (e.g. to a native OpenSSL binding, which
CratonVM has as an optional dependency — `native-tls` — but is not currently
the active engine). Given CBC-mode suites are actively being deprecated
industry-wide in favor of AEAD ciphers, and rustls's exclusion is a
deliberate security-positive choice, this is filed as a known,
environment-level limitation (similar in kind to the existing
"HotSpot's openssl-absent failures" environmental gaps already tracked
elsewhere) rather than a bug to fix.

## Affected classes

| Module | Class | Method |
|---|---|---|
| `module/spring-boot-tomcat` | `org.springframework.boot.tomcat.SslConnectorCustomizerTests` | `sslEnabledProtocolsConfiguration`, `sslEnabledMultipleProtocolsConfiguration` |

Any other Spring Boot (or general) test that requests a CBC-only cipher list
against CratonVM's rustls-backed TLS engine will hit the identical failure
shape.
