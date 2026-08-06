# Bignum long division overflows `u64` estimating `q_hat`, panicking RSA in debug builds

**Status:** OPEN. Intermittent — depends on the limb values of the randomly
generated key, so it reproduces on roughly one run in ten.

**Reproducer:** `cargo test -p cratonvm-native-builtins --lib
crypto_impl::tests::rsa_cipher_roundtrip_all_paddings`, repeated.

```
thread 'crypto_impl::tests::rsa_cipher_roundtrip_all_paddings' panicked at
native-builtins\src\crypto_impl.rs:1671:53:
attempt to multiply with overflow
```

## Why it happens

`crypto_impl.rs` implements Knuth algorithm D. The quotient-digit correction
loop is:

```rust
let base: u64 = 1u64 << 32;
...
let mut q_hat = if u_high == v_top { base - 1 } else { dividend / v_top };
let mut r_hat = dividend - q_hat * v_top;
let u_low = u.limbs[j + n - 2] as u64;
while q_hat >= base || q_hat * v_top2 > base * r_hat + u_low {
    q_hat -= 1;
    r_hat += v_top;
    ...
}
```

Three products in that condition can exceed `u64::MAX`:

* `q_hat * v_top2` — `q_hat` can be as large as `base - 1` (2³²−1) and
  `v_top2` is a full 32-bit limb, so the product needs **64 bits**, which just
  fits — but the loop is entered *before* the `q_hat >= base` guard has reduced
  `q_hat`, and `q_hat` from `dividend / v_top` can exceed `base` when `v_top`
  is small. Then it does not fit.
* `base * r_hat` — `r_hat` is only bounded by `base` *after* the loop's own
  `if r_hat >= base { break; }`, so on entry `base * r_hat` can be 2³² × 2³².
* `q_hat * v_top` on the line above, for the same reason.

Debug builds panic on the overflow. **Release builds wrap silently**, which is
the more serious half: a wrapped comparison makes the correction loop take the
wrong branch and the division returns a wrong quotient, so this is not merely a
test-only defect.

## What must change

Do the estimate in `u128`, which is what the algorithm actually requires and
what removes all three overflows at once:

```rust
while q_hat >= base
    || (q_hat as u128) * (v_top2 as u128)
        > (base as u128) * (r_hat as u128) + (u_low as u128)
```

and evaluate `q_hat >= base` first so the short-circuit still bounds `q_hat`
before it is used. Check the `D4` multiply-and-subtract loop below it for the
same shape.

## Verification when fixed

Run the reproducer above 20+ times — one pass proves nothing, the failure rate
is roughly 1 in 10. Add a direct unit test for the divisor shape that triggers
it (a small leading limb `v_top` with a large `v_top2`), so the coverage does
not depend on random key generation.

## Provenance

Not mine, and not caused by the change that found it: this surfaced in a
`native-builtins --lib` run on a branch that never touched `crypto_impl.rs`
(`git log -- native-builtins/src/crypto_impl.rs` over that branch is empty),
the same test passed 6/6 on re-run there, and it passed on a pristine
`origin/dev` worktree as well. Filed rather than left as an unexplained
intermittent CI failure. Found 2026-08-06.
