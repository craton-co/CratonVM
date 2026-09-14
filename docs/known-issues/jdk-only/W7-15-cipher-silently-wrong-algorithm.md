# `Cipher.getInstance("ChaCha20")` returned AES-256-ECB

> **RUN AND VERIFIED 2026-08-12 (lane A32, record triage). THE HEADLINE DEFECT
> IS GONE, AND THIS IS THE FIRST TIME ANY OF IT WAS EXECUTED.** Every block
> below this one is source reading against an unbuilt tree — the record says so
> five times ("Nothing was rebuilt", "No claim is made that the new code
> works"). Measured here on `cratonvm-merged-dev.exe` against Temurin/Microsoft
> `jdk-25.0.3.9-hotspot` on windows/x64, `--jdk-only` and `--real-jdk`, with
> HotSpot 25 as the oracle in the same session. Fixed key/nonce, no
> `SecureRandom`, hex rendered by hand (Patch D).
>
> **The anchor row is a known-answer test, not a round trip.** All-zero key,
> all-zero nonce, counter 0, all-zero plaintext ⇒ the raw RFC 8439 keystream:
>
> ```text
> HotSpot 25   ks[32]=76b8e0ada0f13d90405d6ae55386bd28bdd219b8a08ded1aa836efcc8b770dc7
> --jdk-only   ks[32]=76b8e0ada0f13d90405d6ae55386bd28bdd219b8a08ded1aa836efcc8b770dc7
> --real-jdk   ks[32]=76b8e0ada0f13d90405d6ae55386bd28bdd219b8a08ded1aa836efcc8b770dc7
> ```
>
> That is the RFC 8439 §2.4.2 vector. It is **not** AES-ECB, and it is not this
> engine agreeing with itself: the expectation came from the RFC and from
> HotSpot independently. `ChaCha20` under a fixed key and `ChaCha20-Poly1305`
> are byte-identical to HotSpot **including the 16-byte tag**, and the ECB
> signature the original measurement turned on — the repeated first and second
> ciphertext block for a plaintext of two identical halves — is gone.
>
> **The AEAD rule holds where it matters most.** One bit flipped in the
> ciphertext:
>
> ```text
> HotSpot 25   ChaCha20-Poly1305  javax.crypto.AEADBadTagException: Tag mismatch
> --jdk-only   ChaCha20-Poly1305  javax.crypto.AEADBadTagException: Tag mismatch
> --jdk-only   AES/GCM/NoPadding  javax.crypto.AEADBadTagException: Tag mismatch
> ```
>
> `TAMPERED ACCEPTED` — the string this record was opened for — does not appear
> in any arm.
>
> **Two rows of the "before and after" table are now STALE IN THE SAFE
> DIRECTION, and one is stale in the other direction. Read them before citing
> this record.**
>
> * `Blowfish`, `RC4`, `ARCFOUR` are **admitted and correct** — byte-identical
>   to HotSpot (`Blowfish` ⇒ `76bd8a813c6f122e…`, `RC4` ⇒ `7b8132ffc9352dc5…`,
>   and `RC4` no longer equals `Blowfish`, which was the whole defect in one
>   line). The table's "→ `NoSuchAlgorithmException`" row for them is stale:
>   they route to the real SunJCE now, they are not refused.
> * `AES/KWP/NoPadding` and `AES/KW/PKCS5Padding` are **admitted and
>   byte-identical to HotSpot** (`9bc75dcd2a6c3e6c…` and `0be6f5769063a5b0…`).
>   The "Removed — advertised, computed by nothing" table lists both as removed
>   because RFC 5649 "is a different scheme". Something implemented them; the
>   table is historical for these two as well.
> * Still refused where HotSpot serves, i.e. honest under-serving and NOT a
>   defect of this species: `RC2`, `AES/CTR/NoPadding`,
>   `AES/CBC/ISO10126Padding`, bare `DESede`, `AES_128`, `IDEA`/`SEED`/`SM4`
>   (HotSpot refuses the last three too). Every one raises
>   `NoSuchAlgorithmException` naming the transformation at the `getInstance`
>   call site. That is the trade this record chose, and it is what it looks like.
>
> **The malformed-input wording matches, verbatim:** `Invalid transformation
> format:AES/CBC` and `Invalid transformation: missing mode and/or
> padding-AES/CBC/`, identical to HotSpot's.
>
> **The rows that must not move, did not.** `AES/GCM/NoPadding`,
> `AES/CBC/PKCS5Padding` and `AESWrap_128` are byte-identical to HotSpot in
> both arms. The "single falsifying observation" this record names — a rebuilt
> binary refusing `AES/GCM`, `AES/CBC/PKCS5Padding` or `AESWrap_128` — did not
> occur.
>
> **Patch 2's live half is confirmed gone by run, not only by grep.**
> `KeyGenerator.getInstance("AES").generateKey()` is 32 random bytes in both
> arms (`len=32`, different every run, never zero); the 2-arg
> `getInstance("AES","SunJCE")` form returns `len=32`, not all zeros — the
> `keygen2arg` cover this record specifies. `DESede` ⇒ 24, `DES` ⇒ 8,
> `HmacSHA256` ⇒ 32, `Blowfish` ⇒ 16, `ChaCha20` ⇒ 32, all matching HotSpot's
> lengths.
>
> **The coverage gap is closed AND scheduled.** `regression-suite/src/
> RChaCha20Cipher.java` is now tracked by git (it was untracked, so
> `prune_missing` dropped it from every run) and is in `run.sh`'s
> `CORE_CLASSES`. Executed: `PASS RChaCha20Cipher (41 checks)` in **both**
> arms. `PASS RCrypto (57 checks)` in both arms. This record's evidence is
> therefore scheduled; the four `CipherCensus`-style probes it was measured
> with are not in the tree and `probes/` is not run by `run.sh` at any `SUITE=`,
> but the two suite vectors cover the same ground and do run.
>
> **Disposition: FIXED, verified by execution. Nothing in the "Out-of-file
> patches" section remains live.** The residual worth keeping is documentary:
> the three stale table rows above.

> **RECONCILED 2026-08-12 (W7-55-record-reconciliation.md) — THREE OF THE FOUR
> "recorded, NOT applied" PATCHES ARE NOW SETTLED.**
>
> * **Patch 1** (the synthetic `SecretKeySpec` twin) — **APPLIED**, via this
>   record's own "better still: delete both registrations" option. Tombstones at
>   `native-builtins/src/phases_early.rs:14959` and `:14735`, commit `911ddb84b`.
> * **Patch 2** (`keygen_default_bits`) — **APPLIED IN PART, and the half that
>   is missing is the half the patch text names.** `keygen_default_bits` exists
>   at `native-builtins/src/phases_early.rs:14302` and is used by
>   `keygen_get_instance_named` (`:14429`, `None` ⇒ `NoSuchAlgorithmException`),
>   with a ratchet test at `native-builtins/src/jca/provider_chain.rs:4202-4238`.
>   But the two 2-arg `KeyGenerator.getInstance` overloads in
>   `register_keygen_dispatch` — `native-builtins/src/jca/cipher.rs:3300` and
>   `:3312` — **still** do `ctx.set_field(obj, 1, Value::Int(128));` with no
>   algorithm check, so the 1-arg and 2-arg paths now diverge. **This is the
>   live item.**
> * **Patch 3** (generalise ChaCha20) — **APPLIED.** `chacha20_xor` at
>   `native-builtins/src/crypto_impl.rs:1158` with four new KATs at `:5925-5996`;
>   `CipherFamily::ChaCha20`/`ChaCha20Poly1305` at
>   `native-builtins/src/jca/cipher.rs:1007`/`:1015`, admitted at `:1231-1232`,
>   advertised at `provider_chain.rs:1187-1188`. Commit `3a594f304`.
> * **Patch 4** (`keystore.rs`) — recorded only; no action was ever needed.
>
> Everything under *"What is deliberately still missing"* remains open and is
> deliberate.

> **RE-GREPPED 2026-08-12 (crypto lane, second pass). Patch 2's live half is
> still live and is now anchored; Patch 3 is complete end to end; Patch 4 is
> re-confirmed as needing nothing.**
>
> * **Patch 2 — LIVE, unchanged, and the anchors moved.** Both 2-arg
>   `KeyGenerator.getInstance` overloads in
>   `native-builtins/src/jca/cipher.rs::register_keygen_dispatch` still do
>   `ctx.set_field(obj, 1, Value::Int(128));` with no algorithm check —
>   `:3346` and `:3358` today, not `:3300`/`:3312`. `keygen_default_bits` still
>   appears nowhere in `cipher.rs`, and it is a private `fn` in
>   `phases_early.rs` (`:14489`), so reaching it needs `pub(crate)` first.
>   **The preferred fix has shifted to "delete both registrations", and the
>   reason is new evidence, not taste:** W7-39 seeded twelve real
>   `KeyGenerator` services on 2026-08-12
>   (`native-builtins/src/jca/provider_chain.rs:1418-1440`), and that file's own
>   comment at `:1401` now asserts *"`KeyGenerator` is **NOT** natively
>   intercepted in `--real-jdk` mode"* — a statement these two registrations
>   falsify. The doc comment's stated reason for the shims (the real path NPEs
>   at `service.getProvider()`) was written before the registry was seeded.
>   `cipher.rs` is outside this lane; see W7-21's Patch A for the exact text.
> **CLOSED 2026-08-12 (third pass, JCA lane). Patch 2's live half is gone.**
> Both 2-arg `KeyGenerator.getInstance` overloads, the enclosing
> `register_keygen_dispatch`, and its call site were **deleted** from
> `native-builtins/src/jca/cipher.rs` — the "delete both registrations" option
> the pass above shifted to, taken for its reason. `keygen_default_bits` stays
> private in `phases_early.rs`; nothing needed it, because nothing replaces the
> shims. Two corrections to the block above, both from re-reading the source
> rather than the record:
>
> * the seed is **thirteen** `SunJCE` `KeyGenerator` services, not twelve —
>   `AES, ARCFOUR, Blowfish, ChaCha20, DES, DESede, HmacMD5, HmacSHA1,
>   HmacSHA224, HmacSHA256, HmacSHA384, HmacSHA512, RC2`;
> * the deletion is safe because of a fact neither pass states: the
>   `sun/security/jca/GetInstance` bridges that answer the real path are gated
>   on `ec_real`, which is `real_jca_mode() || route_ec_to_real() ||
>   route_dsa_to_real()` — and `route_ec_to_real()` is **default ON**
>   (`CRATONVM_SYNTHETIC_EC=1` is its kill switch). Both the named-provider
>   `getService(String,String,String)` and the search overload are registered
>   there. Had that gate been off in shipping builds, deleting the shims would
>   have restored the `service.getProvider()` NPE the doc comment described.
>
> The Java-side cover is `RCrypto`'s `keygen2arg` line: `KeyGenerator
> .getInstance("AES","SunJCE").generateKey().getEncoded()` must be **32 bytes**
> (SunJCE's JDK 25 AES default is 256-bit, measured — the deleted shim
> hardcoded 128) and must not be all zeros.
>
> * **Patch 3 — COMPLETE, and wider than this record asked for.** It did not
>   land as a generalisation of `crypto_impl::chacha20_keystream_fill`; a
>   concurrent lane landed a whole `native-builtins/src/chacha20.rs` carrying
>   ChaCha20, **Poly1305**, and the RFC 8439 §2.8 AEAD, with the RFC's own
>   vectors including **§2.5.2**, both §A.3 reduction edges, five tampering arms
>   and a differential against the `chacha20poly1305` crate. So this record's
>   *"`ChaCha20-Poly1305` is refused, not approximated, and will stay refused
>   until a real Poly1305 exists in this tree"* is **discharged**: the Poly1305
>   exists, the AEAD is wired at `jca/cipher.rs:2282`/`:2296`, and both names are
>   advertised again at `provider_chain.rs:1226-1227`. The "Removed —
>   advertised, computed by nothing" table below is therefore **historical**: all
>   four names went back in on 2026-08-11 once they were computed, which is the
>   order this record insisted on and got.
> * **Patch 4 — re-confirmed, still nothing to do.** `native-builtins/src/
>   keystore.rs`'s two `ChaCha20` references are about the PKCS#12 secret-key
>   OID table (`SECRET_KEY_ALG_OIDS`), and real JDK 25's
>   `AlgorithmId.get("ChaCha20")` raises `NoSuchAlgorithmException` regardless of
>   whether `Cipher` serves the name. Implementing the cipher does **not** make
>   the OID encodable, so the test asserting `setEntry` fails and names the
>   algorithm stays correct. Recorded again because Patch 3 landing is exactly
>   the event that makes the next reader want to "fix" it.
>
> **The coverage gap this record left, now closed in the tree and still
> unrun:** `regression-suite/src/RChaCha20Cipher.java` is named in `run.sh`'s
> `CORE_CLASSES` and cited by `provider_chain.rs:1187` as matching HotSpot
> byte-for-byte, but the file was **untracked**, so `prune_missing` dropped it
> from every scheduled run. It is now in the tree. See
> W7-38-crypto-trio-verified.md.

**Status:** FIXED in source 2026-08-11 (lane W7-15). **Nothing was rebuilt** —
this lane could not run `cargo build`, so every claim below is either a
measurement taken against the *pre-fix* release binary at
`target/release/cratonvm.exe` (dated 2026-08-11 19:41) and HotSpot 25, or a
statement about source. No claim is made that the new code works.

Opened by the advertised-versus-implemented census in the residual pass of
W4-3-security-getalgorithms-short-list.md, which named it "the single worst
thing this lane found" and sketched it as Patch E. The measurement below is
wider than that sketch in three directions, and the fix is correspondingly
wider.

## The failure

`javax.crypto.Cipher` accepted an algorithm name, discarded it, and dispatched
on the transformation's **mode** alone. `parse_transformation` defaults an
absent mode to `ECB`. So `Cipher.getInstance("ChaCha20")` — a stream cipher with
no ECB mode, no block, and a mandatory nonce — became AES-256-ECB, because a
32-byte ChaCha20 key is a valid AES-256 key and `Aes::key_expansion` accepted it
instead of erroring.

Measured. Same fixed 32-byte key (`0x10..0x2f`), same fixed 12-byte nonce, same
32-byte plaintext, no `SecureRandom` anywhere, so the runs are diffable:

```
CratonVM  ChaCha20             ct[48] = c27bed76770d7897735157e3d11726f5 c27bed76770d7897735157e3d11726f5 d70cfea1a650370ce46d7431e48f62cd
CratonVM  ChaCha20-Poly1305    ct[48] = c27bed76770d7897735157e3d11726f5 c27bed76770d7897735157e3d11726f5 d70cfea1a650370ce46d7431e48f62cd
CratonVM  AES/ECB/PKCS5Padding ct[48] = c27bed76770d7897735157e3d11726f5 c27bed76770d7897735157e3d11726f5 d70cfea1a650370ce46d7431e48f62cd
HotSpot   AES/ECB/PKCS5Padding ct[48] = c27bed76770d7897735157e3d11726f5 c27bed76770d7897735157e3d11726f5 d70cfea1a650370ce46d7431e48f62cd
HotSpot   ChaCha20             ct[32] = ed7e0180b33c00ac3e8f765cb17e83b2 30839fc13b72f0821032a63d172b93d7
HotSpot   ChaCha20-Poly1305    ct[48] = ed7e0180b33c00ac3e8f765cb17e83b2 30839fc13b72f0821032a63d172b93d7 31645850f1cb9ac3416cfacfcca9476c
```

Byte-identical to AES-ECB, and identical to each other. Note the repeated first
and second blocks in every CratonVM row: that is ECB's signature, the plaintext
being two identical 16-byte halves. The nonce made no difference — passing a
`ChaCha20ParameterSpec` and passing nothing produced the same ciphertext.

Three separate defects composed into it, and each is independently a defect:

1. **the requested algorithm name was discarded.** `cipher_do_final_impl` began
   `let (_cipher_name, mode_str, pad) = parse_transformation(&algo);` — the name
   was parsed out and bound to a discard;
2. **an absent mode defaulted to ECB**, the one mode a caller almost never
   wants, for *every* algorithm rather than only for the one family that has
   such a default;
3. **an AEAD transformation was served by a non-authenticating cipher.**
   `ChaCha20-Poly1305` produced no tag at all, so `doFinal` had nothing to
   verify and could not fail.

The third is the one that matters most, and it was measured directly. Decrypt a
ciphertext with one bit flipped:

```
CratonVM  ChaCha20-Poly1305   TAMPERED ACCEPTED pt[32]=1488cc8b10b83c815ab758c982dc9bd330313233343536373839616263646566
HotSpot   ChaCha20-Poly1305   javax.crypto.AEADBadTagException: Tag mismatch
```

CratonVM returned 32 bytes and raised nothing anywhere on the path. A caller
that built an integrity guarantee on that AEAD had no guarantee and no way to
find out. **A cipher that cannot fail on a bad tag is worse than a missing
cipher**, which is why this fix refuses rather than approximates.

### It was never only ChaCha20

The accept gate, `cipher_algorithm_known`, listed about forty algorithm names
and its own doc comment argued that being over-inclusive was safe:

> Deliberately over-inclusive on the accept side … the failure mode a too-narrow
> list would produce is a `NoSuchAlgorithmException` for valid input, which is
> strictly worse than the fabrication being fixed.

Measurement falsified that sentence. An accepted name this engine cannot compute
did not fail — it produced AES:

| transformation | CratonVM, before | HotSpot 25 |
|---|---|---|
| `Blowfish` | `0a8c098c8be55dbc…` (AES-128-ECB) | `ea5c0b7ed1fdfe93…` (real Blowfish) |
| `RC4` | `0a8c098c8be55dbc…` — **the same bytes as Blowfish** | `9caf410918301275…` (real RC4) |
| `AES/CBC/PKCS7Padding` | served as PKCS5 | `NoSuchAlgorithmException` — HotSpot has no such padding |
| `AES/CBC/ISO10126Padding` | served as PKCS5 | served with **random** padding bytes |
| `AES_128/GCM/NoPadding` + 256-bit key | encrypted with AES-**256** | `InvalidKeyException: The key must be 16 bytes` |
| `AES/GCM/NoPadding` + 17-byte key | `init` returned OK | `InvalidKeyException: Invalid AES key length: 17 bytes` |
| `AES/CBC` (two tokens) | accepted, padded CBC | `NoSuchAlgorithmException: Invalid transformation format:AES/CBC` |
| `AES/CBC/` | accepted, padded CBC | `NoSuchAlgorithmException: Invalid transformation: missing mode and/or padding-AES/CBC/` |

Blowfish and RC4 answering with the *same* ciphertext is the whole defect in one
line: two different algorithms, one shared 16-byte key, and the engine ran
AES-128-ECB for both.

## A second defect, found while measuring the first: an all-zero AES key

`KeyGenerator.getInstance("AES").generateKey()` returned a key of **all zero
bytes** — correct length, correct algorithm name, no exception anywhere:

```
KeyGenerator.getInstance("AES").generateKey().getEncoded()
  CratonVM 000000000000000000000000000000000000000000000000000000000000000
  HotSpot  a55a06fd157aca61fea2322615a02b6c71c01805cbefca7b89ea10b26efc1ace
```

Identical for `HmacSHA256`. Both arms, `--real-jdk` and `--jdk-only`. Two calls
to `generateKey()` returned the same zeros, and `init(128)` / `init(256)` only
changed the length.

The root cause is not in `KeyGenerator` at all. `SecretKeySpec.<init>` stored the
caller's `byte[]` **by reference**, where the real class is `this.key =
key.clone()` and the javadoc says why: "The contents of the array are copied to
protect against subsequent modification." SunJCE's own generators depend on that
copy — `AESKeyGenerator.engineGenerateKey` is

```java
byte[] keyBytes = new byte[keySize];
this.random.nextBytes(keyBytes);
aesKey = new SecretKeySpec(keyBytes, "AES");
Arrays.fill(keyBytes, (byte)0);     // scrub the working buffer
return aesKey;
```

and `KeyGeneratorCore.implGenerateKey` does the same in a `finally`. The scrub
landed on the key. `DESede` escaped only because `DESedeKeyGenerator` happens not
to scrub, which is why the W4-3 census — reading source, not running it — saw
`DESede` as the suspicious one and missed this entirely.

The same aliasing was measured on three accessors, all of which the real classes
`clone()`:

```
                       CratonVM                          HotSpot
SecretKeySpec after caller scrubs its array   all zeros   unchanged
SecretKeySpec.getEncoded(), then fill(0x99)   0x99s       unchanged
IvParameterSpec.getIV(),   then fill(0x77)    0x77s       unchanged
GCMParameterSpec.getIV(),  then fill(0x55)    0x55s       unchanged
```

The two IV constructors already copied; only the accessors leaked. This is not a
hygiene nicety — it is the mechanism by which the platform's own key generator
produced a zero key.

## How it was measured

Four Java probes, all printing **bytes rather than verdicts** so the two runs
diff line for line; a probe that prints `ok` cannot diff. Run against the
pre-built binary and the oracle:

```
javac -d . CipherCensus.java
"C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot/bin/java" -cp . CipherCensus
target/release/cratonvm.exe --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot" -cp . CipherCensus
target/release/cratonvm.exe --jdk-only --java-home "…" -cp . CipherCensus
```

`--real-jdk` and `--jdk-only` produced identical output on every row, which is
expected: `register_cipher_clinit_shim` is reached from
`register_essential_natives_with_shims`, so this engine is live in both.

The JDK 25 `src.zip` on this host was the oracle for every spec claim
(`javax/crypto/Cipher.java`, `javax/crypto/spec/SecretKeySpec.java`,
`com/sun/crypto/provider/AESKeyGenerator.java`, `KeyGeneratorCore.java`,
`RSACipher.java`).

## The fix

### 1. One admission table, no default arm

`classify_transformation` replaces `cipher_algorithm_known`. It runs
`tokenize_transformation` — a restatement of the JDK's own
`Cipher.tokenizeTransformation`, including the `SHA512/2` guard that stops
`PBEWithHmacSHA512/224AndAES_128` being split at the slash inside its own name —
and then dispatches on a **family**, with no `_ =>` arm on the algorithm match.
The mode and padding sets are per-family properties, so the ECB default survives
exactly where SunJCE really has it (a bare `AES` is `AES/ECB/PKCS5Padding`;
measured, HotSpot's ciphertext for the two is byte-identical) and nowhere else.

This is the shape W4-3's `Mac` lane established in
`phases_late/ssl_security.rs`: an explicit table matching what `provider_chain`
advertises, `Option`/enum returns, no fallback.

### 2. Refusals in the specified exception, in HotSpot's measured wording

The specifying sentences, from every `getInstance` overload's javadoc:

> `@throws NoSuchAlgorithmException` if `transformation` is `null`, empty, in an
> invalid format, or if no provider supports a `CipherSpi` implementation for
> the specified algorithm

> `@throws NoSuchPaddingException` if `transformation` contains a padding scheme
> that is not available

The wording differs by overload and was measured, not recalled:

| shape | anonymous `getInstance(String)` | `getInstance(String, String\|Provider)` |
|---|---|---|
| unknown algorithm | `NoSuchAlgorithmException: Cannot find any provider supporting <t>` | `NoSuchAlgorithmException: No such algorithm: <t>` |
| unavailable mode | same as unknown algorithm | same as unknown algorithm |
| unavailable padding | same as unknown algorithm — the anonymous form **never** raises `NoSuchPaddingException`; it walks the chain, skips the service, and falls out the bottom | `NoSuchPaddingException: Padding not supported: <padding>` |
| malformed | `Invalid transformation format:<t>` · `Invalid transformation: missing mode and/or padding-<t>` · `Invalid transformation: algorithm not specified-<t>` | identical (tokenizing precedes provider lookup) |
| empty/null | `Null or empty transformation` | identical |

### 3. The AEAD rule

`ChaCha20-Poly1305` is **refused**, not approximated, and will stay refused until
a real Poly1305 exists in this tree — see the out-of-file section. `AES/GCM` is
the one AEAD this engine implements, and it implements it properly: measured
byte-identical to HotSpot, `AEADBadTagException: Tag mismatch` on a flipped bit,
`AEADBadTagException: Input too short - need tag` on a truncated input.

### 4. `AES/KW` now answers the same on both surfaces

`AES/KW/NoPadding` was advertised, worked through `wrap()` — byte-identical to
HotSpot: `bc3b4783f41958fdef7f32ef69f09086da1a6666e883f93f` for a 256-bit KEK
over a 16-byte key — and raised an unchecked `IllegalStateException` through
`doFinal()`. One algorithm, two answers, depending on which method the caller
reached for. `doFinal` now runs the same RFC 3394 wrap/unwrap and reports
failure as the checked `IllegalBlockSizeException` SunJCE uses (measured:
`javax.crypto.IllegalBlockSizeException: Integrity check failed`). The detail
text is this module's own, which is more specific than HotSpot's; the class is
what a handler selects on.

`is_aes_key_wrap_transformation` now asks the admission table instead of
restating a second literal list, which had drifted in both directions — it
carried `AESWRAP128` (no underscore) and `AES/KW` (two tokens), neither of which
`getInstance` accepts, and omitted `AES_128/KW/NoPadding`, which SunJCE
advertises and this engine can serve.

### 5. Size-suffixed names pin the key length

`AES_128` means "AES with a 128-bit key". The suffix was decorative:
`AES_128/GCM/NoPadding` initialised happily from a 256-bit key. `Cipher.init`
declares `InvalidKeyException` for exactly this, and now raises it, in HotSpot's
wording (`The key must be 16 bytes`; and `Invalid AES key length: 17 bytes` for
plain AES).

### 6. `SecretKeySpec` and the spec accessors copy

`<init>` clones the key; `getEncoded()`, `IvParameterSpec.getIV()` and
`GCMParameterSpec.getIV()` return clones. This is the all-zero-key fix.

## Before and after, per transformation

"Before" is measured. **"After" is source-level intent — nothing was rebuilt.**

| transformation | before (measured) | after (intended) |
|---|---|---|
| `ChaCha20` | AES-256-ECB, nonce discarded | `NoSuchAlgorithmException` |
| `ChaCha20-Poly1305` | AES-256-ECB, **no tag**, tampering accepted | `NoSuchAlgorithmException` |
| `Blowfish`, `RC4`, `ARCFOUR`, `RC2`, `IDEA`, `SEED`, `SM4`, `Camellia`, `Twofish`, `Serpent`, `CAST5/6`, `Salsa20`, `Skipjack`, `ECIES`, `ElGamal`, `NULL`, … | AES-ECB at the key's length | `NoSuchAlgorithmException` |
| `AES`, `AES/ECB/{PKCS5Padding,NoPadding}` | correct, = HotSpot | unchanged |
| `AES/CBC/{PKCS5Padding,NoPadding}` | correct, = HotSpot | unchanged |
| `AES/CFB/…`, `AES/OFB/…` | correct, = HotSpot | unchanged |
| `AES/GCM/NoPadding` | correct, = HotSpot, tag verified | unchanged |
| `AES/CBC/PKCS7Padding` | served as PKCS5 | `NoSuchPaddingException` (anonymous form: `Cannot find any provider supporting`) |
| `AES/CBC/ISO10126Padding` | served as PKCS5 | as above |
| `AES/CTR/NoPadding` | unchecked `IllegalStateException` at `doFinal` | `NoSuchAlgorithmException` at `getInstance` |
| `AES/{CTS,PCBC,CFB8}/…` | unchecked ISE at `doFinal` | `NoSuchAlgorithmException` at `getInstance` |
| `AES/CCM/NoPadding` | `NoSuchAlgorithmException` | unchanged |
| `AES/KW/NoPadding` — `wrap`/`unwrap` | correct, = HotSpot | unchanged |
| `AES/KW/NoPadding` — `doFinal` | unchecked ISE | RFC 3394 wrap/unwrap |
| `AES/KW/PKCS5Padding` | advertised; unchecked ISE | `NoSuchPaddingException`, and unadvertised |
| `AES/KWP/NoPadding` | advertised; unchecked ISE | `NoSuchAlgorithmException`, and unadvertised |
| `AESWrap`, `AESWrap_128/192/256` | `wrap`/`unwrap` correct; `doFinal` was AES-ECB | `doFinal` is RFC 3394 too |
| `AES_128/CBC/NoPadding` etc. | unchecked ISE (`mode 'CBC' not implemented`) | routed like `AES/CBC` |
| `AES_128/GCM/NoPadding` + wrong-size key | AES-256 under an AES-128 name | `InvalidKeyException` |
| `AES_128` (bare) | AES-ECB | `NoSuchAlgorithmException` (HotSpot refuses it too) |
| `DES/CBC/…`, `DESede/CBC/…` | correct, = HotSpot | unchanged, and now advertised |
| `DESede` (bare), `DESede/ECB/…` | served as **CBC** under an ECB name | `NoSuchAlgorithmException` |
| `RSA`, `RSA/ECB/{PKCS1Padding,OAEPWithSHA-1…,OAEPWithSHA-256…,OAEPPadding}` | correct | unchanged |
| `RSA/ECB/NoPadding`, `…OAEPWithSHA-512…` | unchecked ISE at `doFinal` | `NoSuchPaddingException` |
| `RSA/None/…` | AES-ECB | `NoSuchAlgorithmException` (matches `RSACipher.engineSetMode`) |
| `PBEWithHmacSHA{1,224,256}AndAES_{128,256}` | correct | unchanged; the 224 pair is now advertised |
| `AES/CBC` · `AES/CBC/` · `/CBC/NoPadding` · `""` | accepted, cipher fabricated | the JDK's four `Invalid transformation…` messages |
| `CRATONVM-NO-SUCH-CIPHER` | `NoSuchAlgorithmException` | unchanged (wording now HotSpot's) |
| `KeyGenerator.getInstance("AES").generateKey()` | **32 zero bytes** | random, via the `SecretKeySpec` copy |

## Advertised versus implemented, reconciled

`provider_chain::seed_direct_native_engine_services` seeds the `SunJCE`
`Cipher` services that `Security.getAlgorithms("Cipher")`,
`Provider.getService` and `check_provider_ownership` all answer from.

**Removed — advertised, computed by nothing:**

| name | why |
|---|---|
| `ChaCha20` | no ChaCha20 reachable from `Cipher`; was AES-ECB |
| `ChaCha20-Poly1305` | as above, and no Poly1305 exists in this tree at all |
| `AES/KW/PKCS5Padding` | no path implements the padded variant |
| `AES/KWP/NoPadding` | RFC 5649 is a different scheme (own ICV, length prefix), not RFC 3394 with padding bolted on |

**Added — computed and never advertised, the quieter half of the same defect:**

| name | evidence |
|---|---|
| `DES/CBC/{NoPadding,PKCS5Padding}`, `DESede/CBC/{NoPadding,PKCS5Padding}` | routed to the real SunJCE SPI; measured byte-identical to HotSpot |
| `PBEWithHmacSHA224AndAES_{128,256}` | `pbes2_aes_params` has always derived them; HotSpot advertises them |
| `AES_{128,192,256}/{CBC,CFB,ECB,GCM,KW,OFB}/NoPadding` | the size-pinned family, now with key-length enforcement |
| aliases `AESWrap_{128,192,256}`, `TripleDES` | SunJCE's own aliases; aliases are excluded from `getAlgorithms`, so they widen resolution without lengthening the list |

DES/DESede are spelled in full where HotSpot carries the bare name plus a
`SupportedModes` attribute, because this engine routes only CBC and the bare
name defaults to ECB. Naming what we serve beats matching HotSpot's grouping —
the invariant that matters is that every advertised name resolves.

**The reconciliation is now a ratchet, not a census.**
`provider_chain::every_advertised_sunjce_cipher_is_serviceable` walks the seeded
`SunJCE` `Cipher` services and asserts `Cipher.getInstance` admits each one, and
that the four removed names are neither advertised nor serviceable. A census run
by hand drifts by the next wave; this one fails the build.

## The Compatible-mode behaviour change, and every in-tree caller

`register_cipher_clinit_shim` is reached from
`register_essential_natives_with_shims`, so **this is live in `Compatible`,
`--jdk-only` and synthetic modes alike.**

The contract requires `Compatible` to stay byte-for-byte unchanged, so the
exception is stated explicitly, as W4-3's `Mac` lane did: **the behaviour
changed is "returns ciphertext from a different algorithm" to "raises
`NoSuchAlgorithmException`/`NoSuchPaddingException`/`InvalidKeyException`".** It
is worth taking. A wrong cipher is worse than a missing one, because the caller
cannot catch it, cannot detect it, and cannot decrypt the result anywhere else.

Every in-tree Java caller of `Cipher.getInstance`, and its verdict:

| caller | transformation | affected? |
|---|---|---|
| `regression-suite/src/RCrypto.java:33,36` | `AES/GCM/NoPadding` | no |
| `regression-suite/src/RCrypto.java:45,48` | `RSA/ECB/OAEPWithSHA-256AndMGF1Padding` | no |
| `regression-suite/src/RCrypto.java:52,55` | `RSA/ECB/PKCS1Padding` | no |
| `regression-suite/src/RJdkSecurity.java:225,230` | `AES/GCM/NoPadding` | no |
| `regression-suite/src/RJdkFailure.java:309` | `CRATONVM-NO-SUCH-CIPHER` | no — it asserts the exception **class**, which is unchanged; only the message text moves, from `No such algorithm: …` to HotSpot's `Cannot find any provider supporting …` |
| `probes/JdkOnlyPlatformProbe.java:209,212` | `AES/GCM/NoPadding` | no |
| `probes/JdkOnlyPlatformProbe.java:216` | `AES/CBC/PKCS5Padding` | no |
| `vm/tests/resources/cratonvm/TckSecurity.java:156,163` | `AES/GCM/NoPadding` | no |
| `apps/keycloak-suite-runner/AesWrap128Probe.java:32,39` | `AESWrap_128` | no — still admitted, and `wrap`/`unwrap` are untouched |
| comparison-handoff/keycloak-ecspec-probes/CryptoSmoke.java:21,25 (internal tree) | `AES/GCM/NoPadding` | no |
| fixed-suite-bugs/repros/jca-provider-lookup-parity/ProviderLookupProbe.java:63-91 (internal tree) | `AES/CBC/PKCS5Padding` with four provider arguments | no — the transformation stays admitted and the provider-argument paths are untouched |

**No in-tree Java caller is broken by this change.** Nothing in the tree asks
for ChaCha20, Blowfish, RC4, `AES/CTR`, `AES/KWP`, `AES/KW/PKCS5Padding`, a
size-suffixed AES name, or a malformed transformation.

The out-of-tree risk is a corpus application (Keycloak, Elytron, Tomcat,
BouncyCastle-driven code) that asks for one of the refused names. Such an
application is today receiving AES ciphertext under another algorithm's name; it
will now receive a catchable `NoSuchAlgorithmException` naming the algorithm at
the `getInstance` call site. That is a louder failure and a truthful one.

The two Rust unit tests that pinned the old shapes were re-checked:
`aes_key_wrap_recognises_sunjce_and_keycloak_names` still holds
(`AESWrap`, `AESWrap_128`, `AES/KW/NoPadding` admitted; `AES/KWP/NoPadding`
refused), and the `parse_transformation` tests are untouched — that function
still behaves exactly as before, it is simply no longer the gate.

## Out-of-file patches — recorded, NOT applied

Code is exact. Line numbers are omitted because they rot; anchor on the
enclosing function name.

### Patch 1 — the synthetic-mode twin of the all-zero key (`phases_early.rs`)

`native-builtins/src/phases_early.rs`, `register_phase53_crypto`, registers a
**second** `javax/crypto/spec/SecretKeySpec` with the identical aliasing bug:

```rust
    r.register(sks, "<init>", "([BLjava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let key_bytes = obj_arg(args, 1)?;
        let algo = obj_arg(args, 2)?;
        ctx.set_field(this, 0, Value::Object(Some(key_bytes)));   // <-- aliases
        ctx.set_field(this, 1, Value::Object(Some(algo)));
        Ok(Some(Value::Object(None)))
    });
    r.register(sks, "getEncoded", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))                          // <-- aliases
    });
```

That phase runs **after** `register_essential_natives`, and registration is
last-write-wins, so under `--synthetic-jdk` this one shadows the fixed copy in
`jca/cipher.rs` and the zero-key defect survives there. Apply the same two
changes:

```rust
    r.register(sks, "<init>", "([BLjava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let key_bytes = obj_arg(args, 1)?;
        let algo = obj_arg(args, 2)?;
        // Real `SecretKeySpec.<init>` is `this.key = key.clone()`, and SunJCE's
        // own key generators scrub their working buffer immediately after
        // constructing the key (`AESKeyGenerator.engineGenerateKey`,
        // `KeyGeneratorCore.implGenerateKey`). Storing the caller's array by
        // reference therefore produced an ALL-ZERO AES key — measured, both
        // JDK modes, before the twin of this fix landed in `jca/cipher.rs`.
        let raw = read_bytes(ctx, key_bytes);
        let this_pin = ctx.pin_native_root(this);
        let algo_pin = ctx.pin_native_root(algo);
        let copy = make_bytes_array(ctx, &raw);
        let this = ctx.read_native_pin(this_pin, this);
        let algo = ctx.read_native_pin(algo_pin, algo);
        ctx.unpin_native_roots(this_pin);
        ctx.set_field(this, 0, Value::Object(Some(copy)));
        ctx.set_field(this, 1, Value::Object(Some(algo)));
        Ok(Some(Value::Object(None)))
    });
```

and have `getEncoded` return a fresh copy the same way. (`read_bytes` /
`make_bytes_array` exist in `jca/cipher.rs`; `phases_early` has equivalents —
use whichever that module already carries rather than importing across.)

**Better still: delete both registrations.** They are byte-for-byte weaker
duplicates of the `jca/cipher.rs` pair, which is live in every mode. The two
`javax/crypto/spec/SecretKeySpec` registrars are the same shadowing species
recorded in W4-3's "Sibling surfaces, fixed in passing" section.

**Mode: synthetic-jdk in practice, all modes structurally.**

### Patch 2 — `KeyGenerator` ignores its algorithm (`phases_early.rs`, `jca/cipher.rs`)

Named but not patched by W4-3; still live, and now measured:

```
                CratonVM                       HotSpot 25
AES        len=32, all zeros              len=32, random
DESede     len=24, random                 len=24, random
HmacSHA256 len=32, all zeros              len=32, random
Blowfish   NoSuchAlgorithmException       len=16, random
ChaCha20   NoSuchAlgorithmException       len=32, random
```

Two of the census's claims are corrected by this measurement: `DESede` yields
**24** bytes, not 16, and it is `AES`/`HmacSHA256` that are catastrophic rather
than `DESede`. The three names that resolve are exactly the three
`seed_retired_getalgorithms_literals` seeds, so the advertised set and
`getInstance` already agree; what does not agree is `generateKey`.

The remaining defect after Patch 1 is the default size. `phases_early`'s
`generateKey` reads field 1, which both `getInstance` shims in
`jca/cipher.rs::register_keygen_dispatch` hardcode to `128`:

```rust
            ctx.set_field(obj, 1, Value::Int(128)); // default key size
```

SunJCE's defaults are per-algorithm — AES 256 (JDK 21+), DESede 168, HmacSHA256
256 — so a caller that omits `init(int)` gets a 128-bit key where HotSpot gives
256. Fix in `jca/cipher.rs::register_keygen_dispatch` by reading the algorithm
argument, which those shims already have in hand:

```rust
/// SunJCE's per-algorithm default key size in BITS. `KeyGenerator.getInstance`
/// hardcoded 128 for every algorithm, so `generateKey()` without an explicit
/// `init(int)` produced a 128-bit AES key where HotSpot 25 produces 256
/// (measured: `getEncoded().length == 32`), and a 128-bit DESede key, which is
/// not a valid DESede key length at all.
fn keygen_default_bits(algo: &str) -> Option<i32> {
    match algo.to_ascii_uppercase().as_str() {
        "AES" => Some(256),
        "DESEDE" | "TRIPLEDES" => Some(168),
        "HMACSHA256" => Some(256),
        _ => None,
    }
}
```

`None` must mean refuse, not default — the same rule as the cipher table. Note
DESede's 168 must produce **24** bytes (the parity bits are carried), so the
`key_size / 8` in `phases_early`'s `generateKey` needs a per-algorithm rule too,
and its output must have the DES parity bits set to match HotSpot byte-for-byte.
That is genuine crypto work and should not be landed unverified.

### Patch 3 — implement ChaCha20 (`crypto_impl.rs`), then advertise it

This tree already contains a correct RFC 8439 ChaCha20 core:
`native-builtins/src/crypto_impl.rs::chacha20_keystream_fill`, with an RFC 7539
known-answer test beside it (`chacha20_keystream_rfc7539_zero_key`). It is
private and used only as `secure_random_fill`'s software fallback. Two things
stand between it and a working `Cipher.getInstance("ChaCha20")`:

1. it **writes** its keystream into the buffer rather than XOR-ing, and
2. its block counter is hardcoded to start at 0, whereas JCA's
   `ChaCha20ParameterSpec(nonce, counter)` lets the caller choose it (and
   HotSpot's `ChaCha20-Poly1305` uses counter 0 for the Poly1305 key block and 1
   for the data).

So the patch is to generalise it:

```rust
/// RFC 8439 ChaCha20, XOR-ing the keystream into `buf` starting from block
/// `initial_counter`. `chacha20_keystream_fill` is this function against an
/// implicit zero plaintext with `initial_counter == 0`; keep them one
/// implementation so the RFC 7539 KAT covers both.
pub(crate) fn chacha20_xor(key: &[u8; 32], nonce: &[u8; 12], initial_counter: u32, buf: &mut [u8]) {
    // …existing body, with `let mut counter = initial_counter;` and
    // `buf[pos + i] ^= block[i];` in place of the copy_from_slice…
}
```

then a `CipherFamily::ChaCha20` arm in `jca/cipher.rs` reading the counter from
`ChaCha20ParameterSpec` field 1 and the 12-byte nonce from field 0.

**Do not do this for `ChaCha20-Poly1305`.** There is no Poly1305 in this tree
(`app_shims.rs`'s mention is a JIT-ban note about BouncyCastle's *own* bytecode,
not a Rust implementation), and an AEAD without its authenticator is the exact
defect this record exists for. Refused is the correct state for
`ChaCha20-Poly1305` until Poly1305 lands with its RFC 8439 §2.5.2 test vector.

Land the implementation and its vectors first, then add the name back to the
`SunJCE` `Cipher` seed and delete the corresponding arm of
`every_advertised_sunjce_cipher_is_serviceable`. Implement first, advertise
second; the order is not negotiable, and the test enforces it.

### Patch 4 — `keystore.rs` and `AlgorithmId.get("ChaCha20")`

`native-builtins/src/keystore.rs` refers to `ChaCha20` in two places — a comment
about `AlgorithmId.get("ChaCha20")` raising `NoSuchAlgorithmException`, and a
test (`store_of(vec![secret_entry("x", &[0u8; 16], "ChaCha20")])`) asserting that
`setEntry` with a `ChaCha20` `SecretKeySpec` fails and names the algorithm. Both
are **unaffected** by this lane: they are about the PKCS#12 algorithm-OID table,
not about `Cipher.getInstance`, and both already expect a refusal. Recorded here
only so the next reader does not mistake them for a fifth ChaCha20 surface.

## What is deliberately still missing

Under-serving is the trade this fix takes, and it should be visible. HotSpot's
SunJCE answers 60 `Cipher` names; this engine answers the families above.
Refusing the remainder is truthful precisely *because* `getInstance` refuses
them. The gaps worth closing, in rough order of how often real code asks:

* **`AES/CTR/NoPadding`** — a genuinely common mode, and `Aes::encrypt_block`
  already provides everything it needs. Not implemented here because this lane
  could not build or run, and the counter-increment rule (SunJCE increments the
  full 128-bit block as a big-endian integer) is exactly the sort of detail that
  produces confidently wrong crypto if landed unverified.
* **`AES/KWP/NoPadding`** — RFC 5649, a genuinely different scheme from the
  RFC 3394 already present.
* **`AES/{CTS,PCBC,CFB8,OFB8}`** — real SunJCE modes; `drive_real_cipher`
  probably reaches them with only a widened route table, but that is unmeasured.
* **`ISO10126Padding`** — needs a CSPRNG-filled pad, which this tree has.
* **`Blowfish`, `RC4`/`ARCFOUR`, `RC2`, `DESedeWrap`** — real SunJCE algorithms
  with no implementation here. All legacy; none is worth adding speculatively.
* **`RSA/ECB/NoPadding`** and the SHA-384/512 OAEP variants — bounded work in
  `crypto_impl::RsaCipherPadding`.
* **`AES_nnn/KWP/NoPadding`, `AES_nnn/KW/PKCS5Padding`** — follow KW/KWP above.

## How to verify

Rebuild, then re-run the four probes in both arms. The rows that must change:

```
ROW ChaCha20 …            | getInstance=java.security.NoSuchAlgorithmException: Cannot find any provider supporting ChaCha20
ROW ChaCha20-Poly1305 …   | getInstance=java.security.NoSuchAlgorithmException: Cannot find any provider supporting ChaCha20-Poly1305
ROW Blowfish …            | getInstance=java.security.NoSuchAlgorithmException: Cannot find any provider supporting Blowfish
ROW AES/KW/NoPadding …    | ct[40]=f4c07bbb3da0dc5a56836b8889bb65b674c19cd29e89510222699f92e377c75ca5cca17e5b57441b
KEYGEN AES len=32 bytes=<not zeros, different on every run>
```

and the rows that must **not**: every `AES/GCM/NoPadding`, `AES/CBC/*`,
`AES/ECB/*`, `AES/CFB/*`, `AES/OFB/*`, `DES*/CBC/*`, `RSA*` and `AESWrap*`
ciphertext must be byte-identical to what the pre-fix binary produced, which is
also what HotSpot produces.

The three corpus vectors that touch this code were baselined on the **pre-fix**
binary, `--jdk-only`, so the rebuild has a number to match rather than a
sentiment:

```
PASS RCrypto (7 checks)
PASS RJdkFailure (43 checks)
PASS RJdkSecurity (61 checks)
```

All three must still pass, with the same check counts. `RJdkFailure` is the one
that exercises a refusal (`CRATONVM-NO-SUCH-CIPHER`) and it asserts the
exception class, so the message change does not reach it. `regression-suite/run.sh`
must stay at its baseline in both modes.

## The single falsifying observation

If a rebuilt binary refuses `AES/GCM/NoPadding`, `AES/CBC/PKCS5Padding` or
`AESWrap_128`, then `classify_transformation`'s family table is narrower than
the code it gates and the fix has traded one silent defect for a loud
regression — the failure mode `cipher_algorithm_known`'s doc comment warned
about, which was the wrong worry for the *old* gate and the right one for this
one.

If instead `KeyGenerator.getInstance("AES").generateKey()` still returns zeros,
then a third `javax/crypto/spec/SecretKeySpec` registrar is winning over both
the fixed one and the one in Patch 1; find it with `--dump-native-registry`
before changing anything else.
