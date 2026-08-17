# `NioEventLoopTest` — OP_CONNECT registration never fires, `registeredChannels()` undercounts

**Status: OPEN.** Measured 2026-08-16, commit `3ef3eb744`, Windows host,
`cratonvm.exe` release build, G1. Flagged by a same-day full 657-class
3-collector suite run (FAIL on generational, G1, and ZGC); this page isolates
it (`--shards 1`, one class per process, no other collector running
concurrently) and cross-checks against HotSpot 25 on the same host.

## Summary

| | found | ok | failed | wall |
|---|---|---|---|---|
| CratonVM G1 (isolated) | 13 | **11** | **2** | 22.8s |
| HotSpot 25 (isolated) | 13 | 13 | 0 | 4.5s |

Isolated and reproducible — not host contention from the full-suite run.
HotSpot passes the class cleanly, so both failures are CratonVM-specific.

## Failure 1 — `testSelectableChannel()`: OP_CONNECT registration never delivers

```
java.util.concurrent.TimeoutException: testSelectableChannel() timed out after 3000 milliseconds
	at org.junit.jupiter.engine.extension.TimeoutExceptionFactory.create(TimeoutExceptionFactory.java:29)
	...
	Suppressed: java.lang.InterruptedException: DefaultPromise@54f3(incomplete)
		at io.netty.util.concurrent.DefaultPromise.await(DefaultPromise.java:259)
		at io.netty.util.concurrent.DefaultPromise.get(DefaultPromise.java:352)
		at io.netty.channel.nio.NioEventLoopTest.testSelectableChannel(NioEventLoopTest.java:191)
```

The test (`NioEventLoopTest.java:168-202`) registers a raw `SocketChannel` on
an `IoEventLoop` via the newer `IoRegistration`/`NioSelectableChannelIoHandle`
API, submits `NioIoOps.valueOf(SelectionKey.OP_CONNECT)`, and blocks on a
`CountDownLatch` that the handler's `handle()` callback is supposed to count
down once the connect completes:

```java
IoRegistration registration = loop.register(
        new NioSelectableChannelIoHandle<SocketChannel>(selectableChannel) {
    @Override
    protected void handle(SocketChannel channel, SelectionKey key) {
        latch.countDown();
    }
}).get();
registration.submit(NioIoOps.valueOf(SelectionKey.OP_CONNECT));
latch.await();   // <- never returns; the class's own @Timeout(3000ms) is what fires
```

`latch.await()` never returns, so the method's own `@Timeout(3000ms)`
annotation is what eventually kills it (the 3000ms in the exception is that
annotation, not a harness cap). The connect either isn't completing or the
`IoRegistration`/`NioSelectableChannelIoHandle` OP_CONNECT path isn't
delivering the readiness callback on CratonVM.

## Failure 2 — `testChannelsRegistered()`: registered count is 1, not 2

```
org.opentest4j.AssertionFailedError: expected: <2> but was: <1>
	at io.netty.channel.nio.NioEventLoopTest.testChannelsRegistered(NioEventLoopTest.java:294)
```

```java
assertTrue(loop.register(ch1).syncUninterruptibly().isSuccess());
assertTrue(loop.register(ch2).syncUninterruptibly().isSuccess());
assertEquals(2, registeredChannels(loop));   // <- fails here: registeredChannels(loop) == 1
```

Both `register()` calls report success synchronously (`isSuccess()` is
asserted true for each), yet the immediately-following
`loop.registeredChannels()` (queried via a `Callable` submitted back onto the
event loop, `NioEventLoopTest.java:311-318`) reports only 1. Either the second
registration isn't actually landing in whatever `SingleThreadIoEventLoop`
tracks as its registered-channel count, or that count is being read/updated
inconsistently with the registration completion signal.

## Not the two existing FIXED docs

Two docs under `fixed-suite-bugs/netty/` in the internal record tree mention
this class, but
neither is about either failure above:

* `ea-flag-ignored-so-assert-never-fires-20260812-FIXED.md` — a 123-class
  `-ea` validation sweep flagged `NioEventLoopTest` as the one class that
  differed across 3 arms, and re-running it 16x ABBA-interleaved found `ok=12
  failed=1` identically on **both** the control and the fixed binary — i.e. a
  pre-existing, unnamed single-test flake, unrelated to `-ea`.
* `unsafe-memory-access-property-flips-netty-to-unsafe-paths-20260812-FIXED.md`
  — records the same `ok=12 failed=1` row as unchanged by that work across 16
  interleaved runs, again without naming the failing method.

Both describe a stable `12/13` baseline from 2026-08-12/13. Today's isolated
run is `11/13` — one worse. Whether one of today's two named failures
(`testSelectableChannel` or `testChannelsRegistered`) is that same historical,
never-identified flake, or whether both are new/newly-visible, is not
established by either historical doc (neither names a method). This page
names both methods explicitly so a future session can diff against something
concrete instead of a bare ratio.

## Repro

```bash
cd apps/netty-suite-runner
printf 'io.netty.channel.nio.NioEventLoopTest\n' > /tmp/one.txt
./run-netty-suite.sh --list /tmp/one.txt --gc g1 --shards 1 --out runs/repro
./run-netty-suite.sh --list /tmp/one.txt --hotspot --shards 1 --out runs/repro
```

## Related

* `fixed-suite-bugs/netty/ea-flag-ignored-so-assert-never-fires-20260812-FIXED.md`
* `fixed-suite-bugs/netty/unsafe-memory-access-property-flips-netty-to-unsafe-paths-20260812-FIXED.md`
