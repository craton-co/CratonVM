# `getstatic` costs a helper call — ~35 ns where HotSpot pays ~0

**Status:** OPEN (halved 2026-07-31; the structural fix is still open).

Every `getstatic` in compiled code calls the `jit_getstatic` runtime helper.
`getfield` does not — it is inlined. HotSpot constant-folds a `static final`
reference to its address and emits a plain load.

## Measurement

`probes/StaticFieldProbe.java` — every rung runs the identical loop body and
differs only in where the value comes from. Marginal ns/op over the call-free
control, 2M iterations:

| rung | CratonVM (before) | CratonVM (after) | HotSpot |
|---|---|---|---|
| `static final` REF | +47.19 | **+24.41** | −0.04 |
| `static final` REF, hoisted to a local | +0.58 | +0.60 | −0.04 |
| `static` mutable REF | +51.81 | **+23.37** | −0.04 |
| `static final int` | +0.19 | +0.12 | −0.06 |
| `static` mutable int | +51.96 | **+21.97** | −0.05 |
| instance field (`getfield`) | +1.15 | +0.76 | +0.02 |

Two rows are controls rather than results. `static final int` is folded to a
constant by **javac**, so it never reaches `getstatic` — it reads as free on
both VMs and proves the harness is not charging the loop for the arithmetic.
The *hoisted* row does one `getstatic` before the loop instead of one per
iteration; it is the same field and the same read, so its ~+0.6 ns is what the
value itself costs once the helper call is out of the loop.

## How this was found, and a warning about probes

It surfaced while decomposing an apparent 6x `invokevirtual` penalty. That
penalty was largely **not dispatch**: the probe called `LEAF.addOne(i)` where
`LEAF` is a `static final` field, so every iteration paid a hidden `getstatic`
helper call that the `invokestatic` rung never paid. Hoisting the receiver into
a local took `invokevirtual` from 42.03 to 9.44 ns/op.

> Any microbenchmark that reaches its receiver, or any constant, through a
> `static` field is measuring `getstatic` as well as whatever it meant to
> measure. On this VM that is tens of nanoseconds. Hoist to a local first.

With the receiver hoisted, virtual dispatch costs only ~2.7 ns more than a
direct static call (+8.02 vs +5.34 marginal) — 1.5x, not 6x. Devirtualizing
provably-monomorphic virtuals is therefore **not** where the remaining call
cost is; reproduce with `probes/VirtOnlyProbe.java`.

## Why it costs what it costs

`jit_getstatic` ran, per static read:

1. `note_jit_boundary()`;
2. `ensure_class_initialized_shared(..)` — its own fast path is an atomic, but
   reaching that atomic needs a `class_manager.read()` acquisition;
3. a **second** `class_manager.read()` + `get_class()` + a string compare of the
   class name against `"java/lang/System"`, to service the `System.out`/`err`
   bootstrap intercept — for every class, on every read;
4. `get_static_shared(..)` — a `statics.read()` acquisition plus a hashmap probe.

## Round 2 (2026-08-01) — profile first, then fix

Round 1 guessed at the remaining cost and was wrong. `CRATONVM_DBG_GETSTATIC_PROF=1`
now times each segment of the helper with `rdtsc` and reports whether the
lock-free statics index is actually hit, so the next person does not have to
guess either. Two results:

* **The `RwLock` + hash probe were NOT the bottleneck.** A lock-free
  `ClassId -> base pointer` index was built for them (`StaticsIndex`,
  `StaticsBlock`) and measured **no wall-clock change** across 4 interleaved
  pairs — while showing 4 803 264 index hits against 11 misses, so it was
  working, just not on the critical path. Kept anyway: it is the prerequisite
  for baking the address into generated code, and `CRATONVM_NO_STATICS_INDEX=1`
  turns it off.
* **The `System.out` intercept was still the dominant segment**, at 98 cyc/call
  — the very thing round 1 believed it had fixed. Resolving `java/lang/System`'s
  ClassId by NAME only helps if that lookup resolves; when it does not, every
  call still takes the `class_manager` lock and looks exactly like working code.
  Replaced with a per-`ClassId` memo (`system_class_memo`): asked at most once
  per class, then two bit tests. Segment cost 98.2 -> 20.8 cyc, which is the
  instrumentation floor (`boundary` 18.5 and `init` 19.8 measure near-trivial
  work).

> A guard whose cost is invisible from outside will be "fixed" twice. Measure
> the segment, not the function.

Wall clock, interleaved, `static mutable int` marginal: **25.4 -> 16.0 ns**
(median of 4 pairs; every pair favoured the memo).

Current state against HotSpot, 2M iterations:

| rung | CratonVM | HotSpot |
|---|---|---|
| `static` mutable int | +15.78 | +0.09 |
| `static final` REF | +26.06 | +0.04 |
| `static` mutable REF | +27.40 | +0.04 |
| `static final` REF, hoisted | −0.56 | +0.08 |
| instance field | +0.24 | +0.08 |

Primitive statics are down from ~52 ns marginal at the start of this work to
~16. Reference statics remain dearer than primitives (the `Value::Object` arm
adds a `plausible_heap_pointer` check) and are the next thing to look at.

**A hard floor worth knowing**: a bare direct call costs ~5 ns on this VM
(`probes/VirtOnlyProbe.java`, `invokestatic` marginal). No amount of trimming
inside the helper can beat that. HotSpot's ~1 ns is only reachable by emitting
the load inline, with no call at all.

## What was fixed in round 1 (halved)

* **(3) resolved once.** `java/lang/System`'s `ClassId` is cached in an atomic
  and compared as an integer, so every other class touches no lock and does no
  string compare. Interleaved A/B, 3 pairs: 52.74/59.68/46.45 → 36.78/36.45/29.40.
* **(2) memoized.** Class initialization is monotonic (JVMS §5.5; redefinition
  does not re-run `<clinit>`), so "already initialized" is cached in a lock-free
  bitmap indexed by `ClassId` — one relaxed load and a bit test in steady state.
  Only a *confirmed success* is memoized, so a failed `<clinit>` still re-throws
  through the real check. Ids beyond the bitmap take the slow path, making its
  capacity a performance bound rather than a correctness one.

Measure interleaved, several times. A single-shot A/B of the first fix showed it
doing nothing, because the control had drifted 1.74 → 3.04 ns between runs on a
shared host; the repeat showed a consistent ~30%.

## What is still open (the other half)

Item (4) remains: a lock plus a hashmap probe per read. The real fix is to stop
calling a helper at all and emit a direct load, as HotSpot does. `x64.rs`'s own
bail comment on the `0xb2` handler names the blockers:

* static slot addresses are not stable (the backing `Vec` resizes; the map entry
  is created lazily);
* the `jit` crate has no `vm` dependency, so it cannot resolve a slot address at
  compile time.

Both look solvable — a resolver callback supplied at compile time (the same
shape as `callee_compiler` / `cp_invokespecial_owner_resolver`), plus stable
per-class statics storage so the address can be baked. A class-init barrier is
still needed when the class is not yet initialized at compile time; when it
already is, the barrier can be omitted entirely.

## Reproduction

```
<cratonvm> --java-home <jdk25> -cp <probes> StaticFieldProbe 2000000
<jdk25>/bin/java -cp <probes> StaticFieldProbe 2000000
```
