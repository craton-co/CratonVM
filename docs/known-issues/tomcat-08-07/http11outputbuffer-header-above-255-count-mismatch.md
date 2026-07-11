# TestHttp11OutputBuffer — header-count mismatch for headers above 255 bytes

**Status:** OPEN. **Severity:** medium (HTTP/1.1 wire-protocol correctness).
**HotSpot:** PASS (fresh-verified).

## Summary

`org.apache.coyote.http11.TestHttp11OutputBuffer.testHTTPHeaderAbove255`
fails:
```
1) testHTTPHeaderAbove255(org.apache.coyote.http11.TestHttp11OutputBuffer)
java.lang.AssertionError: expected:<5> but was:<3>
	at java.lang.AssertionError.<init>(AssertionError.java:76)
	at org.junit.Assert.fail(Assert.java:89)
	at org.junit.Assert.failNotEquals(Assert.java:835)
	at org.junit.Assert.assertEquals(Assert.java:647)
```
This test exercises `Http11OutputBuffer`'s handling of an HTTP header value
long enough to exceed 255 bytes (the point at which Tomcat's internal
buffer-growth/write-splitting logic kicks in) and counts something
(likely the number of internal buffer segments, socket writes, or output
chunks produced) — CratonVM produces 3 where 5 are expected. This is a
quantitative mismatch in low-level output-buffer chunking, not a hang or
crash, so the response is probably still well-formed on the wire — but
the discrepancy suggests CratonVM's `Http11OutputBuffer` coalesces writes
differently than HotSpot's for large headers, which could matter for
tests/consumers relying on Tomcat's specific buffer-boundary behavior.

Found via a fresh Linux rerun (dev commit `2335765e`, real JDK, JIT on,
300s timeout) on 2026-07-11. Verified via a fresh same-session HotSpot run:
PASSES on HotSpot.

## Reproduction

```bash
JH=/home/victor/jdk25
CP=$(cat /data/data/apps/tomcat/.suite/cp-linux-fixed.txt)
"$CRATONVM_EXE" --java-home "$JH" -Xmx2g -cp "$CP" org.junit.runner.JUnitCore \
  org.apache.coyote.http11.TestHttp11OutputBuffer
```

## Recommendation

Read `TestHttp11OutputBuffer.testHTTPHeaderAbove255` to identify exactly
what the `5` vs `3` count represents (likely
`TesterOutputBuffer.getBufferCount()`/callback invocations, a common Coyote
test-harness pattern for counting writes to a mock `SocketWrapper`), then
trace `Http11OutputBuffer`'s buffer-growth logic under CratonVM for a >255
byte header value to see where the write count diverges from HotSpot's.
