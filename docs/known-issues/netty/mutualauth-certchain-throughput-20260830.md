# `testMutualAuthSameCertChain` — a 20x throughput gap, not a defect; and three wrong root causes before the right question

## Status

**OPEN, and reframed.** The test does **not** fail on a quiet host: 48/48 on
both VMs. It fails only under the six-shard categorize load, where its own
30-second `@Timeout` has too little headroom.

| | quiet, `MethodRunner`, 48 parameterisations | |
|---|---:|---|
| HotSpot 25 | **23 s** | 48/48 |
| CratonVM `dev` | **452 s** | 48/48 |
| CratonVM + this branch | 488 s | 48/48 |

**~20x.** At 452 s that is 9.4 s per parameterisation against a 30 s cap —
about 3x of headroom, which six-way contention erases. That is why it reads as
`TimeoutException` in the suite and passes when run alone.

## Correction to `compatible-fail-triage-20260830.md`

That page reported "HotSpot 48/48 in 92 s, CratonVM did not finish in 900 s".
**Both numbers were taken while other sessions were building** — 6 `cargo` and
8 `rustc` were running on this box at the time. Re-measured quiet they are 23 s
and 452 s. The ratio it inferred (">10x") was right in direction and wrong in
size, and its implication that CratonVM cannot complete the method at all was
an artefact of the load. The conclusion that this one method sits under 30 of
the 38 FAIL rows stands; the conclusion that it is a hard failure does not.

## Three root causes that were wrong, and how each died

Worth recording because each was plausible and each cost a build.

**1. "RSA keygen is slow because `SelfSignedCertificate` is slow."** Half
right. `CertGenProbe`: one `SelfSignedCertificate` costs 5244 ms here against
HotSpot's 56 ms, and the test builds 96 of them. But that is a COLD number —
six calls with no warm-up. Across the test's 48 parameterisations the JIT
warms and the per-cert cost falls; 96 x 5.2 s would be 500 s, more than the
whole test takes. **A cold per-operation probe does not multiply out to a
warm workload**, and treating it as the budget is what made keygen look like
the whole story.

**2. "`BigInteger.modPow` lacks HotSpot's Montgomery intrinsics."** `modPow`
measured 9.9 ms against HotSpot's 1.95 ms, and this VM registers
`implSquareToLen`/`implMulAdd` but not `implMontgomeryMultiply`/`Square`. So
those two were implemented, transcribed from OpenJDK 25, registered, and
verified — and moved `modPow` **not at all** (12.0 -> 13.1 ms). They are
registered and dead: **`BigInteger.modPow` is itself a CratonVM native**, so
the JDK's `oddModPow` bytecode that would call them never runs. Reverted.

The lesson is the cheap one: *before intrinsifying a JDK method, check whether
the VM already overrides its caller.* `--dump-native-registry <FILE>` answers
it in one run, and would have answered it before the build rather than after.

**3. "The live `modPow` computes on decimal strings."** True of
`math_bignum.rs`'s implementation — square-and-multiply with a decimal
schoolbook multiply and long division per exponent bit, exactly the ~100x
representation loss `gaps/biginteger-limb-rewrite-scope.md` identified in May.
But **there are two registrations of `modPow`** and last-write-wins:
`phases_late.rs`'s is already limb-based Montgomery ("rewrite step 3"), and it
is the live one. Routing the decimal one through `BigInt` anyway is this
branch's only code change (below) — it is a real improvement to a real helper
and it does not touch this test.

## What this branch does land

`bi_mod_pow_str_opt` now parses to `BigInt` limbs once and calls the tested
`BigInt::modpow` (windowed Montgomery for an odd modulus) instead of looping
over decimal strings. `BigInt` has carried that entry point, and a differential
test against this very function, since step 1 landed; nothing routed through
it.

| | before | after |
|---|---:|---:|
| `new SelfSignedCertificate()` mean (cold, n=6) | 5244 ms | **3424 ms** |
| ...max | 14498 ms | **5021 ms** |
| `BigInteger.isProbablePrime(1024)` mean | 104 ms | **83 ms** |
| `modPow` 2048 (the LIVE path, untouched) | 10.3 ms | 9.9 ms |
| `testMutualAuthSameCertChain` | 452 s | 488 s |

So: a genuine ~1.5x on the cold certificate path and ~20% on primality, **and
nothing on the target workload** — the last row is the honest one, and it is
why this page is still open. `regression-suite/run.sh` 79/79.

`RBigIntMontgomery` is the new differential vector: `modPow` over six widths
with the `a^(e1+e2) == a^e1 * a^e2` identity, an even-modulus arm, Fermat on
real primes, `modInverse` round-trips, and RSA sign/verify + encrypt/decrypt
with a negative arm. It is worth keeping whatever implements `modPow`
underneath — a Montgomery reduction that is one conditional subtraction out
still returns a number, and RSA still verifies it for one padding in 2^32.

## Where the 20x actually is — the next question, unanswered

Not established. What is known:

* it is not the representation — the live path is already limbs;
* it is not cert generation alone — see wrong-cause 1;
* `modPow` at 5x and `isProbablePrime` at 5x are both real and neither is
  20x on its own.

The instrument this wants is a sampling profile of the 452 s run attributed by
Java method, which this session did not build. `crate::montgomery`'s inner
loop is the obvious first suspect (windowing, Comba multiplication, a CIOS
reduction), and it is a self-contained optimisation target with `BigInt`'s
differential tests already around it.

## Reproduce

```bash
cd apps/netty-suite-runner
<vm> --java-home <jdk25> @common.args -Dcraton.batch=1 MethodRunner \
 'io.netty.handler.ssl.OpenSslJdkSslEngineInteroptTest#testMutualAuthSameCertChain(io.netty.handler.ssl.SSLEngineTest$SSLEngineTestParam)'
```

Run it on a QUIET box and check the load first — every number on this page
moved by more than 4x between a loaded and an idle host, in both directions.

## Related

* `compatible-fail-triage-20260830.md` — corrected above.
* `internal/gaps/biginteger-limb-rewrite-scope.md` — steps 1 and 3 are done;
  this is the first measurement of what is left after them.
