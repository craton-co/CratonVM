# CratonVM's `HttpURLConnection` never pools/reuses TCP connections (no `KeepAliveCache` equivalent)

## Status
**OPEN** — new finding, 2026-07-22, uncovered by fixing
`bug-h2-testweb-logout-connectexception-mismatch-FIXED.md` (that fix let
`TestWeb.test()` proceed from `testStartWebServerWithConnection` into
`testServer()`, which fails on this unrelated, pre-existing gap).

**A first implementation attempt (same session, same day) was tried and
reverted — not merged.** See "Implementation attempt (reverted)" below
before starting a second attempt: it identifies a specific, load-bearing
failure mode (a pooled connection frequently going stale between put and
take, at a rate high enough to dominate runtime) that a naive pool design
does not survive, and that whoever picks this up next should design around
from the start rather than discover the same way.

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

## Implementation attempt (reverted, 2026-07-22, same session)
Built a scoped, plain-HTTP-only pool in `http_url_connection.rs`:
`(host, port)`-keyed `Mutex<HashMap<PoolKey, Vec<(TcpStream, Instant)>>>`,
small per-key cap, idle timeout, non-blocking `peek()` liveness check before
handing out a pooled connection, chunked/`Connection: close` responses
excluded from pooling, and a fresh-connection retry-once fallback on any
failure (folding in the sibling ConnectException fix's same retry
condition). It **did fix `testServer()`'s `Einstellung` assertion** —
confirmed via a long-timeout run reaching a *later* test method
(`testWebApp`, line 388) that the original bug never let the suite reach.

**Reverted anyway**, for two reasons found during verification:
1. **A large performance regression**: `org.h2.test.server.TestWeb` went
   from a sub-second baseline to a consistent **28–30 seconds** per run
   (5/5 runs, mix of eventual pass and 30s-timeout). Root cause: a pooled
   connection frequently goes stale (peer already closed) *between* being
   put back in the pool and the next take — common enough in this suite
   to dominate runtime, not a rare edge case. The `peek()` liveness check
   only catches a peer that already sent a FIN; it does not catch "our
   write lands in a half-dead connection and the peer never responds",
   which then stalls on `read()`. First cut used the caller's full
   (default 60s) read timeout for that first post-reuse read — worth
   fixing (bounded it to a short ~2s probe timeout instead,
   `POOL_REUSE_PROBE_TIMEOUT`), but that only capped the *per-hit* cost;
   it didn't reduce how *often* reused connections turn out to be stale,
   which is apparently often enough in this suite's request pattern to
   still cost ~25–30s in accumulated short stalls plus fresh-reconnect
   retries. **Whoever attempts this next should treat "why do pooled
   connections go stale this fast/often" as the actual open question** —
   possibilities not yet investigated: H2's `WebServer`/`WebThread` may
   close idle connections far more eagerly than assumed (check
   `WebThread`'s per-request-cycle logic, not just `WebServer.stop()`),
   or the pool's idle-timeout/liveness check needs to be much more
   conservative (e.g. only ever reuse a connection immediately after the
   *previous* response completed, never across any gap), or connection
   reuse needs to be scoped narrower than "any plain-HTTP request to this
   host:port" (e.g. only within one logical client-side session object).
2. **A second, separate residual surfaced** once the suite got past
   `testServer()`: `testWebApp()` (`TestWeb.java:388`,
   `autoCompleteList.do?query=select 'abc`) got an **empty response body**
   where `assertContains(..., "'")` expected content. Not investigated —
   unclear whether this is pooling-connection-reuse-specific (e.g. a
   corrupted request/response boundary from a race the retry-on-stale
   fallback doesn't fully cover) or an unrelated, independently-masked bug
   the same way `bug-h2-testweb-logout-connectexception-mismatch` masked
   this doc's whole topic. Whoever fixes the performance issue should
   re-check whether this is still reachable/still fails before assuming
   it's pool-related.

The reverted code lived on branch `fix/h2-httpurlconnection-keepalive-pool-20260722`
in worktree `/data/wt-h2-testweb-logout-20260722` (branch deleted, never
pushed) — re-derive from this doc rather than trying to recover it; the
design (pool structure, `is_poolable` gating, retry-on-stale fallback
shape) is fully described above and is a reasonable starting point, but
the stale-connection-frequency question needs an answer before it's worth
re-implementing verbatim.

## Related
- `docs/internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-testweb-logout-connectexception-mismatch-FIXED.md` — the fix that exposed this.
- `native-builtins/src/http_url_connection.rs` — where a `KeepAliveCache`-equivalent would need to live, likely keyed alongside the existing `real_reqs`/`real_results` identity-keyed side tables.
