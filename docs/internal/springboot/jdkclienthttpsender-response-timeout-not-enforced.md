# `JdkClientHttpSenderTests.sendShouldTimeoutOnSlowResponse` fixed

**Status: FIXED — 2026-07-18**

## Symptom

`module/spring-boot-micrometer-metrics`, class
`org.springframework.boot.micrometer.metrics.autoconfigure.export.otlp.JdkClientHttpSenderTests`,
previously accepted a response whose headers were delayed for 500 ms despite
the request having `HttpRequest.Builder.timeout(Duration.ofMillis(10))`.

## Root cause

The real-socket `java.net.http.HttpRequest$Builder.timeout(Duration)` native
was a fluent no-op. Its synthetic request stored only method, URI, body, and
headers, so `re5_do_request` always called the raw HTTP exchange with its
fixed 30-second socket timeout. The caller's per-request timeout was therefore
lost before I/O began.

## Resolution

- Preserve the `Duration` on the synthetic builder/request and implement the
  matching `HttpRequest.timeout()` accessor.
- Translate the stored duration to an end-to-end deadline in `re5_do_request`.
- Enforce the deadline through connect, TLS setup, writes, redirects, and each
  response read. Re-arming the remaining socket timeout before each read also
  prevents a slow byte-drip response from extending the deadline indefinitely.
- Normalize socket timeout results to `HttpClient request timed out`, preserving
  the `IOException` text expected by Spring Boot.
- Add a native regression with a local TCP peer that delays response headers
  beyond the request deadline.

While producing the VM executable, current `dev` also failed to compile
because the bound `SocketChannel` path did not handle
`StartConnect::DeferredFailure`. The build repair preserves that deferred error
for non-blocking `finishConnect`/first-write handling, matching the existing
unbound path.

## Validation

- `cargo test -p cratonvm-native-builtins
  re5_http_request_timeout_bounds_delayed_response_headers -- --nocapture`
  passed.
- `JdkClientHttpSenderTests` passed under the worktree-specific CratonVM with
  JIT enabled: 7 tests, 0 failures.
- The same class passed with `--nojit`: 7 tests, 0 failures.

The focused suite results are under
`C:\craton\springboot-suite-jdkhttp-resptimeout-20260718-019f742b`.
