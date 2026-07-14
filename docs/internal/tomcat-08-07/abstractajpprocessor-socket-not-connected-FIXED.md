# TestAbstractAjpProcessor — full-class AJP client socket failure (30/30)

**Status: FIXED (2026-07-13, branch `fix/dohead-family-regressions-v2-20260713`).**
Same root cause as the HTTP/2 cluster this doc already calls out below —
see
[`http2-testconnection-socket-closed-cluster-FIXED.md`](http2-testconnection-socket-closed-cluster-FIXED.md)
for the full writeup (`javax/net/SocketFactory.createSocket` fabricating a
synthetic-layout `java/net/Socket` under `CRATONVM_REAL_NET_SOCKETS=1`,
which real `Socket` bytecode then misread). `SimpleAjpClient.connect()`
goes through the exact same `SocketFactory.getDefault().createSocket(host,
port)` call as `Http2TestBase`. Post-fix validation on the Windows suite
runner: `TestAbstractAjpProcessor` went from 30/30 failing (`Socket is not
connected`) to 2/30 failing — the 2 remaining are newly-VISIBLE,
unrelated, genuine residuals (were masked by the whole-class failure). The
AJP secret residual is now fixed; see
[`ajp-testsecret-secret-attribute-not-enforced-FIXED.md`](ajp-testsecret-secret-attribute-not-enforced-FIXED.md).
The only separately tracked remaining residual is
[`ajp-testnoheaders-response-body-not-empty.md`](../../known-issues/tomcat-08-07/ajp-testnoheaders-response-body-not-empty.md).

Original write-up follows for the record.

**Status:** OPEN. **Severity:** high (whole class fails). **HotSpot:** PASS
(fresh-verified).

## Summary

`org.apache.coyote.ajp.TestAbstractAjpProcessor` fails all 30 of its
tests:
```
1) testPostMultipleContentLength(org.apache.coyote.ajp.TestAbstractAjpProcessor)
java.net.SocketException: Socket is not connected
	at java.net.SocketException.<init>(SocketException.java:47)
	at java.net.Socket.getOutputStream(Socket.java:1043)
	at org.apache.coyote.ajp.SimpleAjpClient.cping(SimpleAjpClient.java:372)
	at org.apache.coyote.ajp.TestAbstractAjpProcessor.doTestPost(TestAbstractAjpProcessor.java:634)
	at org.apache.coyote.ajp.TestAbstractAjpProcessor.testPostMultipleContentLength(TestAbstractAjpProcessor.java:622)
```
`Tests run: 30, Failures: 30` — every single test method fails identically
via `SimpleAjpClient.cping()` (an AJP protocol "CPing" health-check/keep-
alive packet) calling `Socket.getOutputStream()` on a socket the JDK
considers not connected. This is the AJP-protocol analogue of the HTTP/2
[Socket is closed cluster](http2-testconnection-socket-closed-cluster.md)
— both involve a test client's `Socket` object appearing unconnected/
closed at the point some later operation (here `getOutputStream`, there
`setSoTimeout`) is attempted, suggesting a shared underlying issue in how
CratonVM's `Socket`/native socket layer reports or maintains connection
state across a `connect()` → later-use sequence, now confirmed across two
different protocol test harnesses (AJP and HTTP/2). Worth investigating
together rather than as fully independent bugs.

Found via a fresh Windows full-suite rerun (dev commit `080e79256`,
2026-07-12, real JDK, JIT on). Verified via a fresh same-session HotSpot
run: PASSES on HotSpot (all 30 tests).

## Reproduction

```powershell
cd apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName ajpsocket `
  -Start <idx> -Count 1 -TimeoutSec 120 -Parallel 1
# org.apache.coyote.ajp.TestAbstractAjpProcessor
```

## Recommendation

Given the shared symptom shape with the HTTP/2 cluster (a `Socket` that
should be connected reporting itself as not-connected/closed at the point
of a later operation), investigate both together: trace
`SimpleAjpClient`'s connect sequence
(`org.apache.coyote.ajp.SimpleAjpClient`, likely a plain
`new Socket(host, port)` or `Socket.connect(SocketAddress)`) against
CratonVM's `java.net.Socket`/native socket-channel implementation to see
whether the connected-state flag/field is being lost, cleared, or racing
with something else between connect and this later use. If the root cause
is shared with the HTTP/2 cluster, fixing it likely resolves both at once.
