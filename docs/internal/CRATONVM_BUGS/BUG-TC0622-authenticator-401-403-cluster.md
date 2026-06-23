# Bug TC0622 — BASIC/DIGEST auth cluster fails (401/403): client `HttpURLConnection.setRequestProperty` headers never reach the server

> **✅ FIXED 2026-06-23** — merged to `dev` `8e44e8c5` (branch
> `fix/huc-real-jdk-carrier`, commit `7b8f37d1`). Root cause exactly as
> diagnosed below: the real-JDK `sun.net.www...HttpURLConnection` carrier was
> poked through synthetic `HUC_*` slots, so `setRequestProperty` headers landed
> on unrelated real fields and never reached the server. **Fix:** route request
> method/headers/body through an identity-keyed side-table (`real_reqs`) and have
> `huc_real_perform` send them; register `getRequestProperty`; guard every
> synthetic-slot setter on a real carrier. Validated via `run-suite.ps1`:
> `TestAuthInfoResponseHeaders` 2 FAIL→**PASS**, `TestDigestAuthenticator` 17
> FAIL→**0 (serial 18/18 PASS)**, `TestPropertiesRoleMappingListener` 6→2,
> `TestAuthenticatorBaseCorsPreflight` 2→1, `TestRestCsrfPreventionFilter2` 2→1.
> One subsystem fix (`native-builtins/src/http_url_connection.rs`) shared with
> [#2](BUG-TC0622-addcharsetfilter-contenttype-null.md) and
> [#4](BUG-TC0622-httpurlconnection-write-after-connect.md). Residual partials
> (CorsPreflight 1, RestCsrf 1, PropertiesRoleMapping 2) are separate auth-flow
> edge cases, not the dropped-header root cause.

> **Root cause:** CratonVM's `HttpURLConnection` shim drops **every** request
> header set via `setRequestProperty` / `addRequestProperty` — they are stored by
> the synthetic native into the wrong place for the carrier object and the request
> writer emits only its three hard-coded defaults (`Host`, `User-Agent`,
> `Connection`). The test harness (`TomcatBaseTest.getUrl`/`methodUrl`) sets the
> `Authorization` (and `Origin`, CSRF-nonce, etc.) headers via
> `connection.setRequestProperty(...)`, so the **server never receives any auth
> header** and rejects the request: BASIC auth → `401`, CORS-preflight bypass →
> `403`. The defect is entirely **client-side** (the request-header transport of
> the `HttpURLConnection` shim); the server-side BASIC/DIGEST/realm/Base64/digest
> primitives are all correct (verified in isolation).

**Severity:** High — silently drops *all* user-set HTTP request headers on the
shared `TomcatBaseTest` HTTP client. Breaks every test whose request correctness
depends on a `setRequestProperty` header (auth, CORS, CSRF, conditional GET,
content negotiation, …), not just the authenticator suite.

**Status on CratonVM:** FAIL (`401`/`403` instead of `200`; digest first-leg
assertions fail). **HotSpot:** PASS.
**Run date:** 2026-06-22
**Binary:** dev `df11ac00` (worktree `C:\craton\CratonVM-tctest`).

## Affected classes

**Direct `200`/`403`-vs-actual cluster covered here:**

- `org.apache.catalina.authenticator.TestAuthInfoResponseHeaders` — `expected:<200> but was:<401>` (both tests)
- `org.apache.catalina.core.TestPropertiesRoleMappingListener` — `expected:<200> but was:<401>` (6 tests)
- `org.apache.catalina.filters.TestRestCsrfPreventionFilter2` — `expected:<200> but was:<401>` (both tests; fails on the first credentialed GET before any CSRF logic runs)
- `org.apache.catalina.authenticator.TestAuthenticatorBaseCorsPreflight` — `expected:<200> but was:<403>` (the two `Boolean.TRUE`/allow cases: `input[ALWAYS]` and `input[FILTER]`)

**Same root cause — explains the large Digest/Basic `AssertionError` cluster too:**

- `org.apache.catalina.authenticator.TestDigestAuthenticator` (17 failures)
- `org.apache.catalina.authenticator.TestDigestAuthenticatorB` (4 failures)
- `org.apache.catalina.authenticator.TestDigestAuthenticatorAlgorithms`

  These send the digest `authorization` header via the same
  `reqHeaders`/`setRequestProperty` path. Because the header is dropped, the
  server's `401` challenge body / nonce round-trip is wrong and the first-leg
  assertion (`TestDigestAuthenticator.java:196` `assertTrue(bc.getLength() > 0)`)
  and downstream digest assertions fail. **Not** a `MessageDigest`/digest-math
  defect.

## Symptom

```
java.lang.AssertionError: expected:<200> but was:<401>
    at org.apache.catalina.authenticator.TestAuthInfoResponseHeaders.doTest(TestAuthInfoResponseHeaders.java:113)
```
```
java.lang.AssertionError: expected:<200> but was:<403>
    at org.apache.catalina.authenticator.TestAuthenticatorBaseCorsPreflight.test(...)
```
```
java.lang.AssertionError                       (TestDigestAuthenticator)
    at org.junit.Assert.assertTrue(Assert.java:53)
    at org.apache.catalina.authenticator.TestDigestAuthenticator.doTest(TestDigestAuthenticator.java:196)
```

All four direct-cluster tests use `BasicAuthenticator` and build credentials with
`Base64.getEncoder().encodeToString(...)`, then send them via
`reqHeaders.put("authorization"/"Authorization", ...)` →
`TomcatBaseTest.getUrl/methodUrl` → `connection.setRequestProperty(key, value)`.

## Root cause — request headers dropped by the `HttpURLConnection` shim

Isolated end-to-end with an embedded Tomcat (BASIC auth + a debug realm + a valve
that dumps the headers the server actually received), driving the request through
the *exact* harness path `URI.create(url).toURL().openConnection()` +
`setRequestProperty("Authorization", "Basic …")`:

```
HotSpot:
  [SERVER-VALVE] received Authorization header = [Basic Zm9vOmJhcg==]
  [SERVER-VALVE] all header names = [Authorization,Cache-Control,Pragma,User-Agent,Host,Accept,Connection,]
  [REALM] authenticate(user=[foo], cred=[bar]) -> OK
  [CLIENT] HttpURLConnection responseCode = 200

CratonVM:
  [CLIENT] set X-Custom-Foo to [null]                 <-- getRequestProperty also broken
  [SERVER-VALVE] received Authorization header = [null]
  [SERVER-VALVE] all header names = [Host,User-Agent,Connection,]   <-- ONLY the 3 defaults
  [CLIENT] HttpURLConnection responseCode = 401
```

The server received **only** `Host`, `User-Agent`, `Connection` — exactly the
three headers the shim hard-codes. Every header set via `setRequestProperty`
(`Authorization`, `Accept`, the custom `X-Custom-Foo`, etc.) was silently dropped.
This was reproduced with `CRATONVM_REAL_NET_SOCKETS=1` **and** unset (identical),
so it is **not** the socket transport — it is the request-construction path.

Verified *not* a server-side primitive:
- **Base64** round-trip `encodeToString` → `decode(byte[])` is byte-identical to
  HotSpot (`dXNlcjpwd2Q=` → `user:pwd`).
- **`ConstantTime.equals`** (the plaintext credential comparator used by
  `MessageDigestCredentialHandler.matches` when no algorithm is set) is correct
  on CratonVM (nojit + jit), including the `(i - len2) >>> 31` unsigned-shift idiom.
- Feeding the server-side parse + realm directly
  (`BasicAuthenticator.BasicCredentials` on a `ByteChunk` →
  `TesterMapRealm.authenticate`) authenticates **OK** on CratonVM. When the header
  is delivered (raw-socket harness), the realm logs
  `getPassword`→`getPrincipal`→`authenticate -> OK`. The failure is purely that
  the header never arrives via `HttpURLConnection`.

### Mechanism (where the header is lost)

The active client path under this binary is the synthetic shim in
`native-builtins/src/http_url_connection.rs` (the server saw
`User-Agent: Java/CratonVM`, which only `build_request` at line ~358 emits;
`net_phase_e::http_build_request` emits `cratonvm-phaseE/1.0`).

- `huc_set_request_property` (line ~968) stores `"key: value"` lines into the
  synthetic field slot `HUC_REQ_HEADERS = 3`.
- `huc_connect` / `huc_get_response_code` → `ensure_connected` (line ~620) →
  `extract_headers` (line ~583) reads request headers from the **same** synthetic
  slot 3 and `build_request` (line ~327) emits them.

In isolation those two agree, so the loss comes from the **carrier-object
mismatch already documented for the sibling bug**
(`BUG-TC0622-httpurlconnection-write-after-connect.md`): the harness's
`URI.create(path).toURL().openConnection()` yields a **real-JDK**
`sun.net.www.protocol.http.HttpURLConnection` whose instance layout is the JDK's,
**not** CratonVM's synthetic `HUC_*` slots. On that object, the native
`setRequestProperty` write to "slot 3" and the native request-writer read of
"slot 3" land on **unrelated real fields**, so the user headers are never carried
into the emitted request. (`getRequestProperty` is not registered as a native at
all, so it falls through to the real-JDK method reading the real `requests` map —
which the native setter never populated — hence the `[null]` readback.)

A second, latent contributor: there are **two** competing `HttpURLConnection`
natives with *different* synthetic field layouts —
`http_url_connection.rs` (`HUC_REQ_HEADERS = 3`, `HUC_METHOD = 2`,
`HUC_URL_STR = 1`) and `net_phase_e.rs` (`HUC_REQ_HEADERS = 4`, `HUC_METHOD = 1`,
`HUC_URL = 0`). Whichever module wins `connect`/`getResponseCode` registration for
`java/net/HttpURLConnection` may read request headers from a slot the setter never
wrote, so the header set/transmit pair is fragile even on a synthetic carrier.

### Why `403` (CORS preflight) is the same bug

`AuthenticatorBase.allowCorsPreflightBypass` (line ~666) only bypasses the auth
constraint when `request.getHeader("Origin")` is present and valid. The two
failing `TestAuthenticatorBaseCorsPreflight` cases are exactly the ones that send
a valid `Origin` + `Access-Control-Request-Method` via `setRequestProperty` and
**expect `200`**. With those request headers dropped, the server sees no `Origin`,
the preflight bypass never triggers, the request hits the (NullRealm) auth
constraint and returns `403`. All the `Boolean.FALSE` cases happen to expect a
rejection anyway, so they pass for the wrong reason.

## Reproduction

```powershell
$exe = "C:\craton\CratonVM-tctest\target\release\cratonvm-tcfull-0622.exe"
$cp  = (Get-Content C:\craton\CratonVM\apps\tomcat\.tooling\cp.txt -Raw).Trim()
$env:CRATONVM_REAL_NET_SOCKETS=1; $env:CRATONVM_REAL_AQS=1
$env:CRATONVM_DISABLE_DEFAULT_WATCHDOG=1
& $exe -cp $cp org.junit.runner.JUnitCore `
    org.apache.catalina.core.TestPropertiesRoleMappingListener
# -> Tests run: 9, Failures: 6 (all "expected:<200> but was:<401>")
```

Minimal standalone repro (no Tomcat server needed to see the dropped header):

```java
HttpURLConnection c = (HttpURLConnection) URI.create("http://<any-http>/").toURL().openConnection();
c.setRequestProperty("Authorization", "Basic Zm9vOmJhcg==");
c.setRequestProperty("X-Custom-Foo", "barbar");
// HotSpot: getRequestProperty("X-Custom-Foo") -> "barbar"; server receives both headers.
// CratonVM: getRequestProperty(...) -> null; server receives only Host/User-Agent/Connection.
```

(An embedded-Tomcat harness `EmbedAuth.java` reproducing the full chain with a
header-dumping valve was used during triage; see
`apps/tomcat/.tooling/scratch_authdbg/`.)

## Recommendation

**HANDOFF / FIX in the VM** — `native-builtins/src/http_url_connection.rs`
(with `net_phase_e.rs`). Per repo policy no VM source was modified here.

The fix is the same shape as the sibling output-stream bug
(`BUG-TC0622-httpurlconnection-write-after-connect.md`): make the **real-JDK**
`sun.net.www.protocol.http.HttpURLConnection` carrier a first-class case instead
of poking synthetic `HUC_*` field slots on it.

1. In `huc_set_request_property` / `huc_add_request_property` and the request
   writer (`extract_headers` → `build_request`), **detect the real-JDK carrier**
   (field 0 is a `java/net/URL`, as `huc_real_*`/`huc_get_input_stream` already do)
   and store/read the user headers in an **identity-keyed side-table** (the same
   pattern `huc_real_perform` uses for `RealResult`) rather than a synthetic slot,
   so set and transmit use the *same* storage regardless of carrier.
2. Register a matching native **`getRequestProperty`** (and `getRequestProperties`)
   so readback is consistent (and so any Java code that re-reads what it set works).
3. Reconcile the **two divergent `HUC_*` field layouts** between
   `http_url_connection.rs` and `net_phase_e.rs` (or have a single module own the
   whole `HttpURLConnection` surface) so the header set/transmit slot can never
   disagree.

This is high value: it is a generic HTTP-client correctness bug that unblocks the
entire BASIC/DIGEST authenticator suite, the CORS-preflight suite, the REST-CSRF
suite, and any other Tomcat test whose request semantics depend on a
`setRequestProperty` header.
