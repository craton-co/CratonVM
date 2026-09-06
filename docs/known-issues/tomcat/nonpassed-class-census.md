# Tomcat suite — current non-passed set

| | |
|---|---|
| **Measured** | 2026-08-23, `dev@652956429` + the WebSocket fix (`30b8d5b2e`), Azure Linux, real JDK 25, default collector, `-Xmx2g`, JIT on |
| **Method** | complete 640-class suite, **2 shards, 600 s cap**; then **every non-passed class re-run serially at a 1200 s cap** |
| **Sweep** | 623 PASS / 15 FAIL / 2 HANG — no CRASH, no NOSUMMARY |
| **After the serial re-run** | **16 FAIL, nothing stuck** — i.e. **624 of 640** |

The serial re-run is what this page reports. A sharded number conflates a real
failure, a class that needs more wall than the cap allows, and a class that lost
a race for a core; every row below is one observation per class with nothing else
of ours on the host.

## The shard count is part of the measurement

The **same binary** and the same 640 classes scored **599 PASS at 4 shards with a
300 s cap** and **623 at 2 shards with a 600 s cap**. Nothing about the VM
differs between those two numbers. The host has 8 cores and is shared; during the
first sweep roughly 3 of them belonged to other people's builds, so 4 shards
oversubscribed it and twelve classes whose walls had been 156–275 s all landed on
exactly 300 — the cap.

That first sweep was initially read as a 15-class regression against the
WebSocket fix, complete with two `rc=139` SIGSEGVs. It was not one. What settled
it was an **interleaved** serial A/B — one class at a time, arms alternated per
class so host drift could not line up with one binary:

| class | pre-fix | post-fix | the same two classes, sharded |
|---|---:|---:|---|
| `jasper.tagplugins.jstl.core.TestForEach` | 41 s | **28 s** | 30 → 143 |
| `jasper.tagplugins.jstl.core.TestOut` | 23 s | 24 s | 43 → 183 |
| `jasper.runtime.TestJspContextWrapper` | 31 s | 33 s | 55 → 214 |
| `tomcat.util.descriptor.web.TestWebXml` | 35 s | 62 s | 65 → 221 |
| `jasper.runtime.TestPageContextImpl` | 41 s | **38 s** | 79 → 246 |
| `catalina.webresources.TestCachedResource` | 68 s | **67 s** | 149 → 253 (FAIL) |
| `jasper.compiler.TestCompiler` | 106 s | 124 s | 156 → 300 (HANG) |
| `naming.TestEnvEntry` | 133 s | 175 s | 184 → 301 (HANG) |

All sixteen runs PASS, three of the eight are *faster* on the newer binary, and
every serial wall is far below both sharded walls. The two SIGSEGVs did not recur
in either sweep after the first; both classes pass on both binaries when run
alone. `TestCachedResource` is the shape to remember — it asserts on cache expiry
timing, so contention makes it **FAIL**, not merely time out, and a wider cap
would not have saved it.

`results.csv` therefore carries a 5th column, the host's 1-minute load average as
each class finished. Read it before reading a `HANG`, and before reading a `FAIL`
from any class that asserts on timing.

## Nothing in the suite fails to finish

Given 1200 s, every class terminates. `coyote.http2.TestHttp2Section_8_2` — the
last class this page called stuck — runs **6658 tests in 839 s** and fails one of
them. It is slow, and it is a real failure, but it is not a hang.
`jasper.compiler.TestGenerator` is the one class that still needs more than 600 s
(**868 s**, `OK (85 tests)`); that wall is the embedded-server deployment
throughput issue in `04-embedded-server-throughput-wall-OPEN.md`, a performance
problem, not a correctness one.

## The 16, by family

### 1. Tribes group communication — 4

| class | failing |
|---|---|
| `tribes.test.channel.TestUdpPackages` | 6 of 6 |
| `tribes.test.channel.TestDataIntegrity` | 2 of 5 |
| `tribes.test.channel.TestMulticastPackages` | 1 of 5 |
| `tribes.test.channel.TestRemoteProcessException` | 1 of 1 |

Multicast on this host is the long-standing environmental gap recorded in
[tribes-multicast-family-still-environmental.md](tribes-multicast-family-still-environmental.md).
**Still not re-confirmed against HotSpot** — do that before treating any of these
as a VM defect. It is the cheapest open item on this page.

### 2. HTTP/2 — 4

| class | failing |
|---|---|
| `coyote.http2.TestFlowControl` | 2 of 2 |
| `coyote.http2.TestHttp2Section_6_1` | 4 of 14 |
| `coyote.http2.TestHttp2Section_5_1` | 2 of 28 |
| `coyote.http2.TestHttp2Section_8_2` | 1 of 6658 |

Not diagnosed.

### 3. TLS — 2, both accepted limits

| class | failing test |
|---|---|
| `util.net.TestClientCert` | `testClientCertPostZero` (1 of 18) |
| `util.net.TestSsl` | `testClientInitiatedRenegotiation` (1 of 21) |

Both follow from rustls implementing no TLS 1.2 renegotiation, and neither is
worth "fixing" as it stands — see
[ssl-renegotiation-emulation-limits.md](ssl-renegotiation-emulation-limits.md).
`TestSsl.testPost` additionally flakes under load, documented there.

### 4. Class-loader leak detection — 2 — **FIXED 2026-09-05**

`catalina.loader.TestWebappClassLoaderMemoryLeak` and
`TestWebappClassLoaderExecutorMemoryLeak`, 1 of 1 each. **Both now PASS.**

This entry used to read "Not diagnosed … a GC-rooting question, not a Tomcat
one". That framing was wrong twice over. Neither class asserts anything about
the loader becoming unreachable — they assert that Tomcat's
`clearReferencesThreads` actually STOPS a leaked `java.util.Timer` thread (and
a `ThreadPoolExecutor`), so no GC rooting is involved at all. And both were
diagnosed: two stacked defects, each hiding the next.

1. `Thread` did not inherit the parent's `contextClassLoader`, so Tomcat's
   `if (ccl == this)` gate never fired and the stop was never attempted. Fixed
   2026-06-23 (`CRATONVM_INHERIT_THREAD_CCL`).
2. With the gate passing, the reflective stop threw
   `InaccessibleObjectException: module java.base does not "opens java.util" to
   org.apache.tomcat.catalina` — even though the harness passes
   `--add-opens java.base/java.util=ALL-UNNAMED`. A modular jar on the CLASS
   path was being labelled with the module its `module-info.class` declares
   instead of the unnamed module, which an `ALL-UNNAMED` open cannot reach.
   Fixed 2026-09-05 (`CRATONVM_CLASSPATH_JAR_UNNAMED_MODULE`).

Defect 2 is why the Linux suite scored these red while the Windows suite scored
them green on the same commit: the Linux classpath is built from
`output/build/lib/*.jar` (and `catalina.jar` carries a `module-info.class`),
the Windows one from the exploded `output/classes` directory, which carries
none. **A class that passes on one host's classpath and fails on the other's is
not necessarily a platform difference — check the classpath SHAPE first.**

### 5. Individually undiagnosed — 4

| class | failing |
|---|---|
| `catalina.core.TestAsyncContextImpl` | 4 of 70 |
| `catalina.connector.TestSendFile` | 1 of 2 |
| `catalina.manager.TestManagerWebapp` | 2 of 4 on 2026-08-25 (1 of 4 on 08-14) — `testServlets` times out reading `GET /manager/jmxproxy` (`:147`), `testJsps` fails `assertTrue(body.contains("Sessions Administration"))` on `/manager/html/sessions` (`:697`). Both reproduce on pristine `dev`; HotSpot is `OK (4 tests)` in 19.6 s. **Not deploy timing** — the deploy itself finishes in 15.5 s and `testBug57700`/`testDeploy` both pass, which is what closed `webapp-deploy-annotation-scan-interpreted-226x-CLOSED-20260825.md` |
| `catalina.manager.TestHostManagerWebapp` | 1 of 1 — **flaky**: serially `OK` (29 s) on 08-22, `FAILURES` (31 s) on 08-23 |

## WebSocket — FIXED 2026-08-22, all 19 pass

The largest cluster on the previous revision of this page is gone and stayed
gone across both sweeps. `aio_asc_read` — the `CompletionHandler` form of
`AsynchronousSocketChannel.read` — indexed a private slot raw instead of through
`aio_base`, so on a real JDK channel object it addressed an unrelated field and
every frame read failed `read: bad fd for tcp clone`. Tomcat's `WsFrameClient`
reads that as a dropped connection and closed the session immediately after
`onOpen`. Record:
`websocket-19-class-cluster-was-one-raw-private-slot-FIXED-20260822.md`
(plain text — that tree is stripped from public history).

Worth carrying forward: the handler form is NIO2's main read path, so anything
driving a concrete `AsynchronousSocketChannel` through a `CompletionHandler` hit
this. Only the WebSocket classes were measured; **a NIO2-connector sweep has not
been done.**

## The G1 arm's 15 CRASH classes — **FIXED 2026-09-05**

This page measures the DEFAULT collector, so the 15 CRASH classes a 2026-09-05
three-collector run found on the **G1** arm (and nowhere else: 0 in the same
run's Generational and ZGC arms) were never in its scope. They are fixed, and
the reason is worth carrying forward here because it is not a Tomcat fact at
all:

> `CRATONVM_G1_PARALLEL_EVAC` is **on by default**, and every header screen the
> serial evacuator gained in 2026-08/09 — the per-candidate plausibility check,
> the per-holder element clamp, the "a root that is not an object start is not
> evacuated" rule — had been added to the SERIAL arm only. The hardening was
> landing on the code that does not run.

Same-binary A/B on the Azure fixture, six interleaved repetitions of
`catalina.startup.TestHostConfigAutomaticDeploymentXmlExternalWarXml` under
`-XX:+UseG1GC`, with `CRATONVM_G1_PARALLEL_EVAC_SCREEN=0` as the only
difference: **3 CRASH / 6 with the screens off, 0 CRASH / 6 with them on.**
Across all 15 classes at n=1: 4 CRASH → 0.

**Run the suite once per collector.** Two of the three collectors were clean on
the exact commit where G1 lost 15 classes to a defect that had been introduced
by a fix landing on one arm of a two-arm path.

## Reproduction

Whole suite, one shard of two (Linux). Pick the shard count from the cores you
actually have — see the header comment in the runner:

```bash
TC_ROOT=/path/to/apps/tomcat \
CP_FILE=$TC_ROOT/.suite/cp-linux-fixed.txt \
JAVA_HOME25=/path/to/jdk-25 \
CRATONVM_EXE=/path/to/cratonvm \
HTTPD_PATH=/usr/sbin/apache2 \
TIMEOUT_SEC=600 \
apps/tomcat-suite-runner/run-tomcat-suite.sh craton 0 2 <run-name>
```

Two cautions that cost real time if ignored:

* **The OCSP classes take an exclusive `flock`** on
  `test/org/apache/tomcat/util/net/ocsp/ocsp-responder.lock`. Two concurrent
  CratonVM runs of any OCSP class serialise, and the loser scores an `rc=124`
  indistinguishable from a hang. On a shared fixture a neighbouring session is
  enough to cause it.
* **Do not read a sharded non-pass as a defect.** Twenty-four of the 4-shard
  run's non-passes were the harness, and one of them was a `FAIL` rather than a
  timeout.
