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

> ### ⚠ This dump is UNSOUND under a moving collector — read the caveat
>
> `Monitor::wait` captures the awaited `ObjectRef` at entry and holds it as a
> plain Rust local for the whole wait. **Nothing remaps it.** `monitor_wait`
> does remap once, from the GC pointer map, but only *before* entering the wait;
> after that the thread is parked and any G1 evacuation moves the promise out
> from under that address. Over a 420 s stall there are many collections.
>
> **Correction:** an earlier revision of this box also named
> `thread_registry`'s `jmx_waiting_monitor` as unforwarded. That was **wrong**.
> It is pushed into `all_roots` during root scanning and rewritten by
> `update_thread_objs_after_gc` (gc.rs step 21) — exactly as its own doc comment
> claims. The only unforwarded reference was ever `Monitor::wait`'s local, and
> the registry copy is therefore the fix, not a second instance of the bug.
>
> So the `[WAIT-OBJECT]` line above may be reading the **pre-relocation copy**,
> and G1 leaves that copy intact in the from-region until it is reused — which
> is exactly why the fields still parse as a plausible `DefaultPromise`. The
> same applies to the orphan check below: its `0 hits` is not trustworthy in
> either direction, because it re-reads the mark word through the same stale
> address.
>
> **The conclusion does not rest on this dump.** It rests on the
> `CRATONVM_WAIT_SPURIOUS_MS` A/B: waking the parked thread makes the run
> complete, and when it wakes it re-evaluates `isDone()` *in Java*, through a
> properly remapped reference. That is sound, and it is what establishes the
> promise was already complete. Treat the field dump as corroboration that has
> not earned its place until the handle is made GC-safe.
>
> Fixing the instrument means holding the awaited object in something the
> collector forwards (a pinned native root, or re-reading it from a scanned
> slot on each 5 ms poll) — worth doing before this dump is cited again.

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
| `CRATONVM_JIT_DENY=DefaultPromise` (orphan binary) | 10 | 0 |
| `CRATONVM_JIT_DENY=DefaultPromise` (fields binary, engagement proven) | 50 | **4** |

~5–7.5%.

**The JIT is REFUTED.** The first deny arm (0/10) proved nothing — 10 runs at a
6% rate expects 0.6 — so it was re-run toward 50 with the lever's engagement
proven first (below). It stalled at **run 4**, which settled it without needing
50 — 50 runs were only ever required to demonstrate *absence*, and one stall
demonstrates *presence*. The arm was left to finish anyway and ended
**4 stalls in 50 (8%)**, statistically indistinguishable from the 6.25%
baseline (5/80): the lever moves the rate not at all. The stalled run carries the identical signature, with
`DefaultPromise` force-interpreted:

```
[WAIT-OBJECT] class=io/netty/util/concurrent/DefaultPromise
              result=Some(Object(Some(...)))  waiters=Some(Int(1))
orphan hits: 0
```

So the stale read is **not** in `DefaultPromise`'s compiled code, and a compiled
`monitorenter`/`monitorexit` missing a fence is no longer the candidate. **The
defect is tier-independent — it is in the VM's monitor implementation itself**
(the interpreter path included), or in the `Object.wait`/`notifyAll`
bookkeeping around it.

Engagement was proven before trusting either arm: with the lever set,
`still-interpreted` rises 20 → 32 and the hot-but-stuck list names the denied
methods outright —

```
42036 invocations  compile-failed  DefaultPromise.isDone0(Ljava/lang/Object;)Z
29492 invocations  compile-failed  DefaultPromise.isDone()Z
```

`isDone`/`isDone0` are precisely the volatile `result` read at BCI 20, i.e. the
arm did test candidate (1) directly.

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
  above) — **both unsound under a moving collector; see the caveat box.**

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

0. ~~Make the wait-site handle GC-safe first~~ — **DONE** (`95c210f37`).
   `Monitor::wait`'s local was the only unforwarded reference;
   `jmx_waiting_monitor` was already rooted and remapped, so resolving through
   it (`install_wait_object_resolve` → `peek_jmx_waiting_monitor`) makes the
   dump sound. The line now names which handle it used, so a silent fallback
   cannot pass as a sound reading, and prints an explicit `RELOCATED` line when
   the entry pointer and the live one disagree — which measures the staleness
   instead of merely suspecting it. **Re-take the `result`/`waiters` reading on
   this binary before citing it.**
1. A plain missing fence is now UNLIKELY and should not be assumed: the thin
   path CASes `Acquire` on lock (`try_thin_lock`) and `Release` on unlock
   (`try_thin_unlock`), and the inflated path goes through a
   `parking_lot::Mutex` — a release/acquire pair there publishes ordinary heap
   writes just as well as Java fields, because it is one hardware edge. An
   earlier revision of this page asserted "nothing publishes them"; that was
   wrong. Audit instead the **inflation transition** and the
   thin→inflated handover, where the two orderings meet.
2. Cheapest confirmation: log `waiters` as the completer reads it, next to the
   value the waiter wrote — through a GC-safe handle per step 0. A 0-vs-1
   disagreement names the failing edge directly and needs one stall, not a rate.
3. Fix the ordering; re-measure the rate over ≥40 runs.
4. Fix the selector-registry leak independently.

## Repro

```bash
/tmp/sslloop/sslloop.sh 40 <tag>     # rate + watchdog census + [WAIT-OBJECT]
/tmp/sslloop/sslloopC.sh 30 <tag>    # live /proc sampling, no watchdog
```

Azure `vm1`; `gen-openssl-args.sh -o /tmp/ossl.args` first and confirm
`OpenSsl.isAvailable == true`. **The host carries other sessions' builds** — a
load average of 168 on 8 cores was seen, which invalidates any sequential A/B;
interleave the arms (`/tmp/sslloop/sslab.sh`) or check `uptime` first.
