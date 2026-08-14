# JCA engine residuals: four missing algorithms, three wrong provider answers, one coin-flip security test — FIXED

**Status:** FIXED (2026-08-14). `probes/JcaGetInstanceProbe.java` now diffs
**empty** against HotSpot JDK 25 — 63 of 63 lines, every engine, every
algorithm, every provider name, every exception class. The page below is the
open record it replaces, row by row, with what each one turned out to need.

Measured on Azure host 2 (`/data/toolchain/jdk-25`), CratonVM built from
`fix/jca-sublist-residuals-20260814`.

## 1. Algorithms HotSpot served and CratonVM could not

| engine | algorithm | fix | verified by |
| --- | --- | --- | --- |
| `KeyPairGenerator` | `X25519`, `X448`, `XDH` | drive `sun.security.ec.XDHKeyPairGenerator{$X25519,$X448,}` | `KpgEndToEnd` |
| `KeyPairGenerator` | `DH` | drive `com.sun.crypto.provider.DHKeyPairGenerator` | `KpgEndToEnd` |
| `Signature` | `NONEwithRSA` | new PKCS#1 v1.5 **block type 1** primitive | `NoneWithRsaProbe` |
| `KeyAgreement` | `X25519` (+`X448`, `XDH`, `DH`) | drive `sun.security.ec.XDHKeyAgreement$*` / `DHKeyAgreement` | `JcaGetInstanceProbe` |
| `KeyFactory` | `ML-DSA` (umbrella) | drive `sun.security.provider.ML_DSA_Impls$KF` | `JcaGetInstanceProbe` |

**Four of the five are the JDK's own SPI, run unmodified.** Every one of these
names is served on HotSpot by a class in `java.base`, and every one of those
classes is pure Java, so the fix is a class name and a `generateKeyPair()` —
the same shape `drive_real_eddsa_keypair` / `drive_real_pqc_keypair` already
had. The split of WHICH provider serves what is not guessable and was read off
JDK 25 by enumerating `Provider.getServices()`, not inferred: `XDH` is SunEC
while `DH` is SunJCE, the umbrella `XDH`/`EdDSA`/`ML-DSA` names map to the
**non-nested** base class while the parameterised names map to nested
subclasses, and `NONEwithRSA` is the one RSA signature name SunJCE owns rather
than SunRsaSign.

`SLH-DSA` stayed refused, and that is now the RIGHT answer rather than a gap:
JDK 25 registers no SLH-DSA `KeyPairGenerator`, so HotSpot's own answer for it
is `NoSuchAlgorithmException`.

### `initialize(...)` had to be replayed, and nothing before this needed it

The synthetic `KeyPairGenerator` records what `initialize` was told and
forwarded it nowhere, because every algorithm that existed when it was written
either ignores it (EdDSA) or is served by Rust (RSA). An SPI-driven algorithm
has to be told: without `replay_kpg_initialize`,
`kpg.initialize(new NamedParameterSpec("X448"))` on an `XDH` generator would
have silently produced an X25519 key, and `kpg.initialize(1024)` on a `DH`
generator a 2048-bit one. A wrong answer that reports success is the exact
species this page exists to remove, so the fix that closes it must not
introduce one.

`KPG_OFF_STATE` is the "`initialize` was called" latch, so an untouched
generator forwards nothing and the SPI's constructor default stands — which is
what HotSpot does.

### The `KeyFactory` `ML-DSA` row was NOT a re-advertisement

The open page was right to flag it: `W7-63-jca-advertise-vs-serve.md` had
DE-advertised the umbrella deliberately, and re-adding the service row without
an engine arm would have restored the advertised-and-refused defect it closed.

So the umbrella was IMPLEMENTED. `kf_algo_idx` carries `ALGO_MLDSA_GENERIC`
and `pqc_umbrella_keyfactory_class` drives `sun.security.provider.ML_DSA_Impls$KF`
— the JDK's own non-nested factory, which is precisely the thing that resolves
the parameter set from the key's own encoding. `ML-KEM` (SunJCE) and `DH`
(SunJCE `DHKeyFactory`) went with it, both found by the same census.

**And the reason that record gave for declining was answered, not ignored.** It
had RUN the three parameter-set names that already resolved and found them
"partly unusable one accessor in":

```text
KeyFactory.getInstance("ML-DSA-44").getProvider()
    HotSpot : SUN version 25
    CratonVM: NullPointerException: Cannot enter synchronized block
              because "this.lock" is null
```

Widening a surface that is already broken is not a fix — which is why
`kf_get_provider` is registered in the same change (§2 below). The table is a
new separate one rather than two more `pqc_spi_classes` arms, because that
function answers for `KeyPairGenerator` too and widening it would have made
`kpg_can_generate` claim a generator these indices do not have.

### `NONEwithRSA` is the one row that is our own crypto, and a round trip could not check it

`NONEwithRSA` takes the caller's bytes as the already-computed digest and does
NOT wrap them in a DigestInfo: `EM = 00 01 FF..FF 00 || M`. SunJCE implements it
by encrypting under the private key (`com.sun.crypto.provider.RSACipherAdaptor`),
i.e. PKCS#1 v1.5 block type 1.

Sign-then-verify inside one VM passes for **any** padding scheme that is
self-consistent, including a wrong one — the trap
`an-engine-that-validates-a-name-then-dispatches-on-something-else` records. So
`probes/NoneWithRsaProbe` recovers the encoded message the signature actually
carries, `EM = S^e mod n`, with `BigInteger` (identical arithmetic on both VMs)
and prints it. `EM` depends only on the modulus SIZE and the payload, never on
the key, so the line is byte-identical between two VMs that generated
DIFFERENT key pairs. That is what makes it a cross-check rather than a round
trip.

```text
$ diff hs-none.txt vm-none.txt     # IDENTICAL
em=0001ffff…ff00 33322d62797465732d6f662d63616c6c65722d63686f73656e2d646967657374
sign.len245=OK len=256
sign.len246=java.security.SignatureException
```

The refusal boundary matches too: `k - 11 = 245` is the largest payload, `246`
is a `SignatureException` on both. Verify carries the three-valued contract
`try_verify_sha256` established — `Some(false)` is a genuine NO, `None` is "the
question was never asked" — because collapsing a refusal to `false` reports
"forged" where nothing was checked.

## 2. `getProvider()` and the provider filter

`KeyFactory` and `KeyAgreement` threw from `getProvider()` and now answer.
Both are the same species and the same one-line treatment `kpg_get_provider`
got: the synthetic receiver is a REAL `java.security.KeyFactory` whose
constructor never ran, so the real body's opening `synchronized (lock)` faults
on a null field — a plain accessor throwing on a factory that otherwise works.

**`KeyGenerator` did not reproduce.** The open page listed it with the other
two; measured on the same binary that produced every row above, it already
answered `SunJCE` for `AES` and `HmacSHA256`, matching HotSpot. The row was
stale when the page was written or fixed between. Nothing was changed for it.

`Security.getProviders("KeyPairGenerator.Ed25519")` answered `<none>` for an
algorithm `getInstance("Ed25519")` served. `seed_builtin_keypairgenerator_services`
closes it by giving the built-ins service entries, which also collapses the
split `kpg_serviceable` has to straddle: the `any_provider_offers` half of its
disjunction now covers the `kpg_can_generate` half for every built-in name.

**The ratchet for it is TWO-WAY, and that is the point.** Its siblings
(`every_advertised_signature_name_is_offered_by_get_instance`,
`every_advertised_key_factory_name_is_serviceable`) assert only
advertised ⇒ serviceable, and a one-way check cannot see this residual at all:
the defect was UNDER-advertising, not over-advertising.
`every_kpg_algorithm_this_vm_serves_is_advertised` asserts both directions, so a
future algorithm added to `kpg_can_generate` and not to the seed fails the
build.

`DH` and `ECDSA` are deliberately absent from the advertised set: HotSpot
registers `DH` as an ALIAS of `DiffieHellman` (aliases resolve but are not
advertised), and registers no `ECDSA` generator at all.

## 3. The wrong exception type

`SecretKeyFactory.getInstance("TOTALLY-BOGUS-ALG")` raised
`java.lang.SecurityException`, which is not in `getInstance`'s `throws` clause,
so a caller's `catch (NoSuchAlgorithmException | NoSuchProviderException e)`
did not catch it. It now builds the real
`java.security.NoSuchAlgorithmException` through `throw_jca_exc`, so what a
caller catches is what HotSpot throws.

## 4. The security test whose verdict was a coin flip

`crypto_impl::tests::rsa_cipher_failures_carry_the_class_sunjce_raises` failed
1 run in 8 on pristine `origin/dev`.

It encrypts under one 2048-bit key and decrypts under another, asserting a
PADDING failure. The two moduli were unrelated, so roughly half the time the
second was the smaller — and then the ciphertext integer (uniform below the
FIRST modulus) was sometimes at or above the second, where `RSACore.parseMsg`'s
guard rejects it as `BadPaddingException("Message is larger than modulus")`
BEFORE any unpadding runs. Same class, different message, and the message is
what the VULN(1) oracle repair pins.

**Fixed by ORDERING the pair, so the encrypting modulus is the smaller one.**
`ct < n <= other_n` then holds by construction and the failure is always the
unpadding one the test is about. Not by relaxing the assertion, which would
have retired the Bleichenbacher/Manger regression the test exists for.

Measured: **16 consecutive isolated passes** after the change, against a
1-in-8 failure rate before it — and the argument is deterministic, not
statistical, which is what makes 16 enough.

## What this did NOT close

Three rows of `probes/KpgEndToEnd` still differ from HotSpot, all PRE-EXISTING
(they are in the pristine-`origin/dev` control run too) and none of them this
page's species. They are recorded in
`docs/known-issues/jca-keypair-encoding-and-ecdsa-name-20260814.md`:

* `getPublic().getEncoded()` is shorter than HotSpot's for `RSA` (294 vs 422),
  `EC` (91 vs 120) and `RSASSA-PSS` (294 vs 420);
* `KeyPairGenerator.getInstance("ECDSA")` is accepted where HotSpot refuses —
  an over-acceptance of exactly the shape this page's parent defect was about,
  in the one direction that page did not sweep.

## Repro (the closing measurements)

```bash
javac -d /tmp/p probes/JcaGetInstanceProbe.java probes/KpgEndToEnd.java \
                probes/NoneWithRsaProbe.java
java -cp /tmp/p JcaGetInstanceProbe > /tmp/hs.txt
cratonvm --java-home <jdk25> -cp /tmp/p JcaGetInstanceProbe | diff /tmp/hs.txt -   # empty

java -cp /tmp/p NoneWithRsaProbe > /tmp/hs-none.txt
cratonvm --java-home <jdk25> -cp /tmp/p NoneWithRsaProbe | diff /tmp/hs-none.txt - # empty

for i in $(seq 1 16); do
  cargo test -p cratonvm-native-builtins --lib \
    crypto_impl::tests::rsa_cipher_failures_carry_the_class_sunjce_raises 2>&1 | grep "^test result"
done                                                                               # 16x ok
```
