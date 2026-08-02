# WebSocket client deadlock: a `CompletionHandler` ran on the application thread

**Status:** FIXED 2026-08-01 (`fix/ws-serverdeadlock-hang-20260801`).
**Severity:** high — a permanent, unrecoverable hang, not a slowdown.
**HotSpot:** PASS (4/4).

## Symptom

`org.apache.tomcat.websocket.server.TestWsRemoteEndpointImplServerDeadlock`
hung on ~45% of runs — 9 of 20 measured. Not the 19 s that the test's own
polling limit produces on failure: the process never finished at all, and a
900 s suite timeout killed it. That is why it shows up in a suite as a HANG
rather than a FAIL.

Distinct from `wsremoteendpoint-close-delay-near-deadlock-FIXED.md`, which was
a deterministic ~19 s delay in all four parameter combinations caused by
`AtomicReference.toString()`. That fix is intact (its regression test still
passes); this is a different defect.

## Root cause

`main` was executing the **client's** `@OnMessage` handler, which blocks on a
latch that only `main` can release.

`--stack-dump-on-timeout=90` on a hung run — 65 frames on `main`:

```
depth 42  testTemporaryDeadlockOnClientClose        pc=155
depth 43  WsWebSocketContainer.connectToServer
depth 45  WsFrameClient.startInputProcessing
depth 46  WsFrameClient.processSocketRead
depth 47  AsyncChannelWrapperNonSecure.read(buf, att, handler)
depth 48  WsFrameClient$WsFrameClientCompletionHandler.completed     <-- inline
depth 51  resumeProcessing -> processSocketRead -> read -> completed <-- again
depth 59  processInputBuffer -> processData -> processDataText -> sendMessageText
depth 63  PojoMessageHandlerWholeBase.onMessage
depth 64  Bug66508Client.onMessage                  -> clientReceiveLatch.await()
```

Tomcat's WebSocket client arms its first read from `startInputProcessing`,
which runs on the **application** thread inside `connectToServer`.
`native-io/src/async_socket.rs::try_deliver_ready_read` — an inline-completion
fast path added for known-issue tomcat/32.3 latency — saw the loopback socket
already had bytes and invoked `completed()` **on that thread**. The handler
re-armed, completed inline again, and the assembled text message reached
`Bug66508Client.onMessage`, which blocks until `clientReceiveLatch` is counted
down. That countdown happens later in `testTemporaryDeadlockOnClientClose`, on
that same `main` thread, now buried 20 frames deeper inside `connectToServer`
and never returning. Permanent deadlock — `main` is the only thread that can
release the latch it is itself blocked on.

Intermittent because it requires the server's frames to already be buffered
when `startInputProcessing` arms its first read: a race with the peer, hence
roughly a coin flip on loopback.

### The misread contract

`try_deliver_ready_read`'s doc justified itself with:

> "Delivering a completion on the initiating thread is explicitly permitted by
> `AsynchronousChannelGroup`"

The actual javadoc:

> "Where an I/O operation completes immediately, **and the initiating thread is
> one of the pooled threads in the group**, then the completion handler may be
> invoked directly by the initiating thread."

The "pooled thread" qualifier IS the safety property — it exists so that an
application thread is never hijacked into running a handler that may block.
Tomcat's own `Nio2Endpoint` inline-completion guard, also cited in that doc,
applies on the server side, where the initiating thread *is* a container
thread.

## Fix

Establish the missing precondition: **a Java `CompletionHandler` may only be
invoked by a thread designated to deliver completions.**

- New thread-local `DELIVERING_COMPLETIONS`, RAII `CompletionThreadGuard`, and
  `on_completion_thread()`.
- Set by `drain_completions_pub` (the VM-attached AIO dispatcher — our
  `AsynchronousChannelGroup` pool equivalent) and by `iocp_drain`
  (`Iocp`/`EPollPort`/`KQueuePort` `drain`/`poll`, which the JDK calls from its
  own dispatcher loop).
- `try_deliver_ready_read` requires it, checked before the `FIONREAD` probe. A
  read armed by an application thread is queued to the worker pool instead.
- `drain_completions`' opportunistic dispatch (reachable from `aio_asc_write`,
  `aio_asc_is_open`, `aio_asc_connect`, `aio_assc_accept`,
  `aio_acg_await_termination`) is gated the same way — defence in depth for the
  handler-form paths. Its three state flushes still run on any thread; only the
  handler dispatch is gated. When gated it wakes the dispatcher instead, so
  nothing is delayed: every `push_*_completion` already signals that condvar.
- Guarded by `dispatcher_available()` so pure-Rust in-crate users (which
  install no dispatcher launcher) keep the old behaviour rather than stranding
  completions.

## Verification

Interleaved A/B, idle box (load < 1), the two binaries run back-to-back each
round with the order alternated, `--stack-dump-on-timeout=90`, hard timeout
150 s. The binaries differ only by this change. 20 rounds each:

| | PASS | HANG | CLOSE_DELAY |
|---|---|---|---|
| before | 8 | **9** | 3 |
| after | **13** | **0** | 7 |

Hang 9/20 -> 0/20, Fisher exact two-tailed p ~= 0.0008. Across every run of the
fixed code in this investigation — 20 A/B + 8 standalone + 6 on the final
binary — **0 hangs in 34 runs**.

The `CLOSE_DELAY` column is a separate pre-existing defect, not a regression;
see `docs/internal/fixed-suite-bugs/tomcat/wsremoteendpoint-server-close-never-completes-FIXED.md`.
Its raw 3-vs-7 is censored — the `before` arm's 9 hung runs never got the
chance to exhibit a close delay. Conditioned on runs that completed, 3/11 (27%)
vs 7/20 (35%), z ~= 0.45, p ~= 0.65: indistinguishable.

### The tomcat/32.3 fast path is fully preserved

`CRATONVM_DBG_AIO_INLINE=1` on `TestAsyncMessagesPerformance`, 1500 reads:

| | inline | inline_rate | not_ready | depth_capped | foreign_thread | sub-10ms wait mean |
|---|---|---|---|---|---|---|
| before | 752 | 0.501 | 731 | 17 | n/a | 363 us |
| after  | 754 | 0.503 | 729 | 16 | **1** | 341 us |

The gate declines exactly ONE read for the whole workload — the first read of
the connection, armed by the application thread. Every other inlined read is
armed from inside `completed()` on the dispatcher and still qualifies, which is
the case the fast path was built for. A new `AIO_INLINE_FOREIGN_THREAD` counter
reports the declines.

WebSocket cluster, before vs after, identical on both:
`TestWsPingPongMessages` PASS, `TestEncodingDecoding` PASS,
`TestWsSessionSuspendResume` PASS, `TestAsyncMessagesPerformance` FAIL (1),
`TestWsRemoteEndpointImplClient` FAIL (1) — the two failures are pre-existing
and unchanged.

`cargo test -p cratonvm-native-io`: 371 passed, 0 failed.

## Note for whoever touches this next

CratonVM has ONE AIO dispatcher thread, where a real `AsynchronousChannelGroup`
has a pool. After this fix a blocking `CompletionHandler` occupies that single
thread — which is exactly what this test's `@OnMessage` does for about a second.
It did not cost anything measurable here (the close-delay rate is unchanged and
the latency numbers above are flat), because Future-form writes are performed by
worker threads and only their *completion* needs the dispatcher. But a workload
with several concurrently-blocking handlers would serialize on it, and growing
the dispatcher into a small pool is the natural next step if that ever shows up.
