# AtomicInteger `LOCK XADD` intrinsic — 24x, and it hangs DefaultPromiseTest

**Status:** OPEN. Built, measured, NOT landed. The work is parked on
`perf/jit-atomic-intrinsic-parked-20260812`.

`AtomicInteger.getAndIncrement` and its five siblings are registered natives, so
every increment from compiled code pays a full native dispatch around one
uncontended `lock xadd`. Replacing that with the single locked instruction is
worth ~24x and is *implemented and correct in isolation* — but it makes
`DefaultPromiseTest.testListenerNotifyOrder` hang, deterministically, so it is
not landable as written.

## The part that was actually broken: the OSR door

The intrinsic existed in an earlier form and appeared to do nothing. The cause
was not the matcher and not the codegen:

`jit::try_compile_inner` is not the only compile door. `compile_osr_artifact`
reaches `x64::compile_with_param_slots` **directly** and keeps its OWN copy of
the direct-call ladder. A counter loop written inside ONE method — a test body,
a benchmark `main` — is promoted by OSR, never passes through
`jit::try_compile`, and therefore never sees an intrinsic registered only there.
That is exactly the shape this intrinsic exists to speed up. The in-tree comment
on the OSR `HashMap` arm already states the rule:

> a once-invoked benchmark-style method runs its whole life inside the OSR body
> and never passes through `jit::try_compile`.

Registered at `try_compile` only, the ABBA A/B is flat — 4.39 vs 4.32 M/s, inside
the noise. Registered at both doors it separates completely. **Both doors call
the same matcher**, so they cannot drift on which shapes they admit.

A related trap: the first measurement of this work used a probe whose loop lived
in `main`. The invocation counter never promotes `main`, so those numbers came
from interpreted code and no JIT intrinsic could have moved them. The firing
witness printing nothing is what caught it — see "instrumentation" below.

## The win (real, and not the problem)

ABBA-interleaved in ONE binary, B arm = `CRATONVM_JIT_NO_ATOMIC_INTRINSIC=1`.
Cross-run wall time on this host is not a measurement; HotSpot's own atomic rate
moved 94 -> 208 M/s between two runs with no code change.

| arm | `getAndIncrement`, M/s | median |
|---|---|---|
| A (intrinsic) | 136.6  93.1  93.4  92.3  93.7  89.3 | **93.3** |
| B (native) | 4.5  7.4  3.8  3.9  3.7  3.8 | **3.9** |
| HotSpot JDK 25 | | 205.2 |

~24x, no overlap between the arms. `addAndGet` moves with it (129.6 vs 6.8 M/s).
The family goes from ~2% of HotSpot to ~45% of it.

Correctness in isolation is good. `AtomCheck` covers all six methods' return
values, a non-exact subclass receiver, the same call site alternating
exact/subclass receivers after compilation, a null-receiver NPE, and 8 threads x
200,000 increments totalling exactly 1,600,000. It PASSES on HotSpot, on CratonVM
with the intrinsic, and on CratonVM with it disabled — identical semantics
checksum `270002400000` in all three.

## The blocker

`io.netty.util.concurrent.DefaultPromiseTest.testListenerNotifyOrder()` times out
after 120 s with the intrinsic on. ABBA, one class per run, one shard, nothing
else on the host:

| arm | result |
|---|---|
| A (intrinsic) | `ok=19 failed=1`, ms≈134–147k — **5 of 5 runs** |
| B (`CRATONVM_JIT_NO_ATOMIC_INTRINSIC=1`) | `ok=20 failed=0`, ms≈66–91k — **4 of 4 runs** |

Fully deterministic in both directions. The failure is
`java.util.concurrent.TimeoutException`, not an assertion: the test blocks in
`listeners.take()` on a `BlockingQueue`, i.e. a listener is never notified.

Note what the timings say: **the other 19 tests are FASTER with the intrinsic**
(the class spends ~20 s on them in arm A against ~70 s in arm B). This is one
specific site wedging, not a general slowdown.

## Ruled out: mixed-mode lost update

The leading hypothesis was an atomicity-domain mismatch, and it is a real
structural hazard worth recording even though it is not this bug.

The native's atomicity does **not** come from hardware atomics. It comes from two
software locks in `compare_and_swap_field` — a per-object CAS mutex
(`monitors.with_cas_lock`) and the collector's `volatile_stripe_lock`, which
exists to stop the 16-byte `Value` slot tearing and to keep the raw slot access
"atomic with every non-CAS volatile access". A hardware `LOCK XADD` on the field
payload honours neither. So an interpreted read-compare-write can read a value,
have a compiled XADD add to it, then write its own result back and obliterate the
increment.

`MixAtom` tests exactly that: two threads, one running a compiled loop, the other
force-interpreted with `CRATONVM_JIT_DENY=MixAtom.denyLoop`, both incrementing one
counter 300,000 times. **No loss** — `total=600000 want=600000`, 3 of 3 runs with
the intrinsic on, 3 of 3 with it off, matching HotSpot. So the two domains do not
in fact drop updates in this configuration, and the hang has another cause.

(Before reusing that negative result, re-verify that the deny filter actually kept
`denyLoop` interpreted — the probe's setup is code that can be wrong.)

## Open — where to look next

Unexplored, roughly in order of promise:

1. **Deopt storm at one site.** Every bail (null receiver, class-id mismatch)
   goes to the shared uncommon-trap stub with reason 6. If a hot site's receiver
   is not exactly `AtomicInteger`, it could compile/deopt repeatedly. That fits
   "one site wedges while everything else gets faster". Get the registered site
   list first: run the class under `CRATONVM_DBG_ATOMIC_INTRINSIC=1` with a
   timeout well above the ~140 s the class needs, and do not pipe stderr through
   a buffering `grep` or the output is lost when the timeout fires.
2. **A missed wakeup rather than a wrong value**, in `GlobalEventExecutor`'s
   start-up state machine — the test's listeners are dispatched through
   `GlobalEventExecutor.INSTANCE`.
3. **Ordering.** `LOCK XADD` is a full barrier, so this is unlikely to be a
   missing fence; it is more likely the *absence of the stripe lock* changing what
   a concurrent volatile reader observes mid-operation.

## Reproducing

Probes are in `probe/` on the parked branch: `AtomCheck` (correctness matrix),
`AtomRate` (OSR shape — loop inside one method), `AtomRate2` (invocation-counter
shape), `MixAtom` (mixed compiled/interpreted). `ab3.sh` runs the full ABBA plus
the HotSpot reference; `dpt.sh` runs the DefaultPromiseTest ABBA.

## Instrumentation (keep it)

- `CRATONVM_DBG_ATOMIC_INTRINSIC=1` — prints every admitted site with its class id
  and both offsets. Also `ATOMIC_INTRINSIC_SITES`, a count of admissions.
- `CRATONVM_JIT_NO_ATOMIC_INTRINSIC=1` — kill switch, and the B arm of any A/B.

A perf claim about this family is not checkable without both. The intrinsic
answering the same values as the native it replaced proves nothing about whether
it ever ran, and on this host a cross-run wall-time comparison proves nothing at
all.
