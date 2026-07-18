# Embedded LDAP SSL bundle `SocketFactory` abstract-method error — fixed

**Status: FIXED — 2026-07-18**

## Symptom

Spring Boot's `EmbeddedLdapAutoConfigurationTests` failed
`whenSslBundleIsConfiguredLdapsListenerIsConfigured` with:

```
java.lang.AbstractMethodError:
  javax/net/SocketFactory.createSocket(Ljava/net/InetAddress;I)Ljava/net/Socket;
```

After the initial overload bridge, the real JKS-backed LDAPS route exposed
additional residuals: a listener that read as closed due to JDK field-layout
collisions, a client connector thread that lost its SSL-context trust roots,
and legacy DSA fixture compatibility failures.

## Resolution

- Added complete TLS `SSLSocketFactory` InetAddress-overload coverage and
  receiver-aware server-factory dispatch.
- Kept synthetic `SSLServerSocket` lifecycle data in a side table instead of
  raw JDK object fields, so `accept`, `isClosed`, and `close` use one stable
  listener identity.
- Preserved explicit SSLContext trust anchors when UnboundID hands its socket
  factory to its background connector thread.
- Used the scoped OpenSSL legacy-DSA path for the explicitly trusted,
  historical Spring test certificate, retaining chain verification while
  matching JSSE's socket-level hostname policy and fixture compatibility.
- Suppressed the impossible internal `String.setOption(int,Object)` call
  caused by the host `Socket.impl` layout being read from a synthetic TLS
  socket.

## Regression coverage

`vm/tests/resources/cratonvm/UnboundIdLdapsJks.java` reproduces the actual
Spring fixture arrangement: JKS `test.jks`, TLS 1.2, UnboundID LDAPS listener,
and `getConnection("LDAPS")`.

Validated with the real Linux JDK 25 runtime on 2026-07-18:

- the standalone JKS/UnboundID probe passed under JIT and `--nojit`;
- Spring Boot's 17-test LDAP class passed the SSL-bundle LDAPS target under
  JIT and `--nojit`;
- the only remaining class failure was the pre-existing unrelated
  `testQueryEmbeddedLdap` JNDI initial-context failure.
