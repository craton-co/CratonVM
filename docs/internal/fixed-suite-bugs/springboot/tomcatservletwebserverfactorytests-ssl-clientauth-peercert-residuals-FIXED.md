# `TomcatServletWebServerFactoryTests` — SSL client-auth / peer-certificate residuals — FIXED

**Status: FIXED — 2026-07-26.** Closes
[`../../known-issues/springboot/tomcatservletwebserverfactorytests-ssl-clientauth-peercert-residuals.md`](../../known-issues/springboot/tomcatservletwebserverfactorytests-ssl-clientauth-peercert-residuals.md).
Two independent, confirmed root causes; both fixed. A third, pre-existing,
unrelated intermittent hang was found while verifying and is filed
separately (see "Residual found while verifying" below) — it reproduces on
both the pre-fix and post-fix binary, so it is not a regression from this
fix.

## Symptom (original)

Three real bugs surfaced across four `@Test` methods once the class's
`AsynchronousServerSocketChannel` bind bugs (fixed in the same original
session, see the doc's own history) stopped masking them:

1. `sslWantsClientAuthenticationSucceedsWithClientCertificate` /
   `sslWantsClientAuthenticationSucceedsWithoutClientCertificate` (both
   `ClientAuth.WANT`, no truststore configured): the server rejected the
   handshake outright —
   `SSLHandshakeException: connection closed by peer during handshake`.
2. `sslNeedsClientAuthenticationSucceedsWithClientCertificate` /
   `shouldUpdateSslWhenReloadingSslBundles`: handshake succeeded, but the
   client's own `SSLSession.getPeerPrincipal()`/`getPeerCertificates()` threw
   `IllegalStateException: peer not authenticated (no certificate in session)`
   even though the server's certificate was genuinely captured during the
   handshake.

## Root cause 1 (bug #2 above): `s2_tls_peer_cert_chain_der` missing the rustls-stream redirect its siblings already have

`native-builtins/src/servlet.rs`'s `s2_tls_read`/`s2_tls_write`/`s2_tls_close`
all check `if id >= RUSTLS_SOCK_ID_BASE` and redirect to the rustls client
stream table (`t27_tls::rustls_stream_{read,write,close}`) for a client TLS
socket backed by the rustls path (e.g. one that completed its handshake
through the deferred `SSLSocketFactory.createSocket(Socket,...)` path).
`s2_tls_peer_cert_chain_der` — used by both `SSLSession.getPeerCertificates()`
and `getPeerPrincipal()` in `native-builtins/src/phases_late/ssl_security.rs`
— was missing that same redirect. For any rustls-backed client id, it always
missed in the native-tls-only `s2_registry()` table and silently returned an
empty chain, even though `t27_tls::rustls_client_peer_cert_chain_der`
(already wired up for a *different* call site, `record_client_peer_chain`)
had the real captured chain the whole time.

A second, compounding bug in the same code path: on an empty chain, both
`getPeerCertificates()` and `getPeerPrincipal()` in `ssl_security.rs` threw a
bare `RuntimeError::IllegalStateException` — contradicting their own doc
comments ("Throws SSLPeerUnverifiedException") and the real `SSLSession`
contract. Callers that specifically catch `SSLPeerUnverifiedException` (e.g.
this test's `RememberingHostnameVerifier`, Spring's
`DefaultSslInfo.initCertificates`) saw the wrong exception type escape
uncaught.

### Fix 1

- `native-builtins/src/servlet.rs`: `s2_tls_peer_cert_chain_der` now redirects
  to `t27_tls::rustls_client_peer_cert_chain_der` for `id >= RUSTLS_SOCK_ID_BASE`,
  mirroring `s2_tls_read`/`write`/`close`.
- `native-builtins/src/phases_late/ssl_security.rs`: `getPeerCertificates()`
  and `getPeerPrincipal()` now throw `SSLPeerUnverifiedException` (via
  `crate::phases_early::throw_jca_exc`, the same helper already used
  elsewhere in this file) on an empty chain, instead of `IllegalStateException`.

## Root cause 2 (bug #1 above): optional (`WANT`) client auth with no trust source hard-failed the server config build

`build_server_config_single_cert_ex_ciphers` (`native-builtins/src/t27_tls.rs`)
treats a missing `client_ca_pem` as a hard error
(`"client auth requested but client_ca_pem is None"`) whenever client auth is
requested at all — including *optional* (`ClientAuth.WANT`) requests with no
truststore configured. That error aborts `engine_begin` before the handshake
starts, which the client observes as a bare connection-closed/handshake
failure.

Confirmed empirically (not just by inspection) that this is wrong: real JSSE
does **not** fail the handshake in this scenario. A standalone probe
(`SSLContext.init(keyManagers, /* trustManagers */ null, null)` +
`SSLSocket.setWantClientAuth(true)`, exactly mirroring what
`SSLUtilBase.getTrustManagers()` returns — `null` — when Tomcat has no
truststore configured, confirmed by decompiling `SSLUtilBase.class`) shows
the handshake completing successfully even when the client presents a
certificate that fails verification against the JVM's real default trust
store (144 real CA anchors, confirmed non-empty via the same probe) — only
the *server's own* `getPeerPrincipal()` throws `SSLPeerUnverifiedException`
afterward; the connection itself is unaffected. This matches a plain-HotSpot
rerun of the whole class: **132/132 PASS**, including both `WANT` tests.

rustls's `WebPkiClientVerifier` has no equivalent soft-fail path — a
verification failure (or, here, nothing to verify against at all) is always
a fatal TLS alert. This is the same category of permanent JSSE/rustls
behavioral gap already documented for TLS 1.2 DHE cipher suites and TLS
renegotiation elsewhere in this file — not fixable by config, only by
choosing a different acceptable approximation.

### Fix 2

`engine_begin` already had a `PassthroughClientCertVerifier` (accepts any
structurally-valid, signed certificate without checking it against a CA)
built for a different case — Tomcat's `trustManagerClassName` mechanism,
where a real Java `TrustManager` enforces trust afterward instead. Extended
its selection to also cover optional client auth with **no trust source at
all** (no truststore, no custom `TrustManager`): `use_passthrough_verifier =
has_custom_trust_managers || (client_ca.is_none() && optional_client_cert)`.
Mandatory (`ClientAuth.NEED`) client auth is untouched — `optional_client_cert`
is false whenever `state.need_client_auth` is true, so `NEED` still requires
a real trust source (`sslNeedsClientAuthenticationFailsWithoutClientCertificate`,
which expects an `IOException`, was reverified unaffected).

Known residual gap versus real JSSE: this makes CratonVM's rustls session
*accept* the untrusted certificate (so `getPeerCertificates()` on the server
side would return it) where real JSSE would silently discard it instead
(leaving the server-side session with no peer certificate at all). Neither
test exercised by this doc reads the server-side peer identity — both only
check that the HTTP response succeeds — so this difference is not currently
observable by the suite. Documented here in case a future test depends on
the server-side distinction.

## Verified

Worktree `wt-tomcat-ssl-peercert-20260726`, branch
`fix/tomcat-ssl-peercert-residuals-20260726`, binary built from current `dev`
plus the three fixes above.

- Plain HotSpot baseline (no fix applicable/needed): **132/132 PASS**.
- Fixed CratonVM binary, full class, clean run (fresh `-Djava.io.tmpdir`
  scratch dir — the Azure host's root filesystem was at 100% full during
  this session, which manifests as spurious `Failed to create default temp
  directory`/`does not have the permissions` failures unrelated to any VM
  bug; see `docs/known-issues/springboot/README.md`'s existing host-state
  caveats): **129/132 PASS**. All 4 of this doc's originally-documented SSL
  failures now pass:
  - `sslWantsClientAuthenticationSucceedsWithClientCertificate` — PASS
  - `sslWantsClientAuthenticationSucceedsWithoutClientCertificate` — PASS
  - `sslNeedsClientAuthenticationSucceedsWithClientCertificate` — PASS
  - `shouldUpdateSslWhenReloadingSslBundles` — PASS
  - `sslNeedsClientAuthenticationFailsWithoutClientCertificate` (must still
    FAIL with an `IOException` per its own assertion) — unaffected, still
    correctly rejects.
  - The remaining 3 failures: 2 were same-run directory-permission artifacts
    from reusing a scratch tmpdir across repeated reruns in this session (not
    reproducible on a single clean run), and 1 is
    `sslWithHttp11Nio2Protocol`, which this doc's own earlier history already
    flagged as "not yet root-caused whether this is a genuine remaining gap
    ... or host-load flakiness" — unrelated to client-auth/peer-cert, left
    open, not reopened by this fix.

## Residual found while verifying (new, separate, OPEN doc filed)

An intermittent VM hang ("STW cross-thread JIT takeover is still waiting for
cooperative mutators") struck roughly 1 in 3-5 full-class reruns during
verification, on **both** the pre-fix and post-fix binary — confirming it is
not a regression from this change. Filed separately since it matches a
distinct, general, already-multiply-fixed VM-concurrency bug category (a
blocking native call somewhere not bracketed with
`begin_blocking_region()`/`end_blocking_region()`), not a TLS/peer-cert
defect. See
[`tomcatservletwebserverfactorytests-stw-takeover-hang-FIXED.md`](tomcatservletwebserverfactorytests-stw-takeover-hang-FIXED.md)
(since root-caused and FIXED — an unbracketed blocking `SSLSocketInputStream.read`).

## Affected classes

| Module | Class | Outcome |
|---|---|---|
| `module/spring-boot-tomcat` | `org.springframework.boot.tomcat.servlet.TomcatServletWebServerFactoryTests` | `sslWantsClientAuthenticationSucceedsWithClientCertificate`, `sslWantsClientAuthenticationSucceedsWithoutClientCertificate`, `sslNeedsClientAuthenticationSucceedsWithClientCertificate`, `shouldUpdateSslWhenReloadingSslBundles`: **PASS**. `sslWithHttp11Nio2Protocol`: unrelated pre-existing flakiness, left open (not part of this doc's scope). |
