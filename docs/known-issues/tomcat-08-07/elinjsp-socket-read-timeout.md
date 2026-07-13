# TestELInJsp — 4/25 failures with client-side SocketTimeoutException

**Status:** OPEN. **Severity:** medium. **HotSpot:** PASS (fresh-verified).

## Summary

`org.apache.el.TestELInJsp` fails 4 of its 25 tests with a client read
timeout:
```
1) testBug45427(org.apache.el.TestELInJsp)
java.net.SocketTimeoutException: Read timed out
	at org.apache.catalina.startup.TomcatBaseTest.methodUrl(TomcatBaseTest.java:709)
	at org.apache.catalina.startup.TomcatBaseTest.methodUrl(TomcatBaseTest.java:682)
	at org.apache.catalina.startup.TomcatBaseTest.getUrl(TomcatBaseTest.java:676)
```
Unlike the [HTTP/2](http2-testconnection-socket-closed-cluster.md) and
[AJP](abstractajpprocessor-socket-not-connected.md) socket clusters (both
"connection already closed/not-connected" at setup time), this is a
genuine **read timeout** — the client successfully connects and sends a
request but never receives a timely response for these 4 specific
EL-in-JSP test cases (each exercises a different EL expression evaluated
inside a JSP page served by the embedded Tomcat). This could mean the
server-side JSP/EL evaluation for these 4 specific expressions hangs or
takes unexpectedly long (server-side slowness/hang manifesting as a
client-side timeout), rather than a connection-layer bug like the other
two clusters.

Found via a fresh Windows full-suite rerun (dev commit `080e79256`,
2026-07-12, real JDK, JIT on). Verified via a fresh same-session HotSpot
run: PASSES on HotSpot (all 25 tests).

## Reproduction

```powershell
cd apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName elinjsp `
  -Start <idx> -Count 1 -TimeoutSec 120 -Parallel 1
# org.apache.el.TestELInJsp
```

## Recommendation

Identify the other 3 failing test methods alongside `testBug45427` (rerun
and capture the full failure list) to see if they share a common EL
feature/expression pattern. Given this is a read-timeout (not a
connection-refused/closed error), check server-side logs/thread state
during the hang window to determine whether the embedded Tomcat is stuck
evaluating the JSP/EL expression (a JIT or interpreter hang in EL
expression evaluation) versus a response simply not being flushed back to
the client (a buffering/commit issue, similar in spirit to the DoHead
family's `StreamEncoder` commit-threshold bug, though that one is already
confirmed fixed).
