# netty `JdkSslEngineTest` — engine-level gaps, per cause

**Status:** OPEN (2026-08-13). Successor to R6 of
`tls-batch10-residuals-20260813.md`, which is retired to
[`docs/internal/fixed-suite-bugs/netty-tls-batch10-residuals-FIXED-20260813.md`][fixed].

[fixed]: ../../internal/fixed-suite-bugs/netty-tls-batch10-residuals-FIXED-20260813.md

This is **not** a residual of the batch-10 TLS work — none of the causes below
is an encrypted-key, provider-routing, alert-delivery or trust-manager-dispatch
problem. It is the `SSLEngine`/`SSLSession` surface itself, measured against
HotSpot 25 with the same jars on the same host.

| | tests | ok | failed | aborted | wall |
|---|---|---|---|---|---|
| HotSpot 25 | 821 | 755 | **0** | 66 | 103 s |
| CratonVM (`fcb509bda`) | 821 | 398 | **357** | 66 | 630 s |

The 66 aborts are netty-tcnative being absent and are the same 66 HotSpot
aborts. The wall-clock ratio is **6.2x**; re-derive it after fixing failures
rather than before, because netty's engine tests fail by *waiting* (a latch that
never counts down, an `@Timeout` that expires) — the previous round took 124
failures off and 3.1x off the clock without touching a hot path.

## How to read the count

**The parameterisation axes explain nothing.** Every cause fires in all three
buffer types, both protocols and both `delegate` values:

```
-- type                  -- protocolCipherCombo        -- delegate
   88  Direct              136  TLSv1.3                   131  false
   88  Heap                126  TLSv1.2                   131  true
   86  Mixed
```

The collapsing axis is the **test method**: 357 failures are 22 method-level
causes, each failing in all 12 (or 6) parameterisations of its method. Bucket a
run with

```bash
python3 <<'PY'   # or see tb10-bucket2.py in the batch-10 session notes
# group @@TESTFAIL entries by the first io.netty.handler.ssl frame in the trace
PY
```

## The causes

### A. v1 X.509 end-entity certificates — 72 failures

```
java.io.IOException: with_client_auth_cert failed: invalid peer certificate:
    Other(OtherError(UnsupportedCertVersion))
```

* 36 `SSLEngineTest.rethrowIfNotNull` (via `testMutualAuthInvalidClientCertSucceed`)
* 24 `SSLEngineTest.testMutualAuthClientCertFail`
* 12 `SSLEngineTest.testClientHostnameValidationFail`

netty's mutual-auth fixtures are **X.509 version 1**, confirmed directly:

```
mutual_auth_invalid_client.p12   Version: 1 (0x0)
mutual_auth_client.p12           Version: 1 (0x0)
mutual_auth_server.p12           Version: 1 (0x0)
mutual_auth_ca.pem               Version: 3 (0x2)   <- the anchor is fine
```

`rustls-webpki` refuses them **by policy**, not by accident —
`rustls-webpki-0.103.12/src/cert.rs:257`:

```rust
// mozilla::pkix supports v1, v2, v3, and v4, including both the implicit
// (correct) and explicit (incorrect) encoding of v1. We allow only v3.
fn version3(input: &mut untrusted::Reader<'_>) -> Result<(), Error> { … }
```

The JDK's PKIX accepts v1 end-entity certificates, which is why HotSpot passes.

**This is a decision, not a bug fix.** Closing it means vendoring and patching
`rustls-webpki` the way `rustls-cbc` was vendored — a deliberate relaxation of a
certificate parser's version policy, with its own review. Do not fold it into an
unrelated change. The narrower alternative worth costing first: accept v1 only
for the **end-entity** certificate and keep the v3 requirement for anchors and
intermediates, which is what the JDK effectively does.

### B. client-side mTLS material never reaches the session — 84 failures

* 48 `SSLEngineTest.testSessionAfterHandshake0` (36 `SSLPeerUnverifiedException:
  peer not authenticated`, 12 `getLocalCertificates()` non-null)
* 24 `SSLEngineTest.testSessionLocalWhenNonMutual` — `expected: <null> but was: <[[…]]>`
* 12 `SSLEngineTest.verifySSLSessionForMutualAuth` — `SSLPeerUnverifiedException`

Two halves:

**B1. `getLocalCertificates()` reports what was *available*, not what was
*sent*.** `t27_tls`'s session builder records the chain whenever the engine has
an `identity_override`. A client with a `KeyManager` configured against a server
using `ClientAuth.NONE` never sends a certificate, and JSSE returns null there.
`testSessionLocalWhenNonMutual` sets up exactly that. The fix needs a "did this
side actually present a certificate" signal; rustls does not expose one on
`ClientConnection`, so the likely seam is CratonVM's own `ResolvesClientCert`
(`JavaKeyManagerResolver`) recording whether it was consulted and returned
`Some` — but that only covers the KeyManager path, not the
`client_identity` (cert_pem/key_pem) path, which needs its own.

**B2. the server side does not capture the client's chain** when one *is* sent,
so `serverSession.getPeerCertificates()` throws. Note these are the same
fixtures as (A), so (A) must be closed before B2 can be measured at all.

### C. ALPN does not negotiate on the engine path — 24 failures

24 `SSLEngineTest.verifyApplicationLevelProtocol` —
`expected: <my-protocol-http2> but was: <null>`.

`ApplicationProtocolNegotiationHandlerTest` and `SniHandlerTest`'s ALPN cases
pass, so the wiring works somewhere; what fails is `SSLEngine`-level ALPN in
this harness. Start by checking whether `setApplicationProtocols` /
`SSLParameters.setApplicationProtocols` reaches `EngineState.alpn_protocols`
on the path these tests use, and whether the negotiated value is read back
through `getApplicationProtocol()` or through the session.

### D. engine semantics — 116 failures

Each is its own small contract, and each is 12 (one per parameterisation):

| n | method | symptom |
|---|---|---|
| 12 | `testProtocol` | expected `SSLHandshakeException`, nothing thrown — a protocol mismatch still handshakes |
| 12 | `testTlsExtensionNoCompatibleProtocolsClientHandshakeFailure` | `expected: <true> but was: <false>` |
| 12 | `testTlsExtensionNoCompatibleProtocolsServerHandshakeFailure` | as above |
| 12 | `testMutualAuthDiffCertsClientFailure` | `expected: <true> but was: <false>` |
| 12 | `testMutualAuthDiffCertsServerFailure` | as above |
| 12 | `testCloseNotifySequence` | `expected: <false> but was: <true>` |
| 12 | `testCloseInboundAfterBeginHandshake` | bare assertion failure |
| 12 | `testBeginHandshakeAfterEngineClosed` | bare assertion failure |
| 12 | `writeAndVerifyReceived` | `expected: <false> but was: <true>` |
| 12 | `testUnwrapBehavior` | byte accounting — `expected: <69> but was: <35>`, `<55>`/`<28>` |
| 12 | `doHandshakeVerifyReusedAndClose` | `expected: <true> but was: <null>` — session reuse |
| 12 | `SslHandler.channelInactive` | `StacklessClosedChannelException` |
| 12 | `handshake` (helper) | 12 endpoint-identification, 12 local-certificates (see B) |

Five of these are "a handshake that should fail, succeeds", which is one
question, not five: **which negotiation mismatches does this engine fail to
reject?** Answer that once and it is likely worth 60.

### E. session-id identity — 8 failures

`testSSLSessionId` asserts that after a TLS 1.2 handshake the client's and
server's `getId()` are byte-identical, and that under TLS 1.3 they differ.
CratonVM derives a pseudo-id from the session object's identity, so two engines
never agree. rustls exposes no session id; the ServerHello's `session_id` field
is on the wire and could be captured the same way R7's ClientHello SNI now is —
TLS 1.2 only, since under TLS 1.3 the legacy field is echoed and the test wants
the two sides to *differ*.

### F. one-offs — 1 failure

`testInvalidCipher`, bare assertion failure. Not yet triaged.

## Repro

```bash
cd /data/cratonvm/apps/netty-suite-runner
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv-bin> --java-home "$JAVA_HOME" --Xmx 1500m \
    @common.args -Dcraton.batch=1 CratonRunner io.netty.handler.ssl.JdkSslEngineTest
```

Give it at least 1200 s. The runner prints nothing until the class ends, so a
silent process is not a hung one — check `/proc/<pid>/status` and `utime` before
concluding otherwise.
