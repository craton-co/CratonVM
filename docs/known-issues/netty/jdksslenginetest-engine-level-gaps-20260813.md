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
java.io.IOException: ServerConfig with_single_cert failed: …    (same cause, server side)
```

* 36 `SSLEngineTest.rethrowIfNotNull` (via `testMutualAuthInvalidClientCertSucceed`)
* 24 `SSLEngineTest.testMutualAuthClientCertFail`
* 12 `SSLEngineTest.testClientHostnameValidationFail`

#### What is actually v1

Every **end-entity** certificate in these fixtures; the anchor is not.

| fixture | leaf | intermediate | root (anchor) |
|---|---|---|---|
| `mutual_auth_client.p12` | **v1** `CN=NettyTestClient` | v3 `NettyTestIntermediate` | v3 `NettyTestRoot` |
| `mutual_auth_invalid_client.p12` | **v1** `CN=NettyTestInvalidClient` | **v1** `NettyTestInvalidIntermediate` | v3 `NettyTestRoot` |
| `mutual_auth_server.p12` | **v1** `CN=NettyTestServer` | — | v3 `NettyTestRoot` |
| `localhost_server.pem` | **v1** `CN=localhost` | — | v3 `NettyTestRoot` |

The v1 *intermediate* in `mutual_auth_invalid_client.p12` is deliberate — it is
what makes that fixture "invalid", and the test asserts the connection succeeds
anyway because client auth is OPTIONAL. **Do not make that chain validate.** A
v1 CA has no `basicConstraints`, so refusing it is correct under RFC 5280 and
is what the fixture exists to exercise.

#### There are two gates, and only the first one has been measured

**Gate 1 — installing your OWN identity.** This is where all 72 die, and it
involves no trust decision at all. `ClientConfig::with_client_auth_cert` /
`ServerConfig::with_single_cert` call `CertifiedKey::from_der`, which calls
`keys_match()`:

```rust
// rustls-cbc/src/crypto/signer.rs
pub fn from_der(…) -> Result<Self, Error> {
    let private_key = provider.key_provider.load_private_key(key)?;
    let certified_key = Self::new(cert_chain, private_key);
    match certified_key.keys_match() {
        // Don't treat unknown consistency as an error
        Ok(()) | Err(Error::InconsistentKeys(InconsistentKeys::Unknown)) => Ok(certified_key),
        Err(err) => Err(err),
    }
}

pub fn keys_match(&self) -> Result<(), Error> {
    let Some(key_spki) = self.key.public_key() else {
        return Err(InconsistentKeys::Unknown.into());
    };
    let cert = ParsedCertificate::try_from(self.end_entity_cert()?)?;   // <- webpki
    match key_spki == cert.subject_public_key_info() { … }
}
```

`ParsedCertificate::try_from` is webpki's `Cert::from_der`, which calls
`version3()` (`rustls-webpki-0.103.12/src/cert.rs:257`). A v1 cert makes the
**SPKI extraction** fail, and that failure escapes as
`InvalidCertificate(Other(UnsupportedCertVersion))` instead of being folded into
the `InconsistentKeys::Unknown` arm the surrounding code already tolerates.

So: rustls will not let this VM **present** a v1 certificate as its own
identity, because a best-effort self-consistency check cannot parse it. JSSE
has no such restriction.

**Gate 2 — verifying the PEER's chain.** webpki's path building parses every
certificate through the same `Cert::from_der`, so a v1 leaf would be rejected
there too. **This is currently unmeasured — gate 1 masks it.** Whether gate 2
bites at all is the first thing to establish, because it decides whether any of
the vendoring options below are needed.

Note webpki already exempts one position: `anchor_from_trusted_cert` catches
`UnsupportedCertVersion` and re-parses with a v1-only parser, with the reasoning
that a v1 cert "doesn't allow extensions, so there's no need to worry about
embedded name constraints". The v3-only rule is a path-building policy, not a
parser limitation.

#### Variants

**V0 — do nothing.** 72 failures stay. CratonVM is stricter than the JDK for any
application whose own identity is a v1 certificate, which is a legacy-PKI and
test-fixture shape rather than a modern one. Cost: nothing. Records a known
divergence.

**V1 — stop calling `keys_match` at the five identity-install sites. No
vendoring.** Replace `with_client_auth_cert(chain, key)` /
`with_single_cert(chain, key)` with the resolver form built on
`CertifiedKey::new(chain, signing_key)`, which skips the consistency check.
Every API used is public rustls; nothing is vendored or patched.

*Precedent already in the tree:* `t27_tls.rs:1712`
(`SniCertResolver::certified_key_from_pem`) already builds its `CertifiedKey`
this way, so the multi-tenant SNI server path accepts a v1 certificate today
while the five single-cert paths do not. V1 makes them consistent.

*What it gives up:* the early "your certificate and private key do not match"
error. That becomes a handshake-time failure instead of a config-time one. Note
rustls itself treats this check as best-effort — a key provider that cannot
expose a public key already skips it — and CratonVM's own `repair_ec_key_for_ring`
path deliberately reconstructs keys, so the check is not load-bearing here.

*Unknown it resolves:* whether gate 2 exists. **Do this first and re-measure.**
If the 72 clear, there is no vendoring decision to make.

**V2 — vendor `rustls-webpki`, relax v1 at the END-ENTITY position only.** Only
if V1 leaves gate-2 failures. Add a `Cert::from_der_end_entity` that tolerates
v1/v2 and use it exactly where an end-entity is parsed; leave intermediates and
the v3 requirement for CAs untouched. This matches the JDK's effective
behaviour, and it keeps `NettyTestInvalidIntermediate` rejected, which the
fixture wants.

*Cost:* a second vendored crypto crate alongside `rustls-cbc`, with the same
maintenance obligation (pin the version, record the delta, re-base on upgrade).
That is the real price — not the diff, which is small.

**V3 — vendor and relax v1 everywhere.** Not recommended. Accepting a v1
*intermediate* means accepting a CA with no `basicConstraints`, so any leaf can
sign for any other. It would also flip the meaning of
`testMutualAuthInvalidClientCertSucceed`.

**V4 — bypass webpki for peer verification.** Route client-auth to the existing
`PassthroughClientCertVerifier` (`t27_tls.rs:3381`) and let the Java
`TrustManager` decide, which `engine_run_trust_check` already consults
post-handshake. *Cost:* rustls then performs no chain validation for that
connection and correctness rests entirely on this VM's own `x509_manager` path,
which is less exercised. It also cannot be scoped to v1 — the decision has to be
made at config-build time, before any peer certificate exists. Only worth it if
V2 is rejected on maintenance grounds.

**V5 — upstream it.** Ask `rustls-webpki` for an opt-in policy knob for v1
end-entity certificates, in the shape of the existing
`UnknownExtensionPolicy`. Slow and uncertain, but it is the only variant that
ends with nothing vendored. Worth filing in parallel with V1 either way.

#### Recommended order

1. **V1**, then re-run `JdkSslEngineTest` and re-bucket. It is cheap, reversible,
   consistent with code already in the tree, and it answers the gate-2 question
   that every other variant depends on.
2. If gate-2 failures remain, **V2**, as its own change with its own review —
   not folded into unrelated work.
3. **V5** in parallel, so the vendored delta has an exit.

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
