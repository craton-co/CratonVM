# `ParameterizedSslHandlerTest` — a completed promise whose waiter is never woken

**Status: ROOT CAUSE LOCALISED. The awaited promise is ALREADY COMPLETE and the
waiter IS registered, yet the thread stays parked in `Object.wait()` forever.
This is a memory-ordering defect around CratonVM's monitor, not a netty, TLS or
selector problem.** The remaining work is to decide which of two reads went
stale and to fix it.

## The measurement that settles it

At stall time, with the watchdog dumping the state of the object the thread is
parked on:

```
[WAIT-OBJECT] obj=0x2004a843cc8
  class=io/netty/bootstrap/AbstractBootstrap$PendingRegistrationPromise
  result=Some(Object(Some(ObjectRef { ptr: 0x200437e6c08 })))
  waiters=Some(Int(1))
```

Both halves matter:

* **`result != null`** — the promise **has completed**. Nothing is outstanding;
  the reactor did its job.
* **`waiters == 1`** — the parked thread **did** register itself
  (`incWaiters()`) before calling `wait()`.

And it is parked at `DefaultPromise.awaitUninterruptibly` BCI 31, which `javap`
resolves to `Object.wait()`, in `futex_do_wait`, burning no CPU.

So a `notifyAll()` that should have been sent either never was, or was sent to a
monitor with no waiters recorded. netty's completer is:

```java
private synchronized boolean checkNotifyWaiters() {
    if (waiters > 0) { notifyAll(); }   // waiters is a PLAIN int
    ...
}
```

and the waiter's side is

```java
synchronized (this) {                    // DefaultPromise.await*, BCI 18
    while (!isDone()) {                  // BCI 20 — volatile read of `result`
        incWaiters();                    // BCI 27 — waiters++
        wait();                          // BCI 31
    }
}
```

Both blocks synchronize on the same object, and the orphan check (below) proves
they resolve the same monitor. With correct `synchronized` semantics the
observed state is unreachable. Exactly one of these reads must have been stale:

1. **the waiter's volatile read of `result` at BCI 20** — if it saw `null` after
   the completer had already published a non-null `result` and run
   `checkNotifyWaiters()` (legitimately seeing `waiters == 0`), the waiter parks
   with nobody left to notify it; or
2. **the completer's plain read of `waiters`** — if it saw `0` after the
   waiter's `waiters++` inside the monitor, it skips `notifyAll()` entirely.

Either is a happens-before failure across `monitorenter`/`monitorexit`.

## Why it is none of the things previously proposed

| hypothesis | how it died |
|---|---|
| close_notify never arrives / alert never flushed | six observations span **five** test methods and **four** operations, incl. `ServerBootstrap.bind()` — no TLS, no peer, no alert |
| lost `Selector.wakeup()` | `NioIoHandler.select@136` resolves to `Selector.select:(J)I`, the **timed** overload, so a lost wakeup costs latency not a hang; a sweeping probe lost **0 of 5000** wakeups |
| the timed select never returns | two `/proc` samples 6 s apart: 4 of 10 threads left `ep_poll`, several moved `utime` — the reactors cycle normally |
| a moving GC relocates the monitor | the monitor lives in the object's **mark word**, so it travels with the object |
| `prune_dead` orphans the waiter | only ZGC calls it; these runs are **G1**, which calls `remap_after_gc` only |
| the waiter is orphaned from its monitor | the orphan check re-reads the mark word on the existing 5 ms poll — **0 hits on a reproduced stall**, and it is on the branch that actually runs (`interrupted` is always `Some`) |

## Rate, and what does not move it

| arm | runs | stalls |
|---|---:|---:|
| baseline (`c21d766ad`) | 40 | 3 |
| baseline (spurious binary, switch OFF) | 40 | 2 |
| `CRATONVM_JIT_DENY=DefaultPromise` | 10 | 0 |

~5–7.5%. The JIT-deny arm is **not** conclusive at 10 runs (≈0.6 expected) — it
needs ~50 to separate 6% from 0%, and is the obvious next A/B now that the
mechanism is known, since a compiled `monitorenter`/`monitorexit` missing a
fence is the leading candidate for the stale read.

## Instruments added (all default-OFF or watchdog-only)

* `nio_selector::dump_selector_state_to_stderr` — selector key census on the
  watchdog. Showed the selector side is unremarkable: reactors cycling, no
  outstanding wakeup, and exactly one key process-wide (the listener,
  `OP_ACCEPT`, `ready=0`).
* `CRATONVM_WAIT_SPURIOUS_MS=<n>` — makes an untimed `Object.wait()` return to
  Java after `n` ms (JLS 17.2.1 permits a spurious wakeup). A one-binary A/B:
  at 30 s, runs that would hang 420 s instead **passed at baseline + 30 s**,
  which is what first proved the promise was already complete.
  Engagement-proven: with the switch unset a never-notified waiter parks
  forever; with it set the waiter returns.
* the orphan check, and `dump_wait_object_state` (the `[WAIT-OBJECT]` line
  above).

**Do not read `CRATONVM_WAIT_SPURIOUS_MS` as a fix.** It converts a permanent
hang into an `n`-second delay by papering over a lost wakeup; at 100 ms it also
cost 69% wall (72.5 s → 122.8 s), so it is a diagnostic, not a mitigation.

## Also found: the selector registry never shrinks

1056 selectors in one run of one class, 1040 of them closed. `selector_close`
sets `open = false` and nothing removes the map entry, so
`deregister_fd_everywhere` — called on **every channel close** — locks ~1000
dead mutexes by the end of the class. A real defect on its own merits;
explicitly **not** shown to cause this stall.

## Next

1. Decide which read goes stale — instrument `monitorenter`/`monitorexit`
   fencing, or A/B `CRATONVM_JIT_DENY=DefaultPromise` to ~50 runs.
2. Fix the ordering; re-measure the rate over ≥40 runs.
3. Fix the selector-registry leak independently.

## Repro

```bash
/tmp/sslloop/sslloop.sh 40 <tag>     # rate + watchdog census + [WAIT-OBJECT]
/tmp/sslloop/sslloopC.sh 30 <tag>    # live /proc sampling, no watchdog
```

Azure `vm1`; `gen-openssl-args.sh -o /tmp/ossl.args` first and confirm
`OpenSsl.isAvailable == true`. **The host carries other sessions' builds** — a
load average of 168 on 8 cores was seen, which invalidates any sequential A/B;
interleave the arms (`/tmp/sslloop/sslab.sh`) or check `uptime` first.
