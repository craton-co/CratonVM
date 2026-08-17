# AtomicInteger `LOCK XADD` intrinsic — FIXED by unifying the atomicity domain

**Status:** FIXED. The intrinsic ships, and the native path it races is now a
hardware atomic too.

The `AtomicInteger` RMW family is a single `LOCK XADD` from compiled code (~16x
over the native, ~35x over what the native used to cost). Getting there needed
two things that are easy to mistake for one: registering the intrinsic at BOTH
compile doors, and moving the native path onto the same atomic primitive.

## What was actually wrong, in order

**1. The intrinsic was inert.** `compile_osr_artifact` reaches
`x64::compile_with_param_slots` directly and keeps its OWN copy of the
direct-call ladder, so an intrinsic registered only in `jit::try_compile_inner`
is invisible to any method promoted by OSR — exactly the shape (a counter loop
inside one method) this family targets. Registered at `try_compile` only, the
A/B is flat (4.39 vs 4.32 M/s, noise). Both doors now call the SAME matcher.

**2. Once it fired, it corrupted counters.** `AtomicInteger`'s atomicity did not
come from hardware atomics. `compare_and_swap_field` took two SOFTWARE locks —
`monitors.with_cas_lock` and the collector's `volatile_stripe_lock` (which
exists to stop the 16-byte `Value` cell tearing). Those make the native path
atomic against ITSELF, not against a hardware atomic issued from compiled code.
**"Same memory" is not "same atomicity domain."** A compiled `LOCK XADD` and an
interpreted read-compare-write on one counter interleave and lose updates:
24,908 of 600,000 in `MixAtom`.

That is why `DefaultPromiseTest.testListenerNotifyOrder` hung at JUnit's 120 s
timeout, 5 runs out of 5. The intrinsic captured
`java/util/concurrent/LinkedBlockingQueue.take` (`getAndDecrement`) and `.offer`
(`getAndIncrement`); LBQ keeps its element count in an `AtomicInteger` and uses
the returned value to decide whether to signal `notEmpty`/`notFull`. Corrupt the
count and a consumer blocks forever with elements already queued — and the test
blocks in `listeners.take()`, the captured method. It also explains the shape
that looked so strange: the other 19 tests in the class got *faster* while that
one wedged. Nothing was generally slow; one counter was being corrupted.

## The fix

Both sides go through ONE aligned 4-byte hardware atomic at the field payload:

- `atomic_fetch_add_int` — the VM override the trait's default impl had asked
  for in a comment since it was written ("should map this to a single LOCK
  XADD"), never implemented, so every increment ran a CAS retry loop under two
  locks.
- `compare_and_swap_field` — the `int`→`int` case, because
  `AtomicInteger.compareAndSet` must be atomic against a compiled XADD as well.
  Routing only `fetch_add` would have left the same race under another name.

The address comes from `cratonvm_jit::AtomicIntFieldLayout`, the same type the
JIT bakes its displacement from, with the COMPACT/LEGACY arm chosen per OBJECT
on `GC_FLAG_COMPACT` exactly as the codegen does — so the two cannot drift.
Anything unprovable (non-`int` slot, array, unresolvable layout, misalignment)
returns `None` and keeps the lock-based path; misalignment is refused rather
than degraded, because that would be a torn access, not a slow one.

## Results

| `getAndIncrement` | M/s |
|---|---|
| native, lock-based (before) | 3.9 |
| native, hardware (now the fallback) | 8.5 |
| JIT intrinsic | 136.4 |
| HotSpot JDK 25 | 205.2 |

The native path alone got 2.2x faster; the intrinsic is 16x on top of that.

- `MixAtom` (mixed compiled/interpreted): exact, 600,000 of 600,000, 4 runs,
  with `[atomic-intrinsic]` confirming the intrinsic fired — witnessed, not
  assumed.
- `AtomCheck`: PASS, semantics checksum `270002400000`, same as HotSpot.
- `DefaultPromiseTest`: 20/20 three times at 39–46 s, against a 120 s hang
  before. Faster than the intrinsic-off control (48 s), because the hardware CAS
  speeds up the native path too.
- `wave4_a_atomic` 5/5, `wave2_chm` 1/1, `cratonvm-vm --lib` 2504/0,
  `cratonvm-jit` all green.
- netty batch 12/13: no regressions; `DefaultPromiseTest` now green and
  `JfrEventSafeTest` improved 2 failures to 1.

## Two traps this cost, worth not repeating

**A negative from an unwitnessed probe is not a ruled-out hypothesis.** The
first `MixAtom` run reported no loss and was written up here as ruling out the
lost-update theory. Its setup — that `CRATONVM_JIT_DENY` really kept one loop
interpreted — was never verified, and a run where both threads were compiled
looks identical to a pass. Hours went into safepoint and GC theories that were
all wrong. Every probe asserting an absence needs a positive witness that its
precondition held, printed in the same run, and able to tell the arms apart: a
callee-only line ("getAndIncrement fired") cannot, a holder line
(`holder=MixAtom.jitLoop`) can.

**A single-mode contention test cannot catch a mixed-mode atomicity bug.**
`AtomCheck`'s 8-thread, 1,600,000-increment test passed the broken intrinsic
every time, because all eight threads ran the same compiled loop and were
therefore in one domain. The bug needs MIXED modes by construction.

## Instrumentation

- `CRATONVM_DBG_ATOMIC_INTRINSIC=1` — prints each admitted site with class id
  and both offsets.
- `CRATONVM_JIT_NO_ATOMIC_INTRINSIC=1` — kill switch and B arm. Note it now
  disables only the JIT fast path; the native path stays hardware-atomic.

Probes: `AtomCheck` (correctness matrix), `AtomRate` / `AtomRate2` (OSR and
invocation-counter shapes), `MixAtom` (mixed modes — the one that matters).
