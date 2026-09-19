# Synthetic-JDK mode keeps the `f64` `BigDecimal.compareTo`, and its own doc calls that body dead

**Found:** 2026-09-17, by `registrar_drift::no_new_mode_drift` going red on a
merge of `origin/dev` into `fix/jit-review-r7-20260917`.
**Status:** RESOLVED later on 2026-09-17 -- see *Resolution* at the end. The
drift rows (`1273 -> 1275`) correctly remain, because both registrations do.
The filename is kept because two gate docs cite this record by path.
**Not a regression from the merge.** `65c030c47` is correct in what it did; this
is the consequence it did not enumerate.

## What is registered

`65c030c47` ("add native MathContext BigDecimal arithmetic") added exact natives
to `register_bigdecimal_arithmetic_overrides` for two triples that
`register_bigdecimal_natives` already registered:

| triple | shipping pass binds | synthetic-only pass binds |
| --- | --- | --- |
| `java/math/BigDecimal.compareTo(Ljava/math/BigDecimal;)I` | `native_bd_compare_to_exact` | `native_bd_compare_to` |
| `java/math/BigDecimal.divide(Ljava/math/BigDecimal;)Ljava/math/BigDecimal;` | `native_bd_divide_exact` | `native_bd_divide` |

`register()` is last-write-wins and `register_synthetic_overrides` runs last, so
in a `--features synthetic-jdk` build the synthetic bodies hold both slots. In
every shipping mode the exact ones do.

## Why it matters

The two bodies do not agree. `native_bd_compare_to_exact` compares
`(unscaled, scale)` pairs as `BigInt`, with an adjusted-exponent shortcut that
bounds the one rescale it performs by the operands' digit counts.
`native_bd_compare_to` does this:

```rust
let a: f64 = bd_read_unchecked(ctx, this).parse().unwrap_or(0.0);
let b: f64 = bd_read_unchecked(ctx, other).parse().unwrap_or(0.0);
```

Two `BigDecimal`s agreeing in their leading ~17 significant digits parse to the
same `f64` and compare equal. That is not a hypothetical shape: it is what a
Newton-Raphson bisection compares near convergence, which is the exact workload
`65c030c47` was written to fix. `divide` splits the same way.

So the correctness half of that commit does not reach synthetic-JDK mode, and
any measurement taken under `--features synthetic-jdk` is a measurement of the
copy that does not ship. This is the general hazard `registrar_drift`'s panic
text describes; this is one live instance of it.

## The comment that will mislead the next reader

`native_bd_compare_to_exact`'s doc comment calls `native_bd_compare_to`
"dead-for-real-JDK-mode". The hyphenated qualifier is accurate, but the sentence
reads as "dead", and it is not dead — it is the live body in synthetic mode. Left
as-is rather than edited here, because the same commit's author owns that file
and may prefer to close the defect instead of re-wording around it. If the defect
is closed by deletion the comment goes with it.

## What a fix has to prove first

The obvious move — delete the two synthetic-only `register` calls so the exact
bodies serve both modes — is plausible but **not proven**, and `F34-1` §5 is the
precedent for why that class of deletion is not automatically right.

`bd_unscaled_bigint` does have a synthetic arm: when `bd_layout(ctx)` returns
`None` it falls back to `BD_FIELD_VALUE`, reads the decimal string and rebuilds
`(BigInt::from_decimal(s.replace('.', "")), bd_scale_of(...))`. So the exact body
is *written* to be layout-agnostic. What is unverified is whether `bd_scale_of`'s
synthetic answer agrees with the fractional-digit count of that string for every
value the synthetic constructors can produce — if it does not, the reconstructed
unscaled/scale pair is wrong by a power of ten, which is far worse than the
`f64` imprecision it replaces.

Before removing anything: build `--features synthetic-jdk`, take
`--dump-native-registry`, and confirm from `owns_slot` that the shipping body
holds both triples in **both** modes, then exercise `compareTo` and `divide`
against synthetic-constructed values across the compact/inflated boundary.

## Related

* `docs/known-issues/jdk-only/synthetic-jdk-feature-shadows-real-classes-in-real-jdk-mode-20260917.md`
  — the converse direction of the same two-registry hazard.
* `docs/known-issues/jdk-only/synthetic-jdk-tests-fifteen-failures-after-the-compile-blackout-20260917.md`
  — the synthetic-jdk suite is red, so a green run there is not currently
  available as evidence either way.

## Resolution (2026-09-17)

Fixed. The `f64` bodies are gone: synthetic `compareTo` and `divide` now read
their operands through `bd_unscaled_bigint` and run the shipping cores --
`bd_compare_unscaled` and `bd_exact_divide_core` -- so the two modes agree, and
synthetic `divide` throws for `1/3` (a defect this record did not originally
name: the `f64` body RETURNED `0.3333333333333333` where `divide(BigDecimal)`
must throw `ArithmeticException`).

This record's "What a fix has to prove first" was right to be cautious about
`bd_unscaled_bigint`'s synthetic arm, and it did have one real defect:
`BigInt::from_decimal` skips every non-digit, so `"1.5E+10"` read back as the
integer 1510. `bd_parse_decimal_str` now parses scientific strings exactly, into
the `(unscaled, scale)` the JDK constructor builds -- and ONLY those.

### A correction to this record's own first resolution

The first version of this fix parsed EVERY synthetic string exactly and ignored
the stored scale, on a theory that a negative scale rendered as `"150"` would
read back ten times too large. That theory was never verified, and it was wrong:
the synthetic layout's convention -- which every producer follows -- is that the
string's DIGITS (ignoring any `.`) are the unscaled value and the scale lives in
`BD_FIELD_SCALE`. Rendered positive scales satisfy it; negative scales are stored
as bare digits and never rendered; and a scale no string could hold is stored as
`"1"` with the scale beside it. The only producer that ever rendered a negative
scale was that first version's own new `divide`.

Ignoring the stored scale broke `vm::tests::bigdecimal_extreme_scale_refusals_f31`:
`new BigDecimal(ONE, Integer.MIN_VALUE)` read back as 1, where HotSpot's
`intValue()` is 0. It was caught by the full synthetic-jdk `vm::tests` run and
fixed by narrowing the parser to scientific notation and making `divide` store
its result by the convention (rendered when scale >= 0, bare digits when < 0).

### Verified

`math_bignum::synthetic_bigdecimal_exact_tests` (5) pins the parser against the
JDK constructor and both defects. `cratonvm-native-builtins`: 4275 / 0 default,
4452 / 0 under `--features synthetic-jdk`. `cratonvm-vm --features
synthetic-jdk`: 4209 / 0, f31 included.
