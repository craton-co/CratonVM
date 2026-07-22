# CratonVM's `HttpURLConnection` never pools/reuses TCP connections (no `KeepAliveCache` equivalent)

## Status
**OPEN** — new finding, 2026-07-22, uncovered by fixing
`bug-h2-testweb-logout-connectexception-mismatch-FIXED.md` (that fix let
`TestWeb.test()` proceed from `testStartWebServerWithConnection` into
`testServer()`, which fails on this unrelated, pre-existing gap).

## Severity
**MEDIUM** — not a correctness bug in the narrow sense (every individual
request/response still completes correctly), but a missing feature with a
**large blast radius**: real JDK's `sun.net.www.protocol.http.HttpURLConnection`
pools/reuses TCP connections to the same `(host, port, scheme)` across
*separate* `URL.openConnection()` calls whenever the previous response was
fully drained and the connection wasn't closed by either side. CratonVM's
`HttpURLConnection` "real carrier" implementation
(`native-builtins/src/http_url_connection.rs`) has no equivalent: every
`perform()` call does a brand-new `TcpStream::connect()`, sends the
request, reads the response, and implicitly closes the socket when the
Rust function returns. This affects **every test in this repo that relies
on server-side, connection-scoped state persisting across a client's
logical sequence of requests to the same server** — not just this one H2
assertion.

## Symptom (this specific case)
`org.h2.test.server.TestWeb.testServer()`:
```java
client.setAcceptLanguage("de-de,de;q=0.5");
result = client.get(url);              // GET /
client.readSessionId(result);
result = client.get(url, "login.jsp"); // GET /login.jsp?jsessionid=...
assertContains(result, "Einstellung"); // FAILS under CratonVM: page is in English
```
Isolated, minimal reproduction (bypasses the rest of `TestWeb.test()`
entirely — no ordering dependency on other test methods):
```java
TestWeb t = new TestWeb();
t.init();
java.lang.reflect.Method m = TestWeb.class.getDeclaredMethod("testServer");
m.setAccessible(true);
m.invoke(t); // passes on real JDK 21/25, throws AssertionError under CratonVM
```

## Root cause (confirmed via packet capture)
`WebThread`'s per-request `Accept-Language` handling
(`WebThread.parseHeader()`) only persists a resolved locale onto the H2
`WebSession` when `session != null` **at the moment that header line is
parsed** — which is BEFORE the same `process()` call resolves `session`
from the URL's `jsessionid` parameter. So the *first* time a given
`WebThread` instance parses `Accept-Language`, `session` is always `null`
(no persist); it only persists on a *second or later* request handled by
**the same `WebThread` instance** — i.e. only if the underlying TCP
connection is kept alive and reused for that follow-up request, so the
`WebThread`'s `session` field (set during the *first* request's own
processing) is still populated when the *second* request's header parsing
runs.

Captured with `sudo tcpdump -i lo -A -s0 'tcp port 8182'` around a real-JDK
25 run of the isolated `testServer()` repro above: the `GET /login.jsp`
request's TCP segment (`seq 136:324`) is a direct continuation of the same
stream as the preceding `GET /` request (`localhost.46320 -> localhost.8182`,
same 4-tuple, contiguous sequence numbers) — **one persistent connection
serves both `client.get()` calls**, even though they come from two
completely separate `HttpURLConnection` instances created by two separate
`url.openConnection()` calls. That is real JDK's `KeepAliveCache` in
action: because `WebClient.get()` fully drains the response body via
`IOUtils.readStringAndClose(new InputStreamReader(in), -1)` before calling
`conn.disconnect()`, the JDK client recognizes the connection as cleanly
reusable and pools it for the next request to the same host:port.

CratonVM's `perform()` has no such pool: every call is a fresh
`TcpStream::connect()`, so `GET /` and `GET /login.jsp?jsessionid=...` land
on two different `WebThread` instances server-side, `session` starts `null`
in each, and the `Accept-Language` → locale persistence branch's guard
(`if (session != null) { ... }`) never fires for either request. The
translation stays at the `createNewSession()` default (English).

**A naive synthetic test that manually calls `disconnect()` after only a
partial `read()` (not fully draining the body) shows 3 separate accepts on
real JDK too** — don't use that shape to "confirm" HotSpot also lacks
pooling; it doesn't, it's just that a partial-read+disconnect is one of the
documented conditions under which real JDK discards rather than pools a
connection. Fully drain the response before disconnecting when
reproducing/testing this.

## Why this is NOT fixed alongside `bug-h2-testweb-logout-connectexception-mismatch`
Implementing a correct keep-alive pool means: a cache keyed by
`(scheme, host, port)`, safe concurrent reuse across `HttpURLConnection`
instances, honoring `Connection: close` / absence of keep-alive on either
the request or response, respecting a `Keep-Alive: timeout=N, max=M`
response header, idle-connection eviction, and — critically — never handing
out a connection that might have gone stale (peer closed) without a cheap
liveness check. `native-builtins/src/http_url_connection.rs` is already a
large file full of hard-won, narrowly-scoped fixes for `HttpURLConnection`
edge cases (redirects, streaming bodies, TLS handshake failures, custom
`SSLSocketFactory` sockets, real vs. synthetic carriers) — this feature
touches all of those code paths at once and is exercised by essentially
every suite in this repo that does outbound HTTP (Tomcat, Spring, WildFly,
Elasticsearch, Keycloak clients, etc.). Given that blast radius, this is a
dedicated-session feature addition, not a corollary of the ConnectException
fix.

## Repro
```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home /home/victor/jdk25 \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.server.TestWeb   # testServer() fails with the Einstellung AssertionError
```
Or the isolated reflection-based repro above, which avoids depending on
`TestWeb.test()`'s method ordering entirely.

## Related
- `docs/internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-testweb-logout-connectexception-mismatch-FIXED.md` — the fix that exposed this.
- `native-builtins/src/http_url_connection.rs` — where a `KeepAliveCache`-equivalent would need to live, likely keyed alongside the existing `real_reqs`/`real_results` identity-keyed side tables.
