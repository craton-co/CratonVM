# AtomicInteger `LOCK XADD` intrinsic — 24x, and why it cannot land as built

**Status:** ROOT-CAUSED, not landable. Parked on
`perf/jit-atomic-intrinsic-parked-20260812`.

Replacing the registered `AtomicInteger` RMW natives with a single `LOCK XADD` is
worth ~24x and is correct in a single-mode process. It is **unsound in this VM**:
the intrinsic and the native it replaces do not share an atomicity domain, so a
compiled updater and an interpreted one racing the same counter lose updates.

## Root cause: two atomicity domains for one field

CratonVM's atomicity for these fields does **not** come from hardware atomics. It
comes from two software locks taken by `compare_and_swap_field`:

- `monitors.with_cas_lock(obj)` — the per-object linearization point;
- `cratonvm_gc::collector::volatile_stripe_lock(obj, index)` — held to stop the
  16-byte `Value` slot tearing, and, in the code's own words, to keep "this raw
  slot access atomic with every non-CAS volatile access".

A hardware `LOCK XADD` on the 4-byte payload honours neither. The interpreted
read-compare-write can read a value, have a compiled XADD add to it, then write
its own computed result back and obliterate the increment.

**Demonstrated.** `MixAtom` runs two threads over one counter, 300,000
increments each: one loop compiled, the other force-interpreted with
`CRATONVM_JIT_DENY=MixAtom.denyLoop`.

```
[atomic-site] door=osr holder=MixAtom.jitLoop callee=getAndIncrement
total=575092 want=600000  *** LOST 24908 ***
```

~4.2% of increments vanish. The `[atomic-site]` line is the proof the setup is
honest: only `jitLoop` admits the intrinsic, `denyLoop` does not appear, so the
two threads really are in different modes.

> An earlier run of this same probe reported no loss and was recorded here as
> "ruled out". That was wrong — a negative from a probe whose setup was never
> verified. The holder-naming witness (`[atomic-site] door=… holder=…`) was added
> precisely because the callee-only witness could not show which loop had been
> compiled. Do not trust a negative result from an unwitnessed probe.

## Why that hangs DefaultPromiseTest

`testListenerNotifyOrder` blocks in `listeners.take()` and dies on JUnit's
120 s timeout. The witness names exactly which sites the intrinsic captured in
that class:

```
door=try_compile holder=java/util/concurrent/LinkedBlockingQueue.take   callee=getAndDecrement
door=try_compile holder=java/util/concurrent/LinkedBlockingQueue.offer  callee=getAndIncrement
door=try_compile holder=io/netty/util/concurrent/DefaultThreadFactory.newThread callee=incrementAndGet
```

`LinkedBlockingQueue` keeps its element count in an `AtomicInteger` and uses the
value returned by `count.getAndIncrement()` / `getAndDecrement()` to decide
whether to signal `notEmpty` / `notFull`. Corrupt that count and a consumer waits
on `notEmpty` forever with elements already in the queue. The test blocks in
`take()` — the very method the intrinsic captured.

That also explains the shape that looked so confusing: the other 19 tests in the
class get **faster** (~20 s against ~70 s) while this one wedges. Nothing is
generally slow; one counter is being corrupted.

## Not GC / not safepoints

The competing theory was that inlining removed a loop's only safepoint poll and
wedged an STW. Both halves are false:

- Same class on an 8 GB heap, intrinsic ON: still hangs (no `@@RESULT`).
  Intrinsic OFF, same heap: `ok=20 failed=0` in 50.8 s. Heap pressure is not the
  variable.
- `jit_safepoint_polls_enabled()` is **on by default** (unset → enabled), and
  back-edge polls are emitted at ten sites including conditional branches. Also
  `Math.sqrt`/`floor`/`ceil` are inlined with the identical no-context,
  no-transition shape and wedge nothing.

## What a real fix requires

Not a tweak to this intrinsic. Either:

1. **Unify the domain** — make the native RMW path use a hardware atomic on the
   same 4 bytes rather than `with_cas_lock` + stripe lock, so compiled and
   interpreted updaters are atomic against each other. This is the fix that keeps
   the 24x, and it is a change to how volatile int fields are represented and
   accessed VM-wide, including what the collector's stripe lock is protecting.
2. **Take the same locks in compiled code** — sound, but it reintroduces the cost
   the intrinsic exists to remove.

Anything narrower (restricting the intrinsic to "safe" classes) is a guess about
which counters are only ever touched from compiled code, which nothing can
establish statically.

## The other thing this cost, worth keeping

The intrinsic was inert for a long time for an unrelated reason:
`compile_osr_artifact` reaches `x64::compile_with_param_slots` **directly** and
keeps its OWN copy of the direct-call ladder, so an intrinsic registered only in
`jit::try_compile_inner` is invisible to any method promoted by OSR — which is
exactly the shape (a counter loop inside one method) this family targets.
Registered at `try_compile` only the A/B is flat (4.39 vs 4.32 M/s, noise); with
both doors wired it separates completely. Both doors now call the SAME matcher.

Relatedly: a timing loop written inside `main` is never promoted by the
invocation counter, so it measures interpreted or OSR code. The first
measurement of this work was invalid for that reason.

## The measurement (still valid, still not enough)

ABBA-interleaved in one binary, B arm = `CRATONVM_JIT_NO_ATOMIC_INTRINSIC=1`:

| arm | `getAndIncrement`, M/s | median |
|---|---|---|
| A (intrinsic) | 136.6  93.1  93.4  92.3  93.7  89.3 | **93.3** |
| B (native) | 4.5  7.4  3.8  3.9  3.7  3.8 | **3.9** |
| HotSpot JDK 25 | | 205.2 |

`AtomCheck` — all six methods, subclass receiver, megamorphic call site, null
NPE, 8x200,000 contended increments — passes on HotSpot and on both CratonVM
arms with identical checksum `270002400000`. Note what it does **not** cover, and
why it cleared a broken intrinsic: every thread in its contention test runs the
same compiled code, so all updaters are in one mode. The bug needs MIXED modes.

## Instrumentation and levers

- `CRATONVM_DBG_ATOMIC_INTRINSIC=1` — `[atomic-site] door=… holder=… callee=…`
  per admitted site, plus the layout line. The holder is the load-bearing field.
- `CRATONVM_JIT_NO_ATOMIC_INTRINSIC=1` — kill switch / B arm.
- `CRATONVM_ATOMIC_INTRINSIC_ONLY=<substr>[,…]` — admit only in holders matching,
  for bisecting a bad site.

Probes on the parked branch under `probe/`: `AtomCheck`, `AtomRate` (OSR shape),
`AtomRate2` (invocation-counter shape), `MixAtom` (mixed modes — the one that
matters). `exp.sh` runs the whole diagnosis.
