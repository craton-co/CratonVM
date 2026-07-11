# TestStandardHostValve / TestStuckThreadDetectionValve — valve-layer NPE and bare assertion

**Status:** OPEN. **Severity:** medium. **HotSpot:** PASS on both
(fresh-verified).

## Summary

Two `org.apache.catalina.valves`/`core` classes fail with distinct but
adjacent symptoms — both in valve-chain request-processing code:

```
1) testIncompleteResponse(org.apache.catalina.core.TestStandardHostValve)
java.lang.AssertionError
	at org.junit.Assert.fail(Assert.java:87)
	at org.junit.Assert.assertTrue(Assert.java:42)
	at org.junit.Assert.assertNotNull(Assert.java:713)
```
`testIncompleteResponse` asserts something is non-null after driving a
request through `StandardHostValve` with a response that's expected to be
detected as incomplete — the bare assertion gives no further detail on
what was unexpectedly null.

```
1) testInterruption(org.apache.catalina.valves.TestStuckThreadDetectionValve)
java.lang.NullPointerException: Cannot invoke "String.startsWith(String)"
	at org.apache.catalina.valves.TestStuckThreadDetectionValve.testInterruption(TestStuckThreadDetectionValve.java:133)
```
`testInterruption` drives a request handler that's expected to get
interrupted by the stuck-thread detector after exceeding a threshold, then
inspects state afterward; a `String` value the test expects to be
non-null (likely a thread name, stack-trace string, or interrupted-request
URI captured by the valve) comes back `null` on CratonVM, causing the NPE
at the point the test calls `.startsWith(...)` on it.

Found via a fresh Linux rerun (dev commit `2335765e`, real JDK, JIT on,
300s timeout) on 2026-07-11. Verified via a fresh same-session HotSpot run:
both classes PASS on HotSpot.

## Reproduction

```bash
JH=/home/victor/jdk25
CP=$(cat /data/data/apps/tomcat/.suite/cp-linux-fixed.txt)
"$CRATONVM_EXE" --java-home "$JH" -Xmx2g -cp "$CP" org.junit.runner.JUnitCore \
  org.apache.catalina.core.TestStandardHostValve \
  org.apache.catalina.valves.TestStuckThreadDetectionValve
```

## Recommendation

For `TestStuckThreadDetectionValve`: read `TestStuckThreadDetectionValve.java:133`
to identify exactly which `String` is null (likely a field the
`StuckThreadDetectionValve` populates on the request/thread when it detects
and interrupts a stuck request) — trace whether CratonVM's interrupt
delivery or thread-state bookkeeping for this valve loses that value.
For `TestStandardHostValve`: add targeted logging around
`assertNotNull` in `testIncompleteResponse` to capture what's actually
null before investing further (the bare assertion currently gives no
signal to work from).
