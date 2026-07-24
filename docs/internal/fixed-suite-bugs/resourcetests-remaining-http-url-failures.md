# `ResourceTests` remaining HTTP URL failures

Status: FIXED 2026-07-07 (branch `fix/resourcetests-url-userinfo-0707`) — the
class now runs **68/68** under real-JDK+JIT. Retired from `../../known-issues`
to `..` per convention (known-issues holds UNFIXED only).

## Fixes (2026-07-07)

All three failures fell to four changes in `native-builtins`:

1. **`useUserInfoToSetBasicAuth`** — a real `java.net.URL`'s authority
   (field 5) carrying user-info (`alice:secret@localhost:<port>`) passed
   `field5_is_full_url`'s digits-after-colon test, so
   `toExternalForm`/`openStream` treated the authority as the whole URL
   (scheme `alice`). Fixed the discriminator ('@' before any '/' ⇒
   authority), made `toExternalForm` reconstruct `protocol://authority` (so
   user-info survives), taught both native URL parsers
   (`net_phase_e::http_parse_url`, `http_url_connection::parse_url`) to strip
   user-info from the connect target / Host header, and added preemptive
   `Authorization: Basic base64(userinfo)` like the real JDK's
   `HttpURLConnection` (from `url.getUserInfo()`).
2. **`canCustomizeHttpUrlConnectionForRead`** — the
   `UrlResource.getInputStream` native short-circuited to `URL.openStream`,
   whose blocking exchange ran WITHOUT a GC-safe blocking region: with an
   in-process MockWebServer, a concurrent STW froze the server's worker
   threads while the client sat in `recv()`, and the 30s `SO_RCVTIMEO` fired
   as `Resource temporarily unavailable (os error 11)`. It also bypassed
   `customizeConnection`, dropping the test's `Framework-Name` header. Now
   http(s) URLs follow Spring's real path (`openConnection` →
   `customizeConnection` → `con.getInputStream()`, which performs inside
   `http_url_connection.rs`'s blocking regions), and `openStream`'s own
   http(s) arm is GC-parked for direct callers.
3. **`remoteResourceExists`** — two stacked bugs in
   `getContentLength(Long)`: it reported the buffered BODY size (0 for a
   bodiless HEAD) instead of the `Content-Length` response header, and the
   header lookup took the FIRST duplicate while the real JDK's
   `sun.net.www.MessageHeader.findValue` iterates backwards (LAST wins).
   MockWebServer emits its bodiless `Content-Length: 0` default *before* the
   test's `addHeader("Content-Length", "6")` — verified byte-identical on
   HotSpot via curl — so first-match read 0 and Spring's
   `isReadable()`/`contentLength()` called the resource empty.

Note: `remoteResourceExists` hits the LOCAL MockWebServer over plain HTTP in
current Spring — it was never blocked by the (separate) native-HTTPS
handshake-EAGAIN P0.

---

Original report below (status line preserved for history).

Status: OPEN as of 2026-07-05.

## Context

The residual classpath/Mockito/URL/FileSystemResource issues from
`resourcetests-residual-classpath-mockito-url-httpconn-bugs.md` are fixed and
that document has moved to `..`. The full Spring
`org.springframework.core.io.ResourceTests` class now runs at **65/68** under
CratonVM with `--jdk real --jit on`.

Verification command:

```bash
cd /data/cratonvm/apps/spring-suite-runner
CRATONVM_BIN=/data/cratonvm-resourcetests-residuals-20260705-13506 ./run-suite.sh run --jdk real --jit on --batch 1 --batch-to 600 --one-to 240 --tag res-residuals-13506-urlfix --only 'org\.springframework\.core\.io\.ResourceTests$'
```

Run output:

```text
test-methods: found=68 passed=65 failed=3
```

## Remaining failures

```text
FAILCAUSE org.springframework.core.io.ResourceTests :: canCustomizeHttpUrlConnectionForRead() :: java.io.IOException: URL.openStream failed: Resource temporarily unavailable (os error 11)
FAILCAUSE org.springframework.core.io.ResourceTests :: useUserInfoToSetBasicAuth() :: java.io.IOException: URL.openStream: unsupported scheme: alice:secret@localhost:<port>
FAILCAUSE org.springframework.core.io.ResourceTests :: remoteResourceExists() :: org.opentest4j.AssertionFailedError: Expecting value to be true but was false
```

Notes:

- `canCustomizeHttpUrlConnectionForRead()` is separate from the already-fixed
  `canCustomizeHttpUrlConnectionForExists*` cases. The read path still reaches
  `URL.openStream()` and gets an EAGAIN-style native I/O failure.
- `useUserInfoToSetBasicAuth()` appears to parse `alice:secret@localhost:<port>`
  as an unsupported URL scheme instead of preserving it as user-info authority
  for the HTTP URL.
- `remoteResourceExists()` now stands alone as a false `exists()` result for a
  live remote resource.

Suggested next pass: focus on `java.net.URL`/`HttpURLConnection` handling for
HTTP URLs with user-info, `openStream()` nonblocking/EAGAIN behavior, and the
`UrlResource.exists()` remote-resource path.
