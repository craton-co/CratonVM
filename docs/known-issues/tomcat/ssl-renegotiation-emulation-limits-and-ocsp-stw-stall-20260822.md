# What is left of the SSL/TLS cluster: two renegotiation-emulation limits, and one stop-the-world stall — OPEN

**Status: OPEN (2026-08-22).** Supersedes
`ssl-client-cert-renegotiation-and-ocsp-hang-20260821.md`, which listed four
failing classes as three unexplained defects. Two of those three are now
resolved, and the third turned out not to be about TLS at all. This page is what
survived.

Measured on `bin/cratonvm-tls-7b7f66ee5`, Azure Linux, real JDK 25,
`apps/tomcat` fixture, one process per class.

## Corrections to the superseded page

**1. "A client certificate is never presented on mutual TLS" — FIXED.**
A resumed TLS session was cancelling the `CertificateRequest`. Root cause and
fix in `fixed-suite-bugs/tls-session-resumption-cancels-the-client-certificate-request-FIXED-20260822.md`
(plain text — that tree is stripped from public history).
`TestCustomSslTrustManager` 2 failures → **OK (9 tests)**; `TestClientCert`
5 failures → 1.

**2. "Client-initiated renegotiation" was never an open defect.** That page
should not have listed it. There is a 2026-08-02 record —
`fixed-suite-bugs/tomcat/testssl-client-initiated-renegotiation-FIXED.md` —
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

## 2. `ocsp.TestOcspSoftFailInternalError` stalls — and it is not OCSP

`rc=124` at a 900 s cap. HotSpot: **OK (20 tests)**. The superseded page guessed
"a JIT-takeover wait"; the mechanism is now characterised, and the guess about
*where* was wrong.

**What it is not.** Not the OCSP network fetch: `grep -ci ocsp` on the run's log
is **0** — execution never reaches a responder request. The stall begins ~1 s
after `Starting ProtocolHandler`. Not JIT-tier-specific: `CRATONVM_JIT_ENABLE=0`
reproduces identically, ending on the same line. Reproduced 3/3.

**What it is.** Two `/proc` samples 6 s apart on the stuck process:

```
utime=32 stime=6      <- identical in both samples: genuinely blocked, not looping
  main-vm            wchan=locks_lock_inode_wait   (that sample: fixture flock, see below)
```

and on a run with the lock free:

```
utime 743 -> 871 (climbing)      <- the PROCESS burns CPU
  main-vm          wchan=wait_woken        <- the test thread is blocked
  AsyncFileHandle  593 ticks              <- top CPU consumer, spinning
  https-jsse-nio-  futex_do_wait  x10
```

Both `stdout` and the connector's own `catalina.<date>.log` stop at the same
second and never advance, so `AsyncFileHandler` is spinning with nothing to
write. The last line in every arm is

```
WARN cratonvm_vm::runtime::interpreter::gc_and_alloc: STW cross-thread JIT
     takeover is still waiting for cooperative mutators rounds[...
```

That is the shape `t27_tls::gc_blocked_syscall` and `net_phase_e`'s `re5` note
both describe: a stop-the-world request waits for a thread parked in a blocking
syscall that the collector still counts as a cooperative mutator, and everything
that needs a safepoint stalls behind it. This is a **GC/safepoint cooperation
defect**, and belongs with the STW-takeover work, not with TLS.

### Next steps for whoever picks this up

* Name the syscall `main-vm` is in. `/proc/<tid>/syscall` reads empty here;
  `strace -p` or a `gdb` thread apply bt would settle it in one attempt and is
  the single highest-value next measurement.
* `check_revocation` uses a fixed 30 s per-certificate timeout. If the stall is
  a *very* long wait rather than a true deadlock, that constant is where the
  time goes — but note the log shows no OCSP request at all, so this is a
  secondary hypothesis, not the leading one.
* This class was NOT in the 2026-08-14 census. It is worth bisecting whether the
  stall is new; it was masked until 2026-08-22 by the `delegate` NPE, which
  aborted these classes before they got this far.

### A measurement hazard, recorded so the next person does not lose an hour to it

The fixture takes an exclusive **`flock`** on
`apps/tomcat/test/org/apache/tomcat/util/net/ocsp/ocsp-responder.lock`, so two
concurrent CratonVM runs of *any* OCSP class serialise: the loser blocks in
`locks_lock_inode_wait` for its whole timeout and scores an indistinguishable
`rc=124`. Confirmed directly in `/proc/locks` (holder and waiter on inode
`27810154`). Since `/data/cratonvm/apps/tomcat` is a **shared** fixture on a
multi-session host, a neighbouring session is enough to cause it.

Two consequences: run OCSP classes one at a time before believing a hang, and
note that HotSpot takes a **POSIX** (`fcntl`) lock on that same file while
CratonVM takes a **`flock`**. The two kinds do not block each other on Linux, so
a CratonVM run and a HotSpot control do *not* serialise against one another —
which is a divergence in `FileChannel.lock()`'s implementation worth its own
look, and separately means a HotSpot control cannot be used to prove the lock
was free.
