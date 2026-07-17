# TestLargeClientHello session-resumption handshake fixed

**Status:** FIXED on 2026-07-15. **Severity:** medium. **HotSpot:** PASS.

## Cause

The native `HttpsURLConnection` path created a fresh rustls `ClientConfig`
for each request. That discarded the TLS 1.3 ticket cache between the two
connections. It also did not preserve a caller-supplied Java `TrustManager`,
and the TLS engine could split a record from the oversized server certificate
across `SSLEngine.wrap()` calls while reporting `NEED_UNWRAP` before its
queued record had been sent.

The JUL bridge had two related capture defects: it ignored an explicit
`Logger.setLevel(FINE)` for real-JDK logger layouts, and published FINE records
without consulting the handler side table used by `Logger.addHandler`.

## Fix

- Capture and reuse the configured HttpsURLConnection `ClientConfig`, including
  its session cache, custom trust-manager path, and Java post-handshake trust
  check.
- Consume immediately available post-response TLS records so TLS 1.3
  `NewSessionTicket` messages are retained before the next URL connection.
- Make `SSLEngine.wrap()` emit only complete TLS records and keep returning
  `NEED_WRAP` while records remain queued.
- Preserve explicit JUL levels and deliver FINE records to registered handlers.
- Use the standalone SATB barrier for `Reference.get()` keep-alives, avoiding
  an unrelated debug write-barrier assertion during the Tomcat setup path.

## Validation

Using the uniquely named optimized binary
`cratonvm-largeclienthello-session-20260714.exe` and Tomcat's test resource
directory as the working directory:

```text
org.apache.tomcat.util.net.TestLargeClientHello
  testLargeClientHelloWithSessionResumption: PASS
```

The exact test completed both HTTPS requests and observed Tomcat's expected
`handshakeUnwrapBufferUnderflow` FINE log capture. Focused Rust regression:

```text
cargo test -p cratonvm-native-builtins t27_session_resumption --lib -- --nocapture
1 passed
```
