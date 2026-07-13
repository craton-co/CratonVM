# TestHttp2Section_5_1 — max-active-streams / waiting-stream RST behavior (3 residuals)

**Status:** OPEN. **Severity:** medium (HTTP/2 protocol-conformance edge).
**HotSpot:** PASS. **Found:** 2026-07-13, newly VISIBLE (not newly caused)
— these were masked while the whole class failed 26/26 at connection setup
(see `docs/internal/tomcat-08-07/http2-testconnection-socket-closed-cluster-FIXED.md`).

## Summary

After the `javax/net/SocketFactory`-under-`CRATONVM_REAL_NET_SOCKETS`
regression was fixed (branch `fix/dohead-family-regressions-20260713`),
`org.apache.coyote.http2.TestHttp2Section_5_1` went from 26/26 failures
(all at `Http2TestBase.openClientConnection`) to 23/26 passing. The 3
remaining failures are real protocol-behavior differences, not connection
setup:

```
1) testExceedMaxActiveStreams01[0: loop [0], useAsyncIO[false]]
org.junit.ComparisonFailure: expected:<[7-HeadersStart 7-Header-[:status]-[200] ...
2) testErrorOnWaitingStream02[1: loop [0], useAsyncIO[true]]
java.lang.AssertionError: 5-RST-[7] ...
3) testExceedMaxActiveStreams01[1: loop [0], useAsyncIO[true]]
org.junit.ComparisonFailure: expected:<[7-HeadersStart ...
```

`testExceedMaxActiveStreams01` (both useAsyncIO variants) expects the
server to admit stream 7 with a 200 once capacity frees; the observed
exchange differs. `testErrorOnWaitingStream02[1]` sees an unexpected
`RST[7]` ordering on a waiting stream. Both exercise Tomcat's RFC 9113
§5.1 stream-state / SETTINGS_MAX_CONCURRENT_STREAMS enforcement over
CratonVM's NIO stack — likely a timing/readiness difference in how
queued/waiting streams get serviced (cf. the WSAPoll edge-miss and
selector-timing notes in `reference_tomcat_dohead_speed_oncpu_not_shutdown`)
rather than frame encoding.

## Reproduction

```powershell
cd apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName http2s51 `
  -Start 386 -Count 1 -TimeoutSec 600 -Parallel 1
# org.apache.coyote.http2.TestHttp2Section_5_1 (index 386 as of 2026-07-13)
```

## Recommendation

Run the 3 methods in isolation with `CRATONVM_DBG`-level HTTP/2 frame
tracing (or the tests' own `debug` StringBuilder output) to capture the
actual frame sequence vs expected; compare stream-admission timing when
`maxConcurrentStreams` frees a slot. Check the sibling
`TestHttp2Section_5_*` classes for the same pattern — they were also
previously masked by the connection-setup cluster.
