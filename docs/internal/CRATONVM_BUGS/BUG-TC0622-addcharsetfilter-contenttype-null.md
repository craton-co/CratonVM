# Bug TC0622 — `HttpURLConnection.getHeaderFields()` returns an empty map (unregistered plural header accessor on CratonVM's HttpURLConnection native shim)

> **✅ FIXED 2026-06-23** — merged to `dev` `8e44e8c5` (branch
> `fix/huc-real-jdk-carrier`, commit `7b8f37d1`). Registered `getHeaderFields()`
> (builds a real `LinkedHashMap<String,List<String>>` from the parsed response
> headers, with GC-safe pinning) and made the real-JDK carrier cache its response
> headers so both the singular and plural accessors work. Validated:
> `TestAddCharSetFilter` 8 FAIL→**1** (the residual is the separate `ISO-8859-3`
> charset gap, not this bug). Same one-file subsystem fix as
> [#9](BUG-TC0622-authenticator-401-403-cluster.md) /
> [#4](BUG-TC0622-httpurlconnection-write-after-connect.md).

> **One-line root cause:** CratonVM intercepts `HttpURLConnection` with native
> shims that perform the HTTP request in Rust and expose the response headers
> ONLY through the *singular* `getHeaderField(String)` / `getHeaderField(int)` /
> `getHeaderFieldKey(int)` accessors. The *plural* `getHeaderFields()` method
> (the `Map<String,List<String>>` accessor) is **registered nowhere**, so it
> falls through to the real JDK `URLConnection.getHeaderFields()` bytecode —
> which reads the JDK's internal response `MessageHeader` that the native
> `connect()`/`getResponseCode()`/`getInputStream()` shims never populated (they
> short-circuit the JDK's real connection machinery). The result is an
> **empty header map**. `TomcatBaseTest.methodUrl` builds its `resHead` map purely
> from `connection.getHeaderFields()` (TomcatBaseTest.java:713), so `resHead`
> ends up empty, `getSingleHeader("Content-Type", headers)` returns `null`
> (TomcatBaseTest.java:857), and the test's
> `getSingleHeader(...).toLowerCase(Locale.ENGLISH)` NPEs at
> `TestAddCharSetFilter.java:121`.

**Severity:** Medium (general HTTP-client gap, not specific to this filter: ANY
TomcatBaseTest-style test that reads response headers via the `resHead`
`Map<String,List<String>>` out-parameter of `getUrl`/`methodUrl`/`headUrl`/`postUrl`
sees an empty map under CratonVM. It is a real VM behavior gap — the response
*does* contain the headers on the wire and the singular `getHeaderField`
accessor returns them; only the `Map`-returning accessor is missing — but the
blast radius is limited to header-asserting HTTP tests, hence Medium rather than
High.)
**Status on CratonVM:** FAIL (8 of 8 tests NPE). **HotSpot:** PASS (8/8).
**Run date:** 2026-06-22
**Binary:** dev `df11ac00` (worktree `C:\craton\CratonVM-tctest`).

## Affected classes / tests

`org.apache.catalina.filters.TestAddCharSetFilter` — all 8 tests fail:

- `testNoneSpecifiedMode1`, `testNoneSpecifiedMode2`, `testNoneSpecifiedMode3`
- `testDefault`, `testDefaultMixedCase`
- `testSystem`, `testSystemMixedCase`
- `testUTF8`

All 8 fail with the identical NPE at `doTest` line 121. The defect is in
CratonVM's **`HttpURLConnection` native layer**, not in Tomcat, the filter, or
the test. The same gap affects any other `TomcatBaseTest` subclass that passes a
`resHead` map to `getUrl`/`methodUrl`/`postUrl` and then asserts on its contents.

## Symptom (real stacktrace)

```
java.lang.NullPointerException: Cannot invoke "String.toLowerCase(java.util.Locale)"
    at org.apache.catalina.filters.TestAddCharSetFilter.doTest(TestAddCharSetFilter.java:121)
    at org.apache.catalina.filters.TestAddCharSetFilter.doTest(TestAddCharSetFilter.java:85)
    at org.apache.catalina.filters.TestAddCharSetFilter.testNoneSpecifiedMode1(TestAddCharSetFilter.java:45)
    ...
```

Line 121 is:

```java
String ct = getSingleHeader("Content-Type", headers).toLowerCase(Locale.ENGLISH);
```

`getSingleHeader(...)` returns `null` (the `headers` map has no `"Content-Type"`
entry → `getSingleHeader` returns `null` at TomcatBaseTest.java:857), so calling
`.toLowerCase(...)` on it NPEs. The `headers` map is `null`-of-content because it
was populated entirely from `connection.getHeaderFields()` and that returned an
empty map.

The `.log.err` also shows, for `testNoneSpecifiedMode3` only:

```
ERROR ... Servlet.service() for servlet [servlet] ... threw exception
  (java/lang/IllegalArgumentException: Unsupported charset: iso-8859-3)
```

That is a **separate, unrelated** charset-availability gap (CratonVM's JDK image
does not provide the `ISO-8859-3` charset) and is **not** the cause of the test
failures: it affects only Mode3, yet all 8 tests — including `testDefault` and
`testUTF8`, which use ordinary `text/plain` / `utf-8` and produce no servlet
error — fail with the *same* NPE on the *same* null header. The common cause is
the empty header map, not the charset.

## Root cause analysis

### How the test reads the header

`TestAddCharSetFilter.doTest` does a real HTTP round-trip to the embedded Tomcat
(run with `CRATONVM_REAL_NET_SOCKETS=1`):

```java
Map<String,List<String>> headers = new HashMap<>();
getUrl("http://localhost:" + getPort() + "/", new ByteChunk(), headers);
String ct = getSingleHeader("Content-Type", headers).toLowerCase(Locale.ENGLISH);
```

`getUrl(path, out, resHead)` → `methodUrl(...)`, whose ONLY way of filling
`resHead` is (TomcatBaseTest.java:710-718):

```java
if (resHead != null) {
    for (Map.Entry<String, List<String>> entry :
            connection.getHeaderFields().entrySet()) {   // <-- plural accessor
        if (entry.getKey() != null) {
            resHead.put(entry.getKey(), entry.getValue());
        }
    }
}
```

So `resHead` is exactly the contents of `HttpURLConnection.getHeaderFields()`.
The test never calls the singular `getHeaderField("Content-Type")`.

### Why CratonVM returns an empty map

CratonVM does not run the JDK's real `HttpURLConnection` connection logic; it
replaces the connection-level methods with native shims that perform the request
over Rust sockets and stash the parsed response in side state. There are two such
shim families and **neither registers `getHeaderFields()`**:

1. `native-builtins/src/http_url_connection.rs`
   (`sun/net/www/protocol/http/HttpURLConnection`,
   `.../https/HttpsURLConnectionImpl`, `java/net/HttpURLConnection`,
   `javax/net/ssl/HttpsURLConnection`). Registers `getHeaderField(String)`,
   `getHeaderField(I)`, `getHeaderFieldKey(I)` — but **not** `getHeaderFields()`.
   For the real-JDK object path it goes further and *discards the headers
   entirely*: `huc_real_perform` caches only `RealResult { status, body }`
   (the `perform(...)` call's `_headers` is dropped at line 187), so even the
   singular accessor has nothing for that path.

2. `native-builtins/src/net_phase_e.rs` (the synthetic
   `java/net/HttpURLConnection` carrier handed out by `URL.openConnection()`).
   `huc_perform` *does* store the parsed response headers in field
   `HUC_RESP_HEADERS` (net_phase_e.rs:3299) and registers `getHeaderField`
   (singular, by name and by index) reading from it — but again **no
   `getHeaderFields()`** registration.

Because `getHeaderFields()` is registered nowhere, the call dispatches to the
**real JDK `java.net.URLConnection.getHeaderFields()` bytecode**. That method
builds its `Map` from the connection object's internal parsed-response
`MessageHeader` — JDK state that is only populated by the JDK's own
`connect()`/`getInputStream()` implementation. CratonVM's native shims overrode
those methods and performed the request out-of-band, so the JDK-internal
response header state was never filled in. The real `getHeaderFields()`
therefore sees no headers and returns an empty (or empty-content) map.

Net effect: the response genuinely carries `Content-Type: text/plain;charset=...`
on the wire (and CratonVM's own `getHeaderField("Content-Type")` shim would
return it), but the `Map`-returning accessor the harness actually uses returns
nothing → `resHead` empty → `getSingleHeader` null → NPE.

### Why this is a real VM gap, not a test/harness artifact

The header data exists and is reachable through the singular accessor; only the
plural `Map` accessor is missing. The harness code (`methodUrl`) is the standard,
unmodified Tomcat test util used by many passing tests — those tests simply don't
read the `resHead` out-parameter. So this is a genuine, fixable
`HttpURLConnection`-native coverage hole, not a flaky or harness-only artifact.

### Not DF02 / not GC / not JIT

Deterministic on every run and every one of the 8 tests; a clean NPE from a
missing-native fallthrough with no GC-guard fingerprints (no all-zero header / no
"Stale pointer detected"), no ~1/N timing dependence. Purely a native-registration
coverage gap.

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat
$cp  = (Get-Content .tooling\cp.txt -Raw).Trim()
$exe = "C:\craton\CratonVM-tctest\target\release\cratonvm-tcfull-0622.exe"
$env:CRATONVM_REAL_NET_SOCKETS = "1"
$env:CRATONVM_REAL_AQS = "1"
$env:CRATONVM_DISABLE_DEFAULT_WATCHDOG = "1"
& $exe -Xmx2g -cp $cp org.junit.runner.JUnitCore `
    org.apache.catalina.filters.TestAddCharSetFilter
# Expect: Tests run: 8, Failures: 8 (8x NullPointerException at doTest:121)
```

Minimal standalone repro (any HTTP server reachable at the URL; the point is the
plural-vs-singular accessor divergence):

```java
import java.net.*;
import java.util.*;

public class HfRepro {
    public static void main(String[] a) throws Exception {
        HttpURLConnection c = (HttpURLConnection)
            URI.create("http://localhost:" + a[0] + "/").toURL().openConnection();
        c.connect();
        c.getResponseCode();
        // CratonVM: getHeaderField returns the value, but getHeaderFields() is empty.
        System.out.println("single = " + c.getHeaderField("Content-Type"));
        Map<String,List<String>> m = c.getHeaderFields();
        System.out.println("map size = " + m.size());
        System.out.println("map[Content-Type] = " + m.get("Content-Type"));
        // HotSpot: map is non-empty and contains Content-Type.
        // CratonVM: map size = 0, map[Content-Type] = null.
    }
}
```

## Duplicate / relationship check

- **Not a duplicate.** No existing `BUG-*.md` covers
  `HttpURLConnection.getHeaderFields()` / the `resHead` map being empty. The other
  TC0622 bug (`BUG-TC0622-corsfilter-contentequals-bounds.md`) is a
  `StringBuilder`-layout defect and unrelated. `BUG-DF03`/`BUG-DF04`/`BUG-DF07`
  are async-SocketChannel / `Net.available` / WebSocket-client issues, not the
  `HttpURLConnection` header accessor.
- The `ISO-8859-3` "Unsupported charset" line in the `.err` log is a distinct
  charset-availability gap (would, if fixed in isolation, still leave all 8 tests
  failing on the null header). Worth a separate note but not this bug.

## Recommendation

**FIX (bounded VM fix).** Register a `getHeaderFields()
[()Ljava/util/Map;]` native on the `HttpURLConnection` shim classes in BOTH
registration sites and build the `Map<String,List<String>>` from the same parsed
response headers the singular `getHeaderField` already reads:

- `native-builtins/src/net_phase_e.rs`: build the map from `HUC_RESP_HEADERS`
  (mirror the `getHeaderField(String)` parsing at net_phase_e.rs:4332-4358),
  grouping duplicate keys into the per-key `List<String>` and using a
  case-insensitive-friendly key set (HotSpot keys the map by the exact header
  name as sent, plus a `null` key for the status line, which `methodUrl` skips).
- `native-builtins/src/http_url_connection.rs`: register `getHeaderFields()` from
  `ConnState.response_headers`, AND stop discarding headers on the real-JDK
  object path — `huc_real_perform` must cache the `_headers` from `perform(...)`
  (extend `RealResult` to hold `headers`) so both the singular and plural
  accessors work for `URI.create(...).toURL().openConnection()` connections.

Low risk, high value: it closes a general `HttpURLConnection` correctness hole
(any code that calls `getHeaderFields()` currently gets an empty map) and turns
`TestAddCharSetFilter` 8 FAIL → PASS (modulo the separate `ISO-8859-3` charset
gap, which only affects `testNoneSpecifiedMode3` and should be filed/fixed
independently). The simplest robust implementation routes the existing
singular-accessor data into a `LinkedHashMap` and returns it.
