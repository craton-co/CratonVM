# Tomcat suite — current non-passed set

| | |
|---|---|
| **Measured** | 2026-08-22, `dev@652956429` + the WebSocket fix (`30b8d5b2e`), Azure Linux, real JDK 25, default collector, `-Xmx2g`, JIT on |
| **Method** | complete 640-class suite, 4 shards, 300 s cap; then **every non-passed class re-run serially at a 900 s cap** |
| **Sweep** | 599 PASS / 16 FAIL / 23 HANG / 2 CRASH |
| **After the serial re-run** | **12 FAIL · 2 stuck** — everything else is the 300 s cap or shard contention |

The serial re-run is what this page reports, because the sharded numbers conflate
three different things: a real failure, a class that needs more than 300 s, and a
class that lost a race with a neighbouring shard. Every row below is one
observation per class with nothing else of ours running.

**How large that third category is, measured rather than assumed.** Between the
two sweeps on this page's two binaries, 15 classes moved into the non-passed set
that had passed before — including two `rc=139` SIGSEGVs. Every one of them
**passes when run alone**: the two crashers pass on BOTH binaries, and the other
13 pass with walls of 148–349 s against a 300 s cap. None was a regression. Do
not read a sharded non-pass as a defect without re-running it alone.

Non-WebSocket rows below were serially confirmed on `652956429`; the two binaries
differ by one line in the async-socket read path.

## 12 classes are the 300 s cap, not defects

They PASS when given room. Nothing to investigate; they are listed so the sharded
`HANG` count is not read as 13 defects.

| class | wall | result |
|---|---:|---|
| `jasper.compiler.TestGenerator` | 800 s | OK (85 tests) |
| `jasper.optimizations.TestELInterpreterTagSetters` | 501 s | OK (48 tests) |
| `catalina.nonblocking.TestNonBlockingAPI` | 403 s | OK (44 tests) |
| `catalina.startup.TestHostConfigAutomaticDeploymentModification` | 348 s | OK (19 tests) |
| `el.TestELInJsp` | 283 s | OK (25 tests) |
| `catalina.startup.TestHostConfigAutomaticDeploymentAddition` | 279 s | OK (19 tests) |
| `jasper.compiler.TestJspConfig` | 224 s | OK (18 tests) |
| `jasper.compiler.TestEncodingDetector` | 222 s | OK (22 tests) |
| `jasper.compiler.TestJspDocumentParser` | 222 s | OK (22 tests) |
| `catalina.startup.TestHostConfigAutomaticDeploymentUpdateWarOffline` | 196 s | OK (8 tests) |
| `catalina.manager.TestHostManagerWebapp` | 29 s | OK — FAILed in the sweep, so also flaky |

The wall itself is the embedded-server deployment throughput issue tracked in
`04-embedded-server-throughput-wall-OPEN.md` — a performance problem, not a
correctness one.

## 1. WebSocket — FIXED 2026-08-22, all 19 now pass

The largest cluster in the previous revision of this page is gone. `aio_asc_read`
— the `CompletionHandler` form of `AsynchronousSocketChannel.read` — indexed a
private slot raw instead of through `aio_base`, so on a real JDK channel object
it addressed an unrelated field and every frame read failed
`read: bad fd for tcp clone`. Tomcat's `WsFrameClient` reads that as a dropped
connection and closed the session immediately after `onOpen`.

19 of 19 pass, and the walls collapse with them (`server.TestClassLoader` 902 s
stuck → 4 s; `TestWsWebSocketContainer` 382 s → 20 s). Record:
`fixed-suite-bugs/websocket-19-class-cluster-was-one-raw-private-slot-FIXED-20260822.md`
(plain text — that tree is stripped from public history).

Worth carrying forward: the handler form is NIO2's main read path, so anything
driving a concrete `AsynchronousSocketChannel` through a `CompletionHandler` hit
this. Only the WebSocket classes were measured; a NIO2-connector sweep has not
been done.

## 2. Tribes group communication — 4 FAIL

`tribes.test.channel.TestDataIntegrity` (2 of 5), `TestMulticastPackages`
(1 of 5), `TestRemoteProcessException` (1 of 1), `TestUdpPackages` (6 of 6).

Multicast on this host is the long-standing environmental gap recorded in
[tribes-multicast-family-still-environmental.md](tribes-multicast-family-still-environmental.md).
**Not re-confirmed against HotSpot in this run** — do that before treating any of
these as a VM defect.

## 3. HTTP/2 — 3 FAIL + 1 stuck (the only class left that does not finish)

| class | result |
|---|---|
| `coyote.http2.TestFlowControl` | 2 of 2 fail |
| `coyote.http2.TestHttp2Section_5_1` | 2 of 28 fail |
| `coyote.http2.TestHttp2Section_6_1` | 4 of 14 fail |
| `coyote.http2.TestHttp2Section_8_2` | stuck at the 900 s cap |

Not diagnosed. `TestHttp2Section_8_2` is now the only class in the suite that
does not finish given 900 s — `server.TestClassLoader`, previously the other
one, was the WebSocket defect and passes in 4 s.

## 4. TLS — 2 FAIL, both accepted limits

| class | failing test |
|---|---|
| `util.net.TestClientCert` | `testClientCertPostZero` (1 of 18) |
| `util.net.TestSsl` | `testClientInitiatedRenegotiation` (1 of 21) |

Both follow from rustls implementing no TLS 1.2 renegotiation, and neither is
worth "fixing" as it stands — see
[ssl-renegotiation-emulation-limits.md](ssl-renegotiation-emulation-limits.md).
`TestSsl.testPost` additionally flakes under load, documented there.

## 5. Class-loader leak detection — 2 FAIL

`catalina.loader.TestWebappClassLoaderMemoryLeak` and
`TestWebappClassLoaderExecutorMemoryLeak`, 1 of 1 each. Not diagnosed. Both
assert that a stopped webapp's class loader becomes unreachable, so they are
sensitive to any reference this VM retains and HotSpot does not — a GC-rooting
question, not a Tomcat one.

## 6. Individually undiagnosed — 3 FAIL

| class | failing |
|---|---|
| `catalina.connector.TestSendFile` | 1 of 2 |
| `catalina.core.TestAsyncContextImpl` | 4 of 70 |
| `catalina.manager.TestManagerWebapp` | 1 of 4 |

## Reproduction

Whole suite, one shard of four (Linux):

```bash
TC_ROOT=/path/to/apps/tomcat \
CP_FILE=$TC_ROOT/.suite/cp-linux-fixed.txt \
JAVA_HOME25=/path/to/jdk-25 \
CRATONVM_EXE=/path/to/cratonvm \
HTTPD_PATH=/usr/sbin/apache2 \
TIMEOUT_SEC=300 \
apps/tomcat-suite-runner/run-tomcat-suite.sh craton 0 4 <run-name>
```

Two cautions that cost real time if ignored:

* **The OCSP classes take an exclusive `flock`** on
  `test/org/apache/tomcat/util/net/ocsp/ocsp-responder.lock`. Two concurrent
  CratonVM runs of any OCSP class serialise, and the loser scores an `rc=124`
  indistinguishable from a hang. On a shared fixture a neighbouring session is
  enough to cause it.
* **Do not read a sharded `HANG` as stuck.** Twelve of this run's non-passes did
  not survive a serial re-run at a wider cap.
