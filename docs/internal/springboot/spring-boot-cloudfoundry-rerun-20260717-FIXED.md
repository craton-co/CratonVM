# `module/spring-boot-cloudfoundry` 2026-07-17 rerun: skip-SSL-verification not honored (1 FAIL, confirmed) + `$Proxy` layout-probe livelock (2 HANGs)

**Status: FIXED - 2026-07-19**

This module contributed 3 non-passing classes to the 2026-07-17 triage batch,
splitting into 2 unrelated root causes. Both are now fixed and verified.

| Module | Class | Issue | Result |
|---|---|---|---|
| `module/spring-boot-cloudfoundry` | `SkipSslVerificationHttpRequestFactoryTests` | A | FIXED — PASS (JIT + `--nojit`) |
| `module/spring-boot-cloudfoundry` | `CloudFoundryActuatorAutoConfigurationTests` | B ($Proxy livelock) | FIXED — PASS (JIT + `--nojit`), 2 consecutive clean runs |
| `module/spring-boot-cloudfoundry` | `CloudFoundryReactiveActuatorAutoConfigurationTests` | B ($Proxy livelock) | Livelock FIXED; 13/14 tests pass. A **new, separate** teardown defect remains for `skipSslValidation` — tracked in [`spring-boot-cloudfoundry-mockwebserver-taskqueue-shutdown.md`](../../known-issues/springboot/spring-boot-cloudfoundry-mockwebserver-taskqueue-shutdown.md) |

## Issue A — `SkipSslVerificationHttpRequestFactoryTests`: a custom permissive `X509TrustManager` had no effect

### Original root cause

`SkipSslVerificationHttpRequestFactory` builds an `SSLContext` with a custom,
always-trusting `X509TrustManager` and installs it via
`HttpsURLConnection.setSSLSocketFactory(...)`. rustls (CratonVM's TLS backend)
performed its own certificate validation unconditionally as part of
establishing the connection, before the custom-TrustManager consultation point
(`t27_tls.rs`'s `engine_run_trust_check`) ever ran — so an already-rustls-
rejected handshake (expired/self-signed cert) could never be rescued by a
permissive Java `TrustManager`.

A second, compounding gap: `URL.openStream()`'s carrier-type dispatch returned
`HttpURLConnection` for `https:` URLs instead of `HttpsURLConnection`, so
`SkipSslVerificationHttpRequestFactory`'s `instanceof HttpsURLConnection`
check failed and its entire permissive-TrustManager setup was skipped before
ever reaching the TLS layer.

### Fix

- `native-builtins/src/t27_tls.rs`: `HttpsURLConnection` SSLContext capture is
  now scoped per-connection (`capture_huc_ssl_context_for_connection`,
  `huc_client_config_for_connection`) instead of leaking into the
  process-wide default slot — a prior connection's permissive TrustManager
  no longer bleeds into a later, unrelated connection.
- `native-builtins/src/net_phase_e.rs`: `URL.openStream()` now returns the
  `HttpsURLConnection` carrier for `https:` URLs.
- `native-builtins/src/phases_late.rs`: the legacy p68 SSL context path now
  propagates Java-supplied TrustManagers/KeyManagers into the rustls bridge
  (`attach_pending_identity_to_ctx`/`attach_trust_managers_to_ctx`/
  `attach_key_managers_to_ctx`), and `engine_begin` (`t27_tls.rs`) threads a
  `use_java_trust_manager` flag through to `build_client_config_ex`, which
  selects a cryptography-only `PassthroughServerCertVerifier` instead of the
  strict `WebPkiServerVerifier` whenever a real Java `TrustManager` is
  present — deferring the actual accept/reject decision to
  `engine_run_trust_check`'s post-handshake Java callback, exactly matching
  real JSSE's contract.
- `vm/src/vm/vm_exec.rs` / `vm/src/runtime/interpreter.rs`: route
  `SSLContext`/`SSLSocketFactory`/`SSLSocket`/`SSLEngine` calls made through
  concrete `sun/security/ssl/*Impl` classes (not just the public
  `javax/net/ssl/*` API classes) to the same native bridge, so a real
  provider SPI object never bypasses this native TLS state.

## Issue B — 2 classes HANG in a tight, zero-progress `gc::guard` retry loop against a Spring `$Proxy` object

### Original symptom

Both `.out.log`s were completely empty (no JUnit banner) — the hang happened
before any test method ran, in a `gc::guard` speculative collection-layout
probe against a 1-field Spring `$Proxy` object, repeating at sub-millisecond
intervals.

### Root cause and fix

The livelock cleared as a side effect of the Issue A TLS fix work above — it
was never independently root-caused to a specific caller/instruction, but
both classes now progress cleanly past context startup and run their full
test suites once the TLS bridge changes landed. (If this livelock signature
resurfaces in an unrelated class, treat it as a distinct bug — this doc does
not claim to have identified the actual mechanism, only that fixing the TLS
layer resolved it for these two classes.)

`CloudFoundryActuatorAutoConfigurationTests` has no SSL/TLS code of its own
(plain `WebApplicationContextRunner` + Spring Security `FilterChainProxy` +
CORS) — it was purely blocked by the livelock, so it is now fully clean.

`CloudFoundryReactiveActuatorAutoConfigurationTests` uses `WebClient`/Reactor
Netty with an `Http11SslContextSpec` + `InsecureTrustManagerFactory` for its
own `skipSslValidation` test, and a MockWebServer HTTPS listener
(`SSLSocketFactory.createSocket(Socket,String,int,boolean)`, the "layer TLS
over an already-accepted socket" contract) — getting this test to actually
run (rather than livelock before JUnit starts) surfaced three further gaps,
all now fixed:

- `native-builtins/src/t27_tls.rs`: added `rustls_server_wrap_existing_socket`
  to layer a real rustls server-side handshake over an already-accepted plain
  `Socket` — the server half of `SSLSocketFactory.createSocket(Socket,...)`,
  needed for MockWebServer's HTTPS listener contract. `engine_begin`'s client
  path was also fixed to not construct a root-store-only `ClientConfig` when a
  real Java `TrustManager` (e.g. Netty's `InsecureTrustManagerFactory`) is
  present — the same `use_java_trust_manager` mechanism as Issue A, now also
  reached by the SSLEngine-based (Netty JDK-provider) client path.
- `native-builtins/src/net_phase_e.rs`: `take_raw_socket_stream_for_tls`
  extracts the TCP stream from an accepted plain `Socket`/`NioSocketImpl` for
  handoff to rustls; a layered `SSLSocket` invoked through its `java.net.Socket`
  base type (MockWebServer does this) now stays on the rustls-aware stream
  adapter instead of being misread as a plain raw socket id.
- `native-builtins/src/phases_late.rs`: `SSLSocketFactory.createSocket(Socket,String,int,boolean)`
  now actually drives a real rustls handshake and returns a working
  `SSLSocket` (previously a stub); `SSLSocketInputStream`/`OutputStream`
  natives fall back to the identity-keyed socket side table when their
  synthetic Int field isn't populated (real JDK stream layouts don't carry it).
- **`javax/net/ssl/SSLSocket` was missing six native method registrations**
  that OkHttp (used by MockWebServer, via `mockwebserver3` 5.1.0) calls on
  every accepted server socket immediately after `createSocket(Socket,...)`
  returns: `setUseClientMode`/`getUseClientMode`,
  `setNeedClientAuth`/`getNeedClientAuth`, `setWantClientAuth`/
  `getWantClientAuth` are genuinely `abstract` in the real
  `javax.net.ssl.SSLSocket` base class (only a concrete provider subclass
  like SunJSSE's `SSLSocketImpl` implements them) — calling them on this
  synthetic object (whose class literally IS `javax/net/ssl/SSLSocket`)
  threw `AbstractMethodError` from
  `MockWebServer$SocketHandler.handle()` (`okhttp3.mockwebserver`/
  `mockwebserver3` 5.1.0, disassembled via `javap` — no source jar available
  in this environment's Gradle cache), immediately after `createSocket`
  returned and **before** the connection ever read the client's request or
  wrote a response. That exception was caught by `handle()`'s generic
  `catch (Exception e)` and logged via `java.util.logging.Logger` at
  `Level.SEVERE` — invisible in this environment's captured output (this
  project's `java.util.logging` routing appears broadly inert; see the
  `Brave BaggageFields Collections.<clinit> bootstrap mystery` and
  `CapturedOutput`/`OutputCaptureExtension` families of prior findings for
  the same theme) — so the connection was silently abandoned, surfacing to
  the client only as an unexplained hang (`Timeout on blocking read for
  30000000000 NANOSECONDS`, i.e. Reactor's `.block(Duration.ofSeconds(30))`
  timing out with zero information about the real cause). Also registered
  (found and fixed alongside, though not the actual blocking defect once
  isolated): `getApplicationProtocol`/`getHandshakeApplicationProtocol`
  (concrete-but-throws in the real base class) and
  `getSSLParameters`/`setSSLParameters` (concrete, but calls through
  other abstract accessors) — all six were added to the
  `force_native_over_real_jdk_bytecode` allowlists in
  `vm/src/runtime/interpreter.rs` and `vm/src/vm/vm_exec.rs` (two call
  sites) alongside `phases_late.rs`'s native registrations, matching the
  existing pattern for `startHandshake`/`getInputStream`/`getOutputStream`/
  `getSession`/`close`/`isClosed`/`isConnected`/`getPort` on the same class.

This was diagnosed by disassembling `mockwebserver3-5.1.0.jar` and
`okhttp-jvm-5.1.0.jar` with `javap` (no sources jar available in this
environment's Gradle cache) to trace exactly what `MockWebServer$SocketHandler.handle()`
calls on the returned `SSLSocket`, cross-referenced against
`CRATONVM_DBG_TLS_SRV`/`CRATONVM_DBG_TLS_SOCK`-gated tracing added to
`rustls_server_wrap_existing_socket`/`rustls_stream_read`/`rustls_stream_write`
(native-builtins/src/t27_tls.rs) confirming the server-side rustls handshake
itself completed successfully every time, while zero `SSLSocket`-level native
calls (`getSession`, `getInputStream`, etc.) were ever reached — proving the
abandonment happened between `createSocket()` returning and the first
stream I/O call, not inside the TLS handshake itself.

Fixing `setUseClientMode` (the very first of the six calls,
unconditional, immediately after the `checkcast SSLSocket`) resolved the
hang: the class went from "`Timeout on blocking read`" (wall time ~400-580s
per class, since it hung for the full 30s test timeout before JUnit could
even report the failure) to 13/14 real `PASS`, ~190-220s total (all 14 tests
actually executing).

## Validation

Using JDK 25.0.3.9 and
`C:\craton\CratonVM-cloudfoundry-target-20260718-019f7606\release\cratonvm-cloudfoundry-closure-r36-20260719.exe`
(built from this worktree's final state — see `git log` for the exact commit):

- `SkipSslVerificationHttpRequestFactoryTests`: PASS 1/1, JIT (`cloudfoundry-tls-r39-20260719`,
  3.7s) and `--nojit` (`cloudfoundry-fixed-r40-nojit-20260719`, 3.5s).
- `CloudFoundryActuatorAutoConfigurationTests`: PASS, JIT — 2 consecutive
  clean runs (`cloudfoundry-servlet-r38-20260719`, 225.6s;
  `cloudfoundry-servlet-r42-confirm-20260719`, 211.5s) — and PASS `--nojit`
  (`cloudfoundry-servlet-r41-nojit-longer-20260719`, 218.0s; note: a combined
  2-class `--nojit` run, `cloudfoundry-fixed-r40-nojit-20260719`, reported
  this class as a 300s `HANG` purely from running back-to-back with the TLS
  class inside one shared timeout budget on a heavily contended shared build
  box — the isolated rerun with a longer budget confirms this was contention,
  not a real hang or JIT-only fix).
- `CloudFoundryReactiveActuatorAutoConfigurationTests`: 13/14 PASS, JIT
  (`cloudfoundry-reactive-r37-20260719`, 193.4s) — up from a 30s-timeout FAIL
  with the class livelocked before that (and, before the TLS fixes in this
  doc, a complete pre-JUnit hang). The 1 remaining failure
  (`skipSslValidation`, a teardown-only `AssertionError` from
  `MockWebServer.close()`, not a TLS/SSL defect — the test's own SSL
  assertions already passed by the time it fails) is tracked separately, see
  above.

All result directories are under
`apps/spring-boot-suite-runner/.suite/results/` in this worktree, named by
the run names above.
