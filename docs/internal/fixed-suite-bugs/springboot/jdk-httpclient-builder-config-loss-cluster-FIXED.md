# JDK HttpClient builder configuration loss cluster — FIXED

**Status: FIXED 2026-07-18**

## Actual active path

The original report correctly identified lost public `java.net.http.HttpClient`
builder state, but its registration-order conclusion was not valid for the
normal real-JDK CLI. `register_synthetic_overrides()` (and therefore
`http2.rs`) is gated behind `synthetic-jdk`; production `--java-home` runs
only register Phase E's `net_phase_e.rs::register_re5_http_client` surface.
That active implementation allocated a legacy one-field client/builder and
returned empty/default values, so it discarded every configured reference.

## Fix

`native-builtins/src/net_phase_e.rs` now:

- stores all public builder values in distinct client/builder slots and copies
  them on `build()`;
- returns genuine `Optional` values and configured enum/object instances from
  the public accessors;
- invokes a configured `ProxySelector` before connecting;
- observes `Redirect.NEVER` by returning the first redirect response;
- routes an explicit `SSLContext` through rustls and maps handshake errors to
  `SSLHandshakeException`;
- reads configured `SSLParameters.getCipherSuites()` and constrains rustls's
  ClientHello, including the SSL-bundle cipher mismatch case.

`native-builtins/src/t27_tls.rs` now supports a context-scoped, cipher-limited
rustls configuration while retaining the existing trust-manager/key-manager
paths. `native-io/src/socket_channel.rs` also handles the base branch's
previously non-exhaustive deferred non-blocking-connect failure, which was a
necessary CLI build unblocker.

## Validation

All validation used JDK 25.0.3 and the unique binary
`cratonvm-httpclient-builder-config-closure-20260718-r6.exe`.

| Mode | Class | Result |
|---|---|---|
| JIT | `JdkClientHttpRequestFactoryBuilderTests` | PASS, 32 tests, 174.176 s |
| JIT | `reactive.JdkClientHttpConnectorBuilderTests` | PASS, 28 tests, 172.590 s |
| JIT | `ImperativeHttpClientAutoConfigurationTests` | PASS, 9 tests, 142.162 s |
| --nojit | `JdkClientHttpRequestFactoryBuilderTests` | PASS, 32 tests, 182.610 s |
| --nojit | `reactive.JdkClientHttpConnectorBuilderTests` | PASS, 28 tests, 189.231 s |
| --nojit | `ImperativeHttpClientAutoConfigurationTests` | PASS, 9 tests, 160.194 s |

The focused native regression
`re5_http_client_builder_retains_configured_object_values` also passes.
The first JIT builder run specifically verified all 32 parameterized cases,
including successful SSL-bundle connections and both expected
`SSLHandshakeException`s for the deliberately incompatible cipher suites.

## Scope boundary

The broad JIT run also exercised the two remaining consumers. Their classes
now time out during Spring/JUnit startup before a JDK HttpClient test executes:
`ReactiveHttpClientAutoConfigurationTests` and `TestRestTemplateTests` both
spin on repeated `gen_heap::get_field` out-of-bounds reads for Spring proxy
objects. This is a separate proxy-layout livelock, not a builder-state
failure; the original builder assertions are covered by the passing suites
above. It is deliberately not folded into this fixed issue.
