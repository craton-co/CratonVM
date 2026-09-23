# Tomcat suite — class-by-class non-passed census, 2026-09-22 (regression against the 2026-08-23 baseline)

| | |
|---|---|
| **Measured** | 2026-09-22, Azure Linux, commit `1c8a3404c` ("Merge remote-tracking branch 'origin/dev' into jit/perf-round-20260920 (2)"), `--jdk-only`-default, real JDK 25, all defaults (no GC/JIT overrides), 1 shard (`craton 0 1`), no per-class re-run pass |
| **Census** | **PASS=589 / FAIL=24 / HANG=26** (639/640, one class excluded elsewhere) |
| **Baseline** | [`nonpassed-class-census.md`](nonpassed-class-census.md), 2026-08-23, commit `652956429` + the WebSocket fix (`30b8d5b2e`) — 623 PASS / 16 FAIL after a serial re-run (624/640) |
| **Source** | `apps/tomcat/.suite/results/classbyclass-default-20260922/shard-0/results.csv` (columns: `class,rc,seconds,status,loadavg1,panic`) |

This run is a single sharded sweep, **not** the serial-re-run-of-non-passed methodology the 2026-08-23 baseline used (its own page notes a sharded number conflates real failures with contention artifacts — see its "The shard count is part of the measurement" section). Read the totals below as a first pass, not a final verdict, the same way that page's own history treats its first sharded sweep. That said: **50 non-passed vs. the baseline's 16 is a large enough gap that it is unlikely to be entirely re-run noise**, especially given one specific cluster below directly contradicts a fix this project already recorded as landed.

## The headline finding: the WebSocket cluster is back, at nearly idle load

| class | seconds | loadavg1 |
|---|---:|---:|
| `org.apache.tomcat.websocket.TestWebSocketFrameClient` | 300 | 0.05 |
| `org.apache.tomcat.websocket.TestWebSocketFrameClientSSL` | 300 | 0.00 |
| `org.apache.tomcat.websocket.TestWsPingPongMessages` | 300 | 0.07 |
| `org.apache.tomcat.websocket.TestWsWebSocketContainer` | 300 | 0.00 |
| `org.apache.tomcat.websocket.TestWsWebSocketContainerGetOpenSessions` | 300 | 0.07 |
| `org.apache.tomcat.websocket.TestWsWebSocketContainerSSL` | 300 | 0.02 |
| `org.apache.tomcat.websocket.TestWsWebSocketContainerTimeoutClient` | 300 | 0.00 |
| `org.apache.tomcat.websocket.server.TestWsRemoteEndpointImplServerDeadlock` | 300 | 0.19 |
| `org.apache.tomcat.websocket.server.TestWsServerContainer` | 300 | 0.00 |

All 9 hit the full 300s cap (`rc=124`), and every single one has a `loadavg1` under 0.2 — the host was essentially idle when these ran, which is exactly the condition the baseline page's own methodology uses to rule out contention as the cause. The baseline page's own "WebSocket — FIXED 2026-08-22, all 19 pass" section is explicit: the root cause was `aio_asc_read` (the `CompletionHandler` form of `AsynchronousSocketChannel.read`) indexing a private slot raw instead of through `aio_base`, addressing an unrelated field on a real JDK channel object, so every frame read failed with `read: bad fd for tcp clone` — and that every class in the cluster passed after the fix, both times it was swept.

**This reads as a regression, not noise.** Two honest caveats before treating it as confirmed: (1) this run has not had the baseline's serial re-run pass applied, so a load-independent explanation (a hang genuinely reproducing at n=1, not contention) still needs a direct single-class repro to fully rule out an unrelated new trigger; (2) `--jdk-only` becoming the project's default between the baseline measurement and this one is itself a plausible new variable — the fix and this whole 9-class cluster live in NIO2/`AsynchronousSocketChannel`, exactly the kind of real-JDK-object internals `--jdk-only` changes the resolution path for. Reproduce one class directly (`TestWsWebSocketContainer` is probably the cheapest) before filing this as a confirmed re-open of the closed bug.

## HANG — remaining 17, by family

### Jasper/JSP compiler — 6

| class | seconds | loadavg1 |
|---|---:|---:|
| `org.apache.jasper.compiler.TestEncodingDetector` | 300 | 2.74 |
| `org.apache.jasper.compiler.TestGenerator` | 300 | 3.15 |
| `org.apache.jasper.compiler.TestJspConfig` | 300 | 1.31 |
| `org.apache.jasper.compiler.TestJspDocumentParser` | 300 | 1.12 |
| `org.apache.jasper.compiler.TestParser` | 300 | 1.98 |
| `org.apache.jasper.compiler.TestValidator` | 300 | 1.21 |

Plus `org.apache.jasper.optimizations.TestELInterpreterTagSetters` (300s, loadavg 1.07) and `org.apache.el.TestELInJsp` (300s, loadavg 3.05) — adjacent EL/JSP machinery, plausibly the same cluster. Low-to-moderate load throughout; not obviously contention. The baseline page's own history already flagged `jasper.compiler.TestGenerator` as needing >600s for a *legitimate* (non-hang) reason — an embedded-server deployment throughput wall, not a correctness bug — so before reading any of these six as hangs, check whether they're the same "just needs a wider cap" shape rather than a real stall.

### HostConfig automatic deployment — 5

| class | seconds | loadavg1 |
|---|---:|---:|
| `org.apache.catalina.startup.TestHostConfigAutomaticDeploymentAddition` | 300 | 3.24 |
| `org.apache.catalina.startup.TestHostConfigAutomaticDeploymentDeleteB` | 300 | 8.19 |
| `org.apache.catalina.startup.TestHostConfigAutomaticDeploymentDeleteC` | 300 | 14.39 |
| `org.apache.catalina.startup.TestHostConfigAutomaticDeploymentModification` | 300 | 14.79 |
| `org.apache.catalina.startup.TestHostConfigAutomaticDeploymentUpdateWarOffline` | 300 | 6.18 |

Load averages climb across this family (3.2 → 14.8) — worth checking whether these ran back-to-back and the host was genuinely getting loaded down by the suite's own concurrency, not by an external process. `TestHostConfigAutomaticDeploymentDeleteB`/`DeleteC` were seen stalling earlier in this same session's investigation before these results were pulled — consistent with this cluster being real, not incidental.

### Individually undiagnosed / previously known — 6

| class | seconds | loadavg1 | note |
|---|---:|---:|---|
| `org.apache.catalina.servlets.TestDefaultServletRfc9110Section13` | 300 | 5.32 | |
| `org.apache.catalina.valves.TestLoadBalancerDrainingValve` | 301 | 7.63 | |
| `org.apache.coyote.http2.TestHttp2Section_8_2` | 300 | 16.92 | already on the baseline page as "still undiagnosed" — a 6658-test class that needed 839s to run clean in the baseline's serial re-run; this sharded run's 300s cap is very likely just the cap being too short again for the same reason, not a new finding |
| `org.apache.coyote.http2.TestStreamProcessor` | 300 | 3.16 | |
| `org.apache.tomcat.util.net.TestSsl` (also appears in FAIL below via a different sub-test) | — | — | see FAIL section |

## FAIL — 24, by family

### TLS/SSL — 4

| class | tests | seconds |
|---|---:|---:|
| `jakarta.el.TestOptionalELResolver` | 1 | 3.02 |
| `org.apache.catalina.valves.TestSSLValve` | 25 | 5.42 |
| `org.apache.tomcat.util.net.TestClientCert` | 4 | 0.96 |
| `org.apache.tomcat.util.net.TestSsl` | 151 | 1.64 |

`TestClientCert` and `TestSsl` are already on the baseline page as accepted limits (rustls implements no TLS 1.2 renegotiation) — see [`ssl-renegotiation-emulation-limits.md`](ssl-renegotiation-emulation-limits.md). `TestSSLValve` (25 tests) is new to this page; not yet checked against that same limitation.

### Tribes multicast — 3, already a known environmental gap

| class | tests |
|---|---:|
| `org.apache.catalina.tribes.test.channel.TestMulticastPackages` | 51 |
| `org.apache.catalina.tribes.test.channel.TestRemoteProcessException` | 99 |
| `org.apache.catalina.tribes.test.channel.TestUdpPackages` | 88 |

See [`tribes-multicast-family-still-environmental.md`](tribes-multicast-family-still-environmental.md) — this host's multicast networking, not a VM defect. Still not re-confirmed against HotSpot at these exact counts; cheapest item on this page to close out.

### Manager webapp — 2, previously flagged flaky

| class | tests |
|---|---:|
| `org.apache.catalina.manager.TestHostManagerWebapp` | 47 |
| `org.apache.catalina.manager.TestManagerWebapp` | 210 |

The baseline page already called `TestHostManagerWebapp` "flaky" (serially OK once, FAILURES once) and `TestManagerWebapp` partially diagnosed (`testServlets`/`testJsps` timing-sensitive, reproduces on pristine dev, not deploy-timing related). Consistent with prior history rather than new.

### Expression Language (EL) — 3

| class | tests |
|---|---:|
| `org.apache.el.TestValueExpressionImpl` | 1 |
| `org.apache.el.lang.TestELSupport` | 1 |
| `jakarta.el.TestOptionalELResolver` | 1 (listed above under TLS by coincidence of grouping — actually EL, not TLS; not yet cross-checked for a shared cause with the other two) |

### Individually undiagnosed — 12

| class | tests | seconds |
|---|---:|---:|
| `org.apache.catalina.core.TestDefaultInstanceManager` | 7 | 3.94 |
| `org.apache.catalina.loader.TestVirtualContext` | 1 | 1.67 |
| `org.apache.catalina.nonblocking.TestNonBlockingAPI` | 51 | 1.75 |
| `org.apache.catalina.realm.TestJNDIRealm` | 6 | 1.64 |
| `org.apache.catalina.realm.TestJNDIRealmIntegration` | 1 | 1.64 |
| `org.apache.catalina.session.TestPersistentManager` | 3 | 3.78 |
| `org.apache.catalina.startup.TestTomcatStandalone` | 3 | 9.94 |
| `org.apache.catalina.startup.TestWebappServiceLoader` | 8 | 9.18 |
| `org.apache.catalina.valves.TestCrawlerSessionManagerValve` | 13 | 6.38 |
| `org.apache.catalina.valves.rewrite.TestResolverSSLComponents` | 1 | 4.59 |
| `org.apache.coyote.TestRequest` | 6 | 4.04 |
| `org.apache.jasper.servlet.TestTldScanner` | 46 | 1.04 |
| `org.apache.juli.TestPerWebappJuliIntegration` | 2 | 1.17 |

None of these twelve are on the 2026-08-23 baseline page — every one is new to this run and none has been investigated yet.

## Open items, in priority order

1. **Reproduce one WebSocket class directly** (`TestWsWebSocketContainer`, cheapest) to settle whether the closed `aio_asc_read` bug has genuinely reopened under `--jdk-only`-default, or whether this is a different new defect in the same area. This is the single highest-value item on this page — it would mean a previously-fixed, previously-verified-clean 9-class cluster is red again.
2. Run the baseline's own serial-re-run methodology (every non-passed class alone, 1200s cap, with the loadavg column) before trusting any of the remaining 41 rows as real — this page is a first sharded pass, exactly the shape the baseline page's own history warns can conflate real failures with a loaded host.
3. Confirm whether the Jasper/compiler cluster (8 classes) is the known embedded-server-deployment throughput wall (just needs a wider cap) rather than 8 new hangs.
4. The 12 individually-undiagnosed FAILs and the HostConfig deployment cluster (5) have no prior history on this page — genuinely new territory, not yet looked at.
