# TestAbstractAjpProcessor.testNoHeaders — response body not empty for a no-body servlet response

**Status:** OPEN. **Severity:** low (single test). **HotSpot:** PASS
(fresh-verified). Uncovered after fixing the whole-class connection failure
— see
`docs/internal/tomcat-08-07/abstractajpprocessor-socket-not-connected-FIXED.md`.

## Summary

`org.apache.coyote.ajp.TestAbstractAjpProcessor.testNoHeaders` (bug66591
regression test) deploys a servlet that sets no headers and writes no body,
and asserts BOTH an empty response-headers map AND an empty response body:

```
java.lang.AssertionError
	at org.apache.coyote.ajp.TestAbstractAjpProcessor.testNoHeaders(TestAbstractAjpProcessor.java:920)
```

Line 920 is `Assert.assertTrue(body.isEmpty())` — the headers assertion on
the preceding line (917) passes, but the extracted response body is
non-empty when it should be. The connection stays usable afterward (a
follow-up `cping()` at line 925 is not reached because the assertion at 920
throws first, but the test's structure implies the AJP framing itself is
intact — this looks like extra/unexpected byte content being written into
the body chunk, not a protocol-framing corruption).

Found via a fresh Windows rerun (dev tip after
`fix/dohead-family-regressions-v2-20260713` merged, 2026-07-13, real JDK,
JIT on) immediately after fixing the connection-establishment bug that
previously made all 30 tests in this class fail uniformly. HotSpot passes
all 30 tests including this one.

## Reproduction

```powershell
cd apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName ajpnoheaders `
  -Start <idx> -Count 1 -TimeoutSec 120 -Parallel 1
# org.apache.coyote.ajp.TestAbstractAjpProcessor (testNoHeaders)
```

## Recommendation

Instrument or capture the actual (non-empty) body bytes CratonVM sends back
for this no-op servlet (`NoHeadersServlet`, declared in the same test file)
— compare against HotSpot's byte-for-byte empty body. Worth checking
whether this is a sibling of the DoHead family's `NoBodyOutputStream`/
commit-threshold issues (see `dohead-streamencoder-eager-flush-commit-threshold-FIXED.md`
and the `OutputStreamWriter` regression fixed in
`fix/dohead-family-regressions-v2-20260713`) — i.e. some AJP response-body
writer path eagerly emitting bytes (e.g. a spurious flush, or writing a
zero-length-but-present chunk marker HotSpot elides) that a real Tomcat
`Response`/`OutputBuffer` on the AJP side would suppress for a genuinely
empty body. If unrelated to that family, check the AJP `SEND_BODY_CHUNK`
packet framing for an off-by-one that includes a chunk even when zero
bytes were actually written.
