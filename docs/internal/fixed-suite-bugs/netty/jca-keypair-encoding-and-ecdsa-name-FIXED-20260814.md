# `KeyPairGenerator`: three short public keys, one algorithm name HotSpot refuses — FIXED

**Status:** FIXED (2026-08-14). `probes/KpgEndToEnd` and the new
`probes/KeyEncodingProbe` now both diff **empty** against HotSpot JDK 25.

The open page had four rows and one hypothesis. **The hypothesis was wrong**,
and it said so itself — "that is a reading, not a measurement" — which is the
only reason the first step taken here was a probe and not a patch. The probe
then found a fifth defect the page had not predicted, and the worst of the set.

Measured on Azure host 2 (`/data/toolchain/jdk-25`).

## What the page thought, and what it was

> 294 bytes for a 2048-bit RSA public key is about the size of the bare
> `RSAPublicKey` `SEQUENCE { n, e }` … without the `AlgorithmIdentifier`
> wrapper … So the likely reading is that these keys encode the key MATERIAL
> and not the SPKI envelope.

The envelope was there all along. `probes/KeyEncodingProbe` decodes the outer
DER far enough to name the direct children of the top-level SEQUENCE, and both
VMs answer `SEQUENCE{SEQUENCE,BIT_STRING}` — a `SubjectPublicKeyInfo`, not a
bare key. The arithmetic that made the wrong reading plausible was a
coincidence.

**The three short rows were a DEFAULT KEY STRENGTH divergence.** The JDK's
defaults moved and this VM's did not:

| | HotSpot JDK 25 | CratonVM (before) |
| --- | --- | --- |
| default RSA | 3072-bit (JDK 22) | 2048-bit |
| default RSASSA-PSS | 3072-bit | 2048-bit |
| default EC | secp384r1 (JDK 24) | secp256r1 |
| default DSA | 2048-bit | 2048-bit — already correct |

`probes/KpgEndToEnd` never calls `initialize`, so it was measuring the
DEFAULT. Ask HotSpot for `initialize(2048)` and it produces 294 bytes — exactly
what CratonVM produced — and `initialize(256)` gives 91. The two VMs agreed
about encoding and disagreed about policy.

**And policy is the more serious of the two.** A shorter `getEncoded()` is a
symptom; the disease is that
`KeyPairGenerator.getInstance("RSA").generateKeyPair()` handed back a WEAKER
key on CratonVM than the identical line on HotSpot — silently, to exactly the
application that declined to choose and left it to the platform. `DSA` staying
at 2048 is the row that keeps `default_key_strength` from being
"raise everything": it is what HotSpot measures, so it is what this VM does.

The cost is real and is the platform's: on HotSpot itself, RSA-3072 keygen is
**358 ms/keypair against 111 ms for RSA-2048** (measured, n=10 each, same
process). CratonVM pays the same multiple. Nothing in the tree relies on the
old default — every fixture that asserts a size (`RCrypto`, `RJdkSecurity`,
`JcaExceptionTypeProbe`, `JdkOnlyPlatformProbe`) calls `initialize(2048)`
explicitly first, which is what was checked before the default moved.

## The defect the page did not predict

`probes/KeyEncodingProbe` asks one thing `KpgEndToEnd` cannot: **does
`getEncoded()` come back?**

```text
RSASSA-PSS.pub.algorithm    HotSpot RSASSA-PSS      CratonVM RSA
RSASSA-PSS.pub.encoded.algId  HotSpot 300b06092a864886f70d01010a
                              CratonVM 300d06092a864886f70d010101 0500
RSASSA-PSS.pub.roundTrip    HotSpot OK sameBytes=true
                            CratonVM java.security.spec.InvalidKeySpecException
RSASSA-PSS.priv.roundTrip   HotSpot OK sameBytes=true
                            CratonVM java.security.spec.InvalidKeySpecException
```

`algo_idx` collapses `"RSASSA-PSS"` onto `ALGO_RSA`. That is **right for the
key MATERIAL** — the two share it, and the PSS choice belongs to `Signature` —
and **wrong for the key OBJECT**, which carries the algorithm identity forward
in its `AlgorithmIdentifier`. Every PSS key this VM generated was stamped
`rsaEncryption` instead of `id-RSASSA-PSS`.

The consequence is not cosmetic. `KeyFactory.getInstance("RSASSA-PSS")` drives
`sun.security.rsa.RSAKeyFactory$PSS`, which REJECTS the `rsaEncryption` OID —
a fact this file's own `ALGO_RSASSA_PSS` doc comment already recorded, for the
import direction. So **a PSS key pair this VM generated could not be
re-imported by this VM**, and every consumer that serialises a key and reads it
back — `X509EncodedKeySpec`, a CSR builder, a JWK round trip — hit
`InvalidKeySpecException` on a key that had just been produced two lines above.

Fixed with a `RsaKeyType` keyed on the **requested name**, not on `algo_idx`,
which by design cannot tell the two apart. It threads through
`real_rsa_keypair` → `real_rsa_key_from_components` / `real_rsa_crt_private_key`
and selects `$PSS` over `$Legacy`; the fast Rust keygen is untouched, so this
costs nothing but the right factory. The availability probe moved with it —
`real_spi_available(ctx, kind.spi_class())` asks about the SPI that will
actually be driven rather than about its sibling.

## `ECDSA`

`KeyPairGenerator.getInstance("ECDSA")` and `KeyFactory.getInstance("ECDSA")`
both answer `NoSuchAlgorithmException` on HotSpot 25 — measured on both
engines, not just the one the page named. SunEC registers `EC` and no `ECDSA`
alias for either. `algo_idx`'s `"EC" | "ECDSA"` arm lost its second name.

The page hesitated over this ("it makes something stop working rather than
start"), and the check it asked for came back empty: nothing in the tree
requests `ECDSA` from either engine. A caller that does is reaching for
BouncyCastle, which DOES register it — and `kpg_serviceable`'s provider-chain
half still finds it there, so refusing here is what makes that fallback
reachable rather than what breaks it.

**Not the same question as a KEY whose `getAlgorithm()` is `"ECDSA"`.**
BouncyCastle's EC keys answer exactly that, and `kf_check_key_algorithm`
deliberately accepts them for an `EC` factory. Names of keys and names of
engines are different namespaces, and only the engine one moved.

## Two more hardcoded key sizes, removed on the way past

* `kpg_initialize_spec` pinned EC to a literal `256`. A spec pins the curve and
  `drive_real_keypair_spi` prefers it, so this was only the fallback — but a
  fallback that silently generates P-256 for an `ECGenParameterSpec("secp384r1")`
  is the wrong kind of fallback.
* the synthetic RSA public-import path recorded a literal `2048` as the key
  size of a key that had **arrived from outside**, so it was wrong for every
  3072- or 4096-bit key that reached it. Now derived from the imported modulus.

Both now read `default_key_strength`, so there is one place the platform
default lives.

## The probe

`probes/KeyEncodingProbe` prints, per algorithm: the key class, `getAlgorithm()`,
`getFormat()`, the outer DER shape, the `AlgorithmIdentifier` in hex, and
whether the bytes round-trip through `KeyFactory`.

**It prints neither raw key bytes nor exact encoded lengths, and that is the
design.** A DER `INTEGER` gains or loses a leading zero byte with the value, so
two CORRECT encodings of two different keys differ by a byte or two — and a
diff of lengths reads as a defect. The `AlgorithmIdentifier` is the part that
is fixed for a given algorithm, so it is the part that can be compared between
two VMs that each generated their own keys. Proven, not assumed: running the
probe twice on HotSpot against two different key sets produces byte-identical
output.

That property is what let the last row of `probes/KpgEndToEnd` be settled by
measurement instead of argument. `DSA … len=838` vs `839` looked like a
one-byte encoding defect; across 24 key pairs per VM the distributions are

```text
HotSpot   pub {838=22, 839=2}   priv {608=16, 609=8}
CratonVM  pub {838=22, 839=2}   priv {608=18, 609=6}
```

— the same variation, at the same rate, on both. It is the leading-zero byte of
the public value `y`, and it is not a defect on either side. Six consecutive
838s from HotSpot had earlier made it look deterministic; the distribution is
what says otherwise.

## Verification

* `probes/KeyEncodingProbe` — empty diff, 77 lines.
* `probes/KpgEndToEnd` — empty diff.
* `probes/JcaGetInstanceProbe` — still empty (63 lines), no regression from
  the `ECDSA` refusal or the PSS identity change.
* `probes/NoneWithRsaProbe` — still empty.
* `cargo test -p cratonvm-native-builtins --lib` — green.

## Repro

```bash
javac -d /tmp/k probes/KeyEncodingProbe.java probes/KpgEndToEnd.java
java -cp /tmp/k KeyEncodingProbe > /tmp/hs.txt
cratonvm --java-home <jdk25> -cp /tmp/k KeyEncodingProbe | diff /tmp/hs.txt -
```
