# FormAuthenticator A/B/C — bare `assertTrue` failures across cookie/session matrix

**Status:** OPEN. **Severity:** medium (breaks a wide swath of Tomcat's FORM-auth
cookie/session-ID handling test matrix). **HotSpot:** PASS.

## Summary

`org.apache.catalina.authenticator.TestFormAuthenticatorA`,
`TestFormAuthenticatorB`, `TestFormAuthenticatorC` each fail multiple methods
(8 failures in `TestFormAuthenticatorA` alone) with a **bare `assertTrue()`
failure** — no message, no expected/actual values:
```
1) testGetNoClientCookies(org.apache.catalina.authenticator.TestFormAuthenticatorA)
java.lang.AssertionError
	at org.junit.Assert.fail(Assert.java:87)
	at org.junit.Assert.assertTrue(Assert.java:42)
```
Failing methods span the class's whole cookie/session-ID parameter matrix
(`testGetNoClientCookies`, `testTimeoutWithoutCookies`,
`testPostNoContinueNoClientCookies`, `testPostNoContinueNoServerCookies`,
`testPostWithContinuePostRedirectNoServerCookies`, etc.) — this is Tomcat's
FORM authenticator exercising session-ID-in-cookie vs session-ID-in-URL
behavior under various client/server cookie-support combinations.

Found via a full Windows Tomcat suite rerun (real JDK, JIT on, 1500s
timeout, dev commit range `33bef88d`..`0d8fb610`, 2026-07-07/08).

## Why no further detail here

JUnit's `assertTrue(condition)` without a message string produces no
diagnostic beyond "the boolean was false" — the log carries no clue about
*which* boolean or what the test actually observed. Determining the real
symptom requires reading the specific `assertTrue` call site in each failing
test method (`test/org/apache/catalina/authenticator/TestFormAuthenticatorA
.java` and friends) to see what condition is being checked (likely a
session-ID-changed / cookie-presence / redirect-target assertion), then
adding print/log instrumentation or a debugger to see the actual vs expected
values at that point.

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName formauth `
  -Start <idx> -Count 1 -TimeoutSec 120 -Parallel 1
# org.apache.catalina.authenticator.TestFormAuthenticatorA (fastest single-class repro)
```

## Recommendation

Read the failing test methods' source to identify the specific assertion,
then re-run with targeted logging/debugging at that assertion to capture
actual vs. expected. Given the breadth (8/N methods failing in `A` alone,
matching patterns in `B`/`C`), this is likely one shared root cause in
FORM-auth session/cookie handling rather than N independent bugs — check
`org.apache.catalina.authenticator.FormAuthenticator`'s session-ID and
`Set-Cookie` handling first.
