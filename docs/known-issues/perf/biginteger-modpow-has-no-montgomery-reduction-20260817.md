# `BigInteger.modPow` is square-and-multiply with a full division per step — 11x HotSpot, and it is the floor under every certificate test

**Status: OPEN, perf**, measured 2026-08-17 on
`fix/netty-sni-ocsp-rld-residuals-20260817`, Windows host, `cratonvm.exe`
release build. Named while closing `OcspClientTest`, which is 16 RSA-2048
certificates and nothing else.

## The numbers

Same host, same classpath, `RsaKeygenProbe` in
`apps/netty-suite-runner/probe-sniocsp/`:

| primitive | HotSpot 25 | CratonVM | ratio |
|---|---|---|---|
| `BigInteger.modPow`, 2048-bit, ×20 | 16 ms | 176 ms | 11x |
| `BigInteger.probablePrime(1024)` | 21 ms | 276 ms | 13x |
| RSA-2048 keygen (1st / 2nd / 3rd) | 144 / 79 / 39 ms | 3442 / 1798 / 720 ms | ~20x |
| `SHA256withRSA` sign | 4 ms | 32 ms | 8x |
| netty `CertificateBuilder` rsa2048 self-signed, steady | 73-193 ms | 4975-7839 ms | ~40-65x |

## What is already right, so nobody re-fixes it

`modPow` is **not** on the decimal-string path. `math_bignum.rs` registers a
string-based `modPow` built on `bi_mod_pow_str`, but `phases_late.rs` registers
a limb-based one afterwards and registration is LAST-WRITE-WINS, so the
non-negative-exponent case — the whole crypto hot path — already runs on
`crate::bigint::BigInt` words with no decimal round trip. `modInverse` and the
core add/sub/mul/div/rem/mod are limb-based too. The 11x is what a *correct
limb implementation without the algorithmic tricks* costs.

## Where the 11x is

`BigInt::modpow` is textbook binary square-and-multiply:

```rust
let ebits = exp.mag.len() * 32;
for i in 0..ebits {
    if (exp.mag[i / 32] >> (i % 32)) & 1 == 1 {
        result = result.mul(&base).modulo(modulus);
    }
    if i + 1 < ebits {
        base = base.mul(&base).modulo(modulus);
    }
}
```

For a 2048-bit exponent that is ~2048 squarings and ~1024 multiplies, and each
one pays a **full Knuth-D division** in `modulo`. HotSpot's intrinsics do neither:

* **Montgomery reduction** replaces the per-step division with a multiply and a
  shift. This is the big one — the division is roughly twice the cost of the
  multiply it follows, so removing it is the ~2-3x.
* **A sliding window** (4-bit is the usual choice) cuts the multiplies ~4x
  against a small precomputed table, worth another ~20-30% overall.
* `ebits` is `mag.len() * 32` rather than `exp.bit_length()`, so up to 31
  leading zero bits of the top word are squared for nothing. Bounded and minor
  (1.5% on a 2048-bit exponent) but free to fix.

Estimated combined: ~3x on `modPow`, which propagates to prime search, RSA
keygen, and signing.

## Why it was not attempted in that pass

It is a crypto-correctness-sensitive rewrite — a Montgomery modPow that is
subtly wrong produces plausible-looking wrong signatures, which is the failure
mode this tree has already been bitten by (see
`an-engine-that-validates-a-name-then-dispatches-on-something-else` in the
retired records: a `Cipher` that ran the wrong algorithm and round-tripped
cleanly). Doing it properly needs an exhaustive differential test against the
existing limb implementation and against HotSpot across operand sizes, odd and
even moduli (Montgomery needs an odd modulus; the even case must fall back),
and the negative-exponent path. That is its own change, not a rider on a netty
triage pass.

## What it would buy

Every certificate- or TLS-heavy class in every suite. Concretely, from the pass
that measured it: `OcspClientTest` spends 110-240 s building 16 RSA-2048
certificates where HotSpot takes 9-13 s, and `CertificateBuilderTest` has its
own open row for the same cost. Note the split though — RSA keygen is only
~0.7-1.8 s of a ~5 s certificate on this VM, so the other ~3 s is X.509
encoding and BouncyCastle ASN.1 at ordinary interpreted-bytecode rates. A 3x on
`modPow` improves the certificate by well under 3x. Measure the certificate,
not just the primitive, before quoting a figure for the suite.

## Repro

```bash
cd apps/netty-suite-runner
<cratonvm> --java-home <jdk-25> --Xmx 1500m -XX:+UseG1GC \
  -cp "<probe-dir>" RsaKeygenProbe 3
java -cp "<probe-dir>" RsaKeygenProbe 3          # oracle
```

`RsaKeygenProbe` and `CertBuildProbe` are in
`apps/netty-suite-runner/probe-sniocsp/` (gitignored with the rest of `apps/`).

## Related

- `ocspclienttest-is-sixteen-rsa-certificates-CLOSED-20260817` — the retired
  page this was named in, with the full per-operation attribution.
- `netty/certificatebuildertest-fail-status-not-a-regression-20260816.md` — the
  same certificate cost seen from its own test class.
- `g1-retained-store-thread-scaling-20260817.md` — the other residue filed out
  of the same pass.
