# CratonVM's `HttpURLConnection` never pooled/reused TCP connections (no `KeepAliveCache` equivalent)

## Status
**FIXED** — 2026-07-22 (second attempt, same day as the first revert below).
`native-builtins/src/http_url_connection.rs` now implements a scoped,
plain-HTTP-only keep-alive pool inside `perform()`. `TestWeb.testServer()`'s
`Einstellung` assertion passes (265ms, isolated repro), and the full
`TestWeb` class now runs past `testServer()` into `testWebApp()` with no
performance regression (see "Why the first attempt regressed, and what's
different this time" below).

## Severity (as filed)
**MEDIUM** — not a correctness bug in the narrow sense (every individual
request/response still completed correctly), but a missing feature with a
**large blast radius**: real JDK's `sun.net.www.protocol.http.HttpURLConnection`
pools/reuses TCP connections to the same `(host, port, scheme)` across
*separate* `URL.openConnection()` calls whenever the previous response was
fully drained and the connection wasn't closed by either side. CratonVM's
`HttpURLConnection` "real carrier" implementation had no equivalent: every
`perform()` call did a brand-new `TcpStream::connect()`. This affects every
test that relies on server-side, connection-scoped state persisting across a
client's logical sequence of requests to the same server — not just this one
H2 assertion.

## Symptom (this specific case)
`org.h2.test.server.TestWeb.testServer()`:
```java
client.setAcceptLanguage("de-de,de;q=0.5");
result = client.get(url);              // GET /
client.readSessionId(result);
result = client.get(url, "login.jsp"); // GET /login.jsp?jsessionid=...
assertContains(result, "Einstellung"); // was FAILING under CratonVM: page in English
```
Root cause (unchanged from the original filing): `WebThread.parseHeader()`
only persists a resolved `Accept-Language` locale onto the H2 `WebSession`
when `session != null` **at the moment that header line is parsed** — before
the same `process()` call resolves `session` from the URL's `jsessionid`.
It only persists on a second-or-later request handled by **the same
`WebThread` instance**, i.e. only if the underlying TCP connection is kept
alive and reused. Confirmed via packet capture against real JDK 25 (see
git history of this doc for the full tcpdump analysis) — real JDK's
`KeepAliveCache` serves `GET /` and `GET /login.jsp` over one persistent
connection; CratonVM's `perform()` didn't.

## The fix
Added a plain-HTTP-only (`scheme == "http"`, no custom `SSLSocketFactory`
up-call) connection pool inside `perform()` in
`native-builtins/src/http_url_connection.rs`:

- `conn_pool(): Mutex<HashMap<(host, port), Vec<PooledConn>>>`, `PooledConn {
  stream: TcpStream, returned_at: Instant }`, capped at `POOL_MAX_PER_KEY = 4`
  entries per key.
- **Take** (`pool_take`): pops the most-recently-returned entry (LIFO — the
  freshest connection is checked first). If it's older than
  `POOL_IDLE_WINDOW` (2s), the *whole bucket* is dropped without a network
  probe (every other entry, returned earlier, is at least as stale). Else a
  **non-blocking `peek()`** checks for an already-visible FIN/RST/EOF —
  `WouldBlock` (nothing pending) is the only "looks alive" signal; anything
  else drops the whole bucket.
- **Use** (`try_pooled_request`): writes the request, then reads the first
  response bytes with a **short, fixed `POOL_PROBE_TIMEOUT` (300ms)** —
  deliberately NOT the caller's full configured read timeout (which can be
  60s). Once at least one real response byte arrives, the connection is
  proven alive and the caller's normal `read_timeout` takes back over for
  the rest of the body (via `read_response_with_prefix`, so the probe bytes
  aren't re-read).
- **On any failure** at any stage (peek, write, or the bounded first read),
  `perform()` treats it exactly like a plain pool miss — silently discards
  the connection (`pool_clear`s the rest of that key's bucket too, since one
  dead connection strongly implies the whole server generation behind it is
  gone) and falls straight through to the pre-existing fresh-connect path.
  No new error sentinel, no change to `perform_with_retry`'s existing
  retry-on-immediately-closed-fresh-connection logic.
- **Put back** (`pool_put`): only for a response with unambiguous framing —
  explicit `Content-Length`, or a status/method RFC 9110 §6.4.1 guarantees
  carries no body (HEAD, 204, 304, 1xx) — and no `Connection: close` from
  the peer. Chunked responses are excluded (narrower scope, not a framing
  requirement — a fully-consumed chunked body does leave a clean boundary,
  just not validated this round). Happens right after a successful
  `perform()`, not gated on the Java-level `disconnect()` call, since
  CratonVM already fully drains the response synchronously inside
  `perform()` regardless of whether/when the caller calls `disconnect()`.

## Why the first attempt regressed, and what's different this time
The first attempt (below) hit a genuine **28–30s regression** on
`TestWeb`. Root-caused this round via `WebThread`/`WebServer`/`TestWeb`
source inspection (not previously done): `TestWeb.test()` runs **~8
independent `Server`/`WebServer` instances sequentially in one process, all
bound to the same fixed port (8182)** — `testAlreadyRunning`, `testTools`,
`testStartWebServerWithConnection`, `testServer`, `testWebApp`,
`testIfExists`, `testSpecialAutoComplete`, etc. Each one's
`finally { server.shutdown(); }` synchronously force-closes any still-open
kept-alive sockets (`WebServer.stop()` iterates `running` `WebThread`s and
calls `stopNow()` → `socket.close()` on each). A `(host,port)`-keyed pool
therefore **inevitably** hands the next test method's first request a
connection left over from the *previous, now-dead* server instance — that
is the dominant source of "staleness" in this suite, not some inherent
property of the H2 wire protocol or a property of connection pooling in
general.

The first attempt's fix used a 2-second probe-read timeout applied to
*every* candidate it tried, with (implicitly) no bound on how many stale
candidates it could burn through per transition — cheap individually, but
the accumulated cost across ~8 server transitions plus routine staleness
within `testWebApp()`'s ~50 sequential requests added up to 28-30s.

This attempt bounds the same failure mode differently:
- A non-blocking `peek()` catches the common case (peer's `socket.close()`
  sent a FIN well before the next request arrives — test methods do real
  DB/SQL work in between) for **effectively zero added latency**.
- The residual race (peek looks alive, but the peer tears down before or
  during the write/first-read) is bounded by a **short, fixed 300ms probe**,
  not a multi-second one, and only ever fires once per bucket before the
  whole bucket is dropped (no per-candidate repeated cost).

**Verified via temporary debug instrumentation** (`CRATONVM_POOL_DEBUG`,
removed before merge) on a full `TestWeb` run: **31 pool hits, only 2
peek-evictions, 0 age-evictions, 0 post-peek write/read failures** — the
pool almost never even needs its fallback path, and when it does, the cost
is a single non-blocking peek, not a timeout wait. The suite's ~22-25s
total runtime is genuine interpreter work across `testServlet`,
`testWrongParameters`, `testTools`, `testAlreadyRunning`,
`testStartWebServerWithConnection`, `testServer`, and partial `testWebApp`
(~50 real HTTP+SQL round trips) — not pool overhead.

## The second residual (`testWebApp` / `autoCompleteList.do`) — confirmed unrelated, pre-existing, filed separately
The first attempt's doc flagged a second failure once the suite got past
`testServer()`: `testWebApp()` (`TestWeb.java:388`,
`autoCompleteList.do?query=select 'abc`) gets an **empty response body**
where `assertContains(..., "'")` expects content, and it was unclear
whether this was pool-reuse-specific.

**Confirmed NOT pool-related this round**, via a raw `curl` probe against
the same running `WebServer` instance (bypassing CratonVM's
`HttpURLConnection` client entirely — no pooling code involved at all): the
server itself returns `HTTP/1.1 200 OK` / `Content-Length: 0` on the very
first `autoCompleteList.do` request for a brand-new session. Traced further
to a genuine NPE, independent of networking:
`WebSession.loadBnf()`'s `Bnf.getInstance(null)` call throws
`NullPointerException: Cannot invoke
"org.h2.bnf.Rule.autoComplete(org.h2.bnf.Sentence)" because "this.link" is
null` (confirmed with an isolated `Bnf.getInstance(null)` repro — the
`help.csv` grammar resource itself loads fine, 270598 bytes). H2's own
`loadBnf()` silently swallows this (`catch (Exception e) { // ok we don't
have the bnf }`), so `session.getBnf()` returns `null` forever for that
session, and `WebApp.autoCompleteList()` returns `"autoCompleteList.jsp"`
without ever populating the `autoCompleteList` session var the JSP
template interpolates — hence the empty body. Filed as its own known issue:
`../../../known-issues/h2/bug-h2-bnf-ruleelement-link-null-npe-autocomplete.md`.

## Verification
- `cargo test -p cratonvm-native-builtins http_url_connection` — 39 tests
  pass (30 pre-existing + 9 new pool-specific: `is_poolable_response`
  classification, put/take roundtrip, closed-peer eviction, idle-window
  eviction, per-key cap eviction — all against real loopback `TcpStream`
  pairs).
- Isolated `TestWeb.testServer()` reflection repro: **PASS in 265ms**
  (previously failed the `Einstellung` assertion; the reverted attempt
  took 28-30s).
- Full `org.h2.test.server.TestWeb` class: reaches `testWebApp()` (further
  than ever before) in ~22-25s wall-clock, confirmed attributable to real
  work, not pool staleness (see debug-instrumented counts above). Fails on
  the separate, pre-existing, now-filed `Bnf`/`RuleElement.link` NPE.
- Regression smoke check (suites that exercise `HttpURLConnection` heavily
  outside H2): `org.apache.catalina.connector.TestRequest` (40/40 tests)
  and `org.apache.catalina.filters.TestRemoteIpFilter` (27/27 tests) against
  the shared Tomcat fixture at `/data/data/apps/tomcat` — both clean.

## Note: a third, independent concurrent implementation attempt was found merged into dev during this fix's own merge
While merging this fix into `dev`, discovered that a *third*, independent
attempt at this same feature had *also* been merged into `dev` in the
interim (commit `9d1cf3fed`, bundled into an unrelated "fix: wildfly CCE"
commit) — a `perform_pooled`/`pool_take_live`/`conn_pool` implementation
using a 30s idle timeout + 2s reuse-probe-timeout design. **That merged
implementation was silently broken in two ways**, confirmed by checking out
`origin/dev` HEAD in isolation and running `cargo check`:
1. **`dev` HEAD did not compile at all**: a duplicate `fn perform_with_retry`
   definition (two copies of the same function, `E0428`) — i.e. `dev` was
   red for anyone who pulled it before this merge landed.
2. **Even past that, its wiring in `huc_real_perform` was dead/duplicated**:
   the pooled-or-plain result was computed into `resp`, then immediately
   *shadowed* by a second, unconditional `perform_with_retry(...)` call
   whose result was what actually got used — meaning every plain-HTTP
   request would have been sent to the server **twice** (a real correctness
   bug for non-idempotent requests, and the pool's own result was always
   discarded, making the feature dead code even if the compile error were
   fixed).

Resolved by removing that entire implementation (the `PoolKey`,
`conn_pool`/`pool_take_live`/`pool_put`/`is_poolable`/`connect_plain`/
`configure_stream`/`attempt_plain`/`perform_pooled` block, and the duplicate
`perform_with_retry`) and the dead double-call site in `huc_real_perform`,
replacing it with this fix's implementation (integrated inside `perform()`
itself, so it's shared transparently by both `huc_real_perform` and
`ensure_connected` rather than needing its own call site). Verified the
merged result with `cargo check --workspace` (clean) and a full
`cargo test -p cratonvm-native-builtins http_url_connection` run (39/39
pass) before pushing.

## Related
- `docs/internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-testweb-logout-connectexception-mismatch-FIXED.md` — the fix that originally exposed this.
- `../../../known-issues/h2/bug-h2-bnf-ruleelement-link-null-npe-autocomplete.md` — the separate, pre-existing bug this fix newly exposed reachability to.
- `native-builtins/src/http_url_connection.rs` — `perform()`, `pool_take`/`pool_put`/`pool_clear`/`try_pooled_request`/`is_poolable_response`.

## Implementation attempt (reverted, 2026-07-22, earlier same day)
Built a scoped, plain-HTTP-only pool in `http_url_connection.rs`:
`(host, port)`-keyed `Mutex<HashMap<PoolKey, Vec<(TcpStream, Instant)>>>`,
small per-key cap, idle timeout, non-blocking `peek()` liveness check before
handing out a pooled connection, chunked/`Connection: close` responses
excluded from pooling, and a fresh-connection retry-once fallback on any
failure. It did fix `testServer()`'s `Einstellung` assertion, but was
reverted after a confirmed 28-30s regression on `TestWeb`, root-caused this
round as described above. The reverted code lived on branch
`fix/h2-httpurlconnection-keepalive-pool-20260722` in worktree
`/data/wt-h2-testweb-logout-20260722` (branch deleted, never pushed); this
fix (branch `fix/h2-httpconn-keepalive-pool-20260722`, worktree
`/data/wt-h2-httpconn-keepalive-20260722`) is a fresh implementation
informed by, but not reusing, that code.
