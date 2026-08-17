# The native RSA private-key op exponentiates with the full-width `d`, while `p`, `q`, `dP`, `dQ`, `qInv` sit unused on the same struct

**Status: OPEN, perf**, measured 2026-08-17 on
`fix/biginteger-modpow-montgomery-20260817`, Windows host, release
`cratonvm.exe`. Found while closing
`biginteger-modpow-has-no-montgomery-reduction-20260817` — the Montgomery pass
moved `BigInteger.modPow` 2.05x but `SHA256withRSA` sign only 1.21x, and this is
why.

## The measurement that names it

`crypto_impl::tests::rsa_sign_cost_attribution` (`#[ignore]`d timing probe,
already in the tree) on a 2048-bit key, ×10, *after* the Montgomery change:

| RSA-2048 `sign_sha256`, ×10 | ms |
|---|---|
| whole signature | 206.2 |
| secret-exponent modpow | 207.9 |
| `r.modinv(n)` (blinding) | 5.9 |
| `r^e mod n` (blinding) | 1.7 |

The signature **is** the secret-exponent modpow. Blinding is ~3%. There is no
encoding, hashing, or allocation cost worth naming. Against HotSpot's 0.9 ms per
signature on the same host, the native path is ~20 ms — and roughly 4x of that
gap is one algorithm.

## What is missing

`Rsa::sign_sha256` (and `Cipher` decrypt, and every other private-key path)
routes through `rsa_private_modpow_blinded`, which ends in

```rust
base.modpow(d, n)      // 2048-bit exponent, 2048-bit modulus
```

HotSpot, OpenSSL, and every serious implementation instead compute

```
m1 = base^dP mod p     // 1024-bit exponent, 1024-bit modulus
m2 = base^dQ mod q
h  = qInv * (m1 - m2) mod p
m  = m2 + q*h
```

A modexp is `O(exponent_bits × limbs²)`, so halving both gives
`2 × ½ × ¼ = ¼` — **~4x**, and it applies to every RSA private-key operation in
the VM: TLS server handshakes, certificate signing, jar signing, `Cipher`
decrypt.

## Why this is unusually cheap to do here

**The parameters are already there.** `RsaPrivateKey` carries
`p, q, dp, dq, qinv` as `Option<BigUint>`, populated for every generated key,
with `p > q` by JDK convention — and `crypto_impl::tests::rsa_generated_key_is_crt`
already asserts `n == p*q`, `dP == d mod (p-1)`, `dQ == d mod (q-1)`, and
`qInv*q ≡ 1 (mod p)`. They exist because the PKCS#1 DER encoder needs them.
Nothing has to be derived, stored, or plumbed; the private op just has to use
what is on the struct.

The `Option` matters: a key reconstructed from a bare `(n, d)` import has
`None`, so the CRT path must be conditional with the current full-width modpow
as the fallback. That is the shape of the change, not an obstacle to it.

## What must not be skipped

**CRT-RSA without a result check is the Bellcore fault attack.** If either half
computes wrongly — a bit flip, or a bug in exactly the sort of new code this
would add — the faulty signature reveals a factor of `n` to anyone who
subtracts it from a correct one. The standard mitigation is one cheap public
exponentiation before returning:

```
verify s^e mod n == m, else fall back to the non-CRT path
```

`e` is on the struct, `r^e mod n` measures 0.17 ms above, so this costs under 1%
of the ~4x it protects. It is not optional.

Blinding composes normally — blind the base, run CRT on the blinded value,
unblind — which is what OpenSSL does. Note that the CRT halves are also where a
timing difference between `p` and `q` can leak, so the two exponentiations
should keep the same ladder shape the current `BigUint::modpow` uses (see the
VULN(2) note there, and the deliberate no-window decision recorded in
`internal/performance/biginteger-modpow-montgomery-FIXED-20260817.md`).

## Why it was not attempted in that pass

Same reason the Montgomery work was not ridden on the netty triage pass that
named it: it is a correctness- and security-sensitive change to the RSA
private-key path, its failure mode is a signature that verifies for the attacker
and factors the key, and it deserves its own differential test against the
non-CRT path across key sizes, imported-key (`None` CRT params) fallback, and
the fault-check branch. The Montgomery pass had its own such surface to defend
and adding this would have blurred which change any regression belonged to.

## What it would buy

Every TLS handshake with an RSA server key, every certificate this VM signs, and
`OcspClientTest`-shaped classes that build many certificates. **But read the
attribution before quoting a suite figure**: the retired
`ocspclienttest-is-sixteen-rsa-certificates-CLOSED-20260817` measured RSA keygen
at only ~0.7-1.8 s of a ~5 s certificate, the rest being X.509/ASN.1 at ordinary
interpreted-bytecode rates. A 4x on the private op is a 4x on the private op.

Note also that keygen itself is dominated by *prime search* (`probablePrime`),
not by private-key operations, so this row does not help keygen.

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

- `internal/performance/biginteger-modpow-montgomery-FIXED-20260817.md` — the
  pass that found this, with the before/after attribution table and the reason
  the `SHA256withRSA` row moved only 1.21x.
- `internal/fixed-suite-bugs/netty/ocspclienttest-is-sixteen-rsa-certificates-CLOSED-20260817.md`
  — where the `SHA256withRSA` row was first measured.
