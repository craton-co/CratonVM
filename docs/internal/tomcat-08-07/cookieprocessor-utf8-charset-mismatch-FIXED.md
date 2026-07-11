# TestCookieProcessorGenerationHttp - UTF-8 cookie header byte preservation

**Status:** FIXED on 2026-07-11.

## Root cause

`native-builtins/src/http_url_connection.rs` decoded every HTTP response-header
value as UTF-8. `HttpURLConnection` instead exposes header bytes as
ISO-8859-1 code points. Tomcat's `testUtf8CookieValue` deliberately recovers
the raw `Set-Cookie` bytes with ISO-8859-1 before decoding them as UTF-8. The
UTF-8 decoder interpreted the UTF-8 bytes for U+0120 as one character, and the
later ISO-8859-1 encoding substituted `?`.

## Fix

`read_response` now maps each response-header byte directly to its matching
Latin-1 code point. This keeps `HttpURLConnection.getHeaderFields()` byte
preserving, including non-UTF-8 `obs-text`. A focused parser regression covers
the `Set-Cookie: Test=\\xC4\\xA0` sequence.

## Validation

Fresh Linux probes built from the fix worktree with a unique target directory:

```bash
CP=$(cat /data/data/apps/tomcat/.suite/cp-linux-fixed.txt)
/data/data/cratonvm-tomcat-cookieprocessor-utf8-20260711-001.bin \
  --java-home /home/victor/jdk25 -Xmx2g -cp "$CP" \
  org.junit.runner.JUnitCore \
  org.apache.tomcat.util.http.TestCookieProcessorGenerationHttp
```

The clean pre-fix build reproduced the documented `Test=[?]` result. The
post-fix build passes both methods in
`TestCookieProcessorGenerationHttp`.
