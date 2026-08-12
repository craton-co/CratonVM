# `Selector.wakeup()` on a closed selector threw, and `vertx.close()` never returned — FIXED

**Status:** FIXED (2026-08-12). Found while verifying the hibernate-reactive
SCRAM fix: with the SASL blocker gone, roughly half the DB-required classes
still timed out, and they timed out in teardown rather than in a test.

The two defects are unrelated in mechanism but stacked in effect — the SASL one
made every class FAIL, and this one made a passing class HANG.

## Symptom

A hibernate-reactive class runs its tests green, then never exits. Under the
suite harness that is a per-class timeout with a leaked Postgres container. The
only clue in the log is one line with no stack:

```
[LOG] DefaultPromise - An exception was thrown by io.vertx.core.impl.VertxImpl$2.operationComplete()
```

`--stack-dump-on-timeout` puts the main thread here:

```
[45] io/vertx/junit5/RunTestOnContext.afterAll
[46] io/vertx/junit5/RunTestOnContext.cleanUp
[47] java/util/concurrent/CompletableFuture.get
[48] java/util/concurrent/CompletableFuture.waitingGet
[51] java/util/concurrent/CompletableFuture$Signaller.block
```

blocked forever on `vertx.close()`, with every Vert.x event-loop thread already
gone.

## Root cause

`Selector.wakeup()` on a CLOSED selector raised
`IOException("ClosedSelectorException")`. On a JDK it returns normally.

Recovering the swallowed throwable (by routing Netty's `InternalLoggerFactory`
back to JUL, since Vert.x's delegate drops it) gives the whole chain:

```
java.io.IOException: ClosedSelectorException
	at io.netty.channel.nio.SelectedSelectionKeySetSelector.wakeup
	at io.netty.channel.nio.NioIoHandler.wakeup
	at io.netty.channel.SingleThreadIoEventLoop.wakeup
	at io.netty.util.concurrent.SingleThreadEventExecutor.shutdown0
	at io.netty.util.concurrent.SingleThreadEventExecutor.shutdownGracefully
	at io.netty.util.concurrent.MultithreadEventExecutorGroup.shutdownGracefully
	at io.vertx.core.impl.VertxImpl$2.operationComplete            <-- a LISTENER
	at io.netty.util.concurrent.DefaultPromise.notifyListener0
```

`shutdown0()` wakes an event loop whose selector the loop thread may already have
closed — a deliberate, benign race that the JDK's `wakeup()` contract exists to
absorb. Here it threw, and it threw *inside a `DefaultPromise` listener*. Netty
logs a throwing listener and drops it, so `VertxImpl$2.operationComplete` never
reached the rest of its shutdown, the close promise was never completed, and the
`CompletableFuture.get()` waiting on it blocked forever.

That is what made it hard to see: the exception is not propagated to anyone, the
process does not fail, and the log line naming the listener carries no stack.

## Measured contract (jdk-25 vs CratonVM, before)

| call on a CLOSED `Selector` | HotSpot 25 | CratonVM |
|---|---|---|
| `wakeup()` | returns normally | `IOException: ClosedSelectorException` |
| `wakeup()` again | returns normally | `IOException: ClosedSelectorException` |
| `selectNow()` | `ClosedSelectorException` | `IOException: ClosedSelectorException` |
| `keys()` | `ClosedSelectorException` | **returned an empty set** |

All three rows were wrong, in three different ways, and only the first one hung.

## Fixes

1. **`wakeup()` on a closed or unknown selector is a no-op** — in both
   `nio_selector::selector_wakeup_native` (the `SelectorImpl.wakeup0()` entry
   point) and `nio_selector::selector_wakeup` (the id-keyed core).

2. **`selectNow()` / `select()` raise a real
   `java.nio.channels.ClosedSelectorException`.** The old
   `IOException` whose *message* was the string `"ClosedSelectorException"` is a
   different thing in every way that matters: `ClosedSelectorException` extends
   `IllegalStateException` and is UNCHECKED, so a select loop wrapped in
   `catch (IOException)` is meant NOT to catch it — and here it did, and carried
   on.

3. **`keys()` raises it too**, instead of answering an empty set. An empty set is
   a legitimate state ("no channels registered"), so a caller draining keys after
   an unnoticed close got a clean, wrong answer.

The ctx-less internal helpers keep the old `IOException` as a race guard; by the
time they run, the entry point has already checked.

## Verification

* The oracle probe above now diffs **identical** to HotSpot on all seven rows.
* `Vertx.vertx()` → connect → query → `conn.close()` → `vertx.close()`,
  25 consecutive rounds: **25/25 PASS**. Before: failed at round 5, then at
  round 1 on the next run — intermittent, which is why it read as flakiness.
* `org.hibernate.reactive.BatchFetchTest` under the suite harness: **4/4 PASS at
  ~10 s wall**. Before: 240 s timeout, `rc=124`, container leaked.

## Related

- `vertx-pg-sasl-scram-handshake-fails-20260812-FIXED.md` — the defect this was
  found behind. Its "every failing class-fork leaked its Postgres container"
  note has two causes, and this is the second one.
