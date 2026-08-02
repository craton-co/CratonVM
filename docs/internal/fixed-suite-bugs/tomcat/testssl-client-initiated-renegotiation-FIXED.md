# `TestSsl.testClientInitiatedRenegotiation[JSSE]` — three client-side JSSE defects, and one permanent TLS-backend deviation

**Status: RESOLVED 2026-08-02.** Three genuine defects found and fixed. The
fourth finding — client-initiated TLS 1.2 renegotiation — is a deliberate,
permanent property of this VM's TLS backend and is documented as such below;
that one test remains red by design.

Retires `docs/known-issues/tomcat/testssl-client-initiated-renegotiation-20260801.md`.

## What the original doc asked

That doc recorded a bare `AssertionError` with no message:

```
1) testClientInitiatedRenegotiation[JSSE](org.apache.tomcat.util.net.TestSsl)
java.lang.AssertionError
	at org.junit.Assert.assertTrue(Assert.java:53)
```

and asked which assertion fails. The `Assert.java:53` frame is the **1-arg**
`assertTrue` overload, and `TestSsl` has exactly one message-less
`assertTrue` in this method — `TestSsl.java:509`:

```java
Assert.assertTrue(listener.isComplete());
```

Confirmed directly: the failure trace on CratonVM names
`TestSsl.testClientInitiatedRenegotiation(TestSsl.java:509)`. So the failing
condition is **the `HandshakeCompletedListener` never firing**, not any of the
`getLastClientAuthRequestedIssuerCount()` checks.

The doc also flagged the class's ~410–445s runtime as a possible internal
timeout. It is not related: this test reproduces in isolation in **14s**
(`RunMethods org.apache.tomcat.util.net.TestSsl testClientInitiatedRenegotiation`).
5000ms of that is the test's own listener-wait loop. The class runtime is a
whole-class property and is out of scope here.

## How it was measured

`probes/TesterRenegProbe.java` — a JUnit test in
`org.apache.tomcat.util.net` extending Tomcat's own `TomcatBaseTest` and using
`TesterSupport.initSsl` / `configureSSLImplementation`, i.e. the same real
server fixture `TestSsl` uses. Deliberately **not** a synthetic replica: it
runs the identical client sequence and only adds reporting, so nothing about
making it runnable can hide the defect.

It carries two methods:

* `probe` — mirrors `testClientInitiatedRenegotiation` step by step, printing
  the negotiated protocol, whether `startHandshake()` returned or threw, and
  whether the listener fired.
* `probeLayeredListenerFires` — the case `TestSsl` **cannot** reach: a socket
  whose handshake has not yet run when the listener is registered
  (`createSocket(Socket, String, int, boolean)` defers the handshake to
  `startHandshake()`). On both VMs this listener MUST fire. This is what
  separates "we don't renegotiate" (correct) from "we never deliver the event
  at all" (the defect).

Mutation-checked: `probeLayeredListenerFires` fails against the pre-fix binary
with its own assertion message, and both methods pass on stock HotSpot.

Run (local Windows, `apps\tomcat` fixture):

```powershell
cd C:\craton\CratonVM\apps\tomcat
$cp = (Resolve-Path .suite\probe-classes).Path + ';' + (Get-Content .suite\cp.txt)
$env:CRATONVM_REAL='net-sockets,aqs'; $env:CRATONVM_THREADS='-default-watchdog'; $env:CRATONVM_JIT='rootsnap-cache'
<cratonvm.exe> --java-home "<jdk25>" --Xmx 2g -Dtomcat.test.basedir=output\build `
  -Dtomcat.test.relaxTiming=true -cp $cp org.junit.runner.JUnitCore org.apache.tomcat.util.net.TesterRenegProbe
```

## Findings

| | stock HotSpot JDK 25 | CratonVM before | CratonVM after |
|---|---|---|---|
| `getEnabledProtocols()` | `[TLSv1.2]` | `[TLSv1.3]` | `[TLSv1.2]` |
| `getSession()` protocol | `TLSv1.2` | **`null`** | `TLSv1.2` |
| `getSession()` cipher | `TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384` | **`null`** | `TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384` |
| listener fires (deferred handshake) | true @50ms | **never (5000ms)** | true @0ms |
| `removeHandshakeCompletedListener` (unregistered) | `IllegalArgumentException` | silently accepted | `IllegalArgumentException` |
| listener fires (renegotiation) | true @50ms | never | never — *by design, see below* |

The cipher suite is worth noting: post-fix CratonVM negotiates **the same suite
as stock HotSpot**, which is what shows the TLS 1.2 pin is really in effect on
the wire rather than merely being reported correctly. Before the fix, the
reported suite (`TLS_AES_128_GCM_SHA256`) was a hard-coded fallback string —
see finding 4.

### 1. `HandshakeCompletedListener` was accepted and never invoked

`addHandshakeCompletedListener` / `removeHandshakeCompletedListener` had
originally no native registration at all on `javax/net/ssl/SSLSocket` (an
abstract class with no bytecode), so calling either threw
`AbstractMethodError`. That was patched into an **inert no-op pair**, which
stopped the crash but left a worse contract: a listener could be registered
and was then never invoked, on any path, for any handshake — including the
ordinary initial handshake, which has nothing to do with renegotiation. Any
application that learns its session from the event (a common JSSE idiom) got
silence.

Fixed: listeners are stored per socket and a real
`javax.net.ssl.HandshakeCompletedEvent` is constructed and dispatched when a
handshake genuinely completes (`new13_fire_handshake_completed`, called from
`ensure_layered_handshake_started`).

Two implementation notes worth keeping:

* The listeners are held as **GC global roots**
  (`add_global_root`/`resolve_global_root`), not raw `ObjectRef`s. They are
  stored by one native call and consumed by a later one, across which a moving
  collection relocates them — the exact stale-reference hazard that
  `add_global_root`'s doc comment exists for. `close()` releases them.
* The event is built with `new_object_initialized` on the **real** JDK
  `HandshakeCompletedEvent` class, so `getSource()` / `getSession()` /
  `getCipherSuite()` work through real bytecode rather than a synthetic
  stand-in with hand-maintained field indices.

**Known deviation:** stock JSSE dispatches this from a fresh thread named
`HandshakeCompletedNotify-Thread`
(`sun.security.ssl.TransportContext.finishHandshake`); CratonVM delivers
synchronously on the thread that completed the handshake. A listener therefore
observes a different `Thread.currentThread().getName()`, and a listener that
blocks blocks the handshaking call. The event contents are identical.

`removeHandshakeCompletedListener` now also throws `IllegalArgumentException`
for a listener that was never added, matching JSSE (verified against HotSpot).

### 2. `SSLSocket.getSession()` returned `null`

JSSE guarantees `getSession()` is never null. On the
`createSocket(String, int)` path CratonVM returned null, because
`getSession()` read the stored session field raw and that write is dropped by
the field-layout guard — `alloc_concurrent_synthetic` sizes the object with
the **real** `javax/net/ssl/SSLSocket` layout, and a write whose slot type
does not match is silently discarded. This is the same hazard that already
forced the tls id into `net_phase_e`'s side table, documented in
`new13_finish_socket`'s own FIX comment.

Fixed: `new13_resolve_socket_session` treats the field as a fast path and
falls back to rebuilding the session from the live stream via the id that
`new13_resolve_tls_id` resolves reliably.

This also fixed `getEnabledProtocols()`, which read the same field and
**fabricated `"TLSv1.3"`** whenever it came back null — a guess presented as a
fact, independent of what had actually been negotiated.

### 3. A version-pinned `SSLContext.getInstance(...)` was ignored

`SSLContext.getInstance("TLSv1.2")` validated the protocol string, stored it
on the SSLContext object, and then never consulted it again. The resulting
socket negotiated TLS 1.3.

That is not cosmetic. The two protocols differ behaviourally — TLS 1.3 has no
renegotiation and no post-handshake `HandshakeCompletedEvent` — and `TestSsl`
pins 1.2 for exactly that reason, with a comment saying so. Silently upgrading
the connection changes the semantics the caller asked for.

Fixed on all three client paths, which matters because they do not share code:

1. `phases_late::ssl_security`'s native-tls connector (`max_protocol_version`),
2. the deferred layered socket (`set_pending_layered_socket_protocols`),
3. **`net_phase_e`'s `SSLSocketFactory.createSocket(String, int)`** — which
   registers the same `(class, method, descriptor)` triple *later* and
   therefore **wins at runtime** in the real-JDK build.

Point 3 is the trap: the first attempt fixed only the `phases_late` copy and
the probe showed **no change at all**, because that copy is dead for this
overload. `getSupportedProtocols`/`getSession` are not duplicated on
`javax/net/ssl/SSLSocket`, but `createSocket` is.

Only the TLS 1.2 ceiling is honoured. `"TLSv1"`/`"TLSv1.1"` would need a
ceiling below the deliberate `min_protocol_version(Tlsv12)` floor, which is a
security posture, not an oversight — see `new13_ctx_max_protocol`.

### 4. Bonus: a session lookup that silently missed its id space

A rustls-backed socket carries `RUSTLS_SOCK_ID_BASE + rid`, but
`rustls_session_info` is keyed by the raw `rid`. Querying it with the offset
id missed every time and fell through to a hard-coded
`("TLSv1.3", "TLS_AES_128_GCM_SHA256")` pair, reported as if negotiated. Found
because the probe printed a cipher suite the connection had not agreed on.

## What is NOT fixed, and why

**Client-initiated TLS 1.2 renegotiation is not supported and will not be.**

`TestSsl.testClientInitiatedRenegotiation[JSSE]` therefore still fails. This is
a property of the TLS backend, not a gap waiting to be closed:

* CratonVM's TLS is rustls (vendored fork at
  `native-builtins/vendor/rustls-cbc`), on both the client
  (`SSLSocket`) and the server (`SSLEngine`) side of this test.
* rustls does not implement TLS 1.2 renegotiation, and its own manual
  (`vendor/rustls-cbc/src/manual/tlsvulns.rs`) lists that omission as its
  **mitigation** for CVE-2009-3555 and for 3SHAKE. `common_state.rs` actively
  refuses a renegotiation request with a `no_renegotiation` warning alert.
* Client-initiated renegotiation specifically is the DoS/request-splicing
  vector that Tomcat Native disables by default — `TesterSupport
  .isClientRenegotiationSupported` returns `false` for it for that reason. It
  returns `true` for "JSSE" as a hard-coded implementation-name check, not a
  capability probe, which is why this test expects it of CratonVM.

Firing the completion event anyway on a `startHandshake()` over an established
connection was considered and rejected: it would tell the application that
fresh key material had been derived when none had. `startHandshake()` on an
established connection is a no-op that returns normally (the connection is
genuinely handshaked and healthy), and no event is emitted.

Implementing RFC 5746 renegotiation in the vendored fork was explicitly
weighed and declined — it would re-introduce, into every CratonVM TLS
connection, the attack surface rustls documents itself as immune to, to make
one test green.

## Regression check

Six TLS classes, one process per class, pre-fix vs post-fix binary on the same
fixture and host:

| class | pre-fix | post-fix |
|---|---|---|
| `TestSsl` | `Tests run: 21, Failures: 1` | `Tests run: 21, Failures: 1` |
| `TestCustomSsl` | `OK (1 test)` | `OK (1 test)` |
| `TestCustomSslTrustManager` | `OK (9 tests)` | `OK (9 tests)` |
| `TestSSLHostConfigProtocol` | `OK (12 tests)` | `OK (12 tests)` |
| `TestSslHandshakeFailure` | `OK (1 test)` | `OK (1 test)` |
| `TestClientCert` | `Tests run: 18, Failures: 1` | `Tests run: 18, Failures: 1` |

`TestClientCert`'s single failure is the pre-existing `testClientCertPostZero`,
which needs real renegotiation — same by-design cause as this doc's residual,
already recorded under index group 21. No class changed state.

## Sibling flake, still not a defect

The original doc recorded `testPost[JSSE]` failing once in four pre-fix runs
and asked that a future `Failures: 2` not be read as a regression. That
mattered here, because the first post-fix `TestSsl` run *did* come back
`Failures: 2`.

It is the flake. Interleaved A,B,A,B repeats of the whole class:

| binary | run 1 | run 2 | run 3 |
|---|---|---|---|
| pre-fix | reneg only | reneg only | **testPost** + reneg |
| post-fix | **testPost** + reneg | reneg only | reneg only |

One `testPost` failure on **each** binary — symmetric, so it is not caused by
this change. The failing thread is 1 of 8 concurrent POSTers, dying at connect
with `TLS connect: connection closed by peer during handshake`; a genuine
protocol or cipher incompatibility would fail all 8 deterministically.

Independently, the version pin provably cannot reach this test:
`testPost` builds its client through `TesterSupport.configureClientSsl()`,
which uses `SSLContext.getInstance("TLSv1.3")` whenever `TLSV13_AVAILABLE`
(true on CratonVM, since its `getInstance` accepts `"TLSv1.3"`), and
`new13_ctx_max_protocol` returns `None` for that name — no ceiling, unchanged
behaviour.

## Related

* `docs/internal/fixed-suite-bugs/tomcat/tls-hostname-verification-regression-FIXED.md`
  — the defect this one was split out of.
* `docs/known-issues/tomcat/testsslhostconfigcompat-testhostec-read-timeout-20260801.md`
  — a separate OPEN doc from the same split; untouched here.
