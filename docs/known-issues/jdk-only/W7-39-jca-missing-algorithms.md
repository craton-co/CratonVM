# W7-39 — the JCA algorithms that refused, and the advertised/implemented reconciliation

> ## Re-verified in source 2026-08-12 (lane A3) — STILL "SOURCE LANDED, NOT RE-MEASURED"
>
> **This pass did not build or run anything either**, so the header's status is
> unchanged and no CratonVM column in the body has been promoted to a
> measurement. What was done is a check that the source this record claims to
> have landed is actually in the tree, because this campaign has repeatedly
> found records claiming a patch was never applied when it was, and the
> converse.
>
> Present, by symbol:
>
> | claim | symbol | file |
> |---|---|---|
> | Blowfish/RC4 route to the real SunJCE SPI through a second, ECB-shaped driver | `drive_real_ecb_cipher`, `real_spi_ecb_route` | `native-builtins/src/jca/cipher.rs` |
> | one `cipher_block_size` answers `getBlockSize` and `getOutputSize` | `cipher_block_size` | `native-builtins/src/jca/cipher.rs` |
> | the `Mac` ratchet | `every_advertised_sunjce_mac_is_computable` | `native-builtins/src/jca/provider_chain.rs` |
> | the `KeyGenerator` ratchet | `every_keygenerator_the_engine_implements_is_advertised` | `native-builtins/src/jca/provider_chain.rs` |
> | the `Cipher` ratchet (pre-existing, extended) | `every_advertised_sunjce_cipher_is_serviceable` | `native-builtins/src/jca/provider_chain.rs` |
> | `keygen_default_bits` is the `--synthetic-jdk` half the ratchet holds | `keygen_default_bits` | `native-builtins/src/{jca/provider_chain,phases_early}.rs` |
>
> **The record's own self-assessment still stands and should not be softened.**
> Its weakest row is `keygen.Blowfish` — thirteen seeded rows whose correctness
> depends on real JDK classes loading and running inside CratonVM, argued from
> shape rather than measured. Nothing on this pass touched that, and a symbol
> check cannot strengthen it: the ratchet holds the `keygen_default_bits` side
> only, exactly as the record says, and the other direction is not a Rust
> test's to make. **`KeyGenerator.getInstance` over all thirteen names remains
> the single most valuable thing to run against a built binary**, and it is
> still unrun.
>
> **NOT RETIRED.** Everything in "Re-taking this" is still outstanding: the
> whole record is a set of source claims awaiting one run of
> `probes/CryptoTrioProbe.java` in both arms, plus the six additions that
> section lists. A symbol being present is not the same as an algorithm being
> correct — this record's own W7-38 predecessor exists because Blowfish and RC4
> both *returned bytes*, and the bytes were AES's.
>
> **No JCA behaviour was changed by this lane.** The only edit lane A3 made in
> this area is `CertificateFactory.getInstance`'s fallback writing `type` by
> name instead of over raw slot 0 — see
> `W7-29-jca-advertise-implement-gaps.md`'s 2026-08-12 section. It touches no
> `Cipher`, `Mac`, `KeyGenerator` or digest path and none of the four rows
> above.

**Status: SOURCE LANDED, NOT RE-MEASURED.** Nothing in this record was produced by a
CratonVM binary built from this branch. The tree was not rebuilt (the lane may not run
`cargo build`), so every CratonVM column below is the state *before* the change, and every
HotSpot column is a run. The claims about what the source now does are claims about source.
Re-taking the measurement is the last section.

W7-38-crypto-trio-verified.md left four rows where CratonVM refused and HotSpot implements.
Refusing was the correct answer while nothing computed them — the three defects it fixed
were all fabricated successes, and a wrong cipher is worse than a missing one. But a refusal
is a gap against HotSpot, not parity, and a workload that asks for one of these gets a hard
failure here and bytes there.

## The four rows

Measured on jdk-25.0.3.9-hotspot vs CratonVM `--real-jdk` on the same
`probes/CryptoTrioProbe.java` class files, one binary, 2026-08-12.

| observable | HotSpot | CratonVM before | after (source) |
|---|---|---|---|
| `mac.HmacSHA224` | `len=28 86d73d8ce8749e5b49d61d6211e3a5493d4623984d9ca51aac19cc51` | `NoSuchAlgorithmException: Algorithm HmacSHA224 not available` | implemented |
| `Blowfish.ct` | `33b63e40d662746425f71a69f8cffcdadadae7ffa8950336` | `NoSuchAlgorithmException` | implemented |
| `RC4.ct` | `27ca482b161e3ab93f812659b904df95` | `NoSuchAlgorithmException` | implemented |
| `keygen.Blowfish` | `len=16 allZero=false twoDrawsDiffer=true` | `NoSuchAlgorithmException: Blowfish KeyGenerator not available` | advertised |

Key `30..3f` (16 bytes) over the 16-byte plaintext `"sixteen byte msg"` for the two ciphers;
key 32 x `0x0b` over `"hi"` for the MAC. The AES control on the same input is
`178c380cadc0514ffe26d8b26351c673` — which is what *both* ciphers used to return, and the
tell W7-38 recorded.

## HmacSHA224

`hmac::Hmac<sha2::Sha224>`. The note this replaces named the block size as the reason not to
widen the `Mac` set — "each needs its own HMAC block size — 64 for SHA-224, 128 for the
SHA-512 truncations, 144/136/104/72 for SHA3-224/256/384/512 … exactly the sort of
per-algorithm constant that cannot be defaulted". That is right, and it is the argument for
not hand-writing it: `Hmac<D>` takes the block from `D::BlockSize`. The constant appears
nowhere in the change.

Three vectors, each measured on HotSpot AND cross-checked against Python `hashlib.hmac`
(OpenSSL) so the KAT does not rest on one implementation:

```text
key = b"key",      data = "The quick brown fox jumps over the lazy dog"
  -> 88ff8b54675d39b8f72322e65ff945c52d96379988ada25639747e69
key = 32 x 0x0b,   data = "hi"
  -> 86d73d8ce8749e5b49d61d6211e3a5493d4623984d9ca51aac19cc51
key = 200 x 0x00,  data = "hi"
  -> 7d3925babc9604281f17372a195e0f0351157cc3aa17f032cbfc38de
```

The 200-byte key is the load-bearing one. RFC 2104 hashes a key **longer than the block**
before padding it, so that vector is wrong for any block size other than 64 — including 28
(the digest size) and 128 (the neighbouring arms' block). A 32-byte key exercises neither
branch and would pass with all three, which is how a block-size bug survives a KAT.

`MessageDigest.getInstance("SHA-224")` needed nothing: `Security.getAlgorithms("MessageDigest")`
already answers `SHA-224` on CratonVM and `compute_digest` already routes it to
`sha2::Sha224`. The same gap did **not** exist there — checked rather than assumed.

## Blowfish and RC4 — implemented, and not reimplemented

Both route to the real SunJCE SPI (`com.sun.crypto.provider.BlowfishCipher`,
`ARCFOURCipher`), the move `phases_early::drive_real_cipher` already makes for AES-CBC/CFB/OFB
and the whole DES family. Nothing about Blowfish or RC4 is computed in Rust.

That is the answer to "implement them correctly or not at all". A hand-rolled Blowfish means
restating the P-array and four S-boxes — 4168 bytes of pi-derived constants no reviewer can
check by reading — and a hand-rolled RC4 means a KSA/PRGA plus the 40..1024-bit key rule.
The vetted implementation is already in the image the VM is running against; a second one is
the "two implementations of one primitive" defect this campaign keeps finding.

**RC4 is broken cryptography.** RFC 7465 removed it from TLS and nothing here recommends it.
That is not a reason to refuse it: this VM lacking RC4 stops nobody from using RC4, it only
stops a workload that reads a legacy RC4 blob from running. It *is* a reason not to hand-roll
it. Note also that `tls.rs` and `tls_impl.rs` refuse RC4 cipher suites and this change does
not touch that — a `Cipher` a caller asks for by name is a different decision from a suite
this VM will negotiate.

### Why a second SPI driver, and what to do about it

`drive_real_cipher` builds an `IvParameterSpec` unconditionally and calls the four-arg
`engineInit`. Neither family tolerates that. Measured by driving the real SPI by reflection
on HotSpot, in exactly the sequence `drive_real_cipher` uses:

```text
BlowfishCipher ECB/PKCS5Padding params=null  -> 33b63e40d662746425f71a69f8cffcdadadae7ffa8950336
BlowfishCipher ECB/PKCS5Padding params=IV[0] -> InvalidAlgorithmParameterException: Wrong IV length: must be 8 bytes long
BlowfishCipher ECB/PKCS5Padding params=IV[8] -> InvalidAlgorithmParameterException: ECB mode cannot use IV
BlowfishCipher ECB/NoPadding    params=null  -> 33b63e40d662746425f71a69f8cffcda
ARCFOURCipher  ECB/NoPadding    params=null  -> 27ca482b161e3ab93f812659b904df95
ARCFOURCipher  ECB/NoPadding    params=IV[0] -> InvalidAlgorithmParameterException: Parameters not supported
ARCFOURCipher  NONE/NoPadding   params=null  -> NoSuchAlgorithmException: Unsupported mode NONE
```

So `drive_real_ecb_cipher` calls the three-arg `engineInit(int, Key, SecureRandom)` instead.
The right end state is **one** driver taking `Option<&[u8]>` for the IV. `drive_real_cipher`
lives in `phases_early.rs`, which this lane does not own; the two doc comments now point at
each other so neither is edited alone. **This is the one piece of duplication the change
introduces, and it is deliberate and bounded.**

### What is still refused, and why

Under-service, refused honestly — HotSpot serves these and this engine does not:

| transformation | HotSpot | why refused here |
|---|---|---|
| `Blowfish/CBC/PKCS5Padding` | `f096b48f64f1dcb5b49d2f5e4814ac59081e0fdf07a58730`, `getIV()=0417208096327740` | ENCRYPT_MODE with no spec must **generate** a random IV and report it through `getIV()`/`getParameters()`. This engine's `init` surface does not, for a family it drives per-`doFinal`. Admitting the mode and encrypting under an all-zero IV is a fabricated success of exactly the W7-38 shape. |
| `Blowfish/CTR/NoPadding` | `601ae5cbcf3614d55336e9a9cfa487f5`, `getIV()=aeda5a1d84dca045` | same |
| `Blowfish/CFB`, `/OFB`, `/PCBC` | serviceable | same |
| `Blowfish/ECB/ISO10126Padding` | `33b63e40d662746425f71a69f8cffcdadbd72021f29bfc9c` | padding bytes are **random**; serving PKCS5 in its place is a substitution, not an approximation. Same reason `AES/CBC/ISO10126Padding` is refused. |

Parity, not under-service — HotSpot refuses these too, measured:
`Blowfish/None/NoPadding`, `Blowfish/ECB/PKCS7Padding`, `RC4/None/NoPadding`,
`RC4/NONE/NoPadding`, `RC4/CBC/NoPadding`, `RC4/ECB/PKCS5Padding`. Note that the last is
`NoSuchAlgorithmException` on HotSpot and not `NoSuchPaddingException` — SunJCE's stream
service carries no `SupportedPaddings` beyond NoPadding, so the lookup finds no service at
all. The two exceptions are separately catchable, so which one is thrown is part of the
parity and the admission table copies the JDK's choice.

### Key lengths, checked at `init` rather than `doFinal`

The real SPI enforces these inside `engineInit`, but this engine drives it from `doFinal` —
so without a check at `init` the exception arrives from `Cipher.doFinal`, which does not
declare `InvalidKeyException`. Measured bounds and wording:

```text
Blowfish  3, 4, 8, 16, 32, 56 bytes -> accepted     (there is NO lower bound; 3 bytes is fine)
Blowfish  57 bytes                  -> InvalidKeyException: Key too long (> 448 bits)
RC4       5, 16, 128 bytes          -> accepted
RC4       4 and 129 bytes           -> InvalidKeyException: Key length must be between 40 and 1024 bit
```

The asymmetry is why this is a table and not a rule: a 3-byte Blowfish key is accepted while
a 4-byte RC4 key is refused.

## keygen.Blowfish — the refusal was not where it looked

`phases_early::keygen_default_bits` has answered `BLOWFISH => Some(128)` since 2026-08-11.
The refusal came from somewhere else entirely: in `--real-jdk` mode `KeyGenerator.getInstance`
is **not** natively intercepted. It reaches the real `sun.security.jca.GetInstance`, which
`provider_chain::getinstance_instance_search` answers from the service registry and then
*instantiates the named class out of the real image*. The registry carried three
`KeyGenerator` rows, so `Security.getAlgorithms("KeyGenerator")` on CratonVM was
`[AES, DESEDE, HMACSHA256]` against HotSpot's 24, and everything else got
`NoSuchAlgorithmException: <name> KeyGenerator not available` — which is
`getinstance_instance_search`'s wording, byte-identical to the phrase
`keygen_get_instance_named` produces for a name **it** cannot serve. Two refusal sites, one
message; the message named the wrong one.

Reading `keygen_default_bits` and concluding the feature was present would have been wrong,
and reading the exception and concluding `keygen_default_bits` was the gate would also have
been wrong. What settled it was `Security.getAlgorithms("KeyGenerator")` printing the three
names that worked.

Thirteen rows seeded, every class name measured by enumerating `getServices()` per provider
on the platform JDK rather than read off a `javap` of the provider's `<clinit>`:

```text
AES        com.sun.crypto.provider.AESKeyGenerator
ARCFOUR    com.sun.crypto.provider.KeyGeneratorCore$ARCFOURKeyGenerator   (+ Alg.Alias RC4)
Blowfish   com.sun.crypto.provider.BlowfishKeyGenerator
ChaCha20   com.sun.crypto.provider.KeyGeneratorCore$ChaCha20KeyGenerator
DES        com.sun.crypto.provider.DESKeyGenerator
DESede     com.sun.crypto.provider.DESedeKeyGenerator
HmacMD5    com.sun.crypto.provider.HmacMD5KeyGenerator
HmacSHA1   com.sun.crypto.provider.HmacSHA1KeyGenerator
HmacSHA224/256/384/512   com.sun.crypto.provider.KeyGeneratorCore$HmacKG$SHA224/256/384/512
RC2        com.sun.crypto.provider.KeyGeneratorCore$RC2KeyGenerator
```

Exactly the set `keygen_default_bits` implements, so the two modes agree. `HmacSHA3-*` and
`HmacSHA512/{224,256}` are on HotSpot's list and deliberately absent from ours, because
`keygen_default_bits` has no arm for them and the `--synthetic-jdk` path would refuse a name
this registry published. The five `SunTls*` generators are absent because they are
TLS-internal KDFs taking `TlsKeyMaterialParameterSpec`-family specs this engine's `init`
surface does not carry.

**This is the row this record is least able to stand behind.** It is the only one whose
correctness depends on a real JDK class loading and running inside CratonVM, and nothing was
rebuilt. `AESKeyGenerator`, `DESedeKeyGenerator` and `KeyGeneratorCore$HmacKG$SHA256` already
work through this exact path, and the twelve new rows are the same two shapes (a top-level
`KeyGeneratorSpi` subclass, or a nested static one), which is the whole argument. It is an
argument from shape, not a measurement.

## Advertised vs implemented, both directions

The census that started this found the quiet direction — implemented but unadvertised — is
the worse one, because nothing tests a name nobody publishes. So both directions are now
**ratchets** rather than censuses: each derives one side from the registry instead of
restating it, so a name added to one list without the other reds a test.

| type | direction | ratchet |
|---|---|---|
| `Cipher` | advertised -> serviceable | `provider_chain::every_advertised_sunjce_cipher_is_serviceable` (pre-existing; extended) |
| `Cipher` | implemented -> advertised | same test, second loop; plus an alias loop, because an alias is resolvable *without* being advertised and the first loop would never have looked there |
| `Mac` | advertised -> computable | `provider_chain::every_advertised_sunjce_mac_is_computable` (new) |
| `Mac` | implemented -> advertised | same test, second loop |
| `KeyGenerator` | implemented -> advertised | `provider_chain::every_keygenerator_the_engine_implements_is_advertised` (new) |

The `Mac` ratchet also asserts `mac_output_length(a) == mac_compute_hmac(a).len()` for every
advertised name — the retired `_ => 32` arm made those two agree with each other and with
nothing else, which is what made the wrong MAC self-consistent.

`KeyGenerator` is checked in the implemented->advertised direction **only**, on purpose. The
other direction is not a Rust test's to make: an advertised row is serviceable if and only if
the named class loads out of the real image, which no unit test in this crate can observe.
Asserting it from a table would be a probe that cannot fail. What the test *can* hold to
account is `keygen_default_bits`, the `--synthetic-jdk` half — so it also asserts that three
names WITHOUT a `keygen_default_bits` arm stay unadvertised.

### Where the two lists still differ, deliberately

HotSpot's SunJCE advertises 59 `Cipher` names, 28 `Mac` names and 24 `KeyGenerator` names.
Ours advertises fewer of the first two, and every absence is a name `getInstance` refuses —
which is the invariant, not the count. The `Cipher` list is also SHAPED differently on
purpose and was before this change: HotSpot registers a bare `DES`/`DESede` and carries the
mode set in a `SupportedModes` attribute, while this engine routes only CBC to the real SPI,
so it spells the transformation in full. Conversely `Blowfish` and `ARCFOUR` **are** bare
names here, matching HotSpot, because for those two the bare name is exactly what the engine
computes: `Blowfish` defaults to `Blowfish/ECB/PKCS5Padding` on SunJCE and this engine agrees
byte-for-byte.

One divergence surfaced by writing the alias half of the ratchet and left open: `TripleDES`
resolves through the alias map, and `Cipher.getInstance("TripleDES")` is still refused,
because SunJCE aliases it to the **bare** `DESede` and a bare DESede defaults to ECB — a mode
this engine does not route. It predates this lane and this lane does not close it; it is
named in the test rather than asserted, so it is on the record instead of invisible.

Serviceable-but-unadvertised transformations (`AES/ECB/*`, `AES/CBC/*`,
`Blowfish/ECB/PKCS5Padding`, `ARCFOUR/ECB/NoPadding`, `ChaCha20/None/NoPadding`, the
`RSA/ECB/*` paddings) are **not** a gap: HotSpot does not advertise those either. They are
served by the generic `Cipher.<algorithm>` service, which is registered on both sides.

## One thing found in passing

`Cipher.getOutputSize` hardcoded a 16-byte block while `getBlockSize` carried the real table
— two functions deriving one property from two places, and one of them was already the
documented fix for a hardcoded 16 elsewhere in the same file. It over-reported every DES and
DESede answer and would have over-reported Blowfish and rounded RC4 up to a block boundary a
stream cipher does not have. One `cipher_block_size` now answers both. It still ignores the
padding flag, on purpose: `getOutputSize` is an upper bound, and RFC 3394 key wrap outputs
input+8, so an exact-length answer would be short and cost the caller a `ShortBufferException`.

## Not touched

ChaCha20 and Poly1305, in any file — a separate lane owns `native-builtins/src/chacha20.rs`
and its dispatch. The only lines this change puts near that dispatch are the new
Blowfish/ARCFOUR route in `cipher_do_final_impl`, inserted immediately **before** the
`is_chacha20_family` block without altering it, and one `ChaCha20`/`ChaCha20-Poly1305` entry
added to an existing list in a test.

## Re-taking this

```sh
javac -d /tmp/cp probes/CryptoTrioProbe.java
java -cp /tmp/cp CryptoTrioProbe                       # oracle
cratonvm --real-jdk --java-home "$JAVA_HOME" -cp /tmp/cp CryptoTrioProbe
```

The probe covers `Blowfish.ct`, `RC4.ct`, `mac.HmacSHA224` and `keygen.Blowfish` already, and
its `sameCipher` answers `n/a(no-ct,no-ct)` rather than `true` when both operands are
refusals — so a run where the change did not land reads as `n/a`, not as agreement. What it
does **not** cover, and what a re-measurement should add:

  * `Blowfish/ECB/NoPadding` (16 bytes, `33b63e40d662746425f71a69f8cffcda`) — the padded and
    unpadded arms are separate rows in `real_spi_ecb_route`;
  * `getBlockSize()` — 8 for Blowfish, 0 for RC4;
  * the four refusals `Blowfish/CBC/PKCS5Padding`, `Blowfish/CTR/NoPadding`,
    `RC4/ECB/PKCS5Padding`, `Blowfish/ECB/ISO10126Padding`, which must be
    `NoSuchAlgorithmException` (the last, `NoSuchPaddingException`) and not bytes;
  * `Cipher.init` with a 57-byte Blowfish key and a 4-byte RC4 key, which must throw
    `InvalidKeyException` at `init` and not at `doFinal`;
  * `KeyGenerator.getInstance` over all thirteen seeded names — this is the row above whose
    argument is from shape rather than measurement, and the one most likely to be wrong.
