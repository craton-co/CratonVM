# Bug 10 — TestPageContext "contains on null" is the embedded-server serving wall (NOT a JSP/EL bug)

**Status:** SERVER FIXED; remaining blocker is the in-process HTTP **client**.
**Not** a `PageContext`/EL bug (the original hypothesis). Three findings, none
"pure perf":
- **Layer 1 — connector reset (FIXED, `fix/tomcat-suite-bugs-09-10`).** The NIO
  connector reset every accepted request before reading it, because
  `NioEndpoint.setSocketOptions` → `SocketChannel.setOption` hit an
  `AbstractMethodError` (the `sc_set_option` native was registered only with the
  `NetworkChannel` covariant-return descriptor, not the `SocketChannel` one). Fix
  in `native-io/src/socket_channel.rs`.
- **The embedded SERVER serves HTTP correctly — PROVEN.** With layer 1, running a
  minimal embedded Tomcat (programmatic servlet) on CratonVM and hitting it with
  an **external `curl`** returns `HTTP/1.1 200` + the body, servlet `doGet`
  invoked, connection healthy. So group 04's "servers don't serve" framing is
  refuted for the connector — it serves; the prior failures were the `setOption`
  reset.
- **Remaining blocker — the in-process HTTP CLIENT.** `TomcatBaseTest.getUrl`
  (every embedded-HTTP test) uses `HttpURLConnection`, which CratonVM bridges to
  a native Rust HTTP client (`native-builtins/src/http_url_connection.rs::perform`
  → raw `std::net::TcpStream`). When that client runs **in the same process** as
  the server, `getResponseCode()` returns `-1`, the server's `doGet` is **never**
  invoked, and the socket layer logs **no** server-side read (capture empty) — i.e.
  the server never even processes the request. An external curl against the same
  server works, and an in-process raw `java.net.Socket` client (separate
  write/read calls) also gets `doGet` invoked; only the native `perform` (a single
  long blocking native call that connects+writes+reads with no Java safepoint in
  between) fails. Leading hypothesis: `perform`'s uninterrupted native blocking
  I/O on the calling thread starves the server's worker threads (no safepoint /
  GC progress) until its read times out → `-1`. This — not the server — is why
  `TestPageContext` (and getUrl-based tests) still FAIL.
**Severity:** Medium-High (blocks every getUrl-based embedded-HTTP test).
**Repro class:** `jakarta.servlet.jsp.TestPageContext` — `Tests run: 1, Failures: 1`.

## Next step for the remaining blocker

Make the native `HttpURLConnection.perform` cooperate with the VM's
safepoint/thread model during blocking I/O (e.g. run it as a safepoint-safe
"in native" region, or chunk connect/write/read so the calling thread reaches a
safepoint), OR drop the native `HttpURLConnection` bridge so the real
`sun.net.www.protocol.http` bytecode runs over the now-working socket layer.
A minimal repro is `scratch/rec0910/mini/MiniHU.java` (CratonVM
`HttpURLConnection` client + CratonVM server in one process → code=-1) vs the
external-`curl` success.

## Symptom

```
java.lang.NullPointerException: Cannot invoke contains on null
  at jakarta.servlet.jsp.TestPageContext.testBug49196(TestPageContext.java:34)
```

Line 34 is `Assert.assertTrue(result.contains("OK"))`, where
`result = res.toString()` and `res = getUrl(".../bug49nnn/bug49196.jsp")`. So
`res.toString()` is **null** — i.e. the HTTP GET returned an **empty body**.

## Root cause (re-diagnosed — original "EL/PageContext null" hypothesis was WRONG)

The JSP `bug49196.jsp` is trivial (`pageContext.getErrorData()` then prints
`OK`) and `getErrorData()` already null-guards the status code, so the JSP is not
the problem. The real failure is one layer down, in HTTP serving:

The embedded Tomcat NIO connector **starts and binds a port**, **accepts the TCP
connection**, but then **resets the connection without ever sending an HTTP
response** — for *every* request, static or JSP.

Diagnosis (custom `TomcatBaseTest` subclass `TPCDiag`, both real-net + AQS env on):

| request | via | result |
|---|---|---|
| `/test/index.html` (**static**) | `HttpURLConnection` | `getResponseCode() == -1`, body `null` |
| `/test/bug49196.jsp` (**JSP**)  | `HttpURLConnection` | `getResponseCode() == -1`, body `null` |
| `/test/index.html` (**static**) | **raw `java.net.Socket`** | connects OK, then **`SocketException: Connection reset` (os err 10054) on read** |
| `/test/bug49196.jsp` (**JSP**)  | **raw `java.net.Socket`** | same connection reset |

Key conclusions:
- A **static file** fails identically to the JSP → this is **not** JSP/Jasper/EL
  related. The original hypothesis (a `PageContext`/EL accessor returning null,
  shared with bugs 07/08) is **refuted**.
- The raw socket **connects** (TCP accept works) but the server **RSTs the
  connection on read** — so the accepted socket is never read/processed/answered.
  This is a server-side **NIO-connector** defect, downstream of accept.
- Server log corroboration during startup: `StandardWrapperValve[Container is
  null]` / `StandardEngineValve[Container is null]` stop() messages — the
  container pipeline / wrapper wiring is not fully initialised, consistent with
  accepted requests not reaching a working servlet pipeline.

So the chain is: connector accepts → accepted `SocketChannel` is not driven
through the `NioEndpoint` poller/read→process→write cycle → connection reset →
client reads empty/`-1` → `ByteChunk.toString()==null` → `null.contains("OK")`.

This is the same wall as group 04 (embedded-server serving): basic blocking
`Socket` round-trips work (`SockProbe`), but the full `Http11NioProtocol` /
`NioEndpoint` poller + `Selector` + non-blocking `SocketChannel` read/write cycle
does not complete a request. `native-io/src/nio_selector.rs` is a mature, heavily
debugged implementation (deadlock fixes, GC-stable lock ordering), so the
remaining defect is a subtle accept→register→read interaction, not a missing
primitive.

## Why this is NOT a surgical fix

Fixing it means making the Tomcat NIO connector actually serve an HTTP request
end-to-end under CratonVM — i.e. solving group 04, the dominant OPEN wall. That
is a large, high-risk effort in the mature NIO/connector subsystem, well beyond a
per-test fix. Tracking it under group 04; the per-class FAIL/HANG/NOSUMMARY
flakiness of `TestPageContext` across suite runs is consistent with this serving
wall plus timing.

## Next steps (for the group-04 effort)

- Instrument the `NioEndpoint` accept→`Poller.register`→`Selector.select` path:
  confirm whether the accepted `SocketChannel` is registered with the poller's
  `Selector` and whether a read-ready event is ever delivered for it.
- Check the `StandardWrapperValve[Container is null]` pipeline wiring — whether
  the context/wrapper container is actually started for `addWebapp` deployments.
- A passing end-to-end raw-socket `GET` against an embedded `Tomcat` is the
  minimal green target before any JSP/servlet-level test can pass.

## Reproduction

```
cratonvm.exe -Xmx2g -cp <cp> org.junit.runner.JUnitCore \
  jakarta.servlet.jsp.TestPageContext   # CWD: apps/tomcat
# -> Tests run: 1, Failures: 1 ; HotSpot: PASS
# Root cause: embedded NIO connector resets accepted connections without
# responding (status -1 / empty body) for ALL requests, static or JSP.
```
