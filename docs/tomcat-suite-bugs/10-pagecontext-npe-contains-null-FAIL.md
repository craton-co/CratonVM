# Bug 10 — TestPageContext "contains on null" is the embedded-server serving wall (NOT a JSP/EL bug)

**Status:** OPEN — **re-diagnosed**. Real CratonVM gap, but **not** what the
original hypothesis assumed. This is a manifestation of the embedded-server
HTTP-serving wall (see [04](04-embedded-server-throughput-wall-OPEN.md)), **not**
a `PageContext`/EL bug.
**Severity:** Medium (blocks every embedded-server HTTP test, not just this one).
**Repro class:** `jakarta.servlet.jsp.TestPageContext` — `Tests run: 1, Failures: 1`.

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
