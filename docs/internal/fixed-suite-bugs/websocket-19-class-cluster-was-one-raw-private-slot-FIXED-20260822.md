# The 19-class WebSocket cluster was one private slot read without its base offset

| | |
|---|---|
| **Status** | ✅ FIXED — 2026-08-22, `native-io/src/async_socket.rs` (one line) |
| **Severity** | high — every `AsynchronousSocketChannel` `CompletionHandler`-form read on a real JDK channel object |
| **HotSpot** | PASS |
| **CratonVM** | 19 classes FAIL/HANG → **19 of 19 PASS** |
| **Root cause** | `aio_asc_read` read `F_REG_ID` as a RAW slot index instead of through `aio_get`, so on a concrete receiver it addressed an unrelated JDK field |

## The defect

`async_socket.rs` keeps its private state in slots appended after whatever
fields the receiver already has, and every access is supposed to go through
`aio_get`/`aio_set`, which add `aio_base(ctx, o)`. Exactly one call site did not:

```rust
let slot2 = match ctx.get_field(this, 2) {        // <- raw index
```

For a synthetic 3-field carrier `base == 0`, so the raw index and
`aio_get(ctx, this, F_REG_ID)` are the **same slot** — which is why this was
invisible to every unit test and to every code path that allocates its own
channel. They diverge only on a **concrete** receiver: a real JDK
`AsynchronousSocketChannel` implementation object carries the JDK's own fields
first, so raw index 2 lands on an unrelated real field, `slot2` is garbage, and
the read fails.

## How it presented, and why that was misleading

Tomcat's WebSocket **client** (`WsWebSocketContainer`) does its HTTP upgrade with
the **Future form** of read/write and then switches to the
**CompletionHandler form** for frame reading. So the connection succeeds, the
session is created, `onOpen` fires — and the very first frame read fails:

```text
java.io.IOException: read: bad fd for tcp clone
    at WsFrameClient$WsFrameClientCompletionHandler.failed(WsFrameClient.java:188)
    -> WsFrameClient.close -> WsSession.doClose -> unregisterSession
```

`WsFrameClient` treats a failed read as a dropped connection and closes the
session. Every session was therefore **unregistered as fast as it was
registered**, which is why `getOpenSessions()` never returned more than the
caller and the whole family of session-count assertions failed.

`CRATONVM_DBG_AIO=1` is what named it, in three lines:

```text
READ  dispatch fd=9 requested_len=8192 ...        <- Future form: fd resolved, Ok(147)
HREAD dispatch (handler-form) this_fields=52      <- handler form: NO fd, 52-field object
HREAD deliver outcome=Error(read: bad fd for tcp clone)
```

`this_fields=52` is the tell: a real JDK channel implementation object, not a
3-field carrier.

## Two hypotheses this went through first, both wrong

Recorded because each looked conclusive and each cost a measurement to kill.

1. **"The G30-1 reference-slot-coercion guard."** It fires in all 19 logs,
   19–26 times each. Killed by a control: `TestPojoEndpointBase` **passes** with
   22 of them. A signal present in every failure means nothing until it has been
   counted in the passing population.
2. **"`HashMap`/`computeIfAbsent` is losing the entry."** Instrumenting
   `registerSession` showed the same container, the same map, the same key with
   a stable hash — and a **different `HashSet` returned each call** with
   `mapEntries` stuck at 1. That reads as a broken map. It was not: instrumenting
   `unregisterSession` with a stack trace showed the entry was not *lost*, it was
   *removed* — `register → unregister → register → unregister`. `computeIfAbsent`
   measures identical to HotSpot (`probe/CiaProbe.java`), as does `HashSet` with
   identity-hashed elements across a GC (`probe/GcMapProbe.java`).

The lesson worth keeping: **the first instrumented result pointed at the
collections library, and following it would have been days in the wrong file.**
What redirected it was instrumenting the *other* end of the same data structure.

## Fix

One line, plus the comment explaining why a raw index is wrong here:

```rust
let slot2 = match aio_get(ctx, this, F_REG_ID) {
```

## Regression test

`every_private_slot_access_goes_through_the_base_aware_accessor` — a **source**
guard, deliberately. A behavioural test cannot see this defect: on the receiver
shape the unit tests build, `base == 0` and the buggy and correct forms address
the same slot, so such a test passes either way and proves nothing. The guard
scans this file for a private slot indexed without `aio_base` and names the
offending line.

Negative control (fix reverted, test kept):

```text
Offenders: [(3103, "let slot2 = match ctx.get_field(this, 2) {")]
```

The guard splits its own search literals (`concat!("ctx.get_", "field(this, ")`)
so it does not match its own source — the first version reported only itself.

## Verification

`bin/cratonvm-ws-30b8d5b2e`, Azure Linux, real JDK 25, `apps/tomcat` fixture,
serial, 900 s cap. All 19 classes of the cluster:

| class | before | after |
|---|---|---|
| `TestWebSocketFrameClient` | FAIL | **OK (4 tests)** 175 s |
| `TestWsPingPongMessages` | FAIL | **OK (1 test)** 4 s |
| `TestWsRemoteEndpoint` | FAIL 19 s | **OK (8 tests)** 5 s |
| `TestWsSessionSuspendResume` | FAIL | **OK (2 tests)** 6 s |
| `TestWsSubprotocols` | flaky | **OK (1 test)** 3 s |
| `TestWsWebSocketContainer` | FAIL 382 s | **OK (24 tests)** 20 s |
| `TestWsWebSocketContainerGetOpenSessions` | FAIL | **OK (12 tests)** 6 s |
| `…SessionExpiryContainerClient` | FAIL | **OK (1 test)** 10 s |
| `…SessionExpiryContainerServer` | FAIL | **OK (1 test)** 9 s |
| `…SessionExpirySession` | FAIL | **OK (1 test)** 13 s |
| `…TimeoutClient` | FAIL | **OK (2 tests)** 15 s |
| `…TimeoutServer` | FAIL | **OK (2 tests)** 24 s |
| `pojo.TestEncodingDecoding` | FAIL 51 s | **OK (6 tests)** 4 s |
| `server.TestClassLoader` | **HANG 902 s** | **OK (1 test)** 4 s |
| `server.TestCloseBug58624` | FAIL 207 s | **OK (1 test)** 3 s |
| `server.TestShutdown` | FAIL | **OK (1 test)** 4 s |
| `server.TestSlowClient` | FAIL 148 s | **OK (1 test)** 10 s |
| `server.TestWsRemoteEndpointImplServerDeadlock` | FAIL | **OK (4 tests)** 36 s |
| `server.TestWsServerContainer` | FAIL | **OK (37 tests)** 4 s |

**19 of 19.** The walls collapse with the failures — `TestClassLoader` 902 s
(stuck) → 4 s, `TestWsWebSocketContainer` 382 s → 20 s, `TestCloseBug58624`
207 s → 3 s — because those classes were waiting out timeouts on connections
that had already died.

`cratonvm-native-io` unit suite: 519 passed / 0 failed. Full 640-class suite
result is in the merge commit.

## Reach beyond WebSocket

The handler form of `AsynchronousSocketChannel.read` is NIO2's main read path.
Anything driving a concrete `AsynchronousSocketChannel` through a
`CompletionHandler` hit this — Tomcat's `Http11Nio2Protocol` /
`SecureNio2Channel` are named in this function's own comments. Only the
WebSocket classes were measured here; a NIO2-connector sweep is worth doing and
has not been.
