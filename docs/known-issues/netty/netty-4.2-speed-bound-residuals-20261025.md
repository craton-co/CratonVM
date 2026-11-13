# Netty 4.2 — classes that still differ from HotSpot only because they are short-`@Timeout` and CratonVM is slower

Split out of 
(the correctness items there are closed). Measured 2026-10-25 on `origin/dev` `f1ad046a39`, Windows 11 with the
machine otherwise idle, Temurin 25.0.3 as the control, `common-cvm-20261002.args`, one fork per class, JIT on,
`--real-jdk`. Every row below returns the **same `found` count** as HotSpot; the difference is tests that miss a
latch or `@Timeout`.

| class | HotSpot | CratonVM | what the test waits on |
|---|---|---|---|
| `DataCompressionHttp2Test` | 42 ok, 19 s | 25 ok / 17 failed, 62 s | `awaitServer` 5 s latch; 42 parameterised tests run at once, each builds a client and a server `EventLoopGroup`; failing tests fail after ~28 s, even `justHeadersNoData` |
| `Http2MultiplexTransportTest` | 7 ok, 8 s | 5 ok / 2 failed, 21 s | `@Timeout` 5 s on the OpenSSL / flush tests |
| `SniHandlerTest`, `ParameterizedSslHandlerTest` (OPENSSL / OPENSSL_REFCNT variants only) | 32 / 63 ok, 11 s / 16 s | 30 ok / 2 failed, 26 s; 52 ok / 11 failed, 62 s | first tcnative context build misses the 10 s `@Timeout` (6 s vs 1.7 s); nine concurrent OpenSSL handshakes take ~30 s vs ~10 s and trip 30 s / 48 s `@Timeout`s and Netty's 10 s handshake timeout. Each passes alone. Re-measured 2026-11-01; supersedes the retired `websocket-handshaker-and-ssl-alert-defects` page (`CloseNotifyTest` passes 4/4) |
| `WebSocketClientHandshaker00Test`, `...07Test`, `...08Test`, `...13Test` | 21 ok, 2.4 s | 19 ok / 2 failed, 5.7 s | `testHttpResponseAndFrameInSameBuffer{,Codec}` have a 3 s `@Timeout`; HotSpot spends 2.47 s in the same test under the same parallel start, CratonVM 3.1 s |
| `NonStickyEventExecutorGroupTest.testOrdering` x4 | 10 ok, 3 s | 6 ok / 4 failed, 14 s | `@Timeout` 10 s; `2 x cores` threads per parameter, all four parameters at once, on `UnorderedThreadPoolEventExecutor`; each takes 10.5 s |
| `Http2ConnectionRoundtripTest`, `DefaultHttp2ConnectionTest` | pass, 13 s / 8 s | pass, 82 s / 106 s | no failure at this load; the wall time is the gap |
| `EmptyHttpHeadersInitializationTest` | 0 hangs in 12 | hangs 6 of 6 on Windows (passed 1 of 1 on Linux) | not a defect: a JLS 12.4.2 two-class initialisation cycle, which HotSpot also deadlocks when the threads interleave (`Cyc2` repro in the resolution page). The window is the class-load and first-call time of `EmptyHttpHeaders` |
| `HttpHeaderValidationUtilTest` | passes | HANG | `httpheadervalidationutiltest-exhaustive-loop-timeout-20260816.md` |
| compression `*IntegrationTest` (Bzip2, FastLz, JdkZlib, JZlib, Lzf...) | 10-14 s | **PASSES** (quiet host 25-134 s, 0 failed, 2026-11-11) | [RETIRED] `../../internal/defects-and-fixes/fixed-suite-bugs/netty/compression-cluster-testhugedecompress-180s-throughput-wall-20260827.md` |

Passing and equal to HotSpot on the same run, so **not** open any more: `ManualIoEventLoopTest`,
`NioIoHandlerEventCountTest`, `HashedWheelTimerTest`, `JfrEventsTest`, `AlignedPooledByteBufAllocatorTest`
(21 ok / 28 aborted on both), `AutoScalingEventExecutorChooserFactoryTest`, `HttpToHttp2ConnectionHandlerTest`,
`InboundHttp2ToHttpAdapterTest`.

## What would close each row

The same three things as `../tomcat/jit-native-call-crossing-250ns-threadlocal-get-2us-20261005.md` and the compression
page: cold-start interpretation of Netty's bootstrap path, native crossings (~190 ns) on the `ThreadLocal` and Unsafe
paths, and contended `LinkedBlockingQueue` / `ScheduledThreadPoolExecutor` hand-offs. No row has a CratonVM-specific
failure mode; each passes with a larger timeout (the Linux runs of the same classes at load 25 complete).
Re-measure by running the class alone under `CratonRunner` with the default `common-cvm-*.args`; a class that passes
alone and fails in the class run belongs here.

## Linux re-measure, 2026-11-01 (`origin/dev` `39e1145b06`, shared 16-core host, load 10-45, `--real-jdk`)

The two rows that were carried as "HANG" / "defect" on 2026-10-30 are closed as speed-bound in
 and
.

| class | HotSpot | CratonVM | what it waits on |
|---|---|---|---|
| `DataCompressionHttp2Test` | 43-44 ok, 8.9 s, 9.8 CPU-s | 40-44 ok, 0-4 failed, 41.6 s, **98.5 CPU-s** | `awaitServer` 5 s latch; the failing count follows host load |
| `Http2MultiplexTransportTest` | 11 ok | 6-7 ok, 4-5 failed | `@Timeout` 5 s; JDK-provider `SslContextBuilder.forServer(..).build()` is 3.1-3.3 s against HotSpot's 0.49 s (`--compatible` 1.5 s) |
| `DefaultHttp2ConnectionTest`, `Http2ConnectionRoundtripTest` | pass | 54-55 ok / 22 ok, 0 failed, every run | none: pass |
| `ServerBootstrapTest` | 6 ok, 2.0 s | 6 ok, 3.0 s, 20/20 repeats | none: pass (in-memory `LocalChannel`, 3 s bound has a 40x margin warm) |

Levers measured on the way (none closes a row): `renice +15` of the compile threads is worse (5 failed vs 2);
`--nojit` is slower; serialising the class does not rescue the JDK TLS test. Per-call costs that remain, CratonVM vs
HotSpot: `StackWalker.walk().findFirst()` 29-52 µs vs 2-8 µs (Mockito does one per mock call), `Class.forName`
1.4-2.2 µs vs 0.2, `ThreadLocal.get` 160 ns vs 8, direct `ByteBuffer.put(int, ByteBuffer, int, int)` 64 B ~290 ns vs 9
(was 435 ns before the 2026-11-01 flag-read / `ScopedMemoryAccess` gate / `copyMemory` staging changes), and on the compile
thread `resolve_inline_site_from` ~28% and `deopt::rerun_window_closed_at` 8.5% of its inclusive time.

## Windows re-measure, 2026-11-02 (`origin/dev` `264290b000`, `--real-jdk`, LTO-off release, pinned to P-cores `/affinity FFFF`, one process per class, `common-cvm-20261002.args`)

Most rows no longer fail on an idle machine; the remaining failures move between runs, which is what a latch/`@Timeout` race against a slower
runtime looks like.

| class | result |
|---|---|
| `SniHandlerTest` / `ParameterizedSslHandlerTest` | 32 / 63 ok, 0 failed (15 s / 37 s) |
| `WebSocketClientHandshaker00/07/08/13Test` | all ok (4-5 s) |
| `NonStickyEventExecutorGroupTest` | 10 ok, 0 failed (7.4 s) |
| `Http2ConnectionRoundtripTest` | 21 ok (25.7 s) |
| `Http2MultiplexTransportTest` | run 1: 5 ok / 2 failed (`testSSLExceptionOpenSsl{TLSv12,TLSv13}`, 4 skipped); run 2: no `TESTFAIL` |
| `DataCompressionHttp2Test` | 40 ok / 2 failed (19 s); a second run failed a different test (`awaitServer` / `bootstrapEnv` 5 s latch) |

Nothing here is retired: the two flaky classes still miss their 5 s latches under the 42-way parallel start, and
`../netty/netty-buffer-suite-cpu-cost-jit-compile-20261102.md` (9x HotSpot's CPU on warm-up) is untouched. Both need the JIT/native-crossing
work named above, measured with `instructions:u` on the Linux host, not a harness change.
