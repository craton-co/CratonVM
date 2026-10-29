# Tomcat suite — class-by-class non-passed census, 2026-09-22 (regression against the 2026-08-23 baseline)

| | |
|---|---|
| **Measured** | 2026-09-22, Azure Linux, commit `1c8a3404c` ("Merge remote-tracking branch 'origin/dev' into jit/perf-round-20260920 (2)"), `--jdk-only`-default, real JDK 25, all defaults (no GC/JIT overrides), 1 shard (`craton 0 1`), no per-class re-run pass |
| **Census** | **PASS=589 / FAIL=24 / HANG=26** (639/640, one class excluded elsewhere) |
| **Baseline** | `nonpassed-class-census.md`, 2026-08-23, commit `652956429` + the WebSocket fix (`30b8d5b2e`) — 623 PASS / 16 FAIL after a serial re-run (624/640) |
| **Source** | `apps/tomcat/.suite/results/classbyclass-default-20260922/shard-0/results.csv` (columns: `class,rc,seconds,status,loadavg1,panic`) |

> **Progress since this census (2026-09-25).** Individually re-verified on a
> Windows fixture (`apps/tomcat`) with same-fixture HotSpot controls, per
> class:
> - **Fixed**: 12 classes (bootstrap-delegation, proxy modifiers, thread-
>   contention monitoring) — [33](../../internal/fixed-suite-bugs/tomcat/33-classloader-proxy-modifiers-threadcontention-FIXED-20260922.md).
> - **Fixed**: `TestDefaultInstanceManager` (ZGC+Generational class-unload
>   off-by-one); the diagnosed `HttpURLConnection` broken-pipe bug behind
>   `TestNonBlockingAPI` — [34](../../internal/fixed-suite-bugs/tomcat/34-defaultinstancemanager-zgc-generational-classunload-and-httpurlconnection-epipe-FIXED-20260925.md).
>   The class's remaining slowness (not 33's `Util$BufferCache` bug as 34
>   guessed, not a hang as the 2026-10-02 JIT run suggested) was Windows' small
>   blocking socket reads/writes entering the GC-blocking region on every call;
>   fixed 2026-10-04, JIT-on wall 477 s -> 150 s (HotSpot 56 s), under the
>   suite's 300 s class cap —
>   [43](../../internal/fixed-suite-bugs/tomcat/43-testnonblockingapi-jit-slow-windows-socket-blocking-region-FIXED-20261004.md),
>   [44](../../internal/fixed-suite-bugs/tomcat/44-testnonblockingapi-readasync-socket-write-throughput-RETIRED-20261004.md).
> - **Fixed, 2026-10-04**: the 32-class `TestHttpServletDoHeadInvalidWrite*ValidWrite*`
>   JIT-only failures (a callee argument read only by a masked cold throw arm
>   was cleared from the chain snapshot and arrived as `null`) —
>   [42](../../internal/fixed-suite-bugs/tomcat/42-dohead-family-jit-only-masked-cold-throw-arm-pruned-callee-local-FIXED-20261004.md).
> - **Fixed, 2026-10-04/05** (found by the three-GC full run, each HotSpot-passing):
>   `TestNonLoginAndBasicAuthenticator` and `TestSSOnonLoginAndBasicAuthenticator` (unbound `NTLMAuthentication` natives, [45](../../internal/fixed-suite-bugs/tomcat/45-ntlmauthentication-trusted-site-natives-unbound-FIXED-20261004.md));
>   `TestAsyncContextImpl`, the boot-time `Logger` pre-warm that froze the default
>   `LogManager` ([46](../../internal/fixed-suite-bugs/tomcat/46-logmanager-boot-prewarm-froze-default-manager-testasynccontextimpl-FIXED-20261004.md));
>   `TestStartupIPv6Connectors` ([47](../../internal/fixed-suite-bugs/tomcat/47-networkinterface-windows-guid-names-and-scoped-ipv6-inetsocketaddress-FIXED-20261005.md));
>   `TestTomcatStandalone` ([48](../../internal/fixed-suite-bugs/tomcat/48-inetaddress-ipv6-supported-ignored-preferipv4stack-loopback-FIXED-20261005.md));
>   `TestServerInfo` ([49](../../internal/fixed-suite-bugs/tomcat/49-zipfile-first-use-path-slot-map-uninitialised-FIXED-20261005.md)).
> - **Three-GC full run, 2026-10-05** (`cratonvm-tomcatjitonly-g1-12.exe`, JIT on, 651
>   classes per shard, 1800 s class cap, Windows): **generational 642 PASS / 9 non-pass,
>   G1 644 / 7, ZGC 645 / 6**. No DoHead failure and no native crash in any shard.
>   Non-passes, all re-run standalone against HotSpot:
>   `TestMethodPerformance` (HANG on all three; 1.2 G loop iterations at ~8 us each,
>   [jit-native-call-crossing-250ns-threadlocal-get-2us-20261005.md](jit-native-call-crossing-250ns-threadlocal-get-2us-20261005.md));
>   `TestAsyncMessagesPerformance` (all three; sub-millisecond message gaps,
>   [thread-wake-latency-after-idle-3x-hotspot-20261005.md](thread-wake-latency-after-idle-3x-hotspot-20261005.md));
>   `TestDataIntegrity`, `TestRemoteProcessException`, `TestUdpPackages`, `TestMulticastPackages`
>   (Tribes; the first three also **fail on HotSpot** here,
>   [tribes-multicast-family-still-environmental.md](tribes-multicast-family-still-environmental.md));
>   load-only failures that pass standalone with HotSpot-identical results
>   (`TestELParserPerformance`, `TestManagerWebapp`, `TestOcspTimeout`);
>   one intermittent, `TestWsWebSocketContainerTimeoutClient`
>   ([doc 57, retired 2026-10-14](../../internal/fixed-suite-bugs/tomcat/57-testwswebsocketcontainertimeoutclient-intermittent-close-race-bufferoverflow-not-seen-in-160-runs-on-the-windows-channel-layer-RETIRED-20261014.md)).
>   `TestCharsetCachePerformance` and `TestHttp2Section_8_2` PASS under the 1800 s cap.
> - **Three-GC full runs on the later builds, 2026-10-05/06** (same harness, 651 classes per
>   shard, 1800 s cap): `g1-17` (docs 42-50) **generational 644 PASS / G1 641 / ZGC 646**;
>   `g1-22` (adds [51](../../internal/fixed-suite-bugs/tomcat/51-blocking-region-bracket-around-calls-that-cannot-block-udp-receive-30x-hotspot-FIXED-20261005.md)
>   and the monitor/`ThreadLocal`/`inflate` changes) **generational 644 / G1 641 / ZGC 644**.
>   Non-passes of the `g1-22` run, each checked against HotSpot standalone:
>   * HotSpot fails them too here: `TestDataIntegrity`, `TestRemoteProcessException`, `TestUdpPackages` (all three shards).
>   * Open, CratonVM-only: `TestMethodPerformance` (HANG, all three; over the cap by ~3x of compiled-code speed,
>     [page](jit-native-call-crossing-250ns-threadlocal-get-2us-20261005.md)); `TestAsyncMessagesPerformance`
>     (all three; 303-347 of 500 gaps over 0.5 ms, [page](thread-wake-latency-after-idle-3x-hotspot-20261005.md));
>     `TestMulticastPackages` (all three in the suite, 23-109 of 10,000 lost; standalone it now passes 5/8,
>     [page](../../internal/fixed-suite-bugs/tomcat/62-testmulticastpackages-async-send-message-loss-FIXED-20261025.md)).
>   * Timing-bound or near-cap, pass standalone: `TestELParserPerformance` (G1, ZGC), `TestMapperPerformance`
>     (run 2 only), `TestHttp2Section_8_2` (needs 850-1530 s on a loaded shard; HANG at the cap on two shards of the
>     `g1-22` run and one of the `g1-17` run), `TestSwallowAbortedUploads` (G1; client `Connection aborted: write0`;
>     fails 1 of 2 standalone on `g1-17` too, passes on HotSpot).
>   * One unexplained: `TestHttpServletDoHeadInvalidWrite1023ValidWrite513`, one test of 288 on the G1 shard,
>     `NoSuchMethodError: 'boolean ContainerMBean.isPattern()'` inside `ObjectInstance.<init>` (an `ObjectName`
>     receiver that arrived as another class: a stale-reference signature). Not reproduced in 9 standalone G1 runs of the
>     class on `g1-22` or 9 on `g1-17`, and absent from every earlier full run.
> - **Retired, not a bug**: `TestVirtualContext` (HotSpot-identical 500) —
>   [35](../../internal/fixed-suite-bugs/tomcat/35-testvirtualcontext-hotspot-identical-500-RETIRED-20260925.md);
>   the 5-class HostConfig-deployment row and 2 of the 12 undiagnosed FAILs
>   (`TestDefaultServletRfc9110Section13`, `TestStreamProcessor`), all sharded-
>   run timeout artifacts, not hangs — [36](../../internal/fixed-suite-bugs/tomcat/36-census-timeout-artifacts-confirmed-not-hangs-20260925.md).
> - **Fixed**: per-webapp `Logger.getLogger()` isolation
>   (`TestPerWebappJuliIntegration`), as a side effect of the WebSocket fix
>   below — [testperwebappjuliintegration-classloader-logger-isolation-FIXED-20260926.md](../../internal/fixed-suite-bugs/tomcat/testperwebappjuliintegration-classloader-logger-isolation-FIXED-20260926.md).
> - **Partly fixed, 2026-10-05**: ~1-4% multicast message loss under async burst load
>   (`TestMulticastPackages`, previously miscategorized as environmental). The 2026-09-26
>   "warm-up window" reading and the later "it is TCP through `NioReceiver`" reading were both wrong:
>   the data is UDP multicast read by one `McastServiceImpl` thread per channel, whose
>   per-datagram receive cost 35-90 us (HotSpot 1.7) because every datagram native paid the
>   GC-blocking bracket; fixed by doc 51. Residual: occasional 10-150 ms receiver-thread stalls —
>   [62-testmulticastpackages-async-send-message-loss-FIXED-20261025.md](../../internal/fixed-suite-bugs/tomcat/62-testmulticastpackages-async-send-message-loss-FIXED-20261025.md).
> - **Fixed 2026-10-25**: the residual receiver-thread stalls of `TestMulticastPackages` and the 2026-10-25 census
>   items that followed them: a compile on the calling thread under its Java locks
>   ([60](../../internal/fixed-suite-bugs/tomcat/60-callee-door-compiled-the-callee-on-the-mutator-holding-its-locks-100-ms-process-wide-stalls-multicast-burst-lost-4-7-percent-FIXED-20261025.md)),
>   with `TestMethodPerformance` and `TestHttp2Section_8_2` left to
>   [the compiled-code speed page](../jit/small-jdk-method-call-overhead-charsetdecoder-decode-25-80x-tomcat-methodperformance-and-http2-8-2-wall-20261025.md).
> - **Re-diagnosed, then retired, 2026-09-26**: `TestManagerWebapp.testBug57700`'s
>   2026-09-25 socket read timeout was first read as a **recurrence** of an
>   already-FIXED-and-CLOSED throughput issue
>   ([`webapp-deploy-annotation-scan-interpreted-226x-CLOSED-20260825.md`](../../internal/fixed-suite-bugs/tomcat/webapp-deploy-annotation-scan-interpreted-226x-CLOSED-20260825.md)).
>   Re-verification found it does not reproduce: `AnnotationScanCostProbe` measures
>   back inside that fix's own 813.5 us/class band, and the class passes
>   `OK (3 tests)` 3/3 clean back-to-back on `fix18`. The single 77.4 s outlier
>   reads as host-contention noise, not a `dev`-history regression — no bisection
>   needed —
>   [41](../../internal/fixed-suite-bugs/tomcat/41-testmanagerwebapp-bug57700-host-contention-noise-RETIRED-20260926.md).
> - **Re-confirmed environmental** (HotSpot fails identically): `TestUdpPackages`,
>   `TestRemoteProcessException` — [tribes-multicast-family-still-environmental.md](tribes-multicast-family-still-environmental.md).
> - **Fixed**: the Jasper/JSP compiler cluster (`TestEncodingDetector`,
>   `TestGenerator`, `TestJspConfig`, `TestJspDocumentParser`, `TestParser`,
>   `TestValidator`, `TestELInterpreterTagSetters`, `TestELInJsp` — all 8, plus
>   `TestTldScanner` above) — turned out NOT to be hangs or CratonVM bugs at
>   all, but a stale 2016 `ecj-4.6.1.jar` shadowing the correct
>   `ecj-3.45.0.jar` on the classpath, byte-identical on HotSpot until fixed.
>   Fixture-only fix — [37](../../internal/fixed-suite-bugs/tomcat/37-stale-ecj-4.6.1-classpath-jar-jasper-cluster-FIXED-20260925.md).
> - **Fixed 2026-09-26**: the whole 9-class WebSocket cluster, previously a
>   timeout artifact (doc 33's own trace of it was left unresolved) —
>   root-caused via `--stack-dump-on-timeout` to a `java.util.logging.LogManager`
>   concurrent-first-use race, not the AsynchronousSocketChannel/NIO2 mechanism
>   the earlier trace suspected — [38](../../internal/fixed-suite-bugs/tomcat/38-websocket-cluster-logmanager-concurrent-first-use-race-FIXED-20260926.md).
>   The 2 SSL classes' `SSLParameters` `ClassCastException` the hang had been
>   masking is ALSO fixed — root-caused to `G30-1`'s already-catalogued `N4`
>   nomination (`SSLContext#1 contextSpi` field-layout collision under real-JDK
>   mode: a stale `Int` "initialized" flag write in `net_phase_e.rs` colliding
>   with the real class's reference-typed `contextSpi` field, deleted as dead
>   weight rather than relocated) — [40](../../internal/fixed-suite-bugs/tomcat/40-websocket-ssl-sslcontext-contextspi-field-collision-FIXED-20260926.md).
>   **All 9 WebSocket classes are now fully green.**
> - **Fixed 2026-09-26**: `TestDefaultInstanceManager`'s off-by-one recurred a
>   FOURTH time, independently of 34's fix — 4 of the `ClassLoader.defineClass`
>   native call sites recorded a class's defining loader AFTER minting its
>   `Class` mirror, so the mirror never got a `mirror_pin` row and was
>   permanently, unconditionally GC-rooted for every class defined the normal
>   way (JSP-compiled, Groovy, CGLIB) — not a GC-timing/reconciliation bug at
>   all — [39](../../internal/fixed-suite-bugs/tomcat/39-mirror-pin-registration-ordering-classloader-unload-FIXED-20260926.md).
> - **Retired 2026-10-06**: `dohead-family-consolidated-history.md` (Generational-only throughput collapse of the 64
>   DoHead classes, HANG at 3000 s in 2026-08): all 64 pass in 52-151 s on every collector in the three three-GC runs,
>   [53](../../internal/fixed-suite-bugs/tomcat/53-dohead-family-consolidated-history-generational-throughput-collapse-gone-RETIRED-20261006.md).
> - **Retired 2026-10-06**: the `TestWebSocketFrameClient` crash found after the interpreter round i1 merge
>   (thread explosion in `WsRemoteEndpointImplBase$StateMachine`, 2026-09-26) does not reproduce any more:
>   it passes on the `dev` tip and in nine three-GC shards,
>   [54](../../internal/fixed-suite-bugs/tomcat/54-testwebsocketframeclient-post-i1-merge-statemachine-thread-explosion-crash-no-longer-reproduces-RETIRED-20261006.md).
> - **Not yet re-checked individually**: the remaining EL/TLS/manager-webapp
>   rows below.

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
