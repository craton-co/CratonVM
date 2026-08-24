# `ParameterizedSslHandlerTest` — a completed promise whose waiter is never woken

**Status: OPEN. A thread stays parked in `Object.wait()` forever on a promise
whose `result` field is non-null and whose `waiters` count is 1.**

**Read that sentence and not the old one.** This page said "the awaited promise
is ALREADY COMPLETE", and that is an inference, not the observation — see the
UNCANCELLABLE section below, which is the leading hypothesis as of 2026-08-23
and would make the observation entirely benign.

**2026-08-23 — the "not the monitor / not the JIT" narrowing is WEAKER than
this page claimed, and one of its two refutations has been withdrawn.** Three
things changed:

1. **`CRATONVM_JIT_DENY` does not do what the "The JIT is REFUTED" section
   assumes**, and the refutation is withdrawn. The lever is documented as "a
   matching method is force-interpreted (never JIT-compiled)"; until this date
   it was consulted only by `compile_gate::admit`, i.e. by the three COMPILE
   doors, and **the inline planner never asked**. A denied method could still
   be `inline-planned` and spliced into a compiled caller's body — so a run
   with the lever set could have the denied code executing compiled anyway.
   Worse, whether it does is a **compile-order coin toss**: measured on
   `probes/XferProbe2.java`'s new `special1` arm, the SAME denied
   configuration read 5463.9, 444.9, 44.3 and 41.8 ns/op across four runs of
   three binaries, against ~48 ns/op undenied. Engaged in two of them, inert in
   the other two. The page's engagement proof (`still-interpreted` 20 → 32,
   `42036 invocations compile-failed DefaultPromise.isDone0`) cannot tell those
   apart, because it counts COMPILES and the thing that leaks is a SPLICE.

   The planner now consults the lever (`InlineRefusal::ForceInterpreted`), and
   the same arm is deterministic on a binary that has it — 384.3 / 387.1 /
   408.2 ns/op denied against 38.8 / 42.7 / 43.5 undenied over three
   interleaved pairs, with `CRATONVM_DBG=jitc` printing no `inline-planned`
   line for the denied callee at all. **The 50-run
   `CRATONVM_JIT_DENY=DefaultPromise` arm has to be re-run on that binary**
   before "the defect is tier-independent" can be asserted again.

2. **The monitor now reports whether a `notifyAll()` ever reached it, and it
   has been RUN — the answer is that there are TWO different stalls under this
   page's one title.** In ten runs of the instrumented binary, three stalled,
   and no two of them look alike:

   ```
   run  8  DefaultChannelPromise   result=Some(Int(0))   result_is=not-a-reference-slot
           notifies_since_wait=0   (monitor totals: notify=0)   orphan 0
   run  9  DefaultPromise          result_is=other(DefaultPromise$CauseHolder)
           notifies_since_wait=1   (monitor totals: notify=1)   orphan 0   waiters=1
   run 10  rc=124 — the watchdog never fired, so no dump at all
   run 11  PendingRegistrationPromise                     result_is=SUCCESS
           notifies_since_wait=1   (monitor totals: notify=1)   waiters=1
   ```

   Run 11 repeats run 9's signature on a DIFFERENT promise class and a
   DIFFERENT completion value — one failed (`CauseHolder`), one succeeded
   (`SUCCESS`) — so "the promise completed, the `notifyAll()` was delivered to
   this monitor, and the waiter is still parked" is the reproducible shape, not
   a one-off.

   * **Run 9 is the branch the counter was added to find.** The promise
     genuinely completed (a `CauseHolder`, i.e. a failure — NOT the
     `UNCANCELLABLE` case), a `notifyAll()` DID reach this monitor after the
     waiter registered, and the waiter is still parked. The defect is below
     Java, in the handshake — see "Where a NOTIFIED thread can still be stuck".
   * **Run 8 is something else entirely.** `DefaultPromise.result` is
     `private volatile Object`, and the slot holds `Int(0)` — a PRIMITIVE in a
     reference slot, the `G30-1-the-silent-reference-slot-coercion` family, with
     34 `primitive-into-reference` guard hits in that run's log. No `notifyAll`
     was ever served on that monitor (`notify=0`), which is consistent: nothing
     ever completed the promise. Whether the `Int(0)` is the cause or a
     mis-resolved field index in the dump is **not established**.

   Three stalls in ten is well above the 6.25% this page recorded (5/80), and
   the binary that produced them also carries this session's two perf changes.
   **That rate is not yet attributable** — `/tmp/mjnab.sh` interleaves
   `CRATONVM_MAP_VIEW_CACHE=0 CRATONVM_JIT_VIRTUAL_BYTECODE_CALLEE=0` against
   both-on, ON ONE BINARY, which is the only comparison that can answer it.

3. **The plain wait/notify handshake does NOT reproduce in isolation.**
   `probes/PromiseWaitProbe.java` is `DefaultPromise`'s handshake reduced to
   the three fields that carry it. Roughly **1.2 million waits** across five
   configurations — plain, `contend` (a third thread taking `synchronized (p)`
   in a loop, so the monitor inflates from outside while the waiter enters
   `wait()`), `alloc` (per-round garbage under `--Xmx 256m`), `both`, and the
   synthetic-JDK vs real-JDK (`--java-home`, `-XX:+UseG1GC`) paths — produced
   **zero stalls**, on this tree and on HotSpot. That is a real negative: the
   race between `isDone()`, `incWaiters()`, `wait()` and a `synchronized`
   completer is not sufficient on its own, so the stall needs an ingredient
   that probe does not have.

The selector-registry leak recorded at the bottom of this page is **FIXED**
independently, on its own merits, and is still not shown to cause this stall.

## Also: `result != null` does not mean the promise completed — and every stalled promise is a channel-op promise

`io.netty.util.concurrent.DefaultPromise` keeps three kinds of value in one
field:

```java
private static final Object SUCCESS = new Object();
private static final Object UNCANCELLABLE = new Object();
private static boolean isDone0(Object result) {
    return result != null && result != UNCANCELLABLE;
}
public boolean setUncancellable() {
    if (RESULT_UPDATER.compareAndSet(this, null, UNCANCELLABLE)) return true;
    …
}
```

`UNCANCELLABLE` is a **non-null marker for a promise that is still PENDING**.
`setUncancellable()` publishes it and correctly does not notify. A waiter's
`while (!isDone())` correctly parks. `checkNotifyWaiters` is correctly never
called. So `result != null` + `waiters == 1` + no notification + parked forever
is EXACTLY what a promise that was made uncancellable and never completed looks
like — with no memory-ordering defect anywhere.

**And netty makes every channel-operation promise uncancellable on the way
in.** `AbstractChannel$AbstractUnsafe` opens `bind`, `register0`, `connect` and
`close` with `if (!promise.setUncancellable() …) return;`. The two promise
classes this page has ever observed —
`AbstractBootstrap$PendingRegistrationPromise` and `DefaultChannelPromise` —
are precisely those promises. So the state this page calls unreachable is the
ORDINARY state of a pending channel operation.

The dump could not tell the two apart: it printed
`result=Some(Object(Some(ObjectRef{..})))` for both. It now compares the value
against the receiver's own `UNCANCELLABLE` / `SUCCESS` statics and prints
`result_is=UNCANCELLABLE--STILL-PENDING` / `SUCCESS` / `other(<class>)` /
`null(PENDING)`, and `unknown(sentinels-unresolved)` when it cannot resolve
them rather than silently falling back to "not uncancellable".

### The `CRATONVM_WAIT_SPURIOUS_MS` A/B does not survive this either

That A/B is the page's stated foundation — "it is what establishes the promise
was already complete". It does not establish that. The switch makes **every**
untimed `Object.wait()` in the process return after `n` ms, not just this one.
A run that completes under it therefore shows only that SOME untimed wait
somewhere was stuck; if the stuck one belongs to an event-loop or task-queue
thread, waking THAT thread lets the channel operation finish and the observed
promise complete normally — which is indistinguishable, at the level of "did
the run pass", from waking the observed waiter.

That reading is also simpler, and it fits everything: the watchdog dumps the
first parked thread it reaches, which is the TEST thread; the test thread is
parked on a pending promise because the thread that would complete it is itself
parked somewhere else. **This is reasoning, not a measurement.** What would
settle it, in one stall each:

* `result_is=` on the observed promise (instrument added, not yet run);
* **every** thread parked in `Object.wait()` at stall time and what each is
  parked on, rather than just the first. The stall logs already carry
  `N thread(s) dumped`, so the raw material may already be on disk.

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

* ~~**`result != null`** — the promise **has completed**.~~ **WITHDRAWN
  2026-08-23** — see the UNCANCELLABLE section above. `result != null` is
  consistent with a promise that is still pending, and for a channel-operation
  promise it is the EXPECTED state.
* **`waiters == 1`** — the parked thread **did** register itself
  (`incWaiters()`) before calling `wait()`. This half stands.

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
observed state is unreachable **IF the promise really completed** — which the
UNCANCELLABLE section above puts in doubt. On that assumption, exactly one of
these reads must have been stale:

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

**~~The JIT is REFUTED.~~ WITHDRAWN 2026-08-23 — see the status block.** The
argument below is reproduced as it stood, because the DATA is still good and
only the interpretation of the lever has changed. The first deny arm (0/10)
proved nothing — 10 runs at a 6% rate expects 0.6 — so it was re-run toward 50
with the lever's engagement proven first (below). It stalled at **run 4**,
which settled it without needing 50 — 50 runs were only ever required to
demonstrate *absence*, and one stall demonstrates *presence*. The arm was left
to finish anyway and ended **4 stalls in 50 (8%)**, statistically
indistinguishable from the 6.25% baseline (5/80): the lever moves the rate not
at all.

**What that no longer licenses.** "The lever moves the rate not at all" is
consistent with two different worlds: the JIT is irrelevant, OR the lever did
not force-interpret anything. Until 2026-08-23 the second world was not
excluded, because the inline planner ignored the lever and could splice the
denied bodies into their callers anyway — non-deterministically, run to run.
The arm has to be re-run on a binary whose planner consults it.

The stalled run carries the identical signature, with `DefaultPromise`
nominally force-interpreted:

```
[WAIT-OBJECT] class=io/netty/util/concurrent/DefaultPromise
              result=Some(Object(Some(...)))  waiters=Some(Int(1))
orphan hits: 0
```

~~So the stale read is **not** in `DefaultPromise`'s compiled code, and a
compiled `monitorenter`/`monitorexit` missing a fence is no longer the
candidate. **The defect is tier-independent.**~~ Both sentences depended on the
lever having engaged. Neither is established today.

Engagement was *thought* to have been proven before trusting either arm: with
the lever set, `still-interpreted` rises 20 → 32 and the hot-but-stuck list
names the denied methods outright —

```
42036 invocations  compile-failed  DefaultPromise.isDone0(Ljava/lang/Object;)Z
29492 invocations  compile-failed  DefaultPromise.isDone()Z
```

`isDone`/`isDone0` are precisely the volatile `result` read at BCI 20. But both
lines say a standalone COMPILE was refused, which is not the same claim as "no
compiled copy of this body ran": an inline splice into a compiled caller
produces neither line. That is the gap the 2026-08-23 planner fix closes, and
the reason this proof has to be re-taken rather than reused.

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

### The partitioning instrument (added 2026-08-23, not yet run against a stall)

`Monitor` now counts the `notify()` / `notifyAll()` calls it SERVES, and
`Object.wait()` snapshots that total under the state lock a notifier must also
hold — so the snapshot cannot straddle one. The watchdog dump prints the
DELTA:

```
[WAIT-OBJECT] notifies_since_wait=N interrupt_wakes_since_wait=M
              (monitor totals: notify=… interrupt=…)
```

At a stall, with `result != null` and `waiters == 1` already established, that
one number separates the two surviving explanations, which need OPPOSITE
fixes:

* **`N == 0`** — no `notifyAll()` ever reached this monitor after the waiter
  registered. The defect is then ABOVE the monitor: `checkNotifyWaiters` read a
  stale `waiters == 0`, or `setValue0`'s `RESULT_UPDATER.compareAndSet` wrote
  the field without reporting the success, so the branch containing the call
  was never taken at all. (That third possibility is worth naming: netty's
  `setValue0` is `if (CAS(null→v) || CAS(UNCANCELLABLE→v)) { checkNotifyWaiters(); }`,
  so a CAS that writes and returns `false` produces EXACTLY the observed state
  — `result` set, no notify — and `trySuccess` would then return `false`, which
  netty logs as "Failed to mark a promise as success". **Grepping a stall log
  for that string is free and has not been done.**)
* **`N >= 1`** — a notification WAS delivered to this monitor and the waiter did
  not observe it. The defect is then the condvar handshake in `monitor.rs`.

It costs one relaxed add under a lock the notifier already holds, and it needs
ONE stall rather than a rate. `wake_all_for_interrupt` is counted separately,
because it is the VM answering `Thread.interrupt()` rather than Java code
signalling a condition, and folding the two together would let an unrelated
interrupt masquerade as the missing `notifyAll`.

### Where a NOTIFIED thread can still be stuck: the RE-ACQUIRE

`Monitor::wait` has two places a thread can block, and until 2026-08-23 only
one of them could ever be reported.

After the `wait_condvar` loop breaks — which is what a delivered notification
makes it do — the thread must RE-ACQUIRE the monitor before returning to Java.
That was a bare `entry_condvar.wait(&mut state)`: untimed, unpolled, and
invisible to the watchdog, whose `[WAIT-OBJECT]` dump lives in the loop above
it. **A thread that was notified, broke out, and then blocked there produces
exactly run 9's signature** — `notifies_since_wait=1` and a thread still
parked — and nothing on this page could tell that apart from a notification
that was never delivered to the waiter at all.

The re-acquire now polls on the same 5 ms cadence and reports once:

```
[WAIT-REACQUIRE] thread … was NOTIFIED and is now stuck RE-ACQUIRING the
                 monitor, not waiting on it — owner=… entry_count=… …
```

`Monitor::exit` releases with `entry_condvar.notify_one()`, so a release wakes
exactly one of the threads queued there and in `enter_labeled`; any path that
releases the monitor WITHOUT going through `Monitor::exit` leaves a waiter
there with nothing to wake it. Re-testing the condition on a timer is sound for
a lock ACQUIRE in a way it would not be for `Object.wait` (which owes Java a
real notification), so the poll is both the instrument and a removal of that
class of permanent stall. `enter_labeled`'s own contended loop is deliberately
left alone — hot `monitorenter` path, documented as byte-for-byte unchanged,
and `CRATONVM_DBG_MONENTER` already makes it pollable when someone is looking.

**The next stall on a binary with this either prints `[WAIT-REACQUIRE]` — in
which case run 9 is a lock-handover defect and not a lost notification — or it
does not, in which case the notification really was delivered into a
`wait_condvar` the waiter was parked on and did not observe.** One stall
decides it.

### `probes/PromiseWaitProbe.java` — the isolated handshake, which does NOT stall

`DefaultPromise`'s `result` / `waiters` / `notifyAll` handshake with nothing
else attached, driven by a REUSED waiter pool spinning on a sequence number
(thread creation is ~1 ms and the window being hunted is the handful of
instructions between `isDone()` and `wait()`, so a thread-per-round harness
spends all its time outside the race). Its STALL line reports `cas`, `result`,
`notified`, the `waiters` the completer saw and the `waiters` now, so each
candidate mechanism has a distinct fingerprint.

~1.2 M waits, five pressure configurations, both JDK paths: **zero stalls.**
Recorded as a negative rather than dropped — it says the monitor primitive is
not the whole story, and it is the harness the next ingredient should be added
to rather than rebuilt.

**Do not read `CRATONVM_WAIT_SPURIOUS_MS` as a fix.** It converts a permanent
hang into an `n`-second delay by papering over a lost wakeup; at 100 ms it also
cost 69% wall (72.5 s → 122.8 s), so it is a diagnostic, not a mitigation.

## Also found: the selector registry never shrinks — **FIXED 2026-08-23**

1056 selectors in one run of one class, 1040 of them closed. `selector_close`
sets `open = false` and nothing removes the map entry — it cannot, because
other threads hold `MutexGuard`s derived from it and the registry's values are
stored inline — so `deregister_fd_everywhere`, called on **every channel
close**, locked ~1000 dead mutexes by the end of the class purely to read a
`bool` out of each and find nothing. Three other registry-wide walks
(`deregister_channel_everywhere`, `slot_of_key_obj`, `selector_refresh_udp`)
had the same shape.

The entry now carries a lock-free mirror of its `open` flag beside the mutex
(`SelectorSlot`), and every registry-wide walk skips a closed entry without
locking it. `SelectorState::open` stays authoritative — every correctness
decision still reads it under the lock — and the mirror is only ever used to
SKIP. `open` is monotone (open once, closed forever), so a skip cannot race a
re-open: there is no such transition.

A real defect on its own merits; still explicitly **not** shown to cause this
stall, and fixed on those terms rather than offered as the stall's fix.

## Next

0. ~~Make the wait-site handle GC-safe first~~ — **DONE** (`95c210f37`).
   `Monitor::wait`'s local was the only unforwarded reference;
   `jmx_waiting_monitor` was already rooted and remapped, so resolving through
   it (`install_wait_object_resolve` → `peek_jmx_waiting_monitor`) makes the
   dump sound. The line now names which handle it used, so a silent fallback
   cannot pass as a sound reading, and prints an explicit `RELOCATED` line when
   the entry pointer and the live one disagree — which measures the staleness
   instead of merely suspecting it.

   **Re-taken, and the original reading holds.** First stall on the GC-safe
   binary:

   ```
   [WAIT-OBJECT] handle=registry-remapped obj=0x20061300748
   [WAIT-OBJECT] class=io/netty/channel/DefaultChannelPromise
                 result=Some(Object(Some(...)))  waiters=Some(Int(1))
   ```

   `handle=registry-remapped` confirms the resolver engaged rather than
   silently falling back. **No `RELOCATED` line** — the entry pointer and the
   live pointer were identical, so in this instance the promise never moved and
   the earlier stale-local dumps were in fact reading the right memory. The
   caveat was worth raising (it was unfalsifiable as written), but the specific
   failure it warned about did not materialise. `result != null`,
   `waiters == 1`, no orphan — now on a handle that cannot lie.
1. A plain missing fence is now UNLIKELY and should not be assumed: the thin
   path CASes `Acquire` on lock (`try_thin_lock`) and `Release` on unlock
   (`try_thin_unlock`), and the inflated path goes through a
   `parking_lot::Mutex` — a release/acquire pair there publishes ordinary heap
   writes just as well as Java fields, because it is one hardware edge. An
   earlier revision of this page asserted "nothing publishes them"; that was
   wrong. Audit instead the **inflation transition** and the
   thin→inflated handover, where the two orderings meet.
2. ~~Cheapest confirmation: log `waiters` as the completer reads it~~ —
   **SUPERSEDED and DONE differently.** Counting the notifies the MONITOR
   served is strictly stronger than logging what the completer read: it
   distinguishes "the completer never called `notifyAll()`" from "it did and
   the waiter missed it", which is the branch point, whereas a `waiters` log
   only covers the first. See "The partitioning instrument". **Still needs one
   stall to be read.**
3. **RE-RUN the `CRATONVM_JIT_DENY=DefaultPromise` arm** on a binary whose
   inline planner consults the lever (any build from `a36caa907` on). Until
   that is done the JIT is not refuted, and "the defect is tier-independent" is
   not a finding.

   Read it with the `result_is=` tag from step 2b, not just the stall count: if
   the promise is `UNCANCELLABLE` the deny arm is answering a question about a
   defect that is not there.

2b. **Read `result_is=` on the next stall before anything else.** The dump now
   names `UNCANCELLABLE` (see "Also: `result != null` does not mean completed"
   above). If it reads `UNCANCELLABLE--STILL-PENDING`, every "the promise has
   completed" conclusion on this page collapses, the monitor is exonerated
   outright, and the question becomes what failed to complete the promise —
   which is a different investigation with a different suspect list.
3b. **Dump EVERY thread parked in `Object.wait()` at stall time**, not the
   first one. If the test thread is parked on a pending promise because the
   thread that would complete it is itself parked, the current dump names the
   symptom and never the cause. The existing logs report `N thread(s) dumped`,
   so this may be answerable from disk without a new run.
4. Grep an existing stall log for netty's own
   `"Failed to mark a promise as success"` warning. If it is there, `setValue0`
   returned `false` after writing `result`, and the defect is in
   `AtomicReferenceFieldUpdater.compareAndSet` rather than in the monitor at
   all — which no instrument on this page would have shown. Free to check.
   **DONE 2026-08-23 — REFUTED.** No `/tmp/sslloop/*/run-*.log` on the Azure
   host contains that string, nor an `IllegalStateException` / "complete
   already" from the throwing `setSuccess` sibling; the only match anywhere is
   the unrelated "Failed to mark a promise as failure because it has failed
   already". So whenever `result` holds a real completion, `setValue0` returned
   true and `checkNotifyWaiters()` did run.
5. Fix whatever 2/3/4 name; re-measure the rate over ≥40 runs.
6. ~~Fix the selector-registry leak independently~~ — **DONE 2026-08-23.**

## Repro

```bash
/tmp/sslloop/sslloop.sh 40 <tag>     # rate + watchdog census + [WAIT-OBJECT]
/tmp/sslloop/sslloopC.sh 30 <tag>    # live /proc sampling, no watchdog
```

Azure `vm1`; `gen-openssl-args.sh -o /tmp/ossl.args` first and confirm
`OpenSsl.isAvailable == true`. **The host carries other sessions' builds** — a
load average of 168 on 8 cores was seen, which invalidates any sequential A/B;
interleave the arms (`/tmp/sslloop/sslab.sh`) or check `uptime` first.
