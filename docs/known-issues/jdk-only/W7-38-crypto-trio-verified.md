# W7-38 — the three crypto defects, measured fixed

**Status: VERIFIED BY EXECUTION 2026-08-12.** `probes/CryptoTrioProbe.java`, HotSpot 25.0.3.9
vs CratonVM `--real-jdk`, one binary (dev after the wave), same class files.

All three were **fabricated successes** — the call returned a plausible value and nothing errored,
so a probe printing `ok` would have passed. This one prints bytes.

## Fixed, and byte-identical to HotSpot

| observable | HotSpot | CratonVM |
|---|---|---|
| `keygen.AES` | `len=32 allZero=false twoDrawsDiffer=true` | identical |
| `keygen.HmacSHA256` / `keygen.DESede` | 32 / 24 bytes, random | identical |
| `SecretKeySpec.copiesOnConstruct` | `4141…` | identical |
| `SecretKeySpec.copiesOnGetEncoded` | `4242…` | identical |
| `aes128ecb.ct` | `178c380cadc0514ffe26d8b26351c673` | identical |
| `aes256ecb.ct` | `0a0e7bd98cd0ed18…` | identical |
| `mac.HmacSHA256` / `mac.HmacSHA512` | 32 / 64 bytes | identical |

The all-zero key is gone in both directions: `SecretKeySpec` now copies on construct AND on
`getEncoded`, so neither the generator scrubbing its buffer nor a caller scrubbing the returned
array can reach the key.

## Fabrication gone, functionality not yet there

These now REFUSE where they used to serve the wrong cipher. Refusing is the correct safe answer —
a wrong cipher is worse than a missing one — but it is a gap against HotSpot, not parity:

| observable | HotSpot | CratonVM |
|---|---|---|
| `ChaCha20` | `InvalidAlgorithmParameterException` (needs `ChaCha20ParameterSpec`) | `NoSuchAlgorithmException` |
| `ChaCha20-Poly1305` | real AEAD, `2aa22027…` | `NoSuchAlgorithmException` |
| `ChaCha20-Poly1305.tamperedDecrypts` | `AEADBadTagException: Tag mismatch` | `NoSuchAlgorithmException` |
| `Blowfish` | `33b63e40…` (24B) | `NoSuchAlgorithmException` |
| `RC4` | `27ca482b…` (16B) | `NoSuchAlgorithmException` |
| `mac.HmacSHA224` | `len=28 86d73d8c…` | `NoSuchAlgorithmException` |
| `keygen.Blowfish` | `len=16` | `NoSuchAlgorithmException` |

**Before**, every one of those returned AES output instead: ChaCha20 and ChaCha20-Poly1305 were
AES-256-ECB (nonce discarded, no tag, so tampered ciphertext decrypted cleanly), Blowfish and RC4
were AES-128-ECB and byte-identical to each other, and HmacSHA224 returned a 32-byte HMAC-SHA-256.

## One defect found inside the instrument

The first version of this probe reported `Blowfish.equalsRc4=true` — which reads exactly like the
defect ("still the same cipher") when it actually meant "neither ran": both sides were the string
`NoSuchAlgorithmException`, compared equal. That is this campaign's dominant failure shape appearing
**inside the instrument built to detect it**. `sameCipher` now answers `n/a(no-ct,no-ct)` unless both
operands are real ciphertext. An equality test over refusals is not a measurement.

## Re-taking this

```sh
javac -d /tmp/cp probes/CryptoTrioProbe.java
java -cp /tmp/cp CryptoTrioProbe                       # oracle
cratonvm --real-jdk --java-home "$JAVA_HOME" -cp /tmp/cp CryptoTrioProbe
```
