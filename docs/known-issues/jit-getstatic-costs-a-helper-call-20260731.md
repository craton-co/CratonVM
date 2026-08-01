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

## What was fixed (halved)

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
