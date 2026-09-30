# What is left of the SSL/TLS cluster: two assertions that can see the renegotiation emulation — OPEN

**Status: OPEN (2026-08-22).** Supersedes
`ssl-client-cert-renegotiation-and-ocsp-hang-20260821.md`, which listed four
failing classes as three unexplained defects. All three are now resolved or
reclassified; what survives is two assertions that can observe an accepted
design limitation, and they are not worth "fixing" as they stand.

Measured on `bin/cratonvm-tls-7b7f66ee5`, Azure Linux, real JDK 25,
`apps/tomcat` fixture, one process per class.

## Corrections to the superseded page

**1. "A client certificate is never presented on mutual TLS" — FIXED.**
A resumed TLS session was cancelling the `CertificateRequest`. Root cause and
fix in `tls-session-resumption-cancels-the-client-certificate-request-FIXED-20260822.md`
(plain text — that tree is stripped from public history).
`TestCustomSslTrustManager` 2 failures → **OK (9 tests)**; `TestClientCert`
5 failures → 1.

**2. "Client-initiated renegotiation" was never an open defect.** That page
should not have listed it. There is a 2026-08-02 record —
`testssl-client-initiated-renegotiation-FIXED.md` —
which fixed three genuine defects in that test and documented the fourth as
permanent: *"that one test remains red by design"*. The reason is in the
`addHandshakeCompletedListener` registration itself: rustls implements no TLS 1.2
renegotiation, and firing `HandshakeCompletedEvent` for a `startHandshake()` on
an established connection would tell the application fresh key material had been
derived when none had. Listing it as unexplained was a failure to find the
existing record, not a new finding.

## 1. Two assertions that can see the renegotiation emulation

Both are consequences of the same accepted design: rustls has no renegotiation,
so Tomcat's lazy client auth is emulated by failing the connection and
re-requesting the certificate on the next one (`wants_deferred_client_auth`).

| test | expected | actual |
|---|---|---|
| `TestSsl.testClientInitiatedRenegotiation[JSSE]` | `listener.isComplete()` | never fires — by design, see above |
| `TestClientCert.testClientCertPostZero[JSSE]` | `OK-0` | `OK-1024` |

`testClientCertPostZero` sets `maxSavePostSize(0)`, telling Tomcat not to buffer
the POST body across the renegotiation — so on JSSE the body is discarded and
the servlet sees 0 bytes. CratonVM re-sends the request on a new connection, so
the server reads all 1024 bytes. The assertion is on Tomcat's internal buffering
limit, which only means anything when the certificate is obtained *without* a
new request.

**Neither is worth "fixing" as it stands.** Matching `testClientCertPostZero`
would require the VM to read and honour a Tomcat connector setting and then drop
a body the client legitimately sent; firing the handshake event would be a lie
about key material. Both would trade a visible red for a silent one. They move
only if the TLS backend gains renegotiation, which rustls declines to implement
on security grounds.

### `TestSsl.testPost[JSSE]` is a load-sensitive flake, not a third one

It surfaced once during this work and is recorded here so it is not re-opened as
a regression. The test starts **8 concurrent threads**, each doing an SSL POST,
and asserts `errorCount == 0`; it saw 2. Re-run on the SAME binary, 3×
sequentially: **1 failure in 3** (`expected:<0> but was:<2>` once; the other two
runs show only the by-design renegotiation failure). The host was carrying a
load average between 12 and 190 at the time. Treat a `testPost` red as noise
unless it reproduces on a quiet host.

## 2. `ocsp.TestOcspSoftFailInternalError` — FIXED 2026-08-22, no longer open

The superseded page guessed "a JIT-takeover wait" and this page's first revision
guessed "not OCSP at all". Both were wrong, and `sudo gdb -p` settled it: the
server-side `checkClientTrusted` → `check_ocsp` → `ocsp_http_post` fetch blocked
in `recv` while the collector still counted the thread as a cooperative mutator,
so a stop-the-world request could never be satisfied and every other thread
parked behind it. `rc=124` at a 1800 s cap → **OK (20 tests) in 7 s**.

Record: `ocsp-trust-check-parked-a-mutator-the-collector-still-counted-FIXED-20260822.md`
(plain text — that tree is stripped from public history). The measurement hazard
below is kept there too, because it outlives the defect.

## A measurement hazard, recorded so the next person does not lose an hour to it

The OCSP fixture takes an exclusive **`flock`** on
`apps/tomcat/test/org/apache/tomcat/util/net/ocsp/ocsp-responder.lock`, so two
concurrent CratonVM runs of *any* OCSP class serialise: the loser blocks in
`locks_lock_inode_wait` for its whole timeout and scores an indistinguishable
`rc=124`. Confirmed directly in `/proc/locks` (holder and waiter on inode
`27810154`). Since `/data/cratonvm/apps/tomcat` is a **shared** fixture on a
multi-session host, a neighbouring session is enough to cause it. Run OCSP
classes one at a time before believing a hang.

**Still open, and separable:** HotSpot takes a **POSIX** (`fcntl`) lock on that
same file where CratonVM takes a **`flock`**. The two kinds do not block each
other on Linux, so a CratonVM run and a HotSpot control do *not* serialise
against one another — which means a HotSpot control cannot be used to prove the
lock was free, and is a `FileChannel.lock()` implementation divergence worth its
own look. Not investigated.
