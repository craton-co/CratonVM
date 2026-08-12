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

> ## 2026-08-12 — ALL SEVEN REFUSING ROWS ARE CLOSED IN SOURCE. The residual is the INSTRUMENT.
>
> The table below is a **measurement of the 2026-08-12 dev binary**, and every
> row in it has since been implemented. Nothing here was rebuilt, so the table
> stays as the record of what was measured; what follows is what the source now
> says.
>
> * **`ChaCha20` and `ChaCha20-Poly1305` — the Poly1305 gate is DISCHARGED.**
>   `native-builtins/src/chacha20.rs` implements RFC 8439 §2.3–2.4 (ChaCha20),
>   §2.5 (Poly1305, 26-bit limbs) and §2.8 (the AEAD). **The vector this record
>   named as the deliverable is present as a Rust `#[test]`:**
>   `rfc8439_2_5_2_poly1305` asserts `a8061dc1305136c6c22b8baf0c0127a9` over
>   *"Cryptographic Forum Research Group"* under the RFC's own key. Beside it:
>   the §2.3.2 block vector, the §2.4.2 encryption vector, the §2.6.2 one-time
>   key generation, the §2.8.2 AEAD ciphertext **and tag**, both §A.3 reduction
>   edges (`r = 0` and `s = 0`, which are the two cases a wrong final
>   carry/select passes everything else on), a five-way tampering test, and a
>   differential against the `chacha20poly1305` crate over every length around
>   the 16-byte MAC and 64-byte keystream boundaries. `jca/cipher.rs:2282`
>   and `:2296` drive it; `provider_chain.rs:1226-1227` advertise both names.
> * **`Blowfish`, `RC4`/`ARCFOUR`, `mac.HmacSHA224`, `keygen.Blowfish`** — all
>   closed by W7-39-jca-missing-algorithms.md, through the real SunJCE SPI
>   rather than a reimplementation (`provider_chain.rs:1224-1225`, `:1378`,
>   `:1418-1440`).
>
> **The live residual is that this record's own coverage rule was not met, and
> the shape is the one this file exists to name.** `provider_chain.rs:1187`
> asserts *"`regression-suite/src/RChaCha20Cipher.java` matches HotSpot
> byte-for-byte in both modes"*, and `regression-suite/run.sh:106` lists
> `RChaCha20Cipher` in `CORE_CLASSES` — but **the file was untracked**, so
> `run.sh`'s `prune_missing` removed it from every scheduled run. A source
> comment claiming a measurement, a schedule naming a vector, and no vector: the
> instrument was absent while three separate places said it was green. The file
> is now in the tree (30 `check` call sites across seven arms, one of them in a
> 2 × 6 loop, so 41 checks at run time: the §2.4.2 published
> ciphertext, nonce-and-counter sensitivity, the ECB-tell, the §2.8.2 AEAD
> ciphertext and tag, five tampering refusals plus a positive control, SunJCE's
> per-instance nonce-reuse refusal, the spec-type rules, and the AES key wraps).
> **Never run.**
>
> Each assertion in it fails on the old behaviour by construction: AES-256-ECB
> under a ChaCha20 name pads to a 16-byte multiple (the length check), repeats
> its block for a repeated plaintext block (the ECB tell), ignores the nonce and
> the counter (two inequality checks), and produces no tag at all (the five
> tampering arms return bytes instead of `AEADBadTagException`).

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

`probes/` is never run by `regression-suite/run.sh`, which is why the probe
above is not the closing evidence. The scheduled half is:

```sh
cargo test -p cratonvm-native-builtins chacha20::   # the RFC 8439 vectors
bash regression-suite/run.sh                        # RChaCha20Cipher, CORE_CLASSES
```

`PASS RChaCha20Cipher (41 checks)` is what closes this record, and it must be
reached in the default (Compatible) arm — `register_cipher_clinit_shim` is
called from `register_essential_natives_with_shims`, so the ChaCha20 family is
live in Compatible, `--jdk-only` and synthetic modes alike.

## The single falsifying observation

If `RChaCha20Cipher` reaches `aeadRefusesTampering` and any of its five arms
returns bytes, the AEAD is not authenticating and the whole `ChaCha20-Poly1305`
name must come back OUT of `provider_chain::seed_direct_native_engine_services`
before anything else is attempted — a cipher that cannot fail on a bad tag is
worse than a missing cipher, and that ordering is the one this record and W7-15
both refuse to negotiate.

If instead the RFC 8439 §2.4.2 arm fails while the AEAD arms pass, suspect the
nonce first: §2.4.2's nonce is `000000000000004a00000000` and §2.3.2's is
`000000090000004a00000000`. `chacha20.rs`'s own comment records that confusing
the two produced a ciphertext matching SunJCE exactly — both were asked the same
wrong question — while failing the RFC. That is the argument for keeping both
oracles.
