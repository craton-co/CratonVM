# Three TLS-layer defects the `HttpsURLConnectionImpl.delegate` NPE was hiding — OPEN

**Status: OPEN (2026-08-21).** Not root-caused. Each is verified PASS on HotSpot
against the same fixture, so each is a real CratonVM defect rather than an
environment gap.

**Found by:** re-running the 16-class SSL/TLS + OCSP cluster after the
`HttpsURLConnectionImpl.delegate` null defect was fixed (retired record:
`fixed-suite-bugs/httpsurlconnection-delegate-null-ssl-ocsp-cluster-FIXED-20260821.md`,
plain text — that directory is stripped from public history). That NPE fired in
`TomcatBaseTest.methodUrl` *before* any request was made, so every one of these
classes failed at the same first line and nothing underneath it had ever run.
Twelve of the sixteen went straight to green; these four did not.

Measured on `dev@fe600cd7b` + this session's fixes, Azure Linux, real JDK 25,
`apps/tomcat` fixture, one process per class, 900 s timeout.

## 1. A client certificate is never presented on mutual TLS

Two classes, one signature, seven failing tests:

| class | CratonVM | HotSpot |
|---|---|---|
| `org.apache.tomcat.util.net.TestClientCert` | 5 failures / 18 | **OK (18 tests)** |
| `org.apache.tomcat.util.net.TestCustomSslTrustManager` | 2 failures / 9 | **OK (9 tests)** |

```
javax.net.ssl.SSLHandshakeException: connection closed immediately after the TLS
handshake with no response — the peer likely rejected the handshake (e.g. a
required client certificate was not presented): connection closed before response head
	at javax.net.ssl.SSLHandshakeException.<init>(SSLHandshakeException.java:46)
```

`TestClientCert`: `testClientCertGetWithPreemptive[JSSE]`,
`testClientCertGetWithoutPreemptive[JSSE]`, `testClientCertPostSame[JSSE]`,
`testClientCertPostZero[JSSE]`, `testClientCertPostSmaller[JSSE]`.
`TestCustomSslTrustManager`: `testCustomTrustManagerCA[JSSE]`,
`testCustomTrustManagerAll[JSSE]` — the latter through
`doTestCustomTrustManager(TestCustomSslTrustManager.java:140)`.

Both classes configure the connector to **require** a client certificate. The
exception text is CratonVM's own diagnostic, so the client side is what observes
the close; whether the client failed to send the certificate or the server
failed to accept it is **not yet established** — that is the first thing to
separate. `TestClientCertTls13` passes (OK, 6 tests), which is a useful contrast:
whatever this is, it does not reach the TLS 1.3 client-auth path the same way.

### Not yet done

* Not split into client-side vs server-side. A packet capture, or CratonVM as
  client against HotSpot as server and vice versa, answers it in one run.
* Not checked with `--nojit`, nor per collector.
* `[JSSE]` is the only parameterisation in the failing set; the OpenSSL arm of
  these classes was not separately confirmed to pass rather than to be skipped.

## 2. Client-initiated renegotiation

`org.apache.tomcat.util.net.TestSsl` — 1 failure / 21. HotSpot: **OK (21 tests)**.

```
1) testClientInitiatedRenegotiation[JSSE](org.apache.tomcat.util.net.TestSsl)
java.lang.AssertionError
	at org.junit.Assert.assertTrue(Assert.java:53)
```

A bare `assertTrue` with no message, so the log says nothing beyond "the
renegotiation did not do what the test expected". Every other test in the class
passes, including the whole non-renegotiating TLS surface.

### Not yet done

Everything: not reduced, not traced into the handshake, not checked against the
OpenSSL arm.

## 3. `ocsp.TestOcspSoftFailInternalError` hangs

`org.apache.tomcat.util.net.ocsp.TestOcspSoftFailInternalError` — `rc=124`
(900 s timeout). HotSpot: **OK (20 tests)**.

It hangs on the **first** test case:

```
Starting test case [test[JSSE with OpenSSL trust false: softFail false, clientOk false]]
```

and the last two lines of the log are:

```
WARN cratonvm_classloading::jar_signer: jar signer: rejecting signer block:
     SignerInfo is missing authenticatedAttributes — refusing to …
WARN cratonvm_vm::runtime::interpreter::gc_and_alloc: STW cross-thread JIT
     takeover is still waiting for cooperative mutators rounds[…
```

The JIT-takeover warning is the interesting one and it is **not** a TLS
symptom — it is the same shape as the STW-takeover waits tracked elsewhere in
this tree. The three sibling OCSP classes all pass on the same binary
(`TestOcspSoftFail` OK 15, `TestOcspTimeout` OK 10, `TestOcspEnabled` OK 116,
`TestOcspSoftFailTryLater` OK 20), so this is specific to this class, not to
OCSP.

### Not yet done

* Not confirmed as a true hang vs. a perf cliff — take two `/proc/<pid>` `utime`
  + `wchan` samples ~6 s apart to tell looping from blocked before assuming
  either. A repeated frame deposit is not a single long block.
* Not checked whether `--nojit` clears it, which would confirm the takeover
  warning is the mechanism rather than a bystander.
* Only one run; not established as deterministic.

## Why these are worth keeping together

They arrived in one batch, from one masking defect, in one test family — but the
evidence says they are **three** causes, not one: (1) and (2) are TLS-layer and
class-specific, (3) is a JIT/GC takeover wait that names no TLS object at all.
Splitting the page when any one of them is root-caused is expected, not a
failure of this one.
