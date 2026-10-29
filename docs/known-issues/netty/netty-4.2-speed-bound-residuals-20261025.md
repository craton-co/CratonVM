# Netty 4.2 — classes that still differ from HotSpot only because they are short-`@Timeout` and CratonVM is slower

Split out of `docs/internal/fixed-suite-bugs/netty/netty-4.2-interleaved-execution-parity-FIXED-20261025.md`
(the correctness items there are closed). Measured 2026-10-25 on `origin/dev` `f1ad046a39`, Windows 11 with the
machine otherwise idle, Temurin 25.0.3 as the control, `common-cvm-20261002.args`, one fork per class, JIT on,
`--jdk-only`. Every row below returns the **same `found` count** as HotSpot; the difference is tests that miss a
latch or `@Timeout`.

| class | HotSpot | CratonVM | what the test waits on |
|---|---|---|---|
| `DataCompressionHttp2Test` | 42 ok, 19 s | 25 ok / 17 failed, 62 s | `awaitServer` 5 s latch; 42 parameterised tests run at once, each builds a client and a server `EventLoopGroup`; failing tests fail after ~28 s, even `justHeadersNoData` |
| `Http2MultiplexTransportTest` | 7 ok, 8 s | 5 ok / 2 failed, 21 s | `@Timeout` 5 s on the OpenSSL / flush tests |
| `WebSocketClientHandshaker07Test`, `...08Test` | 21 ok, 2.4 s | 19 ok / 2 failed, 5.7 s | `testHttpResponseAndFrameInSameBuffer{,Codec}` have a 3 s `@Timeout`; HotSpot spends 2.47 s in the same test under the same parallel start, CratonVM 3.1 s |
| `NonStickyEventExecutorGroupTest.testOrdering` x4 | 10 ok, 3 s | 6 ok / 4 failed, 14 s | `@Timeout` 10 s; `2 x cores` threads per parameter, all four parameters at once, on `UnorderedThreadPoolEventExecutor`; each takes 10.5 s |
| `Http2ConnectionRoundtripTest`, `DefaultHttp2ConnectionTest` | pass, 13 s / 8 s | pass, 82 s / 106 s | no failure at this load; the wall time is the gap |
| `EmptyHttpHeadersInitializationTest` | 0 hangs in 12 | hangs 6 of 6 on Windows (passed 1 of 1 on Linux) | not a defect: a JLS 12.4.2 two-class initialisation cycle, which HotSpot also deadlocks when the threads interleave (`Cyc2` repro in the resolution page). The window is the class-load and first-call time of `EmptyHttpHeaders` |
| `HttpHeaderValidationUtilTest` | passes | HANG | `httpheadervalidationutiltest-exhaustive-loop-timeout-20260816.md` |
| compression `*IntegrationTest` (Bzip2, FastLz, JdkZlib, JZlib, Lzf...) | 10-14 s | pass on Linux at load ~25, 170-520 s | `compression-cluster-testhugedecompress-180s-throughput-wall-20260827.md` |

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
