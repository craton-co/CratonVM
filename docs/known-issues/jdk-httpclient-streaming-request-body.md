# `java.net.http.HttpClient` — reactive (streaming) request body not sent

| | |
|---|---|
| **Status** | OPEN (functional residual after BUG-04 fix) |
| **Area** | `native-builtins/src/net_phase_e.rs` — RE.5 `java.net.http.HttpClient` bare model |
| **Symptom** | A request whose body is supplied via `HttpRequest.BodyPublishers.fromPublisher(...)` (a reactive `Flow.Publisher`) is sent with an **empty** body. Literal bodies (`BodyPublishers.ofString`) are sent correctly. |
| **Severity** | medium (POST/PUT/PATCH echo-style tests fail on body content; status/header/GET paths are fine) |
| **Discovered** | 2026-06-23, while fixing BUG-04 (`HttpClient.executor()` `AbstractMethodError`) |

## Background

CratonVM models the public `java.net.http.HttpClient` as a synthetic *bare client*
(`register_re5_http_client` in `net_phase_e.rs`): `newHttpClient()`/`build()` hand
back a synthetic `java/net/http/HttpClient` instance, and every instance method
(`send`, `sendAsync`, `executor`, …) is a registered native. A request is
performed synchronously by `re5_do_request` → `http_perform_request`.

The request body is read from the synthetic `HttpRequest`'s body slot, which is
only populated when the body was a **literal** publisher
(`BodyPublishers.ofString` stores the bytes directly). Spring's
`JdkClientHttpRequest` (used by `RestClient` / `JdkClientHttpRequestFactory`)
instead streams the body reactively:

```java
Flow.Publisher<ByteBuffer> publisher = new OutputStreamPublisher<>(
        os -> body.writeTo(StreamUtils.nonClosing(os)), BYTE_MAPPER, this.executor, null);
return HttpRequest.BodyPublishers.fromPublisher(publisher, contentLength);
```

`fromPublisher(...)` carries a `Flow.Publisher`, not literal bytes. Driving that
publisher would mean implementing a `Flow.Subscriber` in the native layer,
re-entering Java to `request(n)`/`onNext(ByteBuffer)` on the configured
`Executor` thread, and assembling the byte stream — a non-trivial reactive
pipeline. The native therefore registers `fromPublisher` (so the call does not
throw `AbstractMethodError`) but stores a null body, and the request goes out
empty.

## Impact

`org.springframework.http.client.JdkClientHttpRequestFactoryTests`: the
body-carrying cases (`echo`, and any POST/PUT that asserts the echoed body)
fail because the server receives no body. The status / header / query / GET
cases pass. `headersAfterExecute` exercises post-execute read-only-header
semantics and is also affected.

## Fix sketch (deferred)

Implement a minimal `Flow.Subscriber` bridge in the bare-client `send`/`sendAsync`
path: when the `HttpRequest` body slot holds a `Flow.Publisher` (rather than a
literal String), subscribe to it, pull `ByteBuffer`s, and accumulate the body
bytes before building the wire request. This must run the publisher's producer
lambda on the request `Executor` (the one `executor()` would have returned), so
it also depends on a real worker-thread submit path.

Until then the literal-body path (`ofString`) and all bodyless verbs work; only
reactive-streaming request bodies are dropped.
