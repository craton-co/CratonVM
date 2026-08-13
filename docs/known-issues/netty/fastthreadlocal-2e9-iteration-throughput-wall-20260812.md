# FastThreadLocalTest — a 2.1-billion-iteration loop against a per-helper-call floor

**Status:** OPEN — **throughput, not correctness** (2026-08-12; **re-measured
2026-08-13**, see "Re-measured" below — every rate in the original table has
moved, two of them by more than an order of magnitude, and the attribution has
changed). The correctness half of this class was fixed on 2026-08-12 (see
[jit-elided-constructor-side-effects-FIXED-20260812.md](../../internal/fixed-suite-bugs/netty/jit-elided-constructor-side-effects-FIXED-20260812.md),
retired 2026-08-13); what remains is a performance workstream, recorded here
with the numbers that size it.

## What the test does

`io.netty.util.concurrent.FastThreadLocalTest.testConstructionWithIndex`:

```java
int ARRAY_LIST_CAPACITY_MAX_SIZE = Integer.MAX_VALUE - 8;
...
while (nextIndex.get() < ARRAY_LIST_CAPACITY_MAX_SIZE) {
    new FastThreadLocal<Boolean>();
}
```

`FastThreadLocal()` is `index = InternalThreadLocalMap.nextVariableIndex()`,
whose body is `nextIndex.getAndIncrement()` plus a bounds check. So the loop is
**~2.147 billion iterations of (allocate one object + one atomic increment)**,
by construction — there is no shortcut and no way to shorten it from the VM
side.

HotSpot JDK 25 runs the whole class in ~50 s.

## The correctness half — already fixed

Before `fix/jit-ctor-side-effects-20260812`, the JIT elided the non-escaping
`new FastThreadLocal<Boolean>()` **together with its constructor's write to the
static `nextIndex`**, so the loop condition never advanced and the loop could
never terminate: 1 000 000 constructor calls advanced `nextIndex` by 11 000.
That is fixed — the counter now advances exactly (1 000 000 of 1 000 000
observed) and the loop is finite. It is simply very long.

## The throughput half — measured, not estimated

Steady-state rates after warm-up, 20 M iterations each, same host, same
classpath, JDK 25, real-jdk mode. The right-hand column extrapolates each loop
to the 2 147 483 639 iterations the test actually performs:

| loop | HotSpot | CratonVM | ratio | CratonVM, full loop |
|---|---|---|---|---|
| `new FastThreadLocal<Boolean>()` | 162 567 323/s | **856 302/s** | **190x** | **2 508 s** |
| `nextIndex.getAndIncrement()` alone | 93 769 913/s | 4 099 127/s | 23x | 534 s |
| `new Object()` alone | 243 980 781/s | 5 494 469/s | 44x | 410 s |

Two things this says:

* **The floor is already over the cap.** Even if the constructor cost collapsed
  to nothing, the *components* alone need 410 s (allocation) and 534 s (atomics)
  for this iteration count. The netty suite's per-class wall is 180 s (240 s in
  the ad-hoc runner). Nothing short of a ~10-15x improvement in BOTH allocation
  and atomic throughput brings this class inside it.
* **The combination is worse than its parts.** 856 302/s against the ~2.35 M/s
  the two component rates predict if they simply serialised — a further 2.7x
  that is specific to the constructor path (the `<init>` call is not inlined, so
  each iteration pays the call in addition to the allocation and the atomic).

~190 ns per allocation and ~250 ns per atomic increment are both consistent with
**one helper/native call each**: `AtomicInteger.getAndIncrement` is a registered
native (`native-builtins/src/phases_early.rs`), and in-tree measurements put a
native call at ~120 ns. Compiled code is paying a call per allocation and per
atomic where HotSpot emits an inline TLAB bump and a `lock xadd`.

## Re-measured 2026-08-13 — every rate above has moved

`probes/CtorShapeRateProbe.java` (`Rate2`: each loop in its own method, so the
measured body is compiled and not interpreted) and
`probes/CtorCallSiteRateProbe.java` (`Rate3`: the constructor site in an
ordinarily invocation-compiled method rather than an OSR'd loop). Same host,
JDK 25, real-jdk mode, 10 M iterations after warm-up. Left-to-right: HotSpot,
`dev` `ae2e1d9c8`, and `dev` + the constructor direct-call change described
below.

| loop (ns/op) | HotSpot | dev | +ctor direct call |
|---|---|---|---|
| `new X()` where `X()` is `i = ATOMIC.getAndIncrement()` | 6.2 | 916.6 | 999.7 |
| `new X()` where `X()` is `i = ++staticInt` | 1.7 | 311.8 | 361.1 |
| `new X(v)` where `X(int v)` is `i = v` | 2.0 | 749.0 | 752.3 |
| `new X()` where `X()` is empty (elidable) | 1.5 | 104.0 | 79.4 |
| `new Object()` | 1.5 | 98.9 | 78.6 |
| `nextIndex.getAndIncrement()` alone | 4.7 | 5.7 | 5.0 |
| **ordinary call site**: `mkAtomic()` (Rate3) | 6.8 | 875.7 | **627.2** |
| **ordinary call site**: `mkPlain()` (Rate3) | 2.2 | 277.3 | **227.5** |

Extrapolated to the 2 147 483 639 iterations the test performs:

| component | 2026-08-12 | 2026-08-13 |
|---|---|---|
| the atomic alone | 534 s | **11 s** |
| the allocation alone | 410 s | **169 s** |
| the whole constructor loop | 2 508 s | ~2 100 s |

**Two of this page's three recommendations have moved, and the ordering has
changed.**

* **Recommendation 1 (atomic intrinsics) is DONE.** `AtomicIntGetAndIncrement`
  is an intrinsic in the JIT's ladder now
  ([record](../../internal/fixed-suite-bugs/netty/atomicinteger-lock-xadd-intrinsic-20260812-FIXED.md)):
  4.1 M/s → **200 M/s**, i.e. **49x**, and within 1.2x of HotSpot. The atomic
  half of this loop is 11 seconds, not 534.
* **Recommendation 2 (allocation) improved 2.3x** without being worked on
  directly — 5.5 M/s → 12.7 M/s, 410 s → 169 s. It is now *under* the 180 s
  per-class wall on its own, so **the sentence "the floor is already over the
  cap" above is no longer true**: allocation plus atomic is ~84 ns/op, i.e.
  ~180 s, right at it.
* **Recommendation 3 (the constructor call) is now the whole gap.** ~915 ns of
  the ~1000 ns per iteration is neither the allocation (79 ns) nor the atomic
  (5 ns) — it is the `<init>` dispatch round trip.
  `CRATONVM_DBG_JIT_SCAN_PROF=1` reports `jit_entries` exactly equal to the
  iteration count, which is the tell: one JIT boundary crossing per `new`.

### What landed for it, and the blocker that remains

`jit/src/lib.rs`'s IR (optimizing) tier refused to bind a constructor site
directly — `!is_ctor` in the direct-call gate — where the single-pass backend
has always bound `matches!(invoke_kind, 1 | 3)` without that exclusion. That
exclusion is gone; the two backends now agree. It is worth **1.40x** on
`mkAtomic` and **1.22x** on `mkPlain` in the default configuration (Rate3
above), and 1.64x on `ctorAtomic` with `CRATONVM_BG_COMPILE=0`.

It is worth **nothing on an OSR'd loop**, and the reason is specific and
actionable. `CRATONVM_DBG_JITC=1` on the Rate2 loops shows the order:

```
OSR-compile Rate2.l1(I)J entry_pc=6 …
full-compile Rate2$CtorAtomic.<init>()V …        <-- AFTER its caller
OSR-reuse   Rate2.l1(I)J entry_pc=6 …            <-- forever
```

The background compile worker deliberately passes a **lookup-only** callee
resolver (`direct_callee_lookup` in `vm/src/runtime/interpreter/jit_bridge.rs`,
whose comment reasons that "the tiered manager compiles leaves before their
callers because leaves reach the invocation threshold first"). For an **OSR**
compile that premise is false: a loop trips the OSR threshold during its *first*
invocation, before the callee it calls has accumulated any call count at all.
The caller's artifact is then cached and reused, so the site never gets a second
chance. Closing this needs either a recompile trigger when a bound-able callee
appears, or a late-binding cache at the site — neither is a one-line change, and
neither is measured here.

`new X(){}` (empty constructor) still tracks `new Object()` to within a
nanosecond in both columns, so the scalar-replacement/elision path this page's
predecessor narrowed is unaffected.

### Ground truth

Run with a 3000-second cap (50 minutes) on the fixed binary: **still `rc=124`**
— it does not finish. That is worse than the 2 508 s the microbenchmark
predicts, which is expected: 2.1 billion immediately-dead allocations against a
1500 m heap pay a GC cost that a 20 M-iteration steady-state probe does not
show. No `@@TESTFAIL` was produced, i.e. nothing has failed — it is still
running.

## A hypothesis that looked right and was not

`jit/src/x64/driver.rs` gates the inline-TLAB allocation path on
`(!has_prim_init && !has_finalizer)`, and the interpreter compile door
hard-codes those flags conservatively:

```rust
new_info.push((pc_new, target_id.as_u32(), num_fields, true, true));
// "conservative true/true here so the JIT goes through the post-init helper...
//  A follow-up should extract the real flags from class metadata to enable the skip path."
```

That reads exactly like the cause of ~190 ns allocations, and there is an
in-tree TODO asking for it to be fixed. **It is not the cause.** Setting
`CRATONVM_JIT_ENABLE_INLINE_NEW=1`, which bypasses that gate, moves nothing:

| | alloc rate |
|---|---|
| default | 5 243 769/s |
| `CRATONVM_JIT_ENABLE_INLINE_NEW=1` | 5 277 311/s |

Measure before implementing this one — the TODO is real but it is not what this
loop is paying for.

## What would actually be needed

Not a bug fix; a performance workstream, roughly in payoff order:

1. **Atomic intrinsics.** Emit `lock xadd` for
   `AtomicInteger.getAndIncrement`/`getAndAdd` (and the `Unsafe.getAndAddInt`
   underneath) instead of dispatching to the registered native. The JIT already
   has an intrinsic ladder (`Math.sqrt`, `Integer` bit ops, `arraycopy`) to hang
   this on; the work is the invokevirtual receiver guard plus the field-offset
   resolution. ~534 s → seconds for the atomic half.
2. **Allocation that does not call a helper.** Find why the inline TLAB path
   costs ~190 ns per object even when forced, since the gating flags are not it.
3. **Constructor inlining.** Would recover the extra 2.7x and, once the
   constructor body is inlined, lets IR-level escape analysis scalar-replace the
   allocation while KEEPING the atomic side effect — which is precisely what C2
   does here and why HotSpot's constructor loop (162 M/s) outruns its own bare
   atomic loop (94 M/s).

## Recommendation

Leave the class as a recorded throughput gap. A `class-overrides.tsv` entry
could convert the HANG into a real result, but the required cap is >50 minutes
for one class in a 657-class suite, which is not a reasonable trade — and it
would not represent a fix.

Worth stating plainly: the other 12 tests that run in this class (HotSpot:
`found=16 started=13 ok=13 skipped=3`) are **not known to fail**. They never get
to report, because JUnit runs `testConstructionWithIndex` in the same fork and
the harness kills the fork at the wall cap before any `@@RESULT` line is
emitted. So "FastThreadLocalTest HANG" in the batch pages is one slow test
hiding twelve unknowns, not twelve failures.

Confirmed with the watchdog on the current binary: the process is RUNNING (not
parked) with the leaf frame at
`io.netty.util.concurrent.FastThreadLocalTest.testConstructionWithIndex` pc=52 —
the loop itself.

## Repro

```bash
cd apps/netty-suite-runner
echo io.netty.util.concurrent.FastThreadLocalTest > /tmp/one.txt
CV_BIN=<binary> bash run-netty-suite.sh --list /tmp/one.txt --shards 1 --timeout 3000 --out /tmp/repro
```

The rate probe used above (`FTLRate.java` — the three loops measured
separately) is the quickest way to re-check progress after any allocation or
atomic work lands.
