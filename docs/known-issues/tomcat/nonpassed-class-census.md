# Tomcat suite — current non-passed set

| | |
|---|---|
| **Measured** | 2026-08-22, `dev@652956429`, Azure Linux, real JDK 25, default collector, `-Xmx2g`, JIT on |
| **Method** | complete 640-class suite, 4 shards, 300 s cap; then **every non-passed class re-run serially at a 900 s cap** |
| **Sweep** | **595 PASS / 32 FAIL / 13 HANG** — no CRASH, no NOSUMMARY |
| **After the serial re-run** | **31 FAIL · 2 stuck · 12 that pass with headroom** |

The serial re-run is what this page reports, because the sharded numbers conflate
three different things: a real failure, a class that needs more than 300 s, and a
class that lost a race with a neighbouring shard. Every row below is one
observation per class with nothing else of ours running.

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
| `websocket.TestWsSubprotocols` | 42 s | OK — FAILed in the sweep, so also flaky |
| `catalina.manager.TestHostManagerWebapp` | 29 s | OK — FAILed in the sweep, so also flaky |

The wall itself is the embedded-server deployment throughput issue tracked in
`04-embedded-server-throughput-wall-OPEN.md` — a performance problem, not a
correctness one.

## 1. WebSocket — 17 FAIL + 1 stuck (the largest cluster)

Every non-passing class under `org.apache.tomcat.websocket`. HotSpot passes the
sampled four on this host and fixture, so these are CratonVM defects.

Two symptom families, not assumed to share a cause: writes to an already-closed
socket (`write(gathering): Broken pipe`), and `getOpenSessions()` returning only
the calling session (`expected:<3> but was:<1>`, `{pojoA=1, client2=1,
client1=1}`).

→ **[websocket-19-class-cluster-20260822.md](websocket-19-class-cluster-20260822.md)**,
which carries the evidence and **two hypotheses already disproved with
controls**. Read it before starting.

`server.TestClassLoader` is the stuck one (902 s); not established whether it
shares a cause.

## 2. Tribes group communication — 4 FAIL

`tribes.test.channel.TestDataIntegrity` (2 of 5), `TestMulticastPackages`
(1 of 5), `TestRemoteProcessException` (1 of 1), `TestUdpPackages` (6 of 6).

Multicast on this host is the long-standing environmental gap recorded in
[tribes-multicast-family-still-environmental.md](tribes-multicast-family-still-environmental.md).
**Not re-confirmed against HotSpot in this run** — do that before treating any of
these as a VM defect.

## 3. HTTP/2 — 3 FAIL + 1 stuck

| class | result |
|---|---|
| `coyote.http2.TestFlowControl` | 2 of 2 fail |
| `coyote.http2.TestHttp2Section_5_1` | 2 of 28 fail |
| `coyote.http2.TestHttp2Section_6_1` | 4 of 14 fail |
| `coyote.http2.TestHttp2Section_8_2` | stuck at the 900 s cap |

Not diagnosed. `TestHttp2Section_8_2` and `server.TestClassLoader` are the only
two classes in the suite that do not finish given 900 s.

## 4. TLS — 2 FAIL, both accepted limits

| class | failing test |
|---|---|
| `util.net.TestClientCert` | `testClientCertPostZero` (1 of 18) |
| `util.net.TestSsl` | `testClientInitiatedRenegotiation` (1 of 21) |

Both follow from rustls implementing no TLS 1.2 renegotiation, and neither is
worth "fixing" as it stands — see
[ssl-renegotiation-emulation-limits-20260822.md](ssl-renegotiation-emulation-limits-20260822.md).
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
