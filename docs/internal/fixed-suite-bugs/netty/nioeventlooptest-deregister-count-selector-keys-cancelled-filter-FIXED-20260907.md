# `NioEventLoopTest.testChannelsRegistered` — deregistering one of two channels dropped the count to 0, not 1 — FIXED

## Status
**FIXED 2026-09-07.** Root cause in `Selector.keys()`, not in registration or
cancellation bookkeeping itself.

## Symptom (as originally filed)

```
@@TESTFAIL io.netty.channel.nio.NioEventLoopTest testChannelsRegistered() FAILED
org.opentest4j.AssertionFailedError: expected: <1> but was: <0>
	at io.netty.channel.nio.NioEventLoopTest.testChannelsRegistered(NioEventLoopTest.java:304)
```

Failed identically on Generational, G1, and ZGC in the full-suite run
(2026-09-06), and reproduced deterministically in isolation.

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
assertEquals(1, registered);   // was failing: registered read as 0
```

## Root cause

`registeredChannels()` (`NioIoHandler.numRegistered()` in Netty) is:

```java
int numRegistered() { return selector().keys().size() - cancelledKeys; }
```

`cancelledKeys` is a counter **Netty itself** increments every time it calls
`SelectionKey.cancel()`. The formula only works because real JDK cancellation
is lazy: `cancel()` marks a key invalid but the key stays in
`Selector.keys()` until the selector's own cancelled-key processing runs at
the next `select()`. Subtracting Netty's own cancellation count from that
still-inflated `keys().size()` is what recovers the true "live" count.

CratonVM's `native-io` selector already implements that lazy-pruning model
correctly at the data-structure level (`KeyState::cancelled`, pruned only by
`select()` — see `selector_key_count` and the
`t19_7_a_key_cancel_removes_from_next_select` unit test). The bug was one
layer up: `selector_keys()`, the native method backing `Selector.keys()`
itself, filtered `!k.cancelled` before building the returned set. That means
CratonVM's `keys()` already excluded a just-cancelled key, and Netty's
formula then subtracted `cancelledKeys` a **second time** from a set that
had never included it — undercounting by exactly the number of
recently-cancelled-but-unpruned keys. For this test: register 2, cancel 1 →
CratonVM's `keys().size()` reads 1 (already filtered) instead of the JDK's 2
(unfiltered), then `1 - cancelledKeys(1) = 0` instead of the correct
`2 - 1 = 1`.

`Selector.selectedKeys()` (a different, correctly-filtered set — a cancelled
key should not appear as "ready") was NOT affected; only the full
registration set `keys()` was wrong.

## Fix

[`native-io/src/nio_selector.rs`](../../../../native-io/src/nio_selector.rs),
`selector_keys()`: removed the `.filter(|k| !k.cancelled)` step. It now
returns every key still present in the map, matching real
`Selector.keys()` semantics (pruned only by the next `select()`).
`selector_selected_keys()` is untouched and keeps its own
`!k.cancelled && k.ready_ops != 0` filter, which is correct for that set.

## Verification

- `cargo test -p cratonvm-native-io --lib nio_selector`: 32/32 pass,
  unchanged (no existing test asserted `keys()`'s cancelled-filtering
  behavior either way, so nothing needed updating — the invariant the fix
  relies on, that the underlying map keeps a cancelled entry until `select()`
  prunes it, was already covered by `t19_7_a_key_cancel_removes_from_next_select`).
- Built on the Azure host and ran the actual class twice against the real
  Netty 733-class suite fixture (`CratonRunner io.netty.channel.nio.NioEventLoopTest`,
  real JDK 25, `--java-home jdk25-linux`):
  `@@RESULT io.netty.channel.nio.NioEventLoopTest found=13 started=13 ok=13
  failed=0 aborted=0 skipped=0` — both runs, deterministic 2/2, matching the
  rigor of the original bug report.

## Related

* `docs/known-issues/netty/` tombstone at the original filing path.
