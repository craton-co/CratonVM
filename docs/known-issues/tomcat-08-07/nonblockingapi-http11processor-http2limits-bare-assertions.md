# NIO/HTTP2 protocol edge cases — bare assertion failures (3 classes)

**Status:** OPEN. **Severity:** medium (protocol-edge-case cluster).
**HotSpot:** PASS on all.

## Summary

Three unrelated-on-the-surface but similarly-shaped classes fail with bare
`AssertionError` (no message, no expected/actual detail), all exercising
low-level HTTP/1.1 and HTTP/2 wire-protocol edge cases:

```
1) testDelayedNBWrite(org.apache.catalina.connector.TestNonBlockingAPI)
java.lang.AssertionError
2) testNonBlockingReadIgnoreIsReady(org.apache.catalina.connector.TestNonBlockingAPI)
java.lang.AssertionError

1) testPipelining(org.apache.coyote.http11.TestHttp11Processor)
java.lang.AssertionError
2) testWithTEChunkedWithCL(org.apache.coyote.http11.TestHttp11Processor)
java.lang.AssertionError

1) testHeaderLimits100x32(org.apache.coyote.http2.TestHttp2Limits)
java.lang.AssertionError
2) testPostWithTrailerHeadersSize0(org.apache.coyote.http2.TestHttp2Limits)
java.lang.AssertionError
```

`TestNonBlockingAPI` covers Tomcat's Servlet 3.1 non-blocking I/O
(`ReadListener`/`WriteListener`, `isReady()`/`setWriteListener()`) —
`testDelayedNBWrite` and `testNonBlockingReadIgnoreIsReady` both hit
timing-sensitive interaction between the NIO connector and the async
read/write-listener callback contract.

`TestHttp11Processor.testPipelining`/`testWithTEChunkedWithCL` cover HTTP/1.1
pipelining and `Transfer-Encoding: chunked` combined with `Content-Length`
(a request-smuggling-adjacent edge case Tomcat deliberately tests) — a wire-
protocol parsing/framing difference.

`TestHttp2Limits.testHeaderLimits100x32`/`testPostWithTrailerHeadersSize0`
cover HTTP/2 frame-size and header-count limit enforcement — the server may
be accepting/rejecting frames at different limits than HotSpot's Tomcat.

Not yet distinguished which of these are host-timing artifacts (the NIO
non-blocking class in particular is a plausible candidate, given this
session's established pattern of timing-sensitive connector tests) versus
genuine protocol-handling divergence (the HTTP/2 limits and HTTP/1.1
pipelining classes are less likely to be pure timing noise since they check
specific protocol enforcement behavior, not elapsed time).

Found via a full Windows Tomcat suite rerun (real JDK, JIT on, 1500s
timeout, dev commit range `33bef88d`..`0d8fb610`, 2026-07-07/08). Note:
`TestParser`'s bare-assertion failures were considered for inclusion in this
cluster but excluded here — verify against
[jasper-jdt-parser-arrayindexoutofbounds.md](../jasper-jdt-parser-arrayindexoutofbounds.md)
first, since the Jasper JDT parser family is closely related and has an
active FIXED/residual history; `TestParser` may belong to that family rather
than this one.

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName protoedge `
  -Start <idx> -Count 1 -TimeoutSec 60 -Parallel 1
# org.apache.catalina.connector.TestNonBlockingAPI
# org.apache.coyote.http11.TestHttp11Processor
# org.apache.coyote.http2.TestHttp2Limits
```

## Recommendation

Since all three fail with bare `AssertionError`, first add targeted logging
or run each single `@Test` method under a debugger/print-based patch to
capture actual vs. expected before investing in root-cause work. Triage
order: (1) re-run `TestNonBlockingAPI` serially/isolated to rule out timing
artifacts, (2) if `TestHttp11Processor`/`TestHttp2Limits` reproduce
consistently in isolation, treat as genuine protocol-framing/limit-
enforcement bugs and prioritize — request-smuggling-adjacent and HTTP/2
limit-enforcement gaps are security-relevant even if not exploitable here.

## 2026-07-09 worker isolation

No assertion detail was extracted in this worker because the full
`apps/tomcat-suite-runner` checkout and Tomcat JUnit classpath are not present
in the available worktree. The only code change made for this track was the
native `SocketChannel.close()` close/drain fix documented in
`swallowabortedupploads-unexpected-socketexception.md`; that fix is plausibly
relevant to connector-level resets, but it does not prove or disprove the
bare assertions in `TestNonBlockingAPI`, `TestHttp11Processor`, or
`TestHttp2Limits`.

This note remains genuine/open pending isolated single-method reruns with
actual expected/actual assertion details.
