# The RSA private-key op now uses the CRT parameters it already had — 3.2x on signing, 7.1x on decrypt

**Status: FIXED 2026-08-17**, on `perf/rsa-private-key-op-crt-20260817`, branched
from `dev` at `2f4b2f82c`. Retires
`known-issues/perf/rsa-private-key-op-ignores-the-crt-parameters-it-already-has-20260817.md`.

Every private-key path in the VM now exponentiates with `(p, q, dP, dQ, qInv)`
when the key carries them, behind a mandatory Bellcore fault check, falling back
to the full-width `d` whenever the CRT answer cannot be *trusted* — not merely
whenever it cannot be computed.

## The numbers

Same host, release binaries, JDK 25 via `--java-home`,
`regression-suite/perf/probe-modpow/ModPowProbe.java`. **Baseline is the same
tree at the fork point**, built and run as its own binary. Eight interleaved
rounds (HotSpot, baseline, CRT, per round) so the ambient load of a shared
machine falls on all three equally. Medians, with the full observed range.

| row | HotSpot 25 | baseline | CRT | gain | gap to HotSpot |
|---|---|---|---|---|---|
| `SHA256withRSA` sign ×10 | 10 ms | 295 (260-417) | **93 (58-117)** | **3.17x** | 29.5x → 9.3x |
| RSA decrypt ×10 | 11 ms | 651 (446-826) | **92 (60-131)** | **7.08x** | 59x → 8.4x |

Both rows separate with **no overlap** between baseline and CRT. The
`crypto_impl` attribution probe agrees from underneath: the CRT private op plus
its fault check is 77.98 ms per ten 2048-bit operations against 247.96 ms for
the full-width exponentiation, **3.18x**, and a whole signature went 206.2 ms →
79.6 ms.

Untouched rows behaved as they should: `modPow` 290 → 278 ms and
`probablePrime` 164 → 173 ms are the same number twice, which is the control
this measurement needed.

### Decrypt beat the estimate, and the reason is not the CRT

The open page predicted ~4x from halving both the exponent and the modulus.
Signing delivered 3.17x — the prediction, less the fault check and the blinding
that surrounds it. **Decrypt delivered 7.08x, and it would be wrong to bank
that as "CRT is better than advertised".** The decrypt path's baseline was
`rsa_private_modpow_blinded_no_e`, which spends **two** full-width secret
exponentiations (`r^d` and `(base·r)^d`) because it has no public exponent to
blind with cheaply. CRT replaced both with one CRT operation. So the row is
~2 × 3.2 ≈ 6.4x from arithmetic that was doubled to begin with, and the
remaining headroom on that path is the *blinding scheme*, not the exponent.

## Why keygen is not in the table

The interleaved run reported a keygen median of 895 ms baseline against 1210 ms
CRT, which reads like a 26% regression and is not one. The distributions overlap
almost entirely (baseline 226-5972 ms, CRT 198-3669 ms; means 1213 vs 1296), CRT
posts both the lower minimum *and* the lower maximum, and **there is no mechanism**
— CRT changes the private-key operation, and keygen is prime search, which does
not perform one. The open page said as much in advance ("keygen itself is
dominated by prime search, so this row does not help keygen").

Recording it because a median over 32 keys still is not enough for this row. The
sibling page already warned not to quote keygen from fewer than ~30 keys; the
honest floor is higher than that, and the right move when a number has no
mechanism behind it is to say "unchanged within noise" rather than to publish a
ratio in either direction.

## What was built

`rsa_crt_exponentiate` — Garner recombination, returning `Option` so that every
refusal routes to the full-width `d`:

* **the fault check is mandatory.** CRT-RSA that returns an unverified result is
  the Bellcore fault attack: one faulty half makes `gcd(s - s_correct, n)` hand
  over a factor of `n`, from a *single* signature. The result is verified with
  one public exponentiation (`e` is 17 bits in practice, ~3% of the operation)
  before it is returned. This is not defence in depth — it is load-bearing here,
  because `parse_rsa_private_key_der` reads all five parameters straight out of
  a PKCS#8 file **without ever checking `n == p*q`**, so hostile CRT parameters
  reach this code directly.
* a key whose `e` is too small to verify with gets **no CRT at all**, rather
  than an unverified fast path. Declining to go fast is always available.
* two things the open page did not mention but the code needs: `BigUint::sub`
  panics on underflow and `m1 < m2` is perfectly ordinary, so the Garner
  difference is taken in `[0, p)` explicitly; and `m2` is reduced mod `p` first
  because the `p > q` convention holds for *generated* keys and an imported key
  may carry them either way round. Garner does not care, as long as
  `qInv·q ≡ 1 (mod p)` — but the code has to not care correctly.

`rsa_private_op_blinded` — blinding outside, CRT inside. That order is the only
one that works: blinding the *output* would leave both CRT halves running on
attacker-chosen data, which is the timing channel VULN(2) exists to close. The
fault check then runs against the blinded base, verifying the arithmetic that
actually executed.

Wired into all four signing paths (`sign_sha256`, `sign_pkcs1_v15`, `sign_none`,
`rsa_sign_pss_ex`) and into `Cipher` decrypt, which was the last full-width
private op left in the tree.

### The `Cipher` decrypt path needed an interlock, and finding that out was the point of looking

`rsa_cipher_decrypt` takes `(n, d)` byte slices and cannot see CRT parameters at
all, so the handle-carrying `rsa_cipher_decrypt_by_id` was added beside it —
both sharing one body, differing only in the private operation, so they cannot
drift into different exception classes for the same input.

The JCA layer resolves that handle from an identity side-table and, failing
that, **from a fixed field slot on the key object**. The existing
`rsa_key_components` only consults that slot after the real
`getModulus()`/`getPrivateExponent()` have failed; the first version of this
change consulted it unconditionally, which on a genuine JDK key reads whatever
that class happens to store in slot 3. Key ids are small consecutive integers,
so an unrelated small `int` there can collide with a live id and name **a
different key** — decrypting to a wrong plaintext rather than to an error.

The fix is an interlock, not a reordering: `rsa_cipher_decrypt_by_id` honours a
handle only when the key it names carries the modulus the caller actually
initialised with. A collision declines and falls back; the cost is one
comparison. `decrypt_by_id_matches_the_n_d_path_including_its_refusals` stores a
second real key and asserts that its handle is refused for the first key's
modulus.

## The differential net

The open page's bar was "a differential test against the non-CRT path across key
sizes, imported-key (`None` CRT params) fallback, and the fault-check branch".
All three, plus the interlock:

* `crt_private_op_matches_the_full_width_exponentiation` — CRT against
  `base.modpow(&d, &n)`, the exact expression it replaced, at 1024 and 2048
  bits over 29 bases including `0`, `1`, `n-1` and randoms that exercise the
  `m1 < m2` borrow. Checks the raw CRT, the dispatcher, and the blinded wrapper.
* `crt_absent_falls_back_and_still_signs` — a stripped `(n, d)` key declines CRT
  and still produces a verifying signature; and each of the five parameters
  removed *individually* declines, so a partial set never half-computes.
* `corrupt_crt_parameters_are_caught_and_do_not_leak_a_factor` — each of `dP`,
  `dQ`, `qInv`, `p`, `q` corrupted in turn is caught by the fault check, **and
  the caller-facing entry points still return the right answer**. Degenerate
  `p`/`q` and an unverifiable `e` decline before any arithmetic.
* `decrypt_by_id_matches_the_n_d_path_including_its_refusals` — same plaintexts
  and same JCA exception classes and messages from both entry points, over three
  paddings, for corrupt / over-long / short ciphertexts; plus the interlock.
* `ModPowProbe` gained an `RSA decrypt ×10` row that gates on a round trip and
  on a corrupted ciphertext still raising `BadPaddingException` **before** it
  times anything, so a private op that silently returns a wrong value fails
  rather than posting a good number.

## State of the tree when this landed

`dev` at `2f4b2f82c` is **red on its own**: 24 failing tests in
`native-builtins` (`panama`, `deprecated_verify`, `nio_file` glob translation,
and others), none of them related to this work. Verified by stashing the change:
clean `dev` gives 4053 passed / 24 failed, this branch gives 4057 passed / 24
failed with a **byte-identical failure list**. Recorded here so the next reader
does not attribute them to the CRT change, and so that "24 red" is a known
number rather than a discovery.

## Repro

```bash
cargo test --release -p cratonvm-native-builtins --lib -- --ignored rsa_sign_cost_attribution --nocapture
```

```bash
cd regression-suite/perf/probe-modpow
javac -d . ModPowProbe.java
<cratonvm> --java-home <jdk-25> -cp . ModPowProbe 4
java -cp . ModPowProbe 4          # oracle
```

## Related

- `biginteger-modpow-montgomery-FIXED-20260817.md` — the pass that found this,
  and whose `SHA256withRSA` row this one finally moves. Read its attribution
  section together with the numbers above: that page explained why 1.21x was all
  Montgomery could do for signing, and this is the rest.
- `internal/fixed-suite-bugs/netty/ocspclienttest-is-sixteen-rsa-certificates-CLOSED-20260817.md`
  — where the `SHA256withRSA` row was first measured. Its §3 conclusion still
  stands: certificate cost is mostly X.509/ASN.1 at interpreted-bytecode rates,
  so **measure the certificate, not the primitive** before quoting a suite figure.
