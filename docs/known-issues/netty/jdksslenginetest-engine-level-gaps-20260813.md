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
| CratonVM (`fcb509bda`) | 821 | 398 | 357 | 66 | 630 s |
| CratonVM (V1 landed) | 821 | 448 | **307** | 66 | **516 s** |

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

## Roadmap

Ordered by what unblocks what, not by failure count. Each item says what it is
waiting on, so nothing here starts before its premise is measured.

| # | item | size | blocked on | worth |
|---|---|---|---|---|
| ~~V1~~ | ~~skip `keys_match` at the five identity-install sites~~ | S | — | **done 2026-08-13, 49 failures** |
| ~~P1~~ | ~~does the JDK accept the invalid v1 chain?~~ | XS | — | **done 2026-08-13 — no. V2 refuted** |
| V6 | OPTIONAL client auth must not abort on a chain that fails validation | M | — | 36 |
| B1 | `getLocalCertificates()` must report what was SENT, not what was available | M | — | ~36 |
| C | ALPN on the `SSLEngine` path | M | — | 24 |
| D | "a handshake that should fail, succeeds" — one question behind five methods | L | — | ~60 |
| E | TLS 1.2 session id shared between the two engines | M | — | 8 |
| ~~V2~~ | ~~vendor `rustls-webpki`, relax the v1 rule~~ | — | — | **parked — premise refuted by P1** |
| V5 | upstream a v1 policy knob to `rustls-webpki` | S to file | — | speculative; no measured cost today |

### V6 — OPTIONAL client auth must not abort on an unvalidatable chain

The 36 that survive V1. Both VMs reject
`mutual_auth_invalid_client.p12`'s chain (P1, above); netty's test asserts the
connection succeeds regardless, because the server is configured
`ClientAuth.OPTIONAL`.

rustls's `allow_unauthenticated()` permits the **absence** of a client
certificate, not a **bad** one: a presented certificate is still verified and a
verification failure is fatal. JSSE's `setWantClientAuth(true)` continues either
way.

Two candidate seams, and the first is worth measuring before building anything:

1. **The JDK client may never send it.** `X509KeyManager.chooseClientAlias`
   filters candidate identities against the server's `certificate_authorities`
   hint; a client that cannot build an acceptable chain sends no certificate at
   all, and then OPTIONAL trivially succeeds. If that is what HotSpot does, the
   fix is client-side and small. *Measure:* does HotSpot's server see a client
   certificate on that connection?
2. **Otherwise, server-side:** when client auth is optional, a verification
   failure must downgrade to "no client identity" instead of aborting. The
   existing `PassthroughClientCertVerifier` (`t27_tls.rs:3381`) is the natural
   place — it already exists for the case where trust is delegated to a Java
   `TrustManager`, and it already has to answer this question.

Whatever lands must keep the negative tests negative: `testMutualAuthClientCertFail`
exists to see the chain rejected, and a change that makes both the "valid" and
"invalid" fixtures succeed has broken the tests' meaning rather than fixed the
VM.

### ~~V2~~ — vendor `rustls-webpki`, relax the v1 rule (parked)

**Parked 2026-08-13: P1 refuted the premise.** V2 existed because 72 failures
looked like "webpki refuses v1 certificates that the JDK accepts". After V1
removed the identity-install half, the surviving 36 turned out to be a chain the
JDK rejects too. No measured case remains where webpki's v3 rule costs this VM
something HotSpot allows.

Kept written down rather than deleted, because the reasoning is what matters if
a genuine case turns up later:

* an end-entity-only relaxation is the safe shape — a v1 CA has no
  `basicConstraints`, so accepting one lets any leaf sign for any other;
* the certificate that actually failed here was an **intermediate**, so
  end-entity-only would not have closed these 36 anyway — V2 would have had to
  relax the CA position, which was already written down as *not recommended*;
* webpki already exempts one position (`anchor_from_trusted_cert` re-parses v1
  anchors), so any patch should follow that pattern — a separate parser entry
  point — rather than loosening `version3` in place;
* the cost is a second vendored crypto crate beside `rustls-cbc`, with the same
  pin/record/re-base obligations. The diff would be small; the carrying cost is
  not.

**Re-open only on a measured case** where the JDK validates a chain and webpki
rejects it for version alone.

### V5 — upstream a v1 policy knob

Speculative now that V2 is parked — there is no measured cost to point at, so
this is worth filing only if a real case appears. Recorded because it is the
one option that ends with nothing vendored.

* **Shape to propose:** an opt-in policy enum in the existing style of
  `UnknownExtensionPolicy` — e.g. `CertificateVersionPolicy::{V3Only, AllowV1EndEntity}`
  on the verifier builder, defaulting to today's behaviour.
* **Argument to make:** webpki already ships a v1 parser and already uses it for
  trust anchors; the JDK, OpenSSL and Go all accept v1 certificates in at least
  some positions; and consumers re-implementing a JVM's TLS surface need to
  match the JDK, not RFC 5280's SHOULD.
* **Relationship to V2:** V5 is what makes V2 unnecessary. If a case for V2
  ever appears, file V5 first so the fork has a documented exit from the day it
  is created.


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

#### V1 — landed 2026-08-13, and it answered the gate-2 question

The five identity-install sites now build their `CertifiedKey` with
`CertifiedKey::new` and hand it to `with_cert_resolver` /
`with_client_cert_resolver`, which is exactly what `with_single_cert` /
`with_client_auth_cert` do minus the `keys_match` call:

```rust
// rustls-cbc/src/server/builder.rs — what with_single_cert IS
let certified_key = CertifiedKey::from_der(cert_chain, key_der, self.crypto_provider())?;
Ok(self.with_cert_resolver(Arc::new(SingleCertAndKey::from(certified_key))))
```

No vendoring, all public API, and `SniCertResolver::certified_key_from_pem` —
which had always done it this way — now routes through the same helper, so
there is one way to build an identity instead of two.

**Result: 356 → 307 failures, 630 s → 516 s.** Cleared outright:

| n | method | was |
|---|---|---|
| 24 | `testMutualAuthClientCertFail` | `with_client_auth_cert failed: … UnsupportedCertVersion` |
| 12 | `testClientHostnameValidationFail` | `ServerConfig with_single_cert failed: …` |
| 12 | `testMutualAuthDiffCertsClientFailure` | (not previously attributed to group A) |

The eight other classes on the parent page were re-run against the same build
and are unchanged, all at or above the oracle.

#### What gate 2 turned out to be

36 failures survive, and they have changed shape — this is the answer V1 was
run to get:

```
before V1:  java.io.IOException: with_client_auth_cert failed: invalid peer certificate:
                Other(OtherError(UnsupportedCertVersion))            <- config-build time
after V1:   javax.net.ssl.SSLHandshakeException: rustls: invalid peer certificate:
                Other(OtherError(UnsupportedCertVersion))            <- handshake time
```

So gate 2 is real. But the 36 are all one call path —
`testMutualAuthInvalidIntermediateCASucceedWithOptionalClientAuth` →
`testMutualAuthInvalidClientCertSucceed` — which uses
`mutual_auth_invalid_client.p12`, the fixture whose **intermediate** is v1, with
`ClientAuth.OPTIONAL`. The test asserts the connection **succeeds anyway**.

That admitted two explanations, and P1 settled it.

#### P1 — the JDK rejects that chain too (measured 2026-08-13)

Ran the JDK's own validators over `mutual_auth_invalid_client.p12`'s chain
against `mutual_auth_ca.pem`, no networking involved:

```
chain length = 3
  v1  UID=ClientWithInvalidCa, CN=NettyTestInvalidClient   issuer=CN=NettyTestInvalidIntermediate
  v1  CN=NettyTestInvalidIntermediate                      issuer=CN=NettyTestRoot
  v3  CN=NettyTestRoot                                     issuer=CN=NettyTestRoot
anchor  = v3  CN=NettyTestRoot

PKIX:    REJECTED -> PKIX path validation failed: basic constraints check failed:
                     this is not a CA certificate
SunX509: REJECTED -> End user tried to act as a CA
```

**So webpki is not stricter than the JDK here — both refuse the chain**, and for
the same reason: a v1 certificate carries no `basicConstraints`, so it cannot be
a CA. The certificate version is incidental; `UnsupportedCertVersion` and
"this is not a CA certificate" are two spellings of one verdict.

**V2's premise is refuted for the fixture that motivated it.** The difference
that remains is entirely about what happens *after* the rejection: netty asserts
the connection succeeds anyway, because client auth is `OPTIONAL`. On HotSpot
nothing valid is ever presented and the server proceeds without a client
identity; on CratonVM the rejection aborts the handshake.

That is item **V6**, it is CratonVM-side, and it needs no vendoring.


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
