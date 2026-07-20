# Embedded Tomcat server occasionally fails `KeyStore.setKeyEntry` with "Private key must be accompanied by certificate chain"

**Status: OPEN — found 2026-07-20**, incidentally, while stress-testing the
fix for
[[reactive-httpcomponents-connector-flaky-tls-engine-identity-and-pool-cipher-leak]]
(`docs/internal/fixed-suite-bugs/`). Not investigated to root cause — this
doc only records what was directly observed.

## Symptom

`reactive.HttpComponentsClientHttpConnectorBuilderTests` (and possibly other
classes that boot an embedded Tomcat with an SSLBundle-configured HTTPS
connector — not checked elsewhere) intermittently fails at **server
startup**, before any client connection is attempted:

```
org.springframework.boot.web.server.WebServerException: Unable to start embedded Tomcat server
  at org.springframework.boot.tomcat.TomcatWebServer.start(TomcatWebServer.java:248)
Caused by: java.lang.IllegalArgumentException: standardService.connector.startFailed
Caused by: org.apache.catalina.LifecycleException: Protocol handler start failed
Caused by: java.lang.IllegalArgumentException: Error creating SSLContext
  at org.apache.tomcat.util.net.AbstractEndpoint.createSSLContext(AbstractEndpoint.java:439)
Caused by: java.lang.IllegalArgumentException: Private key must be accompanied by certificate chain
  at java.security.KeyStore.setKeyEntry(KeyStore.java:1210)
```

Observed rate: 3/45 (~7%) repeated runs of the isolated
`reactive.HttpComponentsClientHttpConnectorBuilderTests`
(`run-spring-boot-suite.ps1 -ClassList <class> -Parallel 1`, same binary,
same test, back to back).

The `java.lang.IllegalArgumentException: Private key must be accompanied by
certificate chain` message is a hardcoded real-JDK `KeyStore.setKeyEntry`
validation (it rejects a call whose `chain` argument is empty/null while a
key is present) — meaning whatever native code backs our keystore
loading/`engineSetKeyEntry` implementation is, on this intermittent path,
handing the real bytecode an empty certificate chain array for an entry
that does have a private key.

Every run (both passing and failing) also logs many
`WARN keystore: JKS key integrity check failed (wrong password?)` lines —
these appear unconditionally, are not correlated with pass/fail in this
sample, and are very likely pre-existing benign noise (probing multiple
keystore format parsers), not related to this bug.

## Not yet determined

- Whether this is a race condition (parsing/caching keystore bytes
  concurrently with something else) or a deterministic-but-rare code path
  (e.g. an edge case in PKCS12/JKS chain assembly triggered by a specific
  entry ordering).
- The relevant native code likely lives in `native-builtins/src/keystore.rs`
  and/or `native-builtins/src/crypto_impl.rs` (not confirmed — a brief look
  found `fn engineGetCertificateChain` and chain-building logic around
  `keystore.rs:864` and `:2152`, not traced further).
- Whether it's specific to the SSLBundle/PKCS12 path this test suite uses,
  or a general keystore-loading hazard.

## Affected classes

| Module | Class | Note |
|---|---|---|
| `module/spring-boot-http-client` | `org.springframework.boot.http.client.reactive.HttpComponentsClientHttpConnectorBuilderTests` | ~7% of repeated runs; failing test varies (seen on both `connectWithSslBundle` and `connectWithSslBundleAndOptionsMismatch`) |
