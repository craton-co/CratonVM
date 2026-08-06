# `KeyStore.setEntry` was a silent no-op on PKCS12 — FIXED

| | |
|---|---|
| **Status** | ✅ FIXED 2026-08-06 |
| **Severity was** | medium — silent data loss; the store succeeded and wrote a valid, empty keystore |
| **Modes** | BOTH. `--real-jdk` and `--jdk-only` were identical; it was never a strict-mode defect |
| **Opened** | 2026-08-05, by `probes/JdkOnlyPlatformProbe`'s `security` section (L8, criterion 6) |
| **Fix** | `native-builtins/src/keystore.rs` — `engine_set_entry` / `engine_get_entry` natives, a PKCS#12 *writer*, and a secret-vs-private discrimination in `engine_set_key_entry` |

## What happened

```java
KeyStore ks = KeyStore.getInstance("PKCS12");
ks.load(null, null);
char[] pw = "changeit".toCharArray();
ks.setEntry("secret",
        new KeyStore.SecretKeyEntry(new SecretKeySpec(new byte[16], "AES")),
        new KeyStore.PasswordProtection(pw));
System.out.println(Collections.list(ks.aliases()) + " isKey=" + ks.isKeyEntry("secret"));
```

| | HotSpot 25 | CratonVM before | CratonVM after |
|---|---|---|---|
| aliases immediately after `setEntry` | `[secret] isKey=true` | `[] isKey=false` | `[secret] isKey=true` |
| `store(…)` output size | 405 bytes | 32 bytes | 363 bytes |
| aliases after `load` of that output | `[secret] size=1 isKey=true` | `[] size=0 isKey=false` | `[secret] size=1 isKey=true` |
| `getKey("secret", pw)` | `AES/16` | `null` | `AES/16` |

The 405 / 363 difference is real and expected: both are valid PKCS#12 files
carrying one SecretBag, differing in salt lengths, iteration counts and whether
the certificate SafeContents is separately encrypted. Byte-identity with
SunPKCS12 was never the goal; being readable by it was, and is verified below.

## Root cause

`setEntry` is not, and never was, a native — the real `KeyStore.setEntry`
bytecode ran and delegated to `keyStoreSpi.engineSetEntry`, with `keyStoreSpi`
a genuine `sun.security.pkcs12.PKCS12KeyStore`. The measurement the original
record asked for (print `ks.getClass().getName()` and the SPI's class)
confirmed that: the receiver reached the real SPI, so this was never about the
wrong SPI being installed. That "Where to start" hypothesis was wrong, not
merely stale.

The gap is one level further in. Real `PKCS12KeyStore.engineSetEntry` does
**not** route through the public `engineSetKeyEntry` /
`engineSetCertificateEntry` that `keystore.rs` already intercepts. It calls its
own **private** `setKeyEntry` / `setCertEntry`, which mutate the SPI object's
own `entries` map. Every *read* on this VM — `engineAliases`, `engineSize`,
`engineContainsAlias`, `engineIsKeyEntry`, `engineGetKey`, `engineStore` — is
served from `keystore.rs`'s side table instead. So the real mutation landed in
a map nothing on this VM ever looks at: `setEntry` returned normally, threw
nothing, and the entry was gone before anything was serialised.

That is also why no native fired and why `CRATONVM_DBG_TLS_HS=1` printed
nothing from `engine_set_key_entry` — the call never reached it. The absence of
a trace was evidence about the call path, not about the store.

## What was actually broken (wider than the record said)

The original record covered `SecretKeyEntry` only and noted that certificate
and private-key entries "were not measured". They have been measured now, with
a probe that prints every mutation API against every entry kind as a **value**
and diffs it against HotSpot. All three `setEntry` arms were silent no-ops, and
two further defects fell out of the same surface:

| Divergence | CratonVM before | HotSpot 25 |
|---|---|---|
| `setEntry(PrivateKeyEntry)` → `aliases()` | `[]` | `[pke]` |
| `setEntry(TrustedCertificateEntry)` → `aliases()` | `[]` | `[tce]` |
| `setEntry(SecretKeyEntry)` → `aliases()` | `[]` | `[ske]` |
| `getEntry(alias, prot)` for any of the above | `null` | the entry |
| `getEntry(certAlias, null)` after `setCertificateEntry` | `UnrecoverableKeyException` | `TrustedCertificateEntry` |
| `setKeyEntry(a, new SecretKeySpec(raw,"DESede"), pw, null)` → `getKey` | `PrivateKey`, algorithm `RSA` | `SecretKeySpec`, algorithm `DESede` |
| `store` → `load` alias order for `[pk, ca]` | `[ca, pk]` | `[pk, ca]` |

The `DESede`-comes-back-as-`RSA` row is the same shape as the headline bug: the
key material survived, its type and algorithm did not, and nothing threw.

## The fix

All in `native-builtins/src/keystore.rs`.

1. **`engineSetEntry` is now a native** on all four engine FQNs
   (`PKCS12KeyStore`, `JavaKeyStore`, and the two `JavaKeyStore` inner
   classes), implementing `PKCS12KeyStore.engineSetEntry`'s decision tree
   message for message — including the two `KeyStoreException`s that are the
   only correct answer to a missing password, and
   `KeyStoreException("Cannot store non-PrivateKeys")` for a secret key aimed
   at a JKS SPI.
2. **`engineGetEntry` is now a native**, implementing `KeyStoreSpi`'s algorithm
   (which is defined purely in terms of the `engine*` accessors this module
   already owns) over the side table.
3. **`engine_set_key_entry` discriminates a secret key from a private one** via
   `Key.getFormat()` (`"RAW"` vs `"PKCS#8"`), the spec-defined answer, and
   stores it as a `SecretKey` entry.
4. **`EntryKind::SecretKey` carries the algorithm name.** It used to be
   discarded on load and reported as `"RAW"` by `getKey` — which is the
   *format*, not the algorithm — so `Cipher.getInstance(key.getAlgorithm())`
   failed on any key recovered from a keystore. The name/OID tables are
   **measured** against `AlgorithmId.get(name)` on JDK 25, not guessed: two
   rows are not what a guess produces (`DESede` is OIW `1.3.14.3.2.17`, not
   PKCS#3's `des-EDE3-CBC`; `Blowfish` is `…3029.1.1.2`, not `…3029.1.2`).
   Getting one of those wrong is not a parse error on either side — the key
   material still round-trips, it just comes back under a different algorithm
   name, which is the quiet kind of wrong this record is about. An algorithm
   this VM cannot encode is refused at `setEntry` time with a
   `KeyStoreException`, which is where a real JDK refuses it too; deferring the
   complaint to `store()` would be a second silent surprise.
5. **A PKCS#12 writer** (`write_pkcs12`). JKS — what `engineStore` wrote for
   every store regardless of declared type — has no representation for a
   `SecretKeyEntry` at all, so the alias was filtered out of the JKS body and
   the caller got a structurally valid, entry-less file and no exception.
   `engineStore` now emits PKCS#12 when the store holds a secret key and keeps
   the byte-for-byte JKS body otherwise, because that is the shape every
   already-validated round trip in this VM (the keycloak truststore merge, the
   TLS identity paths) is measured against, and the loader detects either
   format by magic. If the PKCS#12 encode fails there is **no** fallback to
   JKS: falling back would drop the secret key and hand back a valid-looking
   file, which is the original defect.
6. **`write_jks` enumerates in insertion order.** The `sort_unstable()` that
   used to stand there made a `store` → `load` round trip silently
   re-alphabetise aliases. `IndexMap` iteration is itself deterministic, which
   is all that sort was reaching for.

## Verification

`P12EntryProbe` — every mutation API against every entry kind, 22 printed
values — now diffs **clean** against HotSpot 25 under both `--real-jdk` and
`--jdk-only`, with one pre-existing, out-of-scope exception: `KeyStore.getKey`
hands back this module's compact `java.security.PrivateKey` proxy rather than
`sun.security.rsa.RSAPrivateCrtKeyImpl`. That is a property of
`engine_get_key`'s proxy, unchanged by this work; every other assertion on that
key (algorithm, encoding, chain, byte-equality with the original) matches.

Cross-VM interop, which is the assertion that actually matters for a file
format:

* CratonVM writes a 4-entry PKCS#12 (secret AES, secret DESede, private key +
  chain, trusted cert) → **HotSpot reads all four**, with the right algorithms
  and byte-identical key material.
* `keytool -list` on that same file reports 4 entries — two `SecretKeyEntry`,
  one `PrivateKeyEntry`, one `trustedCertEntry` — and prints the certificate
  fingerprint, with no password needed for the certificate bags.
* CratonVM reads the HotSpot-written equivalent and produces an identical
  transcript.

The `trustedKeyUsage` attribute is why the trusted-cert entry survives that
trip. The first cut of the writer omitted it and HotSpot silently dropped the
bag (it keeps only certificates that pair with a key), producing a 4-entry
keystore that HotSpot listed as 3 — the same class of quiet loss this record is
about, and it was found by asking the other VM rather than by asking ourselves.

Hermetic coverage lives in `keystore.rs`'s own `mod tests` (24 pass): PKCS#12
round trips for a secret key (bytes **and** algorithm **and** alias — asserting
only "an entry came back" would have passed against the `"RAW"` bug), for
`DESede` specifically, and for a mixed three-kind store; a MAC that rejects the
wrong password; the refusal of an unencodable algorithm; the read-side and
dotted-OID name forms that `load` → `store` of somebody else's keystore depends
on; and that JKS still omits secret keys while preserving insertion order.

`cargo test -p cratonvm-native-builtins --lib` is green in BOTH feature
configurations: 3292 tests with default features, 3467 with
`--features synthetic-jdk`. Two integration ratchets are red, and both are red
on `dev` without this change:

* `lock_discipline_ratchet::raw_lock_constructions_do_not_grow` — 432 raw lock
  constructions against a baseline of 438. The count *fell*, and the ratchet
  asks whoever lowered it to re-freeze in the same change. Confirmed identical
  with this change stashed. It adds no lock and leaves the count at 432.
* `shim_inheritance_guard::synthetic_registry_has_no_unlisted_identity_shim_on_an_intercepting_base`
  (`--features synthetic-jdk` only) — names exactly one offender,
  `java/util/prefs/AbstractPreferences.toString()`, registered in
  `phases_late/beans_jndi.rs` on `origin/dev`. This change's registrations are
  all on the four keystore engine FQNs plus `java/util/IteratorEnumeration`.

### The strict-corpus gate

`scripts/jdk-only-strict-probes.sh` over the full three-probe corpus, three
arms, **passes**. `JdkOnlyPlatformProbe`'s `--real-jdk` transcript is now
byte-identical to HotSpot's; the sections still diverging under `--jdk-only`
are the three other defects filed alongside this one (vthreads, agent, jni).

The baseline was re-frozen in this change, and the regenerated file differs by
exactly two deletions:

```
-JdkOnlyPlatformProbe/real/security
-JdkOnlyPlatformProbe/strict/security
```

Nothing else moved — in particular `JdkOnlyPlatformProbe/*/vthreads` and
`JdkOnlyBreadthProbe/strict/serialization` are retained. That mattered: both
are genuinely intermittent (`serialization` diverged with
`NoClassDefFoundError: cratonvm/internal/SystemLogger` in one full run and not
in the previous one, twenty minutes apart on the same binary), and the
baseline's own note records that an earlier re-freeze already landed on a lucky
run and wrote a file too tight to hold. `--update-baseline` regenerates
wholesale from a single run, so the diff was checked line by line before being
kept, and the gate was then re-run without the flag to confirm it passes
against the file it just wrote.

## Residual, stated rather than hidden

PKCS#12 permits a per-entry key password distinct from the store password. A
`LoadedKeyStore` does not carry one, so `engineStore` protects every key with
the **store** password. This is the same limitation `jks_protect_key` already
documents for JKS; it is invisible to any caller that uses one password for
both (every fixture, and the overwhelmingly common case), and it changes the
key password to the store password otherwise.
