# Bignum long division overflowed `u64` estimating `q_hat`

**Status:** FIXED 2026-08-06. Retired from `docs/known-issues/`.

**Symptom:** an intermittent failure of
`crypto_impl::tests::rsa_cipher_roundtrip_all_paddings`:

```
panicked at native-builtins\src\crypto_impl.rs:1671:53:
attempt to multiply with overflow
```

Column 53 on that line is `base * r_hat`.

## What was wrong

`div_rem_long` implements Knuth Algorithm D. Its D3 correction loop was

```rust
while q_hat >= base || q_hat * v_top2 > base * r_hat + u_low {
    q_hat -= 1;
    r_hat += v_top;
    if r_hat >= base { break; }
}
```

which is Knuth's loop with the `rhat < b` guard moved **from in front of the
test to after the body**. That is equivalent on every iteration except the
first — and on the first, `r_hat` is only bounded by `base` on the
`u_high != v_top` branch. On the other branch `q_hat` is `base - 1` and

```
r_hat = dividend - (base-1)*v_top = u_mid + v_top
```

which reaches ~2^33 for a normalised divisor. `base * r_hat` is then
2^32 × 2^33. Debug builds panicked; **release builds wrapped**, so the
comparison answered nonsense, a due correction was skipped, and the division
returned a wrong quotient with no symptom at all. That is the more serious
half, and it is why this was not merely a flaky test.

## Finding it cost more than fixing it, and the reason is reusable

The panic names the expression. Everything after that was an attempt to
*reproduce* it, and three attempts failed:

* a 400,000-pair pseudo-random sweep of `div_rem` shapes — **0 hits**;
* a sweep shaped like the tail of extended Euclid (`u ≈ v`, quotient near 1),
  on the theory that `modinv` makes `u_high == v_top` common — **0 hits**, and
  instrumentation showed that branch never fired at all;
* the real RSA workload with a counter on `r_hat >= base` — **0 hits** over
  three full keygen/encrypt/decrypt cycles, and 11 consecutive runs of the
  failing test itself all passed.

The branch is not rare because the *inputs* are rare. It is rare because
reaching it means steering a **partial remainder several digits into the
division** onto a particular top limb — something no caller can express and no
input sweep can aim at.

**So the fix was to make it reachable.** Extracting the estimate into
`estimate_quotient_digit(u_high, u_mid, u_low, v_top, v_top2)` turns "arrange
a 2048-bit dividend whose fourth partial remainder starts with `v_top`" into
"pass five numbers". With that done, reverting the body reproduced the panic
on the first try, at the same expression:

```
panicked at native-builtins\src\crypto_impl.rs:1451:45:
attempt to multiply with overflow
```

The general move: **when a defect is unreachable from the outside, the bug
report is also telling you the code is factored wrong.** The arithmetic that
can exceed 64 bits was buried inside a 100-line loop that owned normalisation,
the digit loop, multiply-subtract and add-back. Nothing could test it. Split
out, it is a pure function of five integers.

## The fix

`estimate_quotient_digit` restores Knuth's guard to Knuth's position and
computes the products in `u128`:

```rust
while r_hat < BASE
    && u128::from(q_hat) * u128::from(v_top2)
        > u128::from(BASE) * u128::from(r_hat) + u128::from(u_low)
{
    q_hat -= 1;
    r_hat += v_top;
}
```

The guard alone is sufficient; the `u128` means the bound does not have to be
re-derived by whoever edits this next. The `u_high == v_top` test also became
`u_high >= v_top`, the standard formulation — if the caller's invariant is ever
violated, `q_hat` is clamped to `base - 1` instead of overflowing the `u32`
quotient digit it is about to be stored in.

## Verification

Four new tests, all of which fail on the pre-fix body:

* `qhat_estimate_is_exact_or_one_high_without_overflowing` — 900 combinations
  across three normalised `v_top` values and the extremes of `u_mid`, `u_low`,
  `v_top2`, with `u_high` on and around `v_top`. Each result is checked against
  the *definition* — the true digit computed in 128-bit — rather than a
  hard-coded answer: D3 promises the digit or one more, and that is exactly
  what D4/D6 are written to absorb.
* `qhat_estimate_survives_the_operands_that_panicked` — the `u_high == v_top`
  shape with `u_mid` large enough to push `r_hat` past 2^32.
* `biguint_div_rem_identity_over_many_shapes` — `u = q*v + r`, `0 <= r < v`
  over a fixed LCG across divisor/dividend limb counts 2..6 / 2..10. The
  defect reached production as an intermittent failure *because* it depended
  on a randomly generated key, so the coverage that replaces it walks a fixed
  sequence: same pairs on every run, on every machine.
* `biguint_div_rem_extreme_limb_values` — all-ones divisors, minimal
  normalised top limbs, and the shapes that drive `q_hat` to `base - 1`.

`crypto_impl` suite: 134 passed, 0 failed, including
`rsa_cipher_roundtrip_all_paddings`.

## What this one is worth keeping

**A panic message that names an expression is a complete diagnosis; treat
reproduction as a separate problem, and if reproduction keeps failing, suspect
the factoring rather than the diagnosis.** Three sweeps here returned zero
hits, and each one made the original reading look wrong. It was not wrong. It
was just describing a state that only exists in the middle of a loop nobody
can address from outside.
