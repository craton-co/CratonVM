# FIXED: `HttpExchange.getRequestURI()` now returns a complete `URI`

Status: FIXED 2026-07-14 - remote build-and-probe verified.

Date observed: 2026-07-14, Azure Linux build host.

## Resolution (2026-07-14)

`HttpExchange.getRequestURI()` stored the HTTP request target as a `String`,
but then allocated a six-slot synthetic `java.net.URI` and wrote that text to
slot 0. In real-JDK mode slot 0 is the `scheme`, not the URI's full-text
cache/path, so the URI natives correctly observed no usable path or external
form.

The getter now reads the target before allocating and uses the existing
layout-safe `make_uri` helper. That helper populates real JDK URI fields by
name (`string`, `path`, query, and scheme-specific-part), avoiding all
synthetic-versus-real field-slot collisions.

## Verification

A dedicated release build on the Azure Linux host served
`GET /hello/a%20b?mode=check` with `uri=/hello/a%20b?mode=check`, decoded
`path=/hello/a b`, `rawPath=/hello/a%20b`, and `query=mode=check`.

## Original symptom (fixed)

Inside a `com.sun.net.httpserver.HttpHandler.handle(HttpExchange)` callback,
`exchange.getRequestURI()` returns a real, non-null `java.net.URI` instance
(confirmed: `getClass()` reports `java.net.URI`), but both `toString()` and
`getPath()` on it return the **empty string**, regardless of the actual
request path the client sent.

```java
s.createContext("/hello/", ex -> {
    java.net.URI u = ex.getRequestURI();
    System.err.println("uri=" + u);          // prints "uri=" (empty)
    System.err.println("path=" + u.getPath()); // prints "path=" (empty)
    ...
});
```
A real HTTP client hitting `GET /hello/x` should see `uri=/hello/x`,
`path=/hello/x`.

This is **not** a general `java.net.URI` bug — a directly-constructed URI
works correctly:
```java
java.net.URI u = new java.net.URI("http://127.0.0.1:8080/hello/x");
System.out.println(u.toString()); // correctly prints http://127.0.0.1:8080/hello/x
System.out.println(u.getPath());  // correctly prints /hello/x
```
So the defect is specific to however `HttpExchange`'s `requestURI` field gets
constructed/populated during request parsing inside CratonVM's
`com.sun.net.httpserver.HttpServer` implementation
(`native-builtins/src/net_phase_e.rs`), not the `URI` class itself.

## Original impact (fixed)

Blocks any `HttpHandler` that routes on the request path — which is exactly
what Keycloak's `TestClassServer` (see
[`../internal/keycloak-testclassserver-invalidpackage-classnotfound-FIXED.md`](../internal/keycloak-testclassserver-invalidpackage-classnotfound-FIXED.md))
does:
```java
String resource = httpExchange.getRequestURI().getPath().substring(CONTEXT_PATH.length() - 1);
```
With `getPath()` returning `""`, this either throws
`StringIndexOutOfBoundsException` or silently misroutes every request — this
is now the last confirmed blocker preventing that test from running fully
end-to-end via the real upstream `com.sun.net.httpserver.HttpServer` (the two
other blockers found the same day — `String.getBytes()` returning empty, and
the underlying `URLClassLoader` isolation bug — are both fixed).

## Original repro

```java
import com.sun.net.httpserver.*;
import java.net.InetSocketAddress;
public class UriInHandlerRepro {
    public static void main(String[] a) throws Exception {
        HttpServer s = HttpServer.create(new InetSocketAddress("127.0.0.1", 8613), 10);
        s.createContext("/hello/", ex -> {
            System.err.println("uri=" + ex.getRequestURI());
            System.err.println("path=" + ex.getRequestURI().getPath());
            byte[] bytes = "hello-world".getBytes();
            ex.sendResponseHeaders(200, bytes.length);
            ex.getResponseBody().write(bytes);
            ex.close();
        });
        s.start();
        Thread.sleep(8000);
    }
}
```
```
cratonvm --java-home <jdk25> -c . UriInHandlerRepro &
curl http://127.0.0.1:8613/hello/x
# expected stderr: uri=/hello/x, path=/hello/x
# actual:          uri=,        path=
```

## Historical investigation notes (superseded)

The notes below are retained for provenance; the resolution and verification above are authoritative.

Not investigated yet — the fix session for the co-discovered
`String.getBytes()`/`java.util.Properties` bugs above ran out of scope budget
before reaching this one. Likely candidates for whoever picks this up:
- Check how `HttpExchange`'s `requestURI` field gets constructed in
  `native-builtins/src/net_phase_e.rs`'s request-parsing path (`parse_http_request`
  per an earlier session's notes) — does it call `new URI(rawPathString)` (real
  bytecode, should work per the isolated repro above) or construct/allocate the
  `URI` object some other way (a synthetic allocation with unpopulated fields,
  bypassing the real constructor)?
- Given the pattern from the two bugs fixed alongside this one
  (`d8092acb`'s `set_drop_synthetic_stubs` silently dropping ambiently-categorized
  natives), check `CRATONVM_DBG_DROPPED_STUBS=1` output for anything in
  `java/net/URI.*` or in whatever native(s) `HttpExchange`'s request-parsing path
  depends on to build the URI — though a first pass grep for `java/net/URI\.`
  found nothing dropped, so this is likely a different mechanism, not the same
  ambient-category bug.
