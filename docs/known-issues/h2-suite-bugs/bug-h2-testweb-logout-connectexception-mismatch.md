# `TestWeb.testStartWebServerWithConnection` expects `ConnectException` on server-shutdown logout, gets a generic `IOException`

## Status
**OPEN** — new finding, 2026-07-22, uncovered by
`bug-h2-dataoutputstream-writechars-data-loss-FIXED.md`'s fix (which fixed
2 of 3 Cluster B classes in
`bug-h2-nosuchmethoderror-cross-class-dispatch-FIXED.md`; this is why the
3rd, `TestWeb`, still fails). Not related to either of those docs' root
causes — confirmed this class's HTTP request/response cycle otherwise works
correctly, and H2's `WebServer` code path never calls `writeChars` anywhere.

## Severity
**LOW** — affects one test's handling of an intentional server self-shutdown
race, not a data-correctness or dispatch-integrity issue.

## Symptom
```
java.io.IOException: HttpURLConnection response failed: connection closed before response head
	at org/h2/test/server/WebClient.get(WebClient.java:135)
	at org/h2/test/server/WebClient.get(WebClient.java:40)
	at org/h2/test/server/TestWeb.testStartWebServerWithConnection(TestWeb.java:687)
```
`testStartWebServerWithConnection` starts an H2 web console server, does
several `GET`s (login, tools.jsp, etc. — all succeed, confirmed via
`CRATONVM_DBG_SOCK` trace showing correct `200 OK` responses for each), then
calls `client.get(url, "logout.do")` wrapped in:
```java
try {
    client.get(url, "logout.do");
} catch (ConnectException e) {
    // the server stops on logout
}
```
On HotSpot, the web server's self-shutdown (triggered by handling the
logout request) races with this last client request such that the client
observes `java.net.ConnectException` (connection refused — the server has
already stopped listening by the time the client's `connect()` lands).
Under CratonVM, the client instead observes an accepted connection that
then closes mid-response — a plain `IOException`, not the `ConnectException`
subclass the `catch` is scoped to, so it propagates uncaught.

## Analysis (not root-caused, just scoped)
This looks like a **timing/ordering difference in `ServerSocket` shutdown
semantics** rather than a data-corruption bug: real OS-level
`ServerSocket.close()` immediately stops the kernel from accepting further
connections on that port, so a client racing the shutdown either connects
successfully before the close (gets a real response) or is refused
(`ConnectException`) — there's no OS-level window for "accepted, but the
application-level connection handler was already torn down." If CratonVM's
`ServerSocket.accept()`/`close()` implementation continues to service an
already-queued or in-flight `accept()` after the Java-level
`Server.stop()` call has begun tearing down application state (but before
the underlying listener is fully closed), that would produce exactly this
"accepted then reset mid-response" shape instead of a clean refusal.
Not confirmed — this is a hypothesis for whoever picks this up next, not a
established root cause.

## Repro
```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home /home/victor/jdk25 \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.server.TestWeb
```
Trace the actual socket-level exchange with:
```bash
CRATONVM_DBG_SOCK=1 CRATONVM_DBG_SOCK_BYTES=1 <cratonvm-bin> ... org.h2.test.server.TestWeb
```

## Related
- `docs/internal/h2-suite-bugs/bug-h2-nosuchmethoderror-cross-class-dispatch-FIXED.md` — Cluster B, whose 3rd class (`TestWeb`) this is the remaining blocker for.
- `docs/internal/h2-suite-bugs/bug-h2-dataoutputstream-writechars-data-loss-FIXED.md` — the fix that got `TestWeb` far enough to expose this as a distinct, separate issue.
