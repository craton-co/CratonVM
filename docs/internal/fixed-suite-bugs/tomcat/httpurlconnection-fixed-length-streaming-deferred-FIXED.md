# `HttpURLConnection` fixed-length streaming deferred until response retrieval — FIXED

**Resolved:** 2026-07-11. The real legacy HTTP carrier now records
`setFixedLengthStreamingMode`, opens a plain HTTP connection and writes the
request head from `getOutputStream()`, and sends every subsequent output-stream
write on that same TCP connection. Response getters consume that live stream;
an aborted committed request is surfaced as `IOException`, rather than `-1`.

## Root cause

The old bridge treated fixed-length streaming setters as no-ops, returned a
buffered synthetic `ByteArrayOutputStream`, and deferred opening/sending the
request until `getResponseCode()` or `getInputStream()`. That prevented Tomcat
from reacting between the caller's paced writes.

## Fix

- `RealReq` records fixed-length/chunked mode.
- A narrowly scoped native-API BAOS hook lets only live fixed-length HTTP
  streams bypass ordinary BAOS buffering; all other BAOS users retain their
  existing path.
- The live state owns the TCP stream, exact byte count, close state, and
  response handoff. It rejects over/under-writes and reuses the connection for
  response parsing.

## Validation

- Focused native suite:
  `cargo test -p cratonvm-native-builtins http_url_connection_tests --lib`
  (28 passed).
- Loopback `ServerSocketChannel` probe confirmed the request head is visible
  before response retrieval and the 5-byte body is visible immediately from
  `OutputStream.write()`.
- Tomcat single-method runner:
  `TestNonBlockingAPI.testNonBlockingReadIgnoreIsReady` — PASS.
- Tomcat single-method runner:
  `TestNonBlockingAPI.testNonBlockingRead` — PASS.

The implementation intentionally keeps the pre-existing buffered fallback for
HTTPS and non-fixed/chunked modes; neither was part of this HTTP regression.
