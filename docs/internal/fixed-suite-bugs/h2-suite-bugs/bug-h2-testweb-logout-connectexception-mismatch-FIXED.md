# `TestWeb.testStartWebServerWithConnection` expects `ConnectException` on server-shutdown logout, gets a generic `IOException` — FIXED

## Status
**FIXED** — dev, branch `fix/h2-testweb-logout-connectexception-20260722`. Root
cause was **not** a `ServerSocket`/`accept()`/`close()` race as originally
hypothesized — it's a missing retry-on-dead-connection behavior (and a
missing `ConnectException` mapping) in CratonVM's `HttpURLConnection`
"real carrier" native implementation.

## Original symptom
```
java.io.IOException: HttpURLConnection response failed: connection closed before response head
	at org/h2/test/server/WebClient.get(WebClient.java:135)
	at org/h2/test/server/WebClient.get(WebClient.java:40)
	at org/h2/test/server/TestWeb.testStartWebServerWithConnection(TestWeb.java:687)
```

## Actual root cause
`testStartWebServerWithConnection` starts a web console server with
`shutdownServerOnDisconnect=true`, then does `client.get(url, "logout.do")`
wrapped in `catch (ConnectException e)`. Two independent facts combine to
produce the test's exact expectation, confirmed with standalone minimal
repros (not H2 sources) run against real JDK 21 and 25 as well as CratonVM:

1. **H2's own self-shutdown is not an OS-level accept/close race — it's a
   deterministic self-close, identical on every JVM.** `WebApp.logout()`
   calls `server.shutdown()` synchronously, on the very thread that is
   handling the `logout.do` request. That unwinds into
   `WebServer.stop()`, which — after closing the listening `ServerSocket` —
   iterates `running` (every currently-active per-connection `WebThread`,
   **including the one executing `stop()` right now**, since it was added
   to `running` at accept time before its `run()` ever started) and calls
   `c.stopNow()` on each, closing its own socket. The request's response is
   only written *after* `stop()` returns, so it's written to an
   already-closed socket — a `SocketException` server-side, and a
   zero-byte connection close on the wire. A standalone repro mirroring
   just this control-flow shape (`WebServerRepro`/`WebServerRepro2` in the
   session scratch dir, not committed) reproduces this identically on real
   JDK 21/25 and CratonVM: **all three see zero response bytes.** This part
   was never a CratonVM bug.

2. **Real JDK's `HttpURLConnection` transparently retries once on exactly
   this failure shape.** `sun.net.www.protocol.http.HttpURLConnection`'s
   legacy dead-keep-alive-connection recovery heuristic — normally meant
   for a pooled connection the server silently timed out — also covers a
   brand-new connection the peer tears down mid-request with zero response
   bytes. Confirmed with `WebServerRepro2`: the client's `getResponseCode()`
   call transparently reissues the request over a **second, brand-new**
   TCP connection (`Total accepts on server: 1` — the retry's `connect()`
   never even reaches the listener). By the time that retry's `connect()`
   runs, `WebServer.stop()` has already closed the listening socket, so the
   retry gets `ECONNREFUSED` → real JDK surfaces `java.net.ConnectException`
   — exactly what the test's `catch (ConnectException e)` expects.

CratonVM's `HttpURLConnection` native shim
(`native-builtins/src/http_url_connection.rs`, `perform()`/
`huc_real_perform()`) had **no equivalent retry**, and no `ConnectException`
mapping for a refused connect at all (every transport failure — a refused
connect included — folded into a single generic `java.io.IOException`
arm). So it surfaced the *first* attempt's raw "connection closed before
response head" failure directly, instead of ever reaching the retry that
(on real JDK) produces `ConnectException`.

## Fix
Two changes in `native-builtins/src/http_url_connection.rs`:
1. Added `perform_with_retry()`, which wraps `perform()` and retries once
   (fresh TCP connect, resend the same already-buffered request) when the
   first attempt's error is exactly the "connection closed before response
   head" sentinel — mirroring HotSpot's dead-connection recovery heuristic.
   Only used for the buffered (non-streaming, non-custom-socket) path;
   `huc_real_perform`'s call site now calls this instead of `perform()`
   directly.
2. Added `CONNECT_REFUSED_SENTINEL`: `perform()`'s TCP connect loop now
   tags a `std::io::ErrorKind::ConnectionRefused` failure distinctly, and
   `huc_real_perform` maps that to a typed `RuntimeError::ConnectException`
   (matching every other native connect path in this codebase — plain
   `Socket`/`SocketChannel` already did this; `HttpURLConnection` was the
   one path that didn't).

Both pieces are necessary: without (1), the retry that could hit a refused
connect never happens; without (2), even a successfully-retried refused
connect would still surface as a generic `IOException` instead of
`ConnectException`.

## Verification
- `org.h2.test.server.TestWeb` — `testStartWebServerWithConnection` no
  longer throws; the class proceeds past it to the next test method,
  5/5 consecutive runs.
- `org.h2.test.server.TestJakartaWeb` — unaffected, still passes.
- Standalone repros (`SelfCloseRepro`, `WebServerRepro`, `WebServerRepro2`,
  `HeaderCheck`, `KeepAliveCheck` — session scratch dir, not committed)
  confirmed the mechanism on real JDK 21/25 before and after.

## Residual uncovered by this fix
Fixing this let `TestWeb.test()` proceed to `testServer()`, which now fails
on an **unrelated, pre-existing, much larger gap**: CratonVM's
`HttpURLConnection` never pools/reuses TCP connections across separate
`openConnection()` calls (no `KeepAliveCache` equivalent), while real JDK
does. See
`docs/known-issues/h2-suite-bugs/bug-h2-httpurlconnection-no-keepalive-pooling.md`
for the full root-cause writeup — deliberately **not** fixed here: a
correct keep-alive pool is a much bigger, higher-blast-radius feature
(`HttpURLConnection` is exercised by nearly every suite in this repo) that
deserves its own dedicated session rather than a rushed addition riding on
this fix.

## Related
- `docs/internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-nosuchmethoderror-cross-class-dispatch-FIXED.md` — Cluster B, whose 3rd class (`TestWeb`) this was the remaining blocker for.
- `docs/internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-dataoutputstream-writechars-data-loss-FIXED.md` — the fix that got `TestWeb` far enough to expose this as a distinct, separate issue.
- `docs/known-issues/h2-suite-bugs/bug-h2-httpurlconnection-no-keepalive-pooling.md` — the new residual this fix exposed.
