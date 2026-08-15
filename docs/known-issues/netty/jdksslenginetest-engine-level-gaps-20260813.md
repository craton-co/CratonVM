# netty `JdkSslEngineTest` — engine-level gaps, per cause

**Status:** OPEN, but 259 of the 307 failures this page opened with are closed
(2026-08-15). Four causes remain, listed under "What is left". Successor to R6
of `tls-batch10-residuals-20260813.md`, which is retired to
[`docs/internal/fixed-suite-bugs/netty-tls-batch10-residuals-FIXED-20260813.md`][fixed].

[fixed]: ../../internal/fixed-suite-bugs/netty-tls-batch10-residuals-FIXED-20260813.md

This is **not** a residual of the batch-10 TLS work — none of the causes below
is an encrypted-key, provider-routing, alert-delivery or trust-manager-dispatch
problem. It is the `SSLEngine`/`SSLSession` surface itself, measured against
HotSpot 25 with the same jars on the same host.

| | tests | ok | failed | aborted | wall |
|---|---|---|---|---|---|
| HotSpot 25 | 821 | 755 | **0** | 66 | 98 s |
| CratonVM (`fcb509bda`, 2026-08-13) | 821 | 398 | 357 | 66 | 630 s |
| CratonVM (V1 landed, 2026-08-13) | 821 | 448 | 307 | 66 | 516 s |
| CratonVM (`2cb9217e5` = dev, 2026-08-15) | 821 | — | — | — | **never finished** (killed at 2400 s) |
| CratonVM (this page's work, 2026-08-15) | 821 | **707** | **48** | 66 | **342 s** |

The 66 aborts are the same 66 HotSpot aborts (Conscrypt is not on the
classpath, and `testMasterKeyLogging`'s assumption is false on both). The
wall-clock ratio is **3.5x**, down from 6.2x.

**Read the "never finished" row before re-running anything.** Between
2026-08-13 and 2026-08-15 dev acquired a hang in this class: no output for
~25 minutes while burning one CPU, killed at the harness cap. It is gone with
the work below, but it is why the middle rows cannot simply be re-measured.
JUnit's per-test timeout does not fire for it — `SameThreadTimeoutInvocation`
can only check after the invocation returns — so a hung run produces no
`@@TESTFAIL` at all. Run this class with a per-test `executionStarted`
listener (`apps/netty-suite-runner/CratonRunner.java` plus an
`@@START`/`@@END` trace) or a hang tells you nothing about where it is.

## How to read the count

**The parameterisation axes explain nothing.** Every cause fires in all three
buffer types, both protocols and both `delegate` values; the collapsing axis is
the **test method**, each failing in all 12 (or 6) parameterisations. Bucket a
run by the first `io.netty.handler.ssl` frame in each `@@TESTFAIL`'s trace.

## What is left — 48 failures, four causes

| n | method | symptom | what it needs |
|---|---|---|---|
| 12 | `testRSASSAPSS` (via `rethrowIfNotNull`) | `TrustManager rejected the peer certificate chain: signature verification failed at index 0 (cryptographic)` | RSASSA-PSS certificate signatures now DISPATCH (see below) but do not verify. The salt length or the digest is not what `crypto_impl::rsa_verify_pss` assumes (it fixes `slen == hlen` and tries SHA-256/384/512 in turn). Parse `RSASSA-PSS-params` off the certificate's `signatureAlgorithm` instead of guessing — `ParsedCert` keeps only the OID today. |
| 12 | `testMutualAuthDiffCerts` (via `writeAndVerifyReceived`) | `Received fatal alert: CertificateUnknown`, then no message received | Same root cause: `test.crt`/`test2.crt` are PSS-signed. |
| 12 | `testClientHostnameValidationFail` | `IllegalStateException: handshake complete. expected failure` | Client endpoint identification does not fire on this path. The engine is created through `newHandler(alloc, "localhost", 0)` and the algorithm is set with `SSLParameters.setEndpointIdentificationAlgorithm("HTTPS")`; the sibling `testUsingX509TrustManagerVerifies*Hostname` DOES fire the check, so the difference is in how this one reaches `EngineState.endpoint_id_alg`/`peer_host`. Trace both with `CRATONVM_DBG_TLS_AUTH=1` before designing anything. |
| 12 | `doHandshakeVerifyReusedAndClose` | `expected: <true> but was: <null>` | TLS session RESUMPTION with a live `SSLSessionContext` cache — the test puts a value on the session, reconnects, and expects to read it back off the reused session. rustls can resume; nothing on this VM currently maps a resumed connection back to the previous `SSLSession` object. This is a feature, not a defect: size it as one. |

## What was fixed, and what each fix actually was

Every item below was measured on this class against HotSpot 25 with the same
jars, and re-measured after landing.

* **v1 end-entity certificates in the SIGNATURE-VERIFICATION path (was cause A's
  "gate 2", 36).** The 2026-08-13 page concluded gate 2 was path building and
  that V2's premise was refuted. Both halves were wrong. The failure is
  `webpki::EndEntityCert::try_from` inside `rustls::crypto::verify_tls12_signature`
  / `verify_tls13_signature`, which runs `version3()` and so cannot even read a
  v1 certificate's public key — reached *even when the trust decision has been
  delegated to a Java `TrustManager` and no path building is being asked for*.
  `rustls-cbc` gained `verify_tls{12,13}_signature_lenient`, which fall back to
  `anchor_from_trusted_cert` (webpki's own v1-capable parser, already exempted
  for anchors) to get the SPKI and verify against the raw key. Path building is
  untouched: a v1 certificate in a CA position is still refused.
* **Server-side ALPN selection (cause C, 24, plus 24 more in cause D).**
  `SSLEngineImpl.setHandshakeApplicationProtocolSelector` was inert, and netty
  configures a SERVER engine's ALPN through nothing else — so a server
  advertised no protocols at all and ALPN negotiated to null on both sides.
  The selector is now stored, the ClientHello's ALPN list is parsed in
  `do_unwrap` before `engine_begin` (the server's rustls connection is held
  back until then), the Java `BiFunction` is applied, and its answer becomes the
  advertised list. `null` from the selector fails the handshake with
  `no_application_protocol`, which is what
  `testTlsExtensionNoCompatibleProtocolsServerHandshakeFailure` asserts.
* **`getLocalCertificates()` reports what was SENT (cause B1, 24 + 12).** A
  recording `ResolvesClientCert` wrapper records whether rustls actually asked
  for and got a client certificate; a client that had a `KeyManager` but was
  never asked now answers null.
* **The server's view of the client chain (cause B2, 48 + 12).** A registered
  Java `TrustManager[]` is now the authority for the CLIENT chain too, not only
  for the server chain — webpki refuses netty's self-signed `CA:true` test
  certificates in an end-entity position (`CaUsedAsEndEntity`) where JSSE
  accepts them. Trust is unchanged in force: `engine_run_trust_check` still
  runs `checkClientTrusted` and still aborts with a fatal alert.
* **`getPeerPrincipal()` on an engine session.** It only ever consulted the
  native-socket peer chain, so a completed mutual-auth handshake answered
  `getPeerCertificates()` with a chain and `getPeerPrincipal()` with
  `SSLPeerUnverifiedException` in the same breath.
* **A v1 INTERMEDIATE is not a CA.** `validate_chain`'s BasicConstraints step
  exempted v1 certificates entirely; the exemption belongs to a self-signed
  ROOT. This is what makes `mutual_auth_invalid_client.p12` invalid, and both
  `testMutualAuthInvalidIntermediateCAFailWith*ClientAuth` assert the refusal.
* **`isOutboundDone()`/`isInboundDone()`** now mean "the close_notify has been
  sent" and "no more inbound will be accepted (including: the peer closed)",
  not "the setter was called" (`testCloseNotifySequence`).
* **`closeInbound()` throws mid-handshake**, `beginHandshake()` throws on a
  closed engine (`testCloseInboundAfterBeginHandshake`,
  `testBeginHandshakeAfterEngineClosed`).
* **`unwrap` is one application record per call**, and refuses a record whose
  plaintext cannot fit the destination WITHOUT consuming it — BUFFER_OVERFLOW,
  src untouched (`testUnwrapBehavior`).
* **A protocol restriction naming only versions this stack cannot negotiate is
  a handshake failure**, not an invitation to widen (`testProtocolNoMatch`).
* **`setEnabledCipherSuites` validates its argument** and throws
  `IllegalArgumentException` for a non-cipher-suite name (`testInvalidCipher`,
  and `SslContextBuilderTest.testInvalidCipherJdk` on the other page).
* **A cipher restriction is a LIST, not a set** — the caller's order is the
  order the ClientHello offers, and rustls honours client preference
  (`verifySSLSessionForMutualAuth` asserted the suite it configured).
* **TLS 1.2 session ids are the real ones.** The ServerHello's
  `legacy_session_id` is captured on both sides, so `getId()` agrees
  (`testSSLSessionId`); TLS 1.3 keeps the per-object pseudo-id, which the same
  test requires to DIFFER.
* **`SSLParameters.setServerNames`** now reaches the engine, so a client that
  dials an IP and declares an SNI name verifies the certificate against the
  name (`testUsingX509TrustManagerVerifiesSNIHostname`).
* **A client-side `checkServerTrusted` sees no local certificate**, because
  JSSE has not sent one yet at that point in the handshake.

## Repro

```bash
cd /data/cratonvm/apps/netty-suite-runner
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv-bin> --java-home "$JAVA_HOME" --Xmx 1500m \
    @common.args -Dcraton.batch=1 CratonRunner io.netty.handler.ssl.JdkSslEngineTest
```

`common.args` MUST carry `netty-tcnative-boringssl-static-<ver>-<os>.jar` in
place of the dynamic `netty-tcnative` the Maven reactor resolves — see the
sibling page's Repro for why, and for what a run without it silently measures.
Give it at least 600 s.
