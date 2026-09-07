# `NioEventLoopTest.testChannelsRegistered` — deregistering one of two channels drops the count to 0, not 1

| | |
|---|---|
| **Status** | OPEN. Confirmed deterministic, collector-independent. |
| **Severity** | Low-moderate. Wrong bookkeeping count, not a crash or hang; narrow, specific repro. |
| **Discovered** | 2026-09-06, full 733-class 3-GC-arm suite run; confirmed by isolated standalone rerun (2/2, via the harness's own `timeout`/watchdog-disable wrapper, not ad-hoc). |

## Symptom

```
@@TESTFAIL io.netty.channel.nio.NioEventLoopTest testChannelsRegistered() FAILED
org.opentest4j.AssertionFailedError: expected: <1> but was: <0>
	at io.netty.channel.nio.NioEventLoopTest.testChannelsRegistered(NioEventLoopTest.java:304)
```

Fails identically on Generational, G1, and ZGC in the full-suite run, and
reproduces deterministically in isolation (no other process on the host,
9.5s, 2/2 reps) — this is not a host-contention artifact.

## What the test does

```java
Channel ch1 = new NioServerSocketChannel();
Channel ch2 = new NioServerSocketChannel();

assertEquals(0, registeredChannels(loop));
loop.register(ch1); loop.register(ch2);
assertEquals(2, registeredChannels(loop));

ch1.deregister();

int registered;
while ((registered = registeredChannels(loop)) == 2) {
    Thread.sleep(50);
}
assertEquals(1, registered);   // <-- fails: registered is 0
```

`registeredChannels()` submits `loop.registeredChannels()` (Netty's own
`SingleThreadIoEventLoop` accessor, which counts live registrations on the
underlying `Selector`) as a task to the event loop and reads the result back.

Register 2 channels (count reaches 2), deregister exactly one (`ch1`), then
poll until the count leaves 2. On CratonVM the count is observed at 0 by the
time it leaves 2 — both channels' registrations disappear, not just the one
that was deregistered. On a correct implementation the only value the count
can pass through between 2 and settling is 1.

## What is NOT yet known

- Whether `ch2` is actually deregistered from the underlying `Selector`, or
  merely miscounted by `registeredChannels()` while still live and functional.
- Whether this is specific to `NioServerSocketChannel`/`ServerSocketChannel`
  registration, or a general selector-key-count bug that would show up on any
  channel type under the same register/deregister/count sequence.
- Whether the transition genuinely skips 1 (both keys cancelled together) or
  whether it passes through 1 too briefly for the 50ms poll interval to
  observe it before reaching 0 (harder to distinguish from a single run, but
  the deterministic 2/2 reproduction argues against a narrow timing race).

Not traced further into CratonVM's `Selector`/`SelectionKey` native
implementation in this session.

## Reproducing

```bash
cd apps/netty-suite-runner
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 timeout 60 \
  cratonvm --java-home <jdk25> --Xmx 1500m @common.args -Dcraton.batch=1 \
  CratonRunner io.netty.channel.nio.NioEventLoopTest
```

Note: an ad-hoc invocation without the harness's `timeout` wrapper during this
triage did not return within 90s and left a live process behind (killed
manually) — not confirmed as a property of this class specifically (a second,
overlapping ad-hoc attempt was inadvertently launched while the first was
still running, which is a confound this note cannot rule out). Reproduce
through the harness, or with both `timeout` and
`CRATONVM_DISABLE_DEFAULT_WATCHDOG=1` set explicitly, to avoid re-hitting
whatever that was.
