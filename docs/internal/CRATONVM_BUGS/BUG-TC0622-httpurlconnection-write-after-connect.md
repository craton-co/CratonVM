# Bug TC0622 — `HttpURLConnection.getOutputStream()` after `connect()` wrongly throws "cannot write after connect"

> **✅ FIXED 2026-06-23** — merged to `dev` `8e44e8c5` (branch
> `fix/huc-real-jdk-carrier`, commit `7b8f37d1`). Two parts, both fixed:
> (1) `getOutputStream` on a real carrier no longer consults the misread
> synthetic `HUC_CONNECTED` slot — it returns an identity-keyed buffered
> `ByteArrayOutputStream`, so write-after-`connect()` is legal as on HotSpot;
> (2) `huc_real_perform` now sends the real method + buffered body, and
> `getOutputStream` promotes a still-default **GET→POST** (JDK semantics) — the
> POST tests were actually being sent as GET → **405 Method Not Allowed**, which
> was the real failure (not GC staleness — the body buffer delivers fine).
> Validated: `TestInputBuffer` 2 FAIL→**PASS** (9500-byte POST echo round-trips),
> `TestCoyoteInputStream` 1→**PASS**, `TestApplicationDispatcher` 3→1. Same
> one-file subsystem fix as [#9](BUG-TC0622-authenticator-401-403-cluster.md) /
> [#2](BUG-TC0622-addcharsetfilter-contenttype-null.md).

> **Root cause:** the test harness (`TomcatBaseTest.postUrl`) calls
> `connection.connect()` *before* `connection.getOutputStream()` — which is legal
> for a `doOutput=true` POST on HotSpot, where `connect()` only opens the socket
> and the request body is buffered/sent lazily. The connection object is a
> **real-JDK** `sun.net.www.protocol.http.HttpURLConnection` (produced by
> `URI.create(path).toURL().openConnection()`), whose instance layout is the
> JDK's — it does **not** match CratonVM's synthetic `HUC_*` field slots. The
> synthetic `getOutputStream` native (`native-builtins/src/http_url_connection.rs`
> → `huc_get_output_stream`) reads the `HUC_CONNECTED` slot (field index 7) of
> that real object, which lands on an **unrelated real field that reads 1**, and
> unconditionally throws `IOException("HttpURLConnection.getOutputStream: cannot
> write after connect")`. Even with the field read fixed, the shim has **no
> real-URL POST/output-stream path at all**: its only real-URL code
> (`huc_real_perform`) hardcodes `"GET"` with an empty body, so a real-JDK POST
> request body is never sent.

**Severity:** Medium (blocks every Tomcat connector/dispatcher test that POSTs a
request body through the shared `postUrl` harness; the request-body write path of
the real-JDK `HttpURLConnection` shim is unimplemented).

**Status on CratonVM:** FAIL (rejects a legal write-after-connect). **HotSpot:** PASS.
**Run date:** 2026-06-22
**Binary:** dev `df11ac00` (worktree `C:\craton\CratonVM-tctest`).

**Affected classes (3):**
`org.apache.catalina.connector.TestCoyoteInputStream`,
`org.apache.catalina.connector.TestInputBuffer`,
`org.apache.catalina.core.TestApplicationDispatcher`.

(All three fail identically; in `TestApplicationDispatcher` the error fires
3×, in `TestInputBuffer` 2× — once per POST-body test.)

## Symptom

Every test that posts a request body through `TomcatBaseTest.postUrl` throws:

```
java.io.IOException: HttpURLConnection.getOutputStream: cannot write after connect
    at org.apache.catalina.startup.TomcatBaseTest.postUrl(TomcatBaseTest.java:809)
    at org.apache.catalina.connector.TestInputBuffer.testBug60400(TestInputBuffer.java:76)
    ...
```

The harness sequence (`TomcatBaseTest.postUrl`, JDK-legal on HotSpot) is:

```java
HttpURLConnection connection = (HttpURLConnection) url.openConnection();
connection.setDoOutput(true);
connection.setReadTimeout(1000000);
...
connection.connect();                                   // line 806
try (OutputStream os = connection.getOutputStream()) {  // line 809  <-- throws here
    while (streamer != null && streamer.available() > 0) {
        os.write(streamer.next());
        os.flush();
    }
}
int rc = connection.getResponseCode();
```

On HotSpot, `connect()` for a `doOutput=true` connection only establishes the
socket; the request line/headers/body are buffered and not flushed until the
response is requested, so `getOutputStream()` after `connect()` is allowed.

## Root cause (connect / output-stream lifecycle in the shim)

The shim lives in `native-builtins/src/http_url_connection.rs`. Its synthetic
field layout assumes CratonVM's own carrier object:

```
HUC_CONN_ID            = 0   // i32 conn id (-1 = unconnected)
...
HUC_DO_OUTPUT          = 6
HUC_CONNECTED          = 7   // i32, 1 once connected
```

But the harness gets a **real-JDK** `sun.net.www.protocol.http.HttpURLConnection`
(field 0 = `URLConnection.url`, a `java/net/URL`), already documented in the
file's own comment (lines ~121-125): "*`HUC_CONNECTED` lands on an unrelated real
field that reads 1*". Two compounding faults result:

1. **Misread CONNECTED slot.** `huc_get_output_stream` gates on
   `matches!(ctx.get_field(this, HUC_CONNECTED), Value::Int(1))` (field 7 of the
   real object) and, because that slot reads `1`, throws "cannot write after
   connect" — regardless of whether a write is actually legal. (`huc_connect`
   itself also early-returns via the same misread `HUC_CONNECTED`, so no real
   request is even staged.) This is the proximate cause of the IOException.

2. **No real-URL POST path.** The shim's real-JDK fast paths
   (`huc_real_perform`, the http branch of `huc_get_input_stream`,
   `huc_get_response_code`) all perform a hardcoded `"GET"` with empty headers
   and **empty body** (`perform(&parsed, "GET", &[], &[], ...)`).
   `huc_get_output_stream` has *no* real-URL branch at all — it only knows how to
   allocate a `ByteArrayOutputStream` into the synthetic `HUC_REQ_BODY_STREAM`
   slot of a synthetic carrier. So even if fault (1) is suppressed, a real-JDK
   POST body has nowhere to be buffered and would never be transmitted.

This is the same shim and the same real-vs-synthetic layout mismatch as the
sibling `getHeaderFields()`-missing finding; this doc covers the distinct
output-stream/connect-ordering lifecycle gap (request-body write path), not the
response-header accessor gap.

## Reproduction

```powershell
$exe = "C:\craton\CratonVM-tctest\target\release\cratonvm-tcfull-0622.exe"
$cp  = (Get-Content C:\craton\CratonVM\apps\tomcat\.tooling\cp.txt -Raw).Trim()
$env:CRATONVM_REAL_NET_SOCKETS=1; $env:CRATONVM_REAL_AQS=1
$env:CRATONVM_DISABLE_DEFAULT_WATCHDOG=1
& $exe -Xmx2g -cp $cp org.junit.runner.JUnitCore `
    org.apache.catalina.connector.TestInputBuffer
```

A minimal standalone repro is the harness sequence itself against any local HTTP
server: `openConnection()` → `setDoOutput(true)` → `connect()` →
`getOutputStream()` (throws on CratonVM, returns a writable stream on HotSpot).

## Recommendation

**FIX / investigate** in `native-builtins/src/http_url_connection.rs`. The
correct shape is to make the real-JDK `HttpURLConnection` carrier a first-class
case rather than misreading synthetic `HUC_*` slots:

- `huc_get_output_stream` must detect the real-JDK object (field 0 is a
  `java/net/URL`, as `huc_real_object_url`/`huc_get_input_stream` already do) and,
  for it, **not** consult the synthetic `HUC_CONNECTED` slot. Return a buffered
  `ByteArrayOutputStream` keyed by the connection's identity hash (the same
  identity-keyed side-table pattern `huc_real_perform` already uses for
  `RealResult`), so a write-after-`connect()` is allowed.
- The real-URL request path must honor the actual HTTP **method** and **request
  body** instead of hardcoding `"GET"`/empty: when `getResponseCode()` /
  `getInputStream()` runs on a real-JDK connection that has a buffered output
  body, `perform(...)` should be invoked with the real method (POST/PUT) and that
  body, and with the request headers set via `setRequestProperty`.

Scope is bounded to this one file and mirrors the existing real-JDK GET handling;
no VM-core change is required. High value because it unblocks the entire family
of connector/dispatcher POST tests routed through `TomcatBaseTest.postUrl`. (No
fix was applied here — this is a handoff with the cast site and the missing path
both identified.)
