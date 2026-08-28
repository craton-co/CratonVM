# Complete-suite 4-shard ZGC run (2026-08-26): timing-margin FAILs, host at 0GB free RAM

## Status
**Environmental, not individually confirmed per class.** Flagging the pattern and
the evidence rather than asserting each class is definitely contention noise —
see caveat below.

## Context

Complete 657-class netty suite, `dev` HEAD `3c09f9d93`, 4 parallel shards, ZGC
only (`--shards 4 --gc zgc`). `PASS=519 FAIL=40 HANG=35 ABORTED=26 NOTESTS=36
CRASH=1`. Checked host memory immediately after the run finished:
**0 GB free out of 63.7 GB total** — this machine runs many other concurrent
Claude Code sessions/worktrees, and 4 concurrent `cratonvm.exe` processes plus
whatever else was active exhausted it.

## The cluster

Every class below FAILed on either a `java.util.concurrent.TimeoutException`
against a tolerance well inside what the test itself expects to need, or an
`AssertionError`/array-length mismatch whose margin is thin enough that a
contended host is the more likely explanation than a correctness defect. Each
already has a FIXED/CLOSED/RESOLVED history in `fixed-suite-bugs/netty/`
for a *different, specific* original failure — none of the excerpts below match
those original signatures, which is the basis for reading this run's failures as
contention rather than a regression of the original bug.

| class | this run's failure | prior doc (different original issue) |
|---|---|---|
| `channel.SingleThreadEventLoopTest.scheduleTaskAtFixedRateA` | `120099400L` not `< 120000000L` — **99.4µs over a 120ms bound** | — |
| `channel.nio.NioEventLoopTest.testSelectableChannel` | timed out after 3000ms | `foreign-nio-subclass-and-bc-provider-object-FIXED-20260819.md`, `nioeventlooptest-unbound-registration-fd-slot-collision-FIXED-20260817.md` |
| `channel.oio.OioEventLoopTest.testTooManyAcceptedChannels` | `ConnectException` (connection refused, loopback) | — |
| `channel.socket.nio.NioSocketChannelTest` (2 methods) | `ConnectException` (connection refused, loopback) | `nio-channels-abstract-classed-adaptor-bridges-FIXED-20260813.md` |
| `handler.codec.http.HttpClientCodecTest.testServerCloseSocketInputProvidesData` | `assertTrue`: expected true, was false | `unsafe-memory-access-property-flips-netty-to-unsafe-paths-20260812-FIXED.md` |
| `handler.codec.http2.Http2ConnectionRoundtripTest.flowControlProperlyChunksLargeMessage` | `assertTrue`: expected true, was false | — |
| `handler.proxy.ProxyHandlerTest` (8 of 47 parameterizations) | `array lengths differ, expected: <0> but was: <1>` — one extra byte arrived in an AUTO_READ success-path check | `netty-stackwalker-option-clinit-nameless-constants-FIXED-20260813.md` (unrelated content, incidental classpath match) |
| `handler.ssl.JdkSslRenegotiateTest.testRenegotiateServer` | timed out after 30000ms | `tls-client-windows-schannel-leaf-only-chain-20260817-FIXED-20260818.md` |
| `handler.ssl.SslHandlerTest.testSessionTicketsWithTLSv12AndNoKey` | timed out after 5000ms | multiple, all a different SSL residual family — see `fixed-suite-bugs/netty/` |
| `test.udt.nio.NioUdtByteRendezvousChannelTest.basicEcho` | byte-count mismatch, expected 1964976 got 1932208 (~98% delivered) | `netty-jni-native-codec-sigsegv-FIXED-20260812.md` |
| `util.RecyclerTest` (3 of 67 parameterizations) | `testThreadCanBeCollectedEvenIfHandledObjectIsReferenced` timed out | — |
| `util.ResourceLeakDetectorTest.testConcurrentUsage` | timed out after 60000ms | `resourceleakdetector-concurrentusage-is-slow-not-hung-CLOSED-20260817.md` — **this class's own prior doc title is literally "slow, not hung"; a contended host is exactly what pushes "slow" over a 60s cap** |
| `util.ThreadDeathWatcherTest.testThreadGroup` | timed out after 2000ms | — |
| `util.concurrent.DefaultThreadFactoryTest` (2 methods) | timed out after 2000ms | `classpath-directory-stat-per-class-FIXED-20260817.md`, `misc-non-tls-residuals-CLOSED-20260813.md` |
| `util.concurrent.NonStickyEventExecutorGroupTest` (1 of 10 parameterizations) | timed out after 10000ms | — |

(`util.HashedWheelTimerTest` also failed this run on its usual `testExecutionOnTime`
signature — that one is NOT contention, see its own page:
`fixed-suite-bugs/netty/hashedwheeltimertest-two-dispatch-defects-not-a-funnel-floor-20260827.md`,
which supersedes the open page this used to name and corrects its root cause.
Its second failure this run,
`testNewTimeoutShouldStopThrowingRejectedExecutionExceptionWhenExistingTimeoutIsExecuted`
timing out after 3000ms, fits this contention bucket instead.)

## Update 2026-08-27: three of these reproduced again on a quiet host — not contention after all

Retested this run's full non-passing set on a quiet single-shard host
(2026-08-27, twice, including after a fresh `dev` rebuild). Most of this page's
rows did NOT reproduce (genuinely contention, as this page guessed). Three did,
consistently:

- **`util.ResourceLeakDetectorTest.testConcurrentUsage`** — this is the
  already-known "slow, not hung" throughput characteristic
  (`resourceleakdetector-concurrentusage-is-slow-not-hung-CLOSED-20260817.md`),
  not contention. The 60s timeout in this test is just tight enough that any
  slowdown — contended host OR the VM's own baseline throughput — pushes it
  over.
- **`util.RecyclerTest`** (`testThreadCanBeCollectedEvenIfHandledObjectIsReferenced`)
  — reproduces on a quiet host too. Not yet root-caused beyond that; the
  original guess (GC-timing-sensitive test, plausible contention) is no longer
  supported by the evidence and this needs its own look.
- **`test.udt.nio.NioUdtByteRendezvousChannelTest.basicEcho`** — reproduces on a
  quiet host too (byte-count mismatch, ~98% of expected data transferred). Also
  not yet root-caused beyond the original report.

Treat these three as **not** contention-explained any more; they need
individual investigation. The rest of the table's rows are unaffected by this
update — they were not re-tested and the original caveat below still applies to
them.

## Caveat — this is a pattern, not 17 individual confirmations

None of these were re-run in isolation on a quiet host this session, so this
page is not proof that every row is contention rather than a real regression —
it's the evidence available right now (thin timing margins, loopback connection
refusals, a host at 0GB free, and the observation that the failure signature
doesn't match any class's previously-documented bug). Treat a class here as
"probably contention, not yet individually cleared" — if one of these shows up
again on a quiet host, or with a *different* failure signature than listed
above, it needs its own investigation rather than being waved through as this
page.

## Repro (to re-check any one class on a quiet host)

```bash
cd apps/netty-suite-runner
cratonvm.exe --java-home <jdk25> --Xmx 1500m -XX:+UseZGC @common.args -Dcraton.batch=1 \
  CratonRunner <fully.qualified.ClassName>
```
