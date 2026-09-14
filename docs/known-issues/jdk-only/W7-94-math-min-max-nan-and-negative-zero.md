# W7-94 — `Math.min`/`max` violated the JLS for NaN and `-0.0`

**Status: FIXED 2026-08-12. Found by running a probe, not by reading a record.**
**Both shipping modes were affected. Not `--jdk-only`-specific.**

> **RETIRED 2026-08-12 (lane B8) — independently re-run against the shipping
> binary, by raw bits.** The fix was verified by its author; this is a second
> measurement by a lane that did not write it, on
> `/c/craton/jdkonly-wave2-target/release/cratonvm.exe --jdk-only` vs Temurin
> `jdk-25.0.3.9-hotspot`, with every value compared as
> `Double.doubleToRawLongBits` — the comparison this record itself says is the
> only valid one:
>
> ```
> Math.min_1_NaN=9221120237041090560 (NaN)
> Math.max_1_NaN=9221120237041090560 (NaN)
> Math.min_-0_0=-9223372036854775808 (-0.0)
> Math.max_-0_0=0 (0.0)
> StrictMath.min_1_NaN=9221120237041090560 (NaN)
> StrictMath.max_-0_0=0 (0.0)
> ```
>
> All six bit-identical to HotSpot, inside a 29-value diff that came back
> `IDENTICAL` (the rest is W7-54's `StrictMath` family). Note
> `Math.min(-0.0, 0.0)` = `0x8000000000000000` and `Math.max(-0.0, 0.0)` = `0`:
> the two differ in the sign bit only, which is precisely the distinction an
> `==` assertion cannot see and this record warned about. Retired.

## The measurement

HotSpot 25.0.3+9 as oracle, same host, same class file, `--real-jdk`:

```
                          HotSpot   CratonVM (before)
Math.min(1.0, NaN)        NaN       1.0
Math.max(1.0, NaN)        NaN       1.0
Math.min(-0.0, 0.0)       -0.0      +0.0
Math.min(1.0f, NaN)       NaN       1.0
StrictMath.min(1.0, NaN)  NaN       1.0
Double.min(1.0, NaN)      NaN       1.0
Math.min(1, 2)            1         1        <- correct
Math.min(1L, 2L)          1         1        <- correct
```

`-0.0` compared with `Double.doubleToRawLongBits`, never `==` — `-0.0 == 0.0`
is `true`, so an equality-shaped check passes against this defect. Any test
written the obvious way would have been vacuous.

## Cause

Two wrong primitives in four bodies:

* **Rust's `f64::min`/`f32::min` are IEEE `minNum`** — they return the
  **non**-NaN operand. Java propagates NaN.
* **`<` cannot see the sign of a zero.** Java orders `-0.0` strictly below
  `+0.0`.

## Reach — larger than the four bodies

* `register_math_natives` is called for **both** `java/lang/Math` and
  `java/lang/StrictMath`, so four bodies were **eight** wrong triples.
* `Float.min`/`max` inherit these with **no registration of their own**.
* `Double.min`/`max` is a **third copy** with its own body, equally wrong. JDK
  25's `Double.min` is literally `return Math.min(a, b);`, so sharing one tree
  is a theorem here, not a convenience.

## Why nothing caught it — three independent reasons, each worth keeping

**1. The census is blind to this species by construction.** The registrar opens
with `set_category(NativeKind::Intrinsic)`, ambient over the whole function.
`Intrinsic` is exempt from shadow retirement **and** is not the
`native-shadows-bytecode` kind. A `--jdk-only --jdk-only-report` run of a
program calling `Math.min` four times yields **zero** `java/lang/Math` rows.
The best instrument this campaign built could not see it.

**2. The pass/fail boundary is per *descriptor*.** `min(II)I` and `min(JJ)J`
were correct throughout while `(DD)D` and `(FF)F` were wrong. Every screen in
this campaign reports at class or method granularity; none reports per
descriptor. Same class, same method name, one signature passing and another
failing.

**3. The correct implementation already existed, with one caller.**
`phases_late::streams::p56_java_math_min`/`_max` implement exactly the right
tree, and their doc comment *describes this exact trap*. They were written for
one stream fold. The four natives that every `Math`/`StrictMath` caller in the
process goes through kept the Rust form.

That third point is the **fix-the-positive-half-and-leave-the-twin** shape, and
this is its fourth confirmed instance in a single campaign — after
`ProcessBuilder.start` vs `Runtime.exec`, `Function.*` vs
`Predicate`/`Consumer`, and `aio_completed_future` applied to three call sites
and none of the four `FutureTask` mints beside them. **When a correct helper
exists, the question is never "is it right" but "how many callers does it
have".**

## The fix

Four `#[inline(always)]` helpers in `native-builtins/src/lang_math.rs`
(`java_math_{min,max}_{f64,f32}`), NaN-propagating and signed-zero-aware, used
by the four `Math`/`StrictMath` bodies and by `Double.min`/`max` in
`phases_early.rs`. `set_leaf(true)` is in force over the block; the helpers are
branch-only, allocate nothing, and take no `ctx`, so nothing about propagation
changes.

`DoubleSummaryStatistics.accept` becomes correct with no edit of its own — it
was reported as a separate residual (`min=1.0, max=3.0` beside `sum=NaN`:
three fields that cannot have come from the same data) and is closed from
below.

## Residuals

* **`DoubleStream.min()`/`max()` are separately wrong for NaN**
  (`OptionalDouble[1.0]` where HotSpot answers `OptionalDouble[NaN]`). Distinct
  natives in `native-collections`; not covered by this fix.
* Suite cover is being added to `RJdkStrictMath` (already in `CORE_CLASSES`,
  and it had **no** `Math.min`/`max` assertions at all) and to `RJdkViews`'
  primitive-stream section. Until those land, this fix has no scheduled guard.
* The general hazard stands: **an ambient-`Intrinsic` block is invisible to the
  shadow census.** How many other wrong bodies sit behind that tag is unknown,
  and a census of `Intrinsic`-tagged natives against HotSpot is the obvious
  next instrument. `Intrinsic` is meant to mean "an accelerated implementation
  of identical semantics"; nothing checks the second half of that claim.
