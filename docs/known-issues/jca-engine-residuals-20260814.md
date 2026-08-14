# JCA engine residuals: four missing algorithms, three wrong provider answers, one coin-flip security test

**Status:** OPEN (2026-08-14). Everything here was measured with
`probes/JcaGetInstanceProbe.java` and `probes/KpgEndToEnd.java` against HotSpot
JDK 25 while closing
[the `getInstance` accept-anything defect](../internal/fixed-suite-bugs/netty/keypairgenerator-getinstance-accepts-any-algorithm-FIXED-20260813.md).
None of it is netty-specific and none has a known failing application test —
which is why it is one page of small, individually-cheap items rather than
several.

## 1. Algorithms HotSpot serves and CratonVM cannot

| engine | algorithm | HotSpot provider | CratonVM |
| --- | --- | --- | --- |
| `KeyPairGenerator` | `X25519`, `X448`, `XDH` | `SunEC` | `NoSuchAlgorithmException` |
| `KeyPairGenerator` | `DH` | `SunJCE` | `NoSuchAlgorithmException` |
| `Signature` | `NONEwithRSA` | `SunJCE` | `NoSuchAlgorithmException` |
| `KeyAgreement` | `X25519` | `SunEC` | `NoSuchAlgorithmException` |
| `KeyFactory` | `ML-DSA` (umbrella) | `SUN` | `NoSuchAlgorithmException` |

The first four are genuine gaps. **The `KeyFactory` one is deliberate** and
should be read before "fixing" it: `key_factory.rs`'s `get_instance_offers`
records that the `ML-DSA` umbrella name was advertised by the `SUN` `KeyFactory`
seed and refused by the engine for several waves, and that de-advertising it for
`KeyFactory` only is what closed that inconsistency (`W7-63-jca-advertise-vs-serve.md`).
`KeyPairGenerator` now serves the umbrella; making `KeyFactory` match means
implementing it, not re-advertising it.

`X25519`/`X448`/`XDH` are the interesting group: the `KeyPairGenerator` rows
used to *look* served — `getInstance` returned a generator and
`generateKeyPair` threw. They now refuse honestly, which is what surfaced them.

## 2. `getProvider()` and the provider filter

* `KeyFactory`, `KeyGenerator` and `KeyAgreement` **throw** from
  `getProvider()` (the probe records `?`); `KeyPairGenerator` returned `null`
  until 2026-08-13 and now answers correctly. The same one-line treatment
  (`kpg_provider_name` + a registration) applies to each.
* `Security.getProviders("KeyPairGenerator.Ed25519")` answers `<none>` for an
  algorithm `getInstance("Ed25519")` serves. `security_get_providers_filtered`
  consults only the service registry, which real `Provider` objects populate —
  the algorithms CratonVM serves from its own natives are in no registry at all.
  Fixing it means giving the built-ins service entries, which is also what would
  let the two worlds `kpg_serviceable` has to ask separately become one.

## 3. A wrong exception type

```
SecretKeyFactory.getInstance("TOTALLY-BOGUS-ALG")
    HotSpot : java.security.NoSuchAlgorithmException
    CratonVM: java.lang.SecurityException
```

`SecurityException` is not in `getInstance`'s `throws` clause, so a caller's
`catch (NoSuchAlgorithmException | NoSuchProviderException e)` does not catch
it — the same shape as the `IOException`-where-`CertificateException`-belongs
defect batch 11 found in the trust manager.

## 4. A security test whose verdict is a coin flip

`crypto_impl::tests::rsa_cipher_failures_carry_the_class_sunjce_raises` fails
**1 run in 8 on pristine `origin/dev`** (measured, 8 isolated runs; it also
failed 2 of 2 full-suite runs on an unrelated branch, which is how it was
noticed and nearly mis-blamed).

It generates two 2048-bit keys, encrypts under the first, decrypts under the
second, and asserts a padding failure:

```
assertion `left == right` failed: Pkcs1
  left: "Message is larger than modulus"
 right: "Padding error in decryption"
```

Both moduli are 2048-bit, so the ciphertext integer is sometimes ≥ the second
modulus and the RSA primitive rejects it *before* unpadding. The test is right
about what it wants to protect — the Bleichenbacher/Manger oracle repair that
collapsed every padding failure to one opaque message — and wrong to leave the
comparison to chance. Fix by pinning the two key pairs (or by choosing the
second modulus larger than the first), not by relaxing the assertion.

## Repro

```bash
javac -d . probes/JcaGetInstanceProbe.java probes/KpgEndToEnd.java
java -cp . JcaGetInstanceProbe > hs.txt
cratonvm --java-home <jdk25> -cp . JcaGetInstanceProbe | diff hs.txt -

# the flaky test, on pristine dev — one failure in eight
for i in $(seq 1 8); do
  cargo test -p cratonvm-native-builtins --lib \
    crypto_impl::tests::rsa_cipher_failures_carry_the_class_sunjce_raises 2>&1 | grep "^test result"
done
```
