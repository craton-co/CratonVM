# FIXED — the empty provider map view, and a TripleDES that meant CBC

## Status

**FIXED 2026-08-27** on `fix/provider-mapviews-and-desede-20260827`.

Two items the alias work of 2026-08-26 left behind, one of which that work
turned from dormant into live.

| | before | after |
|---|---|---|
| `Provider.keySet()`, SUN / SunJCE / SunRsaSign / SunEC / SunJSSE | **0, 0, 0, 0, 0** | 124, 251, 60, 85, 21 |
| `Cipher.getInstance("TripleDES", "SunJCE")` | CBC/PKCS5, **random IV** | **ECB/PKCS5, no IV** — HotSpot's bytes |
| `Cipher.getInstance("DESede")` | `NoSuchAlgorithmException` | **serves, byte-identical to HotSpot** |
| `DESede/ECB/{PKCS5Padding,NoPadding}`, `DES`, `DES/ECB/PKCS5Padding` | refused | **serve, byte-identical** |

## The TripleDES defect is one I introduced

`Alg.Alias.Cipher.TripleDES` was seeded as `DESede/CBC/PKCS5Padding`. HotSpot's
row is the bare name:

```text
SunJCE|Alg.Alias.Cipher.TripleDES => DESede
```

That was a wrong spelling with no consequence for as long as nothing consulted
the Cipher alias registry. On 2026-08-26 `canonical_transformation` started
consulting it — the change that made 43 Cipher OID aliases resolve — and from
that moment `getInstance("TripleDES", "SunJCE")` returned a **CBC** cipher with
a **random IV** where HotSpot returns **ECB** with none:

```text
HotSpot   TripleDES  ct=61db204ee34fb78a8f45b5be23f16c4e  iv=none
CratonVM  TripleDES  ct=61e7b090a1d7f6f33395732b13c73b60  iv=5be8635cf0d74aa2
```

Different ciphertext, silently, from a name that had previously refused. That
is worse than the exception it replaced.

It is also exactly the substitution this tree already had two comments about:

* `classify_transformation`'s DES arm — *"admitting `DESede/ECB/PKCS5Padding`
  would serve CBC under an ECB name"*;
* `cipher_do_final_impl`'s route table — *"widening the admission table without
  widening this line would serve CBC under another mode's name"*.

Both guards were correct and both were bypassed, because the alias reached the
engine through the registry rather than through the transformation string
either guard inspects. A guard that inspects a name does not protect a path
that rewrites the name upstream of it.

## Lifting the guards instead of restoring the refusal

Correcting the alias alone restores the refusal, because bare `DESede` was not
serviceable. Both comments stated the condition for lifting them, so this
change lifts them together and measures the result.

SunJCE's defaults, measured on HotSpot 25 with one 8-byte block under a fixed
key — the bare name is byte-identical to the ECB spelling, which is what makes
the algorithm-only form admissible:

```text
DES                      feee300d8eecb85e8207ea5d3e19a5fd   iv=none
DES/ECB/PKCS5Padding     feee300d8eecb85e8207ea5d3e19a5fd   iv=none
DESede                   61db204ee34fb78a8f45b5be23f16c4e   iv=none
DESede/ECB/PKCS5Padding  61db204ee34fb78a8f45b5be23f16c4e   iv=none
DESede/ECB/NoPadding     61db204ee34fb78a
DESede/CBC/PKCS5Padding  61db204ee34fb78a942288836d960969   iv=0*8
```

Four things moved together:

1. `classify_transformation` admits ECB and the algorithm-only form for the DES
   family. `parse_transformation` already defaults an unspelled mode to ECB, so
   the bare name needs no special case. One token spelled and not the other is
   still refused — that form was not measured.
2. `cipher_do_final_impl` **matches** the mode and forwards it to
   `engineSetMode`, with no wildcard arm, so a mode this match does not name
   takes the no-route path rather than a silent CBC.
3. `drive_real_cipher` passes **null parameters** when the IV is empty. SunJCE's
   own `engineInit` refuses an `IvParameterSpec` in ECB, and an empty spec is
   not the same as none — that is what made `DESede/ECB/*` unroutable even
   after the first two changes.
4. SunJCE advertises the bare `DES` and `DESede`, as HotSpot does, replacing
   four fully-spelled CBC names. The spelled forms are still SERVED: the Cipher
   arm of `check_provider_ownership` splits on `/` and asks about the base.

`auto_generated_iv_len` already returned `None` for ECB, so no IV is minted and
`getIV()` is null where HotSpot's is. Nothing needed to change there — checked,
not assumed.

## The empty map view

`put_service` / `put_service_attribute` / `put_alias` capture into the
structured maps `getInstance` reads. Nothing ever mirrored them into the
property table the map views read, so `keySet`, `entrySet`, `values`, `keys`,
`elements`, `size`, `isEmpty` and `getProperty` were **silently wrong rather
than merely incomplete**:

| provider | HotSpot 25 | before | after |
|---|---|---|---|
| SUN | 257 | 0 | 124 |
| SunJCE | 496 | 0 | 251 |
| SunRsaSign | 84 | 0 | 60 |
| SunEC | 158 | 0 | 85 |
| SunJSSE | 25 | 0 | 21 |

The counts are lower than HotSpot's because this VM seeds fewer services and
almost no `SupportedModes`/`SupportedKeyFormats` attributes. That is the honest
number: the view reports what this provider actually has. Inflating it to match
would be the same lie in the other direction.

Rendered on demand from the structured maps rather than duplicated at `put`
time, so there is one source of truth and no way for the two to disagree. Three
key shapes, HotSpot's own (68 services + 86 attributes + 103 aliases on SUN):

```text
MessageDigest.SHA-256                 -> sun.security.provider.SHA2$SHA256
MessageDigest.SHA-256 ImplementedIn   -> Software
Alg.Alias.MessageDigest.SHA1          -> SHA-1
```

An explicit `put` still wins over the projection for the same key, and a
service with an empty class name — an artefact of the legacy `put` flow — is
not rendered, because it is not a row any provider advertises.

### Why the alias map grew a struct

Lookups normalise type and alias to upper case. Rendering a row from the
normalised key would produce `ALG.ALIAS.MESSAGEDIGEST.SHA1`, which is not a key
any caller recognises and not one `getProperty` could match. Upper-casing is
lossy and `TripleDES` is the counter-example that proves it, so the alias map
now keeps both raw spellings beside the canonical — for the same reason
`ServiceEntry` always kept `type_str` and `algorithm`.

`a_seeded_provider_renders_its_registry_as_property_rows` pins presence AND
spelling, and asserts the mangled forms are *absent*, which is the half a
presence-only test would miss.

## Regression

| | result |
|---|---|
| `cargo test -p cratonvm-native-builtins --lib` | **4169 passed, 0 failed** |
| `cargo test -p cratonvm-types` (all targets, every gate) | 585 + 20 passed, 0 failed |
| `cargo test -p cratonvm-gc -p cratonvm-vm --lib` | 2632 passed, 0 failed |
| bc-java `cms.test.AllTests` | `OK (433 tests)` |
| bc-java `jce.provider.test.AllTests` | `OK (1 test)` |
| bc-java `openssl.test.AllTests` | `OK (5 tests)` |

`openssl` is in that list on purpose: PEM private-key decryption is the DES/CBC
consumer, so it is the suite a wrong mode would break.

## What is not claimed

**Not that `SecretKeyFactory.getInstance("DESede")` works.** It still refuses.
That engine serves only the PBE/PBKDF2 families, and a DESede factory needs a
real SPI route; advertising it without one would break the advertise-vs-serve
invariant the rest of this change restores. Measured and left.

**Not that the map view equals HotSpot's.** It equals this VM's own registry,
which is smaller. The gap is missing services and attributes, not a missing
projection — and it is now legible, where an empty view made it invisible.

**Not that every mode of the DES family is served.** CBC and ECB are. CFB, OFB
and CTR are refused at `getInstance`, and the route has no arm for them.

## The transferable part

**A guard on a name does not protect a path that rewrites the name.** Both DES
comments were right, both were load-bearing, and neither fired — because the
alias registry substituted the transformation upstream of the string each guard
reads. When adding a resolution step in front of a validator, the question is
not "is the validator correct" but "does the validator still see what the
caller wrote".

**An empty collection is a wrong answer, not a missing feature.** `keySet()`
returning nothing looked like an unimplemented corner for as long as nobody
compared it to an oracle. It is the same shape as a method returning `null`
where a value is required: every caller that iterates it silently concludes the
provider has no services.
