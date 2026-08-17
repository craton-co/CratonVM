# `BigInteger.modPow` now reduces with Montgomery — 2.05x on the primitive, 2.09x on RSA keygen, and the estimate that did not survive contact

**Status: FIXED 2026-08-17**, on `fix/biginteger-modpow-montgomery-20260817`,
branched from `dev` at `a9b94266b`. Retires
`known-issues/perf/biginteger-modpow-has-no-montgomery-reduction-20260817.md`.

Every lever the open page named is implemented, tested, and measured:
Montgomery reduction for odd moduli, a window over the exponent, and the
`mag.len() * 32` bound replaced by the real bit length. Both limb bignums in the
tree got it — `bigint::BigInt` (behind `java.math.BigInteger.modPow`) and
`crypto_impl::BigUint` (behind the native RSA private-key path), which the open
page's `SHA256withRSA` row was measuring without saying so.

## The numbers

Same host as the open page (Windows, release `cratonvm.exe`, JDK 25 via
`--java-home`), `regression-suite/perf/probe-modpow/ModPowProbe.java`.
**Baseline is the same tree at the fork point**, built and run as its own
binary — not the open page's numbers, which were taken on a different operand
set. HotSpot 25 is the oracle.

Eight interleaved rounds (HotSpot, baseline, Montgomery, in that order, per
round) so the ambient load of a shared build machine falls on all three
equally. Medians; keygen pools all 32 keys.

| row | HotSpot 25 | baseline | Montgomery | gain | gap to HotSpot |
|---|---|---|---|---|---|
| `BigInteger.modPow`, 2048-bit, ×20 | 63 ms | 516 ms | 252 ms | **2.05x** | 8.2x → 4.0x |
| RSA-2048 keygen, per key | 96 ms | 1894 ms | 905 ms | **2.09x** | 20x → 9.4x |
| `BigInteger.probablePrime(1024)` | 29 ms | 243 ms | 157 ms | **1.55x** | 8.4x → 5.4x |
| `SHA256withRSA` sign, ×10 | 9 ms | 418 ms | 346 ms | **1.21x** | 46x → 38x |

Spreads, because two of these rows are noisy enough that a single run misleads:
`modPow` 458-601 baseline / 230-342 after; keygen **281-10515 ms baseline** per
key, 283-4646 after. Keygen variance is prime-search attempt count, not the VM.
**Do not quote a keygen figure from fewer than ~30 keys.**

## The estimate the open page gave, and why it was optimistic

The open page estimated "~3x on `modPow`". The delivered figure is 2.05x, and
the shortfall was predictable from the page's own sentence:

> the division is roughly twice the cost of the multiply it follows, so removing
> it is the ~2-3x

If a square-and-multiply step costs `mul + div` and `div ≈ 2·mul`, the step is
`3·mul` before and `2·mul` after (CIOS is two interleaved limb passes). That is
**1.5x, not 2-3x** — the same premise, carried one step further, gives a
different answer. The window supplies the rest: at a 2048-bit exponent it cuts
~3072 multiplies to ~2470, another ~1.24x. 1.5 × 1.24 ≈ 1.9x, against 2.05x
measured. The model is right; the open page's reading of it was not.

The lesson for the next page in this area: state the step cost before and after
in the same unit, and divide. An intuition about "the big one" is not a ratio.

## What was built

`native-builtins/src/montgomery.rs` — new, and deliberately **type-free**: it
operates on `&[u32]` limb slices, so both bignums share one implementation, one
set of invariants, and one differential test surface. Nothing in it knows about
signs or allocates a `BigInt`/`BigUint`.

* `Montgomery::new` **returns `Option`** and rejects zero, one, and **even**
  moduli rather than documenting oddness as a precondition and trusting callers.
  An even modulus has no Montgomery form at all (`gcd(2^32k, m) != 1`), and both
  callers fall back to their division path for it. This is the one place the
  rewrite could have silently produced wrong answers, so it is a type-level
  refusal, not a comment.
* `Montgomery::mul` is Koc's CIOS. The invariant `t < 2m` is stated at the site
  with its derivation, because it is what licenses the *single* conditional
  subtraction at the end and what makes the overflow word provably 0 or 1.
* `modpow_odd` is a fixed left-to-right window.

### The window widths are not the JDK's, on purpose

The obvious move is to copy `BigInteger.oddModPow`'s thresholds. That would be
wrong here: the JDK's table holds only **odd** powers for a *sliding* window
(`2^(w-1)` entries), and this is a full fixed-window table (`2^w` entries), so
the break-even sits at a different exponent length. Setting
`(2^w - 2) + E/w` equal for `w` and `w+1` puts the crossover at
`E = 2^w · w · (w+1)` — 4, 24, 96, 320, 960, 2688. At a 2048-bit exponent that
picks `w = 6`; the JDK's threshold would have picked `w = 7`, whose 64 extra
table entries cost more than the 48 multiplies they save. Copying the constants
would have been a ~4% regression wearing the JDK's authority.

### `crypto_impl::BigUint` keeps its ladder

`BigInt::modpow` (public `java.math.BigInteger.modPow`) is windowed, matching
what HotSpot itself does there. `BigUint::modpow` is **not**, and that is
deliberate: it runs RSA/DSA *private-key* operations, a window indexes its table
with secret exponent bits, and the existing routine was already shaped as a
regular ladder for that reason (see the VULN(2) note). Montgomery reduction was
dropped in underneath the ladder, leaving the one-multiply-one-square-per-bit
operation sequence exactly as it was. This is why the two arms earn different
ratios — 2.05x with the window, 1.34x without it — and the difference is the
price of not regressing a side-channel property to win a benchmark row. It is
not an oversight to be "fixed" later.

## Why the `SHA256withRSA` row barely moved, and what it is really bound by

1.21x looked like a failure of the change until it was attributed rather than
guessed at. `crypto_impl::tests::rsa_sign_cost_attribution` (kept, `#[ignore]`d,
`--nocapture`) breaks a 2048-bit signature down:

| RSA-2048 `sign_sha256`, ×10 | baseline | Montgomery |
|---|---|---|
| whole signature | 300.6 ms | 206.2 ms |
| secret-exponent modpow | 277.9 ms | 207.9 ms |
| `r.modinv(n)` (blinding) | 7.0 ms | 5.9 ms |
| `r^e mod n` (blinding) | 2.3 ms | 1.7 ms |

The signature **is** the modpow — blinding is ~3% — and the native modpow did
improve, by 1.34x, exactly the ladder-without-window figure. The Java-level row
reads 1.21x because ~140 ms per ten signatures is JCA/VM overhead outside the
primitive (`Signature.getInstance`, `initSign`, key marshalling).

So the remaining 38x on that row is **not** a missing Montgomery reduction. It
is that `Rsa::sign_sha256` exponentiates with the full-width `d` while HotSpot
uses CRT — two half-width exponentiations, ~4x cheaper — even though the CRT
parameters are already present and validated on every generated key
(`rsa_generated_key_is_crt`). That is a different defect with a different name,
and it is filed as
`known-issues/perf/rsa-private-key-op-ignores-the-crt-parameters-it-already-has-20260817.md`
rather than ridden in here. The open page made exactly this call about
Montgomery itself, and it was the right one.

## The differential net

The open page's stated bar was "an exhaustive differential test against the
existing limb implementation and against HotSpot across operand sizes, odd and
even moduli, and the negative-exponent path", because the failure mode is a
signature that is wrong but plausible. What exists:

* `montgomery::tests` (8) — `neg_inv` against every odd word shape; `mul`
  against schoolbook `mulmod` for 1-9 limbs including the 0/1/m-1 boundary
  operands and an assertion that results are always fully reduced;
  `modpow_odd` against an independent square-and-multiply at **every window
  width boundary and one either side**, with all-ones and single-bit exponents;
  degenerate bases and exponents.
* `bigint::tests::modpow_montgomery_and_classic_agree_with_decimal` — three-way.
  Montgomery, the retained division path, and the decimal `bi_mod_pow_str`
  oracle must all agree, across 12 modulus widths × both parities × 7 exponent
  widths, plus structural cases (modulus 1, word-boundary moduli, powers of two,
  negative bases, exponents with 31 leading zero bits in the top word). Neither
  arm is ever checked only against itself.
* `bigint::tests::modpow_round_trips_an_rsa_keypair` — `(m^e)^d == m (mod n)`
  and both CRT halves, on a real keypair.
* `crypto_impl::tests::modpow_montgomery_matches_the_dividing_ladder` — the new
  arm against the routine it replaced (preserved verbatim as `modpow_dividing`),
  plus a `u128` oracle on small operands.
* `ModPowProbe` gates on correctness **before** timing anything — Fermat,
  an independently written square-and-multiply over odd and even moduli,
  negative exponents including the non-invertible `ArithmeticException`, and RSA
  algebra. A build that is fast and wrong fails instead of printing a good
  number. The `modPow` checksum it prints is byte-identical on HotSpot,
  baseline, and Montgomery (`1777d2a7`) — which also proves the native limb path
  is what `--java-home` runs, rather than the JDK's own bytecode.

## Also landed

* The negative-exponent arm of the `modPow` native (`phases_late.rs`) moved off
  the decimal `bi_mod_inverse_str`/`bi_mod_pow_str` round trip onto limb
  `mod_inverse` + `modpow`. It was the last O(digits²) hop in `modPow`.
* `BigInt::modpow_classic` (the even-modulus fallback) iterates
  `exp.bit_length()` rather than `exp.mag.len() * 32`, which is the third lever
  the open page named. Worth ~1.5% and free.
* A doc comment describing Miller-Rabin had drifted onto `mod_inverse`; moved
  back onto `is_probable_prime`.
* **A pre-existing ~3% flake in `rsa_cipher_failures_carry_the_class_sunjce_raises`**,
  surfaced by running the suite for this change and initially suspected of being
  it. `rsa_cipher_encrypt` emits exactly `k` bytes via `to_bytes_be_padded`, so
  a ciphertext below `2^(8(k-1))` — ~1 in 128 for this modulus — carries a
  leading zero byte. The test's "a ciphertext SHORTER than the modulus must not
  decrypt" row then passes `ct[1..]`, which is **the same integer**, and it
  decrypts correctly. Measured 3/60 on this branch and 1/60 on the fork point
  (same assertion, same `"payload"` plaintext), so it predates this work.
  Fixed by re-rolling the randomized encryption until the leading byte is
  non-zero: 120/120 after.

  Recording the verification mistake too, because it is the kind that makes a
  real failure look like noise: the first attempt to reproduce ran
  `<test-binary> <bare_fn_name> --exact`, but `--exact` matches the *full* path
  (`crypto_impl::tests::…`). Every invocation selected zero tests, printed
  `test result: ok. 0 passed`, and a grep for `test result: ok` scored it a
  pass — 360 green runs that ran nothing. The giveaway was that the result
  contradicted the failure probability the mechanism predicted. **A pass count
  is not evidence unless the run count is checked against it.**

## Not touched, and why

`classloading::jar_signer`'s private `BigUint::modpow` is a third copy with an
even worse `modulo` (bit-at-a-time long division). It is left alone: it verifies
jar signatures with a **public** exponent — one 17-bit exponentiation per signed
jar — and its crate does not depend on `native-builtins`, so sharing this module
would mean a new cross-crate dependency for a workload that does not appear in
any measurement on this page.

## Repro

```bash
cd regression-suite/perf/probe-modpow
javac -d . ModPowProbe.java
<cratonvm> --java-home <jdk-25> -cp . ModPowProbe 4
java -cp . ModPowProbe 4          # oracle
```

```bash
cargo test --release -p cratonvm-native-builtins --lib -- --ignored rsa_sign_cost_attribution --nocapture
```

## Related

- `known-issues/perf/rsa-private-key-op-ignores-the-crt-parameters-it-already-has-20260817.md`
  — the residual this pass found and did not take, with the attribution above.
- `internal/fixed-suite-bugs/netty/ocspclienttest-is-sixteen-rsa-certificates-CLOSED-20260817.md`
  — the page this was named in. Its §3 conclusion still holds: keygen is a
  minority of a certificate, so a 2x on `modPow` moves a certificate by well
  under 2x. **Measure the certificate, not the primitive**, before quoting a
  figure for a suite.
- `known-issues/perf/g1-retained-store-thread-scaling-20260817.md` — the other
  residue filed out of the same netty pass, untouched here.
