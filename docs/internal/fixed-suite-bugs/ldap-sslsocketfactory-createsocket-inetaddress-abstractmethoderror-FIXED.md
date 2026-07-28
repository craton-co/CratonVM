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

## Regression note (2026-07-23) — same test/method fails again, different mechanism

**`EmbeddedLdapAutoConfigurationTests.whenSslBundleIsConfiguredLdapsListenerIsConfigured`
fails again** in the `RunName=craton-rerun-20260723` rerun (`tests=17
failed=1`), the exact same method this doc validated as fixed 2026-07-18.
The symptom is **not** the original `AbstractMethodError` — it's a private-key
parsing failure starting the LDAPS listener against the same `test.jks`
fixture:

```
Caused by: org.springframework.beans.BeanInstantiationException: Failed to instantiate [com.unboundid.ldap.listener.InMemoryDirectoryServer]: Factory method 'directoryServer' threw exception with message: An error occurred while attempting to start listener 'ldaps':  IOException(ServerConfig with_single_cert failed: unexpected error: failed to parse private key as RSA, ECDSA, or EdDSA; platform TLS fallback: Встречено неверное значение тега ASN1. (os error -2146881269)), ldapSDKVersion=7.0.4
```

Log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard4/logs/module_spring-boot-ldap.org.springframework.boot.ldap.autoconfigure.embedded.EmbeddedLdapAutoC-3c6a8c4df0c6.out.log`.

The error text ("failed to parse private key as RSA, ECDSA, or EdDSA")
explicitly does not list DSA among the types the TLS layer tried — and this
doc's own "Resolution" section says the fix "used the scoped OpenSSL
legacy-DSA path for the explicitly trusted, historical Spring test
certificate" (i.e. `test.jks`'s server key is understood to be DSA,
requiring a special-cased path outside rustls's normal RSA/ECDSA/EdDSA
handling). The current error additionally shows a **different** fallback
mechanism than "OpenSSL legacy-DSA" — a "platform TLS fallback" that itself
fails with a Windows CryptoAPI ASN.1 tag error (`os error -2146881269` =
`CRYPT_E_ASN1_BADTAG`), which this doc never mentions. Two explanations are
plausible, neither confirmed this session (no source diff was done against
the 2026-07-18 fix commit):

- The scoped OpenSSL legacy-DSA path this doc describes was removed,
  superseded, or broken by later `dev` changes, and the "platform TLS
  fallback" now reached in its place does not correctly convert/parse the
  same DSA key material.
- The fallback chain itself is unchanged but was always reachable for this
  exact key and previously happened to succeed; something else (an
  unrelated Windows CryptoAPI or key-encoding change) now makes it fail.

**Not treated as a fresh unrelated bug** given it's the identical
class+method+fixture this doc already root-caused and fixed once — flagged
as a likely regression in the DSA/legacy-key handling path this doc
describes. Whoever picks this up should start by diffing
`native-builtins/src/keystore.rs`/`x509_manager.rs` (and wherever the
"platform TLS fallback" lives) against the state at the 2026-07-18 fix
commit referenced above, rather than re-diagnosing from scratch.
