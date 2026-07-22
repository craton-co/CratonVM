# TestAbstractAjpProcessor.testNoHeaders — AJP empty response body

**Status: FIXED/RETIRED (2026-07-14).** **HotSpot:** PASS.

## Original symptom

`org.apache.coyote.ajp.TestAbstractAjpProcessor.testNoHeaders` (Tomcat bug
66591) had reported a non-empty decoded AJP response body after a servlet
called only `resp.flushBuffer()`. The response headers were empty as expected;
the assertion that failed was `Assert.assertTrue(body.isEmpty())`.

## Resolution

The report was stale after the adjacent AJP and JMX repairs landed. On current
`dev` (`073697d0`), a focused launcher ran only `testNoHeaders` against the
real-JDK Tomcat fixture and confirmed the complete request contract: empty
headers, empty body chunk, normal response end, and a usable connection.

The fresh current-dev build initially exposed a separate JMX delegate-startup
regression before Tomcat could reach the AJP request. That was fixed on `dev`
by `6a0eedd8` (`fix(vm,native): retag JMX + Function$Identity natives Bridge`),
so it is not an AJP response-body residual.

## Verification

Azure Linux, uniquely named release binary
`/data/cratonvm-ajp-testnoheaders-body-20260714-001/cratonvm-ajp-testnoheaders-body-20260714-001`,
compiled from current `dev`:

```
AJP_TESTNOHEADERS runs=1 failures=0
```

The focused probe passed in both interpreter (`--nojit`) and JIT-enabled modes.
The broad `TestAbstractAjpProcessor` class is intentionally not used as the
retirement gate because it contains unrelated AJP cases.

The original issue note has therefore moved from `docs/known-issues` to this
archive location.
