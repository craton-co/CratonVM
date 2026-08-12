# FastThreadLocalTest — a 2.1-billion-iteration loop against a per-helper-call floor

**Status:** OPEN — **throughput, not correctness** (2026-08-12). The correctness
half of this class was fixed earlier the same day (see
[jit-elided-constructor-side-effects-20260812.md](jit-elided-constructor-side-effects-20260812.md));
what remains cannot be closed by a targeted fix and is recorded here with the
numbers that size it.

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
