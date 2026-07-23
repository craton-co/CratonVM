# `value_stack.rs` `usize` underflow panic on a background NIO worker thread — REAL CratonVM bug

**This is a genuine CratonVM defect**, unlike the other docs in this folder —
it lives alongside the "true fixture gap" bucket only because one of its two
known-affected classes (`TestNonBlockingAPI`) *also* fails on HotSpot for an
unrelated reason, which put it in the "both VMs fail" bucket during the
2026-07-24 diff. Don't let that classification hide it; treat as a
high-priority, reasonably well-isolated real bug.

## Symptom

```
thread 'http-nio-127.0.0.1-auto-22-exec-3' panicked at vm/src/runtime/value_stack.rs:237:25:
index out of bounds: the len is 24 but the index is 18446744073709551615
```

`18446744073709551615` = `u64::MAX` — i.e. a `0usize - 1` underflow wrapping
around to the maximum representable value, then used as a Vec/slice index.
Classic off-by-one or stale-index bug in the interpreter's value stack.

## Where it fires

On a **background NIO worker thread** (`http-nio-*-exec-N` /
`https-jsse-nio-*-exec-N`, not the main JUnit thread), inside
`java/util/concurrent/LinkedBlockingQueue.take()`, during Tomcat connector
pause/stop teardown between two parameterized test methods. Because it's on
a background thread, CratonVM's panic handler catches it and the process
survives — the JUnit class as a whole still prints a normal
`FAILURES!!! Tests run: N, Failures: M` summary afterward instead of the
whole VM dying, which is why this didn't originally surface as an obvious
crash.

## Confirmed reproducing in (at least) two independent classes

- `org.apache.catalina.nonblocking.TestNonBlockingAPI` — panic fires around
  test ~30-35 of 44 parameterized cases. (This class ALSO fails on HotSpot
  for an unrelated, not-yet-triaged reason — see
  [untriaged-oddities.md](untriaged-oddities.md) if that gets root-caused
  separately; it does NOT explain the panic, which is CratonVM-only.)
- `org.apache.tomcat.websocket.TestWebSocketFrameClientSSL` — same
  `value_stack.rs:237:25`, same `u64::MAX` signature, over WebSocket/TLS
  traffic instead of plain NIO HTTP. **This class PASSES cleanly on
  HotSpot**, so it's an unambiguous, uncontestable regression on its own.

Two independent classes hitting the identical file:line with the identical
underflow signature, across two different protocol paths (plain HTTP NIO
and WebSocket-over-TLS), is a strong signal this is a real, moderately
common interpreter defect in shared async-I/O plumbing — not a one-off.

## Reproduction

```sh
CRATONVM_EXE=<binary> TC_ROOT=/data/data/apps/tomcat \
  bash apps/tomcat-suite-runner/run-tomcat-suite.sh craton 0 1 valuestack-panic-repro \
  <(printf 'org.apache.catalina.nonblocking.TestNonBlockingAPI\norg.apache.tomcat.websocket.TestWebSocketFrameClientSSL\n')
```
Full JIT, real JDK, real sockets (`CRATONVM_REAL_NET_SOCKETS=1`). Logs:
`.suite/results/valuestack-panic-repro/shard-0/*.log` on the Azure host.

## Next steps for whoever picks this up

1. Read `vm/src/runtime/value_stack.rs` around line 237 — identify what
   index computation can underflow (likely a stack-pointer decrement past
   zero, or a frame-local slot lookup using a signed-vs-unsigned mismatch).
2. Since it only fires on a background thread during connector teardown,
   look for a race between the main thread tearing down the connector
   (interrupting/closing the NIO selector) and the worker thread's value
   stack being torn down or resized concurrently — a stale index captured
   before a stack shrink is the classic shape of this bug family.
3. `RUST_BACKTRACE=1` on a standalone repro (not the batch harness) would
   give the actual call chain into `value_stack.rs` — worth capturing before
   attempting a fix.
