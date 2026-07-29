# craton-rerun-20260728 small residuals — 3 independent single-class findings

**Status: OPEN — found 2026-07-28**

These three findings share no common root cause — they are bundled in one
doc, each as its own clearly-delineated case, following this codebase's
established convention for small independent residuals from the same
triage batch (see e.g. `mongodb-3class-residuals-20260723-FIXED.md`,
`core-spring-boot-uncategorized-residuals-20260723-FIXED.md`). Do not treat
"Case 1", "Case 2", "Case 3" below as related to each other.

## Case 1 — `EmbeddedLdapAutoConfigurationTests.whenSslBundleIsConfiguredLdapsListenerIsConfigured`: no Windows TLS fallback for a legacy DSA key

| Module | Class | Test method |
|---|---|---|
| `module/spring-boot-ldap` | `org.springframework.boot.ldap.autoconfigure.embedded.EmbeddedLdapAutoConfigurationTests` | `whenSslBundleIsConfiguredLdapsListenerIsConfigured` |

```
Caused by: LDAPException(resultCode=82 (local error), errorMessage='An error occurred while attempting to start listener 'ldaps':  IOException(ServerConfig with_single_cert failed: unexpected error: failed to parse private key as RSA, ECDSA, or EdDSA; platform TLS fallback: Встречено неверное значение тега ASN1. (os error -2146881269)), ldapSDKVersion=7.0.4, ...')
       com.unboundid.ldap.listener.InMemoryDirectoryServer.startListening(InMemoryDirectoryServer.java:444)
       org.springframework.boot.ldap.autoconfigure.embedded.EmbeddedLdapAutoConfiguration.directoryServer(EmbeddedLdapAutoConfiguration.java:116)
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260728/shard1/logs/module_spring-boot-ldap.org.springframework.boot.ldap.autoconfigure.embedded.EmbeddedLdapAu-3c6a8c4df0c6.err.log` (context init trace) /
`...embedded.EmbeddedLdapAu-3c6a8c4df0c6.out.log` (test failure)

**Root cause (confirmed):** the test's fixture keystore
(`apps/spring-boot/module/spring-boot-ldap/src/test/resources/org/springframework/boot/ldap/autoconfigure/embedded/test.jks`)
contains a **1024-bit DSA key** (`keytool -list -v` confirms: "Signature
algorithm name: SHA1withDSA (weak)", "Subject Public Key Algorithm:
1024-bit DSA key (weak)"). `EmbeddedLdapAutoConfiguration.createListenerConfig`
builds the LDAPS listener via `sslBundle.createSslContext()` →
`SSLContext.getServerSocketFactory()` → UnboundID's own
`SSLServerSocketFactory.createServerSocket(...)`, which CratonVM handles in
`native-builtins/src/t27_tls.rs`'s `create_ssl_server_socket`
(lines 4187-4252):

1. It first tries `build_server_config_single_cert(...)` (rustls). rustls
   has **no DSA `SigningKey` implementation at all** — confirmed by this
   same file's own comment on `legacy_dsa_acceptor`
   (`t27_tls.rs:4153-4166`): "Originally written for DSA (rustls has no
   DSA `SigningKey` at all)". This attempt fails with the generic
   `"failed to parse private key as RSA, ECDSA, or EdDSA"` rustls error —
   exactly the first half of the observed message.
2. On failure, it falls back based on platform (`t27_tls.rs:4230-4250`):
   - `#[cfg(unix)]`: `legacy_dsa_acceptor` — an OpenSSL-backed acceptor
     built specifically to accept DSA and other legacy identities rustls
     refuses (`set_security_level(0)`).
   - `#[cfg(not(unix))]` (this Windows host): `native_tls::Identity::
     from_pkcs8(...)`  → `native_tls::TlsAcceptor::new(...)`, i.e. Windows
     SChannel via the `native-tls`/`schannel` crates. This path has **no
     DSA-specific accommodation** — SChannel's PKCS#8 key import rejects
     the DSA key, surfacing as the observed
     `"platform TLS fallback: ... Встречено неверное значение тега ASN1."`
     (Russian for "invalid ASN1 tag value encountered" —
     `CRYPT_E_ASN1_BADTAG`/`-2146881269`, a Windows CryptoAPI error from
     trying to decode a DSA key blob it doesn't recognize).

**Conclusion:** on Windows, there is no working TLS fallback for a legacy
DSA server identity — the OpenSSL-backed `legacy_dsa_acceptor` that solves
this exact problem on Unix (`#[cfg(unix)]`) has no equivalent in the
`#[cfg(not(unix))]` branch. This is a genuine Windows/Unix platform-parity
gap, not a rustls limitation being hit for the first time (rustls's DSA gap
is already known and already has a working Unix fallback).

**Fix direction (not implemented/verified here):** either give the Windows
branch its own legacy/DSA-tolerant acceptor (if a suitable Windows-native or
pure-Rust TLS backend with DSA support is available), or explicitly
document this as a permanent Windows-only limitation (matching the existing
posture for rustls's CBC/TLS-1.1/DHE gaps in
`docs/internal/fixed-suite-bugs/rustls-cbc-cipher-suites-not-supported.md`
and `rustls-tls11-protocol-not-supported.md`) if reviving DSA support on
Windows isn't worth the effort.

## Case 2 — `Saml2RelyingPartyAutoConfigurationTests`: plain-HTTP `MockWebServer.close()` times out waiting for a still-open keep-alive connection

| Module | Class | Failing methods |
|---|---|---|
| `module/spring-boot-security-saml2` | `org.springframework.boot.security.saml2.autoconfigure.Saml2RelyingPartyAutoConfigurationTests` | 6 of 21 (`autoconfigurationWhenMetadataUrlAndPropertyPresentShouldUseBindingFromProperty`, `autoconfigurationWhenMultipleProvidersAndNoSpecifiedEntityId`, `autoconfigurationWhenMultipleProvidersAndSpecifiedEntityId`, `autoconfigurationShouldUseBindingFromMetadataUrlIfPresent`, `signRequestShouldApplyIfMetadataUriIsSet`, `autoconfigurationShouldQueryAssertingPartyMetadataWhenMetadataUrlIsPresent`) |

```
=> java.lang.AssertionError: Gave up waiting for queue to shut down
       java.lang.AssertionError.<init>(AssertionError.java:76)
       mockwebserver3.MockWebServer.close(MockWebServer.kt:417)
       okhttp3.mockwebserver.MockWebServer.close(MockWebServer.kt:184)
       org.springframework.boot.security.saml2.autoconfigure.Saml2RelyingPartyAutoConfigurationTests.autoconfigurationWhenMetadataUrlAndPropertyPresentShouldUseBindingFromProperty(Saml2RelyingPartyAutoConfigurationTests.java:203)
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260728/shard2/logs/module_spring-boot-security-saml2.org.springframework.boot.security.saml2.autoconfigure.Sam-94eb3b21e8c2.out.log`

This is the same `AssertionError: Gave up waiting for queue to shut down`
symptom already root-caused and fixed once for
`CloudFoundryReactiveActuatorAutoConfigurationTests.skipSslValidation` in
`docs/internal/fixed-suite-bugs/springboot/spring-boot-cloudfoundry-mockwebserver-taskqueue-shutdown-FIXED.md`
— but **that fix does not cover this case**. That doc's root cause: OkHttp's
`MockWebServer.close()` (confirmed via `javap` disassembly of
`mockwebserver3-5.1.0.jar`'s `close()`, this session, matching that doc's
description) closes only the *listening* `ServerSocket`; it never force-
closes already-accepted per-connection `Socket`s, so `close()` can only
finish once each connection's own server-side keep-alive read naturally
unblocks (client closes, or a read timeout fires). That doc's fix added a
3-second read timeout specifically inside
`rustls_server_wrap_existing_socket` (`native-builtins/src/t27_tls.rs`) —
the **TLS** server-wrap path.

`Saml2RelyingPartyAutoConfigurationTests` never calls `.useHttps()` on its
`MockWebServer` instances (confirmed: `grep -n useHttps` on the test source
returns nothing; all 5 `new MockWebServer()` usages are plain HTTP). Its
server-side accept/read therefore goes through the **plain** (non-TLS)
socket path (`native-builtins/src/plain_socket.rs`), which the TLS-only fix
does not touch. `plain_socket.rs` sets no default `SO_TIMEOUT` unless Java
code explicitly calls `Socket.setSoTimeout()` — matching real JDK's default
blocking-forever-without-a-timeout behavior, which is *not* itself a bug
(OkHttp's `MockWebServer` never sets one either, by design).

**Not root-caused further this session** (no build/test execution
performed). Since the plain-socket accept/read path correctly matches real
JDK's own no-default-timeout behavior, the actual gap must be on the far
side of the same interaction: whatever client keeps this connection open
past the point real HotSpot would have closed it. The metadata-fetching
code path here is OpenSAML's `HTTPMetadataResolver` (backed by Apache
HttpClient 5, not Spring's `RestTemplate`/`WebClient` used elsewhere in this
suite) — worth checking whether its connection pool is not releasing/closing
connections promptly under CratonVM the way it does under HotSpot, as the
next step.

## Case 3 — `ConfigDataEnvironmentPostProcessorIntegrationTests`: `SimpleApplicationEventMulticaster.invokeListener` NPEs on its own `null`-checked `errorHandler` field

| Module | Class | Test methods |
|---|---|---|
| `core/spring-boot` | `org.springframework.boot.context.config.ConfigDataEnvironmentPostProcessorIntegrationTests` | `runWhenHasNonOptionalImportThrowsException`, `runWhenConfigLocationHasNonOptionalMissingClasspathDirectoryThrowsLocationNotFoundException` |

```
=> java.lang.AssertionError:
Expecting actual throwable to be an instance of:
  org.springframework.boot.context.config.ConfigDataResourceNotFoundException
but was:
  java.lang.NullPointerException: Cannot invoke "org.springframework.util.ErrorHandler.handleError(java.lang.Throwable)" because "errorHandler" is null
	at org.springframework.context.event.SimpleApplicationEventMulticaster.invokeListener(SimpleApplicationEventMulticaster.java:169)
	at org.springframework.context.event.SimpleApplicationEventMulticaster.multicastEvent(SimpleApplicationEventMulticaster.java:151)
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260728/shard1/logs/core_spring-boot.org.springframework.boot.context.config.ConfigDataEnvironmentPostProcessor-a55b1811c6d7.out.log`

Both failing methods expect a specific `ConfigData*Exception` to propagate
out of `run(...)` when an event listener rejects a bad config-data import —
instead, the listener dispatch itself blows up with an NPE, masking the
real exception.

**Root cause (confirmed via javap disassembly of the real jar):**
`SimpleApplicationEventMulticaster.invokeListener`
(`spring-context-7.1.0-SNAPSHOT.jar`) is:

```
0: aload_0
1: invokevirtual  getErrorHandler:()Lorg/springframework/util/ErrorHandler;
4: astore_3
5: aload_3
6: ifnull  31                          // if errorHandler == null, skip the try/catch entirely
9: aload_0 / aload_1 / aload_2
12: invokevirtual doInvokeListener(...)   // else: call inside a try
15: goto 37
Exception table: 9..15 -> 18 (catches Throwable)
18: astore 4                          // exception handler entry
20: aload_3                          // re-read errorHandler (local slot 3)
21: aload 4
23: invokeinterface ErrorHandler.handleError(Throwable)V   // <-- NPEs here
28: goto 37
31: aload_0 / aload_1 / aload_2
34: invokevirtual doInvokeListener(...)  // errorHandler == null path: no try/catch, exception propagates directly
37: return
```

Real Spring's default `SimpleApplicationEventMulticaster` never sets an
`errorHandler` unless the application explicitly configures one — these
tests use the default, so `errorHandler` is `null`, and the `ifnull` branch
at offset 6 is supposed to skip straight to the no-try/catch call at offset
31, letting the listener's real exception propagate untouched (which is
exactly what the tests expect).

The observed trace shows execution instead reached the **catch handler**
(offset 18-28) — meaning the `ifnull` branch was *not* taken even though
`errorHandler` is null — and then NPE'd trying to actually use it. Since
local slot 3 (`errorHandler`) must have read as non-null at offset 6 (for
the `ifnull` branch to fall through into the try) but then reads as null at
offset 20 (the same local slot, in the exception handler), this is a
**local-variable-value-across-an-exception-handler-boundary** bug — the
exact bug *family* already found, and fixed for a different shape, in this
codebase's `try`/`catch` handling (see
`reference_handler_liveness_needs_per_pc_union.md`, "FIXED 4d1795a07": "a
block-level exception edge LOOKS right but intra-block defs after the throw
site still kill the local ⇒ handler reads 0"). This specific instance (a
short, non-loop `try` region with the guard read *before* entering the try,
not after) was not found in any existing doc and is not confirmed to be a
residual of that exact fix or a new instance of the same class of bug —
flagged as the strongest lead given the identical general shape ("handler
entry reads back a locally-defined value differently than the code
immediately before the try region did").

**Not fully bisected**: whether the `ifnull` comparison itself
mis-evaluates a null reference as non-null, or whether local slot 3's value
is genuinely being corrupted somewhere between offset 6 and offset 20 (e.g.
by JIT-compiled-frame interaction with the `doInvokeListener` call and its
exception unwind), was not determined this session — either would produce
the observed symptom, and disambiguating needs a debugger attached mid-call
or a targeted `CRATONVM_DBG_*` trace on this exact method.

## Confirming/refuting each case

- **Case 1**: build a minimal reproduction that starts a
  `javax.net.ssl.SSLServerSocket` from a DSA-keyed `SSLContext` on Windows
  and confirms the same `native_tls`/SChannel rejection outside the full
  LDAP/Spring stack.
- **Case 2**: trace Apache HttpClient 5's connection-pool release timing for
  the OpenSAML `HTTPMetadataResolver` path under CratonVM vs. real HotSpot.
- **Case 3**: attach a debugger (or add a `CRATONVM_DBG_*` trace) to
  `SimpleApplicationEventMulticaster.invokeListener`'s bytecode execution
  and inspect local slot 3's value at offsets 6 and 20 directly.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-ldap` | `org.springframework.boot.ldap.autoconfigure.embedded.EmbeddedLdapAutoConfigurationTests` |
| `module/spring-boot-security-saml2` | `org.springframework.boot.security.saml2.autoconfigure.Saml2RelyingPartyAutoConfigurationTests` |
| `core/spring-boot` | `org.springframework.boot.context.config.ConfigDataEnvironmentPostProcessorIntegrationTests` |
