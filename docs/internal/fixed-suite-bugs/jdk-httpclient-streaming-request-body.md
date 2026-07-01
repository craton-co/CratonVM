# `java.net.http.HttpClient` - reactive request bodies now sent

| | |
|---|---|
| **Status** | FIXED 2026-07-01 |
| **Area** | `native-builtins/src/net_phase_e.rs` - RE.5 `java.net.http.HttpClient` bare model |
| **Symptom** | Requests whose bodies are supplied via `HttpRequest.BodyPublishers.fromPublisher(...)` were sent with an empty body. Literal bodies (`ofString`, `ofByteArray`) were sent correctly. |
| **Severity** | medium: Spring POST/PUT echo-style tests using `JdkClientHttpRequestFactory` lost the request body. |
| **Discovered** | 2026-06-23, while fixing BUG-04 (`HttpClient.executor()` `AbstractMethodError`) |
| **Fixed** | 2026-07-01 |

## Root cause

CratonVM models `java.net.http.HttpClient` as a synthetic bare client in
`register_re5_http_client`: `newHttpClient()` / `build()` return a synthetic
`HttpClient`, and `send` / `sendAsync` perform the request through native
helpers.

Literal publishers already stored body bytes directly on the synthetic
`HttpRequest`. Spring's `JdkClientHttpRequestFactory` instead uses:

```java
HttpRequest.BodyPublishers.fromPublisher(publisher, contentLength)
```

where `publisher` is a `Flow.Publisher<ByteBuffer>` produced by
`OutputStreamPublisher`. The old native registered `fromPublisher` only to avoid
`AbstractMethodError`; it discarded the publisher, so the eventual request body
slot was empty and the server received no bytes.

## Fix

`BodyPublishers.fromPublisher(...)` now stores the original `Flow.Publisher` in
the synthetic body publisher. `HttpRequest.Builder.POST`, `PUT`, and
`method(String, BodyPublisher)` keep that body publisher object until `send`.

At send time, `re5_request_body_bytes` distinguishes:

- direct string bodies;
- literal synthetic body publishers (`ofString`, `ofByteArray`, `noBody`);
- reactive synthetic body publishers (`fromPublisher`).

Reactive publishers are collected by a scoped generated `Flow.Subscriber` proxy.
The proxy handler is a private synthetic class
`cratonvm/net/http/BodyPublisherCollectorHandler` with a native
`InvocationHandler.invoke(...)` implementation. It requests `Long.MAX_VALUE`,
copies emitted heap `ByteBuffer` ranges into a byte vector, advances each buffer
position to its limit, and waits up to five seconds for `onComplete`.

This keeps the bridge deliberately narrow: it covers the Spring
`OutputStreamPublisher` / heap `ByteBuffer` path that exposed the bug without
turning the bare-client model into a complete JDK HttpClient implementation.

## Verification

Targeted native regression tests:

```text
cargo test -p cratonvm-native-builtins re5_ -- --nocapture
```

Result on 2026-07-01:

```text
running 4 tests
test net_phase_e::tests::re5_heap_byte_buffer_reader_copies_remaining_bytes_and_advances ... ok
test net_phase_e::tests::re5_request_builder_keeps_reactive_body_publisher_until_send ... ok
test net_phase_e::tests::re5_body_publishers_from_publisher_preserves_flow_publisher ... ok
test net_phase_e::tests::re5_body_publishers_of_byte_array_reads_static_arg_slot_zero ... ok

test result: ok. 4 passed; 0 failed
```

The adjacent 2026-07-01 literal-body fix is included: static
`BodyPublishers.ofByteArray(byte[])` now reads the byte array from argument slot
0, matching static native call layout.
