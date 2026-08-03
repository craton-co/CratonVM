# `TestSecurity2018.testCVE_2018_8034` — the `SSLEngine` lane never performed endpoint identification — FIXED

| | |
|---|---|
| **Status** | ✅ **FIXED** 2026-08-03 — retired from `docs/known-issues/tomcat/` |
| **Was** | `testsecurity2018-hostname-verification-not-enforced-regression.md` |
| **Severity** | high — a hostname-verification bypass in the `SSLEngine` client lane |
| **Retraction** | the old doc called this "a genuine regression … appeared somewhere in the ~170 commits merged into `dev`" and recommended bisecting. **That framing was wrong.** Hostname verification had never existed on this lane; the earlier PASS was vacuous. Evidence below. |
| **Fix** | `native-builtins`: `SSLContext.createSSLEngine(host, port)` now records the peer host, `SSLEngine.setSSLParameters` now honours `setEndpointIdentificationAlgorithm`, and the post-handshake path runs RFC 2818 / RFC 6125 endpoint identification |

## Symptom

```
1) testCVE_2018_8034(org.apache.tomcat.security.TestSecurity2018)
java.lang.Exception: Unexpected exception, expected<jakarta.websocket.DeploymentException> but was<java.lang.AssertionError>
Caused by: java.lang.AssertionError: Hostname verification should have failed
for 127.0.0.1 with a certificate issued for localhost only.
```

The test connects to `wss://127.0.0.1:<port>` with a server certificate issued
for `localhost` only, and installs `TesterSupport.TrustAllCerts` — a
`TrustManager` that accepts every chain — so the *only* thing that can refuse
the connection is endpoint identification. CratonVM accepted it.

## Root cause — three gaps on one path, all original

Tomcat's WebSocket client is the CVE-2018-8034 fix itself
(`WsWebSocketContainer.createSSLEngine`):

```java
SSLEngine engine = sslContext.createSSLEngine(host, port);   // host = "127.0.0.1"
SSLParameters sslParams = engine.getSSLParameters();
sslParams.setEndpointIdentificationAlgorithm("HTTPS");
engine.setSSLParameters(sslParams);
```

Every one of those three lines was dropped on the floor:

1. **`net_phase_e.rs::createSSLEngine`** registered ONE closure for both the
   no-arg and the `(String, int)` descriptor and ignored its arguments. The
   engine therefore never knew which host it was dialling. `engine_begin`'s
   fallback (`peer_host.unwrap_or("localhost")`) meant every client engine in
   the process also announced `localhost` as its SNI, whatever host it
   actually reached.
2. **`t27_tls.rs::setSSLParameters`** copied ALPN, cipher suites, and
   need/want-client-auth off the `SSLParameters` — but not the identification
   algorithm. `EngineState` had no field for it.
3. **The post-handshake path** (`engine_run_trust_check`) consulted the
   application's `TrustManager[]` and stopped there. `x509_manager` has had a
   complete RFC 6125 implementation (`verify_hostname` /
   `check_endpoint_identity`) since the `HttpURLConnection` work — its own
   doc comment says it "exposes … as the public entry point for the SSL-engine
   layer (tls.rs) to call … Until that wiring lands, endpoint identity is
   enforced by whatever caller threads the host in". Nothing ever threaded the
   host in on this lane. The `SSLEngine` client lane was that missing caller.

Note that #3 is not softened by #1 or #2: even had the host been recorded, a
check that never runs cannot fail. And matching against the `localhost`
fallback would have been worse than no check — it accepts a `localhost`
certificate for every host on the internet.

### Why an accepting `TrustManager` does not switch the check off

In real JSSE the two gates are independent. An application-supplied plain
`X509TrustManager` is wrapped by
`SSLContextImpl$AbstractTrustManagerWrapper`, which calls the application's
`checkServerTrusted` and **then** `checkAdditionalTrust` →
`X509TrustManagerImpl.checkIdentity`. So `TrustAllCerts` accepting the chain is
expected and irrelevant; identification still runs. Symmetrically, when no
application `TrustManager` is installed, JSSE's own default
`X509TrustManagerImpl` performs the identification — so the check must run on
that path too.

## Retraction: the earlier PASS was vacuous, not a regression

The old doc anchored on a PASS recorded for this class at `dev` `36f1157ad`
(binary `cratonvm-bccp-20260803.exe`, table in
[`bouncycastle-easymock-classpath-fixture-gap-FIXED.md`](bouncycastle-easymock-classpath-fixture-gap-FIXED.md))
and concluded that something in the following ~170 commits broke hostname
verification. Three independent pieces of evidence say otherwise.

**1. The code could not have passed for the stated reason.** At `36f1157ad`,
`git show 36f1157ad:native-builtins/src/t27_tls.rs | grep -c
'endpoint_id_alg\|check_endpoint_identity'` is `0`, exactly as at `dev` tip
`c685f1e65`. There was no endpoint-identification code on the `SSLEngine` lane
to regress.

**2. The test is `@Test(expected = DeploymentException.class)`.** It scores
PASS for *any* `DeploymentException` — including one thrown because the wss://
connection never completed. That is what happened: before
`fedb11592 fix(tls): SSLEngine.wrap must not consume app data until FINISHED is
reported`, the engine drained and encrypted 16384 bytes of Tomcat's static
`AsyncChannelWrapperSecure.DUMMY` buffer mid-upgrade, killing **every** wss://
connect. The old doc listed `fedb11592` among the commits it had "ruled out …
none of these touch hostname-verification logic on inspection". Correct — it
does not touch hostname verification. It is the commit that stopped *masking*
its absence.

**3. Measured, one commit apart.** The two binaries built either side of that
commit (same tree otherwise), Windows fixture, same command:

| Binary | Commit | `TestSecurity2018` |
|---|---|---|
| `cratonvm-wsjsse-base-20260803.exe` | `bdd405d3a` (parent) | PASS `OK (1)` in **67.9 s** |
| `cratonvm-wsjsse-fix-20260803.exe` | `fedb11592` | FAIL — `Hostname verification should have failed` |

HotSpot runs this class in **2.4 s**. A 68-second "pass" is a connection that
never completed, timing out into the `DeploymentException` the test accepts.

The probe added with this fix,
[`probes/WsHostnameVerificationProbe.java`](../../../../probes/WsHostnameVerificationProbe.java),
makes the distinction non-inferential — it runs the same scenario and prints
which of the three outcomes occurred (`REJECTED-BY-HOSTNAME-VERIFICATION`,
`REJECTED-FOR-ANOTHER-REASON`, `ACCEPTED`) instead of collapsing two of them to
"OK (1 test)".

> **Lesson worth keeping.** A test whose assertion is "some exception of class
> X was thrown" is not evidence that the mechanism under test works — a broken
> connection scores identically to a correct rejection. Before treating such a
> row as a baseline, check the *reason*, or check the wall-clock against the
> HotSpot control.

## The fix

`native-builtins/src/net_phase_e.rs`

* `createSSLEngine(String, int)` records the peer host and port via the new
  `t27_tls::set_engine_peer_host` (arity test, since one closure backs both
  descriptors), and mirrors them onto the Java object's own
  `peerHost`/`peerPort` fields so `SSLEngine.getPeerHost()`/`getPeerPort()`
  stop answering `null`/`-1`.
* The engine ObjectRef is now pinned across the allocations in that closure
  (`ReentrantLock`, the host `String`) instead of being held raw — family-1
  shape, pre-existing for the lock, newly load-bearing for the string.

`native-builtins/src/t27_tls.rs`

* `EngineState` gains `peer_port` and `endpoint_id_alg`.
* `setSSLParameters` reads the algorithm through
  `getEndpointIdentificationAlgorithm()` (an accessor call, not a field slot —
  in real-JDK mode this is a genuine `javax.net.ssl.SSLParameters` whose field
  indices are not ours to address) and stores it. An empty/absent value clears
  it, matching JSSE.
* `getSSLParameters` echoes the stored algorithm back, so the JSSE round-trip
  holds.
* `engine_take_pending_trust_check` now yields a pending check when *either*
  a `TrustManager[]` is attached *or* a client engine has an identification
  algorithm configured — previously it returned `None` outright when the
  context had no trust managers.
* New `engine_check_endpoint_identity` runs
  `x509_manager::check_endpoint_identity` against the recorded peer host after
  the `TrustManager` consultation (JSSE's order) and on the no-TrustManager
  path, failing the handshake as `SSLHandshakeException`.
* `endpoint_alg_verifies_identity` recognises `HTTPS`/`LDAPS`
  case-insensitively and **nothing else** — an unknown algorithm means "no
  check", so we never reject what JSSE accepts.
* `engine_begin` keeps the historical `"localhost"` stand-in for SNI when the
  recorded host is not a name rustls can represent (rustls demands *some*
  `ServerName`; JSSE would simply omit the extension). Endpoint identification
  does not read that stand-in — it matches against `peer_host` directly — so
  the fallback cannot launder a mismatch into a pass.

Server engines are untouched: identification is a client-side gate and is
skipped when `is_client` is false.

## Verification

### Linux

Azure host, `/data/data/apps/tomcat`, JDK 25.0.3, one process per class, JIT
on, status from the JUnit banner. `base` = `dev` `c685f1e65` unmodified; `fix`
= same tree + this change. Arms **interleaved per class, alternating order**,
never one block each.

| Class | base | fix |
|---|---|---|
| `tomcat.security.TestSecurity2018` | **FAIL ×1 of 1** | **PASS `OK (1)`** |
| `websocket.TestWsWebSocketContainerSSL` | PASS `OK (3)` | PASS `OK (3)` |
| `websocket.TestWebSocketFrameClientSSL` | PASS `OK (6)` | PASS `OK (6)` |
| `websocket.TestWsWebSocketContainer` | PASS `OK (24)` | PASS `OK (24)` |
| `util.net.TestSsl` | FAIL ×1 of 21 † | FAIL ×1 of 21 † |
| `util.net.TestCustomSsl` | PASS `OK (1)` | PASS `OK (1)` |
| `util.net.TestCustomSslTrustManager` | PASS `OK (9)` | PASS `OK (9)` |
| `util.net.TestSSLHostConfigCompat` | PASS `OK (78)` | PASS `OK (78)` |
| `util.net.TestSSLHostConfigIntegration` | PASS `OK (3)` | PASS `OK (3)` |
| `util.net.TestSSLHostConfigProtocol` | PASS `OK (12)` | PASS `OK (12)` |
| `util.net.TestClientCertTls13` | PASS `OK (6)` | PASS `OK (6)` |
| `util.net.TestSslHandshakeFailure` | PASS `OK (1)` | PASS `OK (1)` |
| `catalina.authenticator.TestSSLAuthenticator` | PASS `OK (1)` | PASS `OK (1)` |
| `tomcat.security.TestSecurity2017Ocsp` | PASS `OK (5)` | PASS `OK (5)` |
| `tomcat.security.TestSecurity2019` | FAIL ×1 of 3 ‡ | FAIL ×1 of 3 ‡ |
| `tomcat.security.TestSecurity2023` | PASS `OK (2)` | PASS `OK (2)` |
| `tomcat.security.TestSecurity2025Http2` | PASS `OK (2)` | PASS `OK (2)` |

† `testClientInitiatedRenegotiation[JSSE]`, identical failure in BOTH arms —
pre-existing, and permanently so: rustls categorically rejects renegotiation
(see the long note in `t27_tls.rs::engine_begin`).
‡ `testCVE_2019_0232`, identical failure in BOTH arms — pre-existing, a
separate defect with nothing to do with TLS.

A second batch, same protocol, covering the paths that share
`createSSLEngine` (server-side TLS, HTTP/2, the non-TLS WebSocket family) and
the two classes the classpath doc measured alongside `TestSecurity2018` —
**15 of 15 identical in both arms**, with real counts:
`coyote.http2.TestHttp2Section_3_5` `OK (2)`,
`coyote.http2.TestHttp2InitialConnection` `OK (6)`,
`coyote.http11.TestHttp11Processor` `OK (66)`,
`util.net.TestSSLHostConfigCipher` `OK (12)`,
`util.net.TestSSLHostConfig` `OK (11)`,
`catalina.manager.TestManagerWebappSsl` `OK (3)`,
`catalina.valves.rewrite.TestResolverSSL` `OK (3)`,
`websocket.TestWsPingPongMessages` `OK (1)`,
`websocket.TestWsRemoteEndpoint` `OK (8)`,
`websocket.TestWsSubprotocols` `OK (1)`,
`websocket.server.TestWsServerContainer` `OK (37)`,
`util.net.TestClientCertTls13` `OK (6)`,
`catalina.core.TestAsyncContextImpl` `OK (70)`,
`util.net.TestPQC` `OK (26)`,
`util.net.TestLargeClientHello` `OK (1)`.

32 classes measured in total; the only verdict that moved is the target's.

**Re-verified after merging `origin/dev` `63446c7f8`** (dev moved ~17 commits
during this work): the 17-class batch re-run against the merged binary
reproduces the same table row for row — `TestSecurity2018` PASS, the same two
pre-existing failures with the same failing test names, everything else
identical.

`--nojit`, `TestSecurity2018`, both arms interleaved twice: base FAIL, FAIL;
fix PASS, PASS.

Probe, same fixture:

| VM | `WsHostnameVerificationProbe` verdict |
|---|---|
| HotSpot 25.0.3 | `REJECTED-BY-HOSTNAME-VERIFICATION` — `CertificateException: No subject alternative names matching IP address 127.0.0.1 found` |
| CratonVM base | `ACCEPTED` — the connection succeeded, hostname verification did not run |
| CratonVM fix | `REJECTED-BY-HOSTNAME-VERIFICATION` — `SSLHandshakeException: endpoint identification (HTTPS) failed for host "127.0.0.1": certificate identity does not match host "127.0.0.1"` |
| CratonVM at `bdd405d3a` (Windows, pre-`fedb11592`) | `REJECTED-FOR-ANOTHER-REASON` — `SSLException: Bytes were consumed from the input during a write` ← **the vacuous pass, named** |

### Windows — the platform the bug was filed on

`apps/tomcat` fixture, `cratonvm-tlsepid-fix-20260803.exe` vs
`cratonvm-wsjsse-merged-20260803.exe` (dev of the same day), interleaved:

| Class | base | fix |
|---|---|---|
| `tomcat.security.TestSecurity2018` | **FAIL, FAIL** | **PASS `OK (1)`, PASS `OK (1)`** |
| `websocket.TestWsWebSocketContainerSSL` | PASS `OK (3)` | PASS `OK (3)` |
| `websocket.TestWebSocketFrameClientSSL` | PASS `OK (6)` | PASS `OK (6)` |
| `util.net.TestSSLHostConfigCompat` | PASS `OK (78)` | PASS `OK (78)` |
| `util.net.TestCustomSslTrustManager` | PASS `OK (9)` | PASS `OK (9)` |

Probe on Windows: base `ACCEPTED`, fix `REJECTED-BY-HOSTNAME-VERIFICATION`.

`CRATONVM_DBG=tls-auth` on the fixed binary shows the two gates firing in JSSE's
order, which is the point of the fix:

```
[dbg-tls-auth] engine_run_trust_check: 1 trust manager(s), method=checkServerTrusted, chain_len=1, auth_type=RSA
[dbg-tls-auth] engine_run_trust_check: invoke_virtual[0] -> Ok          <- TrustAllCerts accepts
[dbg-tls-auth] endpoint identification (HTTPS) failed for host "127.0.0.1": certificate identity does not match host "127.0.0.1"
```

Rust unit tests added in `native-builtins/src/t27_tls.rs`:

* `endpoint_alg_only_https_and_ldaps_verify_identity`
* `endpoint_identity_is_pending_and_rejects_a_mismatched_host` — drives a real
  loopback handshake, then asserts the pending check appears with no
  `TrustManager` attached, accepts the dialled host, refuses a host the leaf
  does not name, and never fires for a server engine.
