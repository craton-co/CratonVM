# Bug TC0622 — DefaultServlet Range request: GET `206`-vs-`200` FOLDS into the (fixed) dropped-header bug; residual HEAD `-1` is a distinct HEAD-response-body read defect

> **✅ FULLY FIXED 2026-06-23 — `TestDefaultServletRfc9110Section14` FAIL→PASS.**
> Both layers are now resolved on `dev`: the GET `206` case folds into the
> HttpURLConnection-carrier fix (`dev` `8e44e8c5`), and the residual HEAD `-1`
> is fixed by making `read_response` HEAD-aware (`dev` `873355f1`, branch
> `fix/tomcat-quick-wins`): a HEAD response carries the would-be Content-Length
> but no body, so the reader now returns immediately after the head instead of
> blocking until the read timeout and collapsing a valid status to `-1`.
> Validated end-to-end: the class is **PASS**. (Root-cause detail below.)

> **DISAMBIGUATION (the headline result).** The original `expected:<206> but
> was:<200>` failure on the GET range request **FOLDS INTO the already-fixed
> dropped-`setRequestProperty`-header bug**
> ([`BUG-TC0622-authenticator-401-403-cluster.md`](BUG-TC0622-authenticator-401-403-cluster.md),
> fixed on `dev` `8e44e8c5` / commit `7b8f37d1`). It is **NOT** a DefaultServlet
> range-handling bug — CratonVM parses the `Range` header and returns `206` correctly
> once the header actually reaches the server. **Re-running this exact class with the
> fixed binary makes the GET-`206` assertion (line 61) PASS.** What remains is a
> **separate, distinct** failure: the **HEAD** range request now reaches the server,
> the server answers `200` with a `Content-Length`, but the CratonVM
> `HttpURLConnection` shim's response reader **hangs reading a body that a HEAD
> response never sends**, times out, and `getResponseCode()` returns `-1`
> (`expected:<200> but was:<-1>` at line 67). This residual is in
> `native-builtins/src/http_url_connection.rs::read_response`, not in DefaultServlet.

**Severity:** Medium. The user-visible range bug (GET `206`) is already fixed; the
remaining HEAD defect breaks any TomcatBaseTest-style test that issues a `HEAD`
request through `HttpURLConnection` against a server that returns a `Content-Length`
(common: HEAD on a static resource).

**Status on CratonVM:** FAIL. **HotSpot:** PASS.
**Run date:** 2026-06-23
**Binary:** original failure observed on `dev df11ac00`
(`.tooling/results/tcfull0622/craton`); re-run for disambiguation on the fixed
binary `C:\craton\CratonVM-hucfix\target\release\cratonvm-hucfix.exe` (carries the
`fix/huc-real-jdk-carrier` header fix, `dev` `8e44e8c5`).
**Affected class:** `org.apache.catalina.servlets.TestDefaultServletRfc9110Section14`
(`testRangeHandlingDefinedMethods`). The sibling test in the same class,
`testUnsupportedRangeUnit`, **PASSES** on both binaries.

## Symptom

Original run (`df11ac00`, before the header fix) — fails at the **GET** assertion
(`TestDefaultServletRfc9110Section14.java:61`):

```
java.lang.AssertionError: Range requests is turn on, SC_PARTIAL_CONTENT of GET is expected expected:<206> but was:<200>
    at org.apache.catalina.servlets.TestDefaultServletRfc9110Section14.testRangeHandlingDefinedMethods(TestDefaultServletRfc9110Section14.java:61)
```

Re-run with the **fixed** binary (`cratonvm-hucfix.exe`) — the GET assertion now
PASSES; the failure moves on to the **HEAD** assertion (line 67):

```
java.lang.AssertionError: Range requests is turn on, SC_OK of HEAD is expected expected:<200> but was:<-1>
    at org.apache.catalina.servlets.TestDefaultServletRfc9110Section14.testRangeHandlingDefinedMethods(TestDefaultServletRfc9110Section14.java:67)
```

(`Time: 260.918` — the HEAD leg spins for ~4 min on the response read timeout
before returning `-1`; cf. the original run's `Time: 87.806`.)

## The test (what it does, and why each leg matters)

`testRangeHandlingDefinedMethods` serves `test/webapp/index.html` via `DefaultServlet`
and sends `Range: bytes=0-10`:

1. **GET** (line 60–62) — expects `206 SC_PARTIAL_CONTENT`. *(was the reported bug)*
2. GET — expects `Accept-Ranges: bytes` response header.
3. **HEAD** (line 66–67) — `methodUrl(..., Method.HEAD)`, expects `200 SC_OK`
   (a HEAD must not return partial content). *(the residual)*

The harness drives all of this through `TomcatBaseTest.getUrl` / `methodUrl`
(`TomcatBaseTest.java:685`), which sets the `Range` header via
`connection.setRequestProperty("Range", "bytes=0-10")` — the **exact** mechanism the
dropped-header bug broke.

`testUnsupportedRangeUnit` (sends `Range: Chars=0-10`, expects `200`) **passes on both
binaries**; that result is unconditionally consistent — a present-but-unknown unit and
an absent header both yield `200` — so it is not a discriminator on its own.

## Why the GET `206`-vs-`200` is the dropped-header bug, not a range bug

`DefaultServlet.parseRange` (`DefaultServlet.java:1572`) returns `FULL` (→ `200`)
whenever the `Range` header is **absent**:

```java
String rangeHeader = request.getHeader("Range");
if (rangeHeader == null) {
    // No Range header is the same as ignoring any Range header
    return FULL;                        // -> serveResource sends 200, full body
}
...
ranges = Ranges.parse(new StringReader(rangeHeader));   // "bytes=0-10" -> units=bytes
...
if (!ranges.getUnits().equals("bytes")) return FULL;    // the Chars= path
...
return ranges;   // -> serveResource sets SC_PARTIAL_CONTENT (206) at DefaultServlet.java:1212
```

Under the **buggy** `df11ac00` HUC shim, every header set via `setRequestProperty`
was dropped (the synthetic `HUC_*` slot write landed on the wrong real-JDK carrier
field — see the authenticator bug doc), so the server saw **no** `Range` header,
`parseRange` took the `rangeHeader == null` branch, and returned the full body with
`200`. That is precisely the observed `expected:<206> but was:<200>`.

**Proof it folds in:** the same class, re-run **unchanged** on the
fixed-header binary, advances past the GET assertion (line 61) and fails at the HEAD
assertion (line 67). The GET path now returns `206`, so DefaultServlet's
`Range`-parsing, `validate`, and `setStatus(SC_PARTIAL_CONTENT)` logic is correct on
CratonVM; the original divergence was entirely the dropped header. `Ranges.parse`,
`isRangeRequestsSupported()` (returns `true`, `DefaultServlet.java:2505`) and the
`getUnits().equals("bytes")` comparison were never at fault.

## Root cause of the residual (HEAD `-1`) — `read_response` ignores the request method

`native-builtins/src/http_url_connection.rs`, `read_response` (line ~563), reads the
response body purely from the response's framing headers and **has no knowledge of the
request method**:

```rust
if let Some(target) = content_length {              // HEAD response DOES carry Content-Length
    let target = target.min(MAX_RESPONSE_BODY);
    while body_buf.len() < target {                 // ...but sends NO body bytes
        let n = stream.read(&mut tmp)?;             // blocks; eventually read-timeouts -> Err
        if n == 0 { break; }
        body_buf.extend_from_slice(&tmp[..n]);
    }
    body_buf.truncate(target);
}
```

RFC 9110 §9.3.2: *the server MUST NOT send a message body in the response to a HEAD,*
even though it sends the same `Content-Length` it would for the GET. Tomcat does
exactly that — `200`, `Content-Length: <file size>`, empty body. CratonVM's reader
sees `Content-Length: N`, loops waiting for `N` body bytes that never arrive, and
blocks until the 60 s read timeout (`perform`'s `read_timeout`,
`http_url_connection.rs:270`) fires. The `read` then errors, `perform` returns
`Err(_)`, and `huc_get_response_code` returns **`-1`** (line ~287). The valid `200`
status line was already parsed and is sitting in `headers`/`status`, but it is
discarded because the *body* read failed.

`perform` calls `read_response(&mut s)` (lines 754 / 759) **without** passing
`method`, so the reader cannot suppress body-reading for HEAD. (`build_request`
correctly emits `HEAD …`, and `huc_set_request_method` accepts `"HEAD"` at line
~1288, so the request itself is well-formed and the server responds correctly — this
is purely a response-side framing bug.)

This is a **distinct** defect from the dropped-header bug (a different code path:
response framing, not request-header transport) and is currently **undocumented** —
no existing `BUG-*.md` covers a HEAD `-1` / response-body-vs-HEAD issue.

## Reproduction

```powershell
$exe = "C:\craton\CratonVM-hucfix\target\release\cratonvm-hucfix.exe"   # fixed-header binary
$cp  = (Get-Content C:\craton\CratonVM\apps\tomcat\.tooling\cp.txt -Raw).Trim()
$TC  = "C:\craton\CratonVM\apps\tomcat"
$env:CRATONVM_REAL_NET_SOCKETS=1; $env:CRATONVM_REAL_AQS=1
$env:CRATONVM_DISABLE_DEFAULT_WATCHDOG=1; $env:CRATONVM_ROOTSNAP_CACHE=1
& $exe -Xmx2g -Dfile.encoding=UTF-8 -Djava.net.preferIPv4Stack=true `
  "-Dtomcat.test.basedir=$TC\output\build" "-Dtomcat.test.temp=$TC\output\test-tmp" `
  "-Dtomcat.test.tomcatbuild=$TC\output\build" -Dtomcat.test.relaxTiming=true `
  --add-opens java.base/java.lang=ALL-UNNAMED --add-opens java.base/java.io=ALL-UNNAMED `
  --add-opens java.base/java.util=ALL-UNNAMED --add-opens java.base/java.util.concurrent=ALL-UNNAMED `
  -cp $cp org.junit.runner.JUnitCore `
  org.apache.catalina.servlets.TestDefaultServletRfc9110Section14
# -> Tests run: 2, Failures: 1; failure now at line 67 (HEAD), "expected:<200> but was:<-1>"
#    (the GET-206 assertion at line 61 PASSES). Wall time ~260s (HEAD read timeout).
```

Minimal standalone repro of the residual (no DefaultServlet needed — any HTTP server
that returns `Content-Length` to a HEAD):

```java
HttpURLConnection c = (HttpURLConnection) URI.create("http://<host>/index.html").toURL().openConnection();
c.setRequestMethod("HEAD");
int rc = c.getResponseCode();
// HotSpot: 200.  CratonVM: -1 (after a ~60s read-timeout stall).
```

## Recommendation

**FIX in the VM** (per repo policy no VM source was modified here). The user-facing
range bug is *already fixed* (header transport); only the HEAD response-read defect
remains, and it is small and well-localized.

In `native-builtins/src/http_url_connection.rs`:

1. **Pass the request `method` into `read_response`** (from `perform`, which already
   has it at line ~266). When `method == "HEAD"`, **do not read a response body** —
   parse the status line + headers (including the advertised `Content-Length`) and
   return immediately with an empty body, regardless of `Content-Length` /
   `Transfer-Encoding`. (RFC 9110 §9.3.2.) The same applies to `304 Not Modified` and
   `204 No Content` responses, which also carry no body — those should be skipped by
   status code on every method.
2. **Defensive hardening:** if a body read times out/errors *after* a valid status
   line was parsed, prefer returning the already-parsed `status` (and the headers/body
   read so far) rather than collapsing the whole result to `-1`. A successful status
   line should not be lost to a body-framing mismatch.

Bounded change; high value — it unblocks `HEAD` through the shared `TomcatBaseTest`
HTTP client (this class and any other range/conditional/HEAD test) and removes a
60-second-per-HEAD timeout stall.

## Cross-checks

- **Not the dropped-header bug for the residual** — that bug is fixed
  (`8e44e8c5`); with it fixed the GET-`206` assertion passes, proving the
  range/header *transport* is correct. The residual is response-side framing.
- **Not `getHeaderFields()`-empty** ([`BUG-TC0622-addcharsetfilter-contenttype-null.md`](BUG-TC0622-addcharsetfilter-contenttype-null.md),
  also fixed in `7b8f37d1`) — that fails with an NPE on an empty `resHead`, not a
  `-1` response code; here the failure is `getResponseCode()` returning `-1`.
- **Not `write-after-connect`** ([`BUG-TC0622-httpurlconnection-write-after-connect.md`](BUG-TC0622-httpurlconnection-write-after-connect.md))
  — that concerns POST/output-stream buffering; HEAD has no request body.
- **Not a DefaultServlet defect** — `parseRange` / `Ranges.parse` /
  `serveResource` (`setStatus(SC_PARTIAL_CONTENT)`, `DefaultServlet.java:1212`) are
  exercised correctly once the header arrives; the GET leg returns `206`.
