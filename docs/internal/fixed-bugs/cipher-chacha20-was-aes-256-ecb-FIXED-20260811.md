# `Cipher` ChaCha20 was AES-256-ECB, and ChaCha20-Poly1305 had no tag — FIXED 2026-08-11

**Status:** FIXED by IMPLEMENTING both algorithms, not by removing them.
Found by the JDK-only W4-3 advertised-versus-implemented census, which filed it
as the worst thing that lane found and proposed refusing the names (its Patch E)
as the fallback if implementing was out of scope. It was not out of scope.

## The defect

`Cipher.getInstance("ChaCha20")` succeeded and then encrypted with
**AES-256-ECB**. Four things had to line up, and they did:

1. `cipher_algorithm_known` accepted `CHACHA20` and `CHACHA20POLY1305`, so
   `getInstance` returned a working `Cipher`.
2. `cipher_do_final_impl` discarded the cipher name outright —
   `let (_cipher_name, mode_str, pad) = parse_transformation(&algo);` — and
   dispatched on the MODE alone.
3. `parse_transformation` defaults a transformation with no `/` to mode `ECB`.
4. **A ChaCha20 key is 32 bytes, which is a valid AES-256 key.** So
   `Aes::key_expansion` succeeded instead of erroring, and the `"ECB"` arm ran
   AES-256-ECB with PKCS7.

Nothing raised at any point. The consequences, in order of severity:

* **`ChaCha20-Poly1305` produced no authentication tag at all.** Decrypting
  attacker-modified ciphertext returned "plaintext" with no
  `AEADBadTagException` and no exception anywhere on the path. An AEAD that
  authenticates nothing is worse than no AEAD, because the caller believes the
  opposite.
* **The nonce was discarded**, so output was deterministic per (key, plaintext
  block) — the exact property ChaCha20's nonce exists to destroy. Two messages
  under one key were trivially correlatable.
* **The block counter was discarded**, so `ChaCha20ParameterSpec`'s counter did
  nothing and seeking into a stream was impossible.
* Output was ECB-shaped: padded to a 16-byte multiple, with identical plaintext
  blocks giving identical ciphertext blocks.

**Mode: all.** `Cipher.getInstance` is registered by `register_cipher_dispatch`
on the essential path, so this was live in Compatible, `--jdk-only` and
synthetic-JDK alike.

## Why a round-trip test could not catch it

The tree had a test named `t2_6_5_chacha20_poly1305_round_trip`, and it was
green the whole time. Two reasons, both worth remembering:

* **Encrypt-then-decrypt with the same wrong algorithm round-trips perfectly.**
  A round trip proves the two directions agree with each other, never that
  either is the algorithm asked for.
* That test drives the `chacha20poly1305` **Rust crate** directly and never
  touches `javax.crypto.Cipher`. It asserted a dependency worked.

And the dependency did: `chacha20poly1305 = "0.10"` has been a normal
`[dependencies]` entry of `native-builtins` throughout. The gap was never
"CratonVM cannot do ChaCha20" — it was that nothing connected the cipher engine
to it.

## The fix

`native-builtins/src/chacha20.rs`, new: ChaCha20 (RFC 8439 §2.3-2.4), Poly1305
(§2.5) and the AEAD construction (§2.8), with the RFC's own vectors beside them.

`cipher.rs` routes the family **before** `Aes::key_expansion` — that ordering is
the fix, because the expansion is what silently accepted the key:

```rust
if is_chacha20_family(&algo) {
    return chacha20_do_final(...);
}
let aes_key = match Aes::key_expansion(&key_bytes) { ... };
```

Everything else follows SunJCE as measured on OpenJDK 25.0.4, because for a
crypto API the exception CLASS is part of the contract:

| case | behaviour |
|---|---|
| tampered ciphertext / tag / AAD / nonce | `AEADBadTagException: Tag mismatch` |
| input shorter than the tag | `AEADBadTagException: Input too short - need tag` |
| 128-bit key | `InvalidKeyException: Key length must be 256 bits` |
| `ChaCha20` + `IvParameterSpec` | `InvalidAlgorithmParameterException: ChaCha20 algorithm requires ChaCha20ParameterSpec` |
| `ChaCha20-Poly1305` + `ChaCha20ParameterSpec` | `…requires IvParameterSpec` |
| `updateAAD` on the raw cipher | `IllegalStateException: Cipher is running in non-AEAD mode` |
| `ChaCha20/ECB/NoPadding` | `NoSuchAlgorithmException` at `getInstance` |
| second ENCRYPT `init`, same key+nonce, same object | `InvalidKeyException: Matching key and nonce from previous initialization` |
| no spec on ENCRYPT | a fresh CSPRNG nonce, recoverable via `getIV()` |

The nonce-reuse guard is **per Cipher instance**, matching SunJCE. A first
attempt made it process-global and it refused this change's own regression
vector — two independent `Cipher` objects may legitimately use one pair, and
banning that prevents nothing.

Rejecting `ChaCha20/ECB/NoPadding` at `getInstance` is not pedantry: a
mode-only dispatch that tolerated `ECB` is precisely how this defect existed.

## The adjacent key wraps, also implemented

`AES/KW/PKCS5Padding` and `AES/KWP/NoPadding` were advertised and unimplemented,
and `doFinal` on them raised an **unchecked** `IllegalStateException`
("Cipher mode 'KW' not implemented in WP6.3 dispatch") that no
`catch (GeneralSecurityException)` can see. `AES/KW/NoPadding` was in the same
state through `doFinal` even though RFC 3394 was already implemented — it was
reachable only from `Cipher.wrap`/`unwrap`.

All three now work through `doFinal`:

* `AES/KW/NoPadding` — the existing RFC 3394.
* `AES/KW/PKCS5Padding` — PKCS#5 at an **eight**-byte block size, then RFC 3394.
  (Measured: a 16-byte payload wraps to 32 bytes, which is only consistent with
  padding to 24 first.)
* `AES/KWP/NoPadding` — RFC 5649, including the single-block case that RFC
  3394's schedule is undefined for, and the length field that makes an unwrap
  return the ORIGINAL length rather than a zero-padded approximation.

Refusals are now checked exceptions carrying SunJCE's own wording
(`IllegalBlockSizeException: data should be at least 16 bytes and multiples of 8`;
`data should have at least 1 byte` for KWP).

## Evidence

Five RFC 8439 vectors (§2.3.2 block, §2.4.2 encryption, §2.5.2 Poly1305, §2.6.2
key generation, §2.8.2 AEAD) plus §A.3's reduction edges, as Rust unit tests.

**A differential test against the `chacha20poly1305` crate** over 84
plaintext × AAD length combinations around the 16-byte MAC and 64-byte
keystream boundaries. Hand-written Poly1305 is exactly where a carry bug hides
and the RFC only pins the inputs the RFC chose; crossing two independent
implementations covers the rest.

`regression-suite/src/RChaCha20Cipher.java`, 41 checks, **PASS in `--real-jdk`
and `--jdk-only`, byte-identical to HotSpot**. It asserts the RFC vectors, that
nonce and counter each change the keystream, that output length equals input
length (ECB padded), that identical plaintext blocks do NOT give identical
ciphertext blocks, that five distinct tamperings are each refused AND that the
untampered message still decrypts, and all three key wraps against SunJCE's
answers.

Suites: `SUITE=all` and the `--jdk-only` corpus unchanged; `native-builtins`
`--lib` green.

## The reading worth keeping

The census asked "what is advertised but not implemented", and the answer that
mattered was one layer down: **an engine that validates a name at
`getInstance` and then dispatches on something else entirely.** The name was
checked, so `getInstance` was honest; the dispatch used the mode, so the
algorithm was not. Any engine with a `_name` binding in its dispatch line is
worth reading twice — `cipher_do_final_impl` wrote it out, and the underscore
was the whole bug.
