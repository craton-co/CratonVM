# `java.net.http.HttpClient` reactive request bodies

| | |
|---|---|
| **Status** | FIXED |
| **Area** | `../../../native-builtins/src/net_phase_e.rs` - RE.5 `java.net.http.HttpClient` bare model |
| **Resolved** | 2026-07-01 |

## Background

CratonVM models the public `java.net.http.HttpClient` surface with synthetic
instances and native method bodies. Before this fix, `HttpRequest.BodyPublishers`
handled literal bodies (`ofString`, `ofByteArray`) but
`fromPublisher(Flow.Publisher[, long])` stored `null` in the synthetic
`BodyPublisher` body slot. Spring's `JdkClientHttpRequest` uses
`fromPublisher(...)` for streaming POST/PUT/PATCH request bodies, so those
requests were sent with an empty body.

## Fix

`fromPublisher(...)` now keeps the original `Flow.Publisher` object in the
synthetic `BodyPublisher`. When `HttpClient.send` or `sendAsync` prepares the
wire request, the RE.5 path detects a non-literal body and subscribes a native
collector stamped as the bare `Flow.Subscriber` interface, matching the VM's
existing synthetic-interface receiver pattern:

- `onSubscribe` requests `Long.MAX_VALUE` demand from the publisher's
  `Flow.Subscription`.
- `onNext(Object)` accepts emitted `ByteBuffer`, `byte[]`, and `String` payloads.
  `ByteBuffer` items are consumed from `position` to `limit`, with heap-buffer
  field reads first and a virtual `remaining()`/`get(byte[])` fallback for other
  layouts.
- `onComplete` releases the synchronous send path to use the collected bytes.
- `onError` turns the publisher failure into an `IOException`.

The collector state is stored in a native side table keyed by a stable collector
id written into the subscriber object, so a moving GC can relocate the
subscriber during re-entrant Java calls without losing the side-table lookup.
The subscriber is also held as a global root for the duration of collection.

## Coverage

Focused native tests cover:

- `BodyPublishers.ofByteArray(byte[])` still reads static argument slot 0.
- `BodyPublishers.fromPublisher(...)` preserves the supplied publisher object.
- The request body bridge drives a scripted synchronous publisher that emits a
  `HeapByteBuffer` and returns the collected bytes.

The original known issue is closed; future work should track any narrower
publisher-specific incompatibility as a new `../../known-issues` entry.
