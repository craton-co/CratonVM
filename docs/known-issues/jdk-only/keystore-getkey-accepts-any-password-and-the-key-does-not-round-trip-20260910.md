# `KeyStore.getKey` accepts ANY password, and the key it returns does not round-trip

**Status: OPEN.** Found 2026-09-10 by lane T while pricing the retirement of
`native-builtins/src/keystore.rs`' cross-cutting `engine*` registrar (136 rows
over eight `KeyStoreSpi` implementations). Both compatibility modes. JDK 25.0.3+9
on Windows; the oracle is that image's own HotSpot.

Instrument: `apps/probes/LTKeyStoreEngineSweep.java` (52 rows), written for this
because `apps/probes/KeyStoreTypeProbe.java` — the only keystore probe in the
promoted corpus — exercises roughly seven of the seventeen registered `engine*`
methods and none of the private-key half.

---

## 1. The four rows, and the first one is a security defect

`--jdk-only`, unarmed, against HotSpot. The same four appear for **JKS, PKCS12
and JCEKS**; only the JKS lines are quoted:

```text
KS-1  [JKS] getKey with the WRONG password
        HotSpot   THREW java.security.UnrecoverableKeyException: Cannot recover key
        CratonVM  java.security.PrivateKey@570

KS-2  [JKS] key survives a store/reload round trip   (non-null / equals / algorithm / format)
        HotSpot   true/true/RSA/PKCS#8
        CratonVM  true/false/RSA/PKCS#8

KS-3  [JKS] setEntry then the key comes back equal
        HotSpot   true
        CratonVM  false

KS-4  [JKS] a store reloaded with the WRONG password
        HotSpot   THREW java.io.IOException: Keystore was tampered with, or password was incorrect
        CratonVM  THREW java.io.IOException: JKS HMAC integrity check failed
```

**KS-1 is the one to read twice.** `KeyStore.getKey(alias, password)` is the API
that gates a private key on a password, and this VM hands the key back for any
password at all. For JKS the returned object is worse than wrong: the recovery
in `keystore.rs::jks_recover_key` DOES check the integrity digest and correctly
returns `None`, so `keystore_unlock_private_keys` leaves `key_der` holding the
**still-encrypted `EncryptedPrivateKeyInfo`** — and `engine_get_key` then wraps
those ciphertext bytes in a `java/security/PrivateKey` mirror and returns it.
`engine_get_key`'s own comment says the unlock exists "so consumers never
receive an `EncryptedPrivateKeyInfo` masquerading as PKCS#8", which is exactly
what the wrong-password path produces.

KS-2 and KS-3 are one defect: `engine_get_key` returns a VM-minted
`java/security/PrivateKey` carrying four int/long slots, not the real
`sun.security.rsa.RSAPrivateCrtKeyImpl`, so `Key.equals` — which the JDK defines
as an encoding comparison — is `false` against the key that was stored. The
algorithm and format strings survive, which is why a probe that asks only those
two reports success.

KS-4 is cosmetic and is listed only so a fix does not have to rediscover it.

## 2. Retiring the registrar fixes two of the three store types, and this is measured

`CRATONVM_ENFORCE_NATIVE_SHADOW` armed over the four receiver prefixes
(`sun/security/pkcs12/PKCS12KeyStore`, `sun/security/provider/DomainKeyStore`,
`sun/security/provider/JavaKeyStore`, `com/sun/crypto/provider/JceKeyStore`),
one binary, one probe, the dial the only difference:

```text
   unarmed   24 differing lines of 52   (four rows x three store types)
   armed      8 differing lines of 52   (the same four rows, JKS only)
   dial       930 reached, 930 yielded, 0 declined_no_bytecode
   census     ZERO keystore natives invoked under the arm
```

So the real `PKCS12KeyStore` and `JceKeyStore` bytecode runs correctly on this
VM — 11.2 million `MessageDigest.update` dispatches, i.e. the genuine PBE and
MAC loops — and the natives standing in front of it are strictly worse. **This
is the §1.4 remedy working exactly as the contract says it should**, for two of
the three formats.

JKS is the exception and the four rows survive the arm unchanged, which means
they are NOT the registrar's rows: with every keystore native yielded, the real
`sun.security.provider.JavaKeyStore` + `KeyProtector` path reaches the same
wrong answers. That is a separate defect, in `sun/security/` (lane 6's prefix),
and it has to be found before the JKS half of this can be closed.

## 3. Why the retirement is nevertheless BLOCKED

The natives are not only an implementation of the SPI; they are the **producer
of a Rust-side store that the TLS stack reads out of band**.
`engine_get_key`'s body registers the recovered RSA material with
`crypto_impl::rsa_key_store` and encodes a `(store_id, alias_hash)` composite
into the mirror's slot 3, and `keystore_unlock_private_keys` calls
`t27_tls::install_identity_from_der`. `keystore_get_private_key()` is how the
rustls layer obtains a client identity at all.

Retire the registrar and the real bytecode populates the real objects, the
side table stays **empty**, and nothing reports it — mTLS stops working with no
exception at the keystore boundary. No probe in the promoted corpus can see
that (`SecuritySurfaceSweep`, `JcaFunctional`, `DhAgree` and `KeyStoreTypeProbe`
are all byte-identical under the arm), which is precisely the shape lane 4's
page calls "a loud failure turned into a rare silent one".

**So the order is fixed and is not this record's choice:** the TLS identity path
has to stop reading a Rust-side store before the keystore registrar can be
retired. Until then the 136 rows stay `Bridge`, and KS-1 has to be fixed *in the
natives* rather than by yielding to bytecode.

## 4. What would close this

1. **KS-1, narrowly and now:** `engine_get_key` must fail rather than return
   ciphertext. After `keystore_unlock_private_keys`, an entry whose `key_der`
   still parses as a JKS `KeyProtector` `EncryptedPrivateKeyInfo` means the
   password was wrong; throw `java.security.UnrecoverableKeyException("Cannot
   recover key")`. The PKCS12 and JCEKS paths need the same check against their
   own per-entry protection, which they currently do not model at all — those
   two decrypt with the STORE password at load time and never consult the entry
   password, which is why they accept any.
2. **KS-2/KS-3:** requires `getKey` to return a real key object rather than the
   four-slot mirror, which is the same "the class's state has to become real
   before its shadow can be retired" ordering `native-api/src/retired_shadow.rs`'s
   header states for every other family.
3. **The blocker in §3:** a TLS identity path that reads the `KeyStore` object
   rather than a store keyed by `store_id`.
4. Re-run `apps/probes/LTKeyStoreEngineSweep.java` in all three arms. It is not
   in `scripts/baselines/jdk-only-strict-corpus-25-*.probes` and must not be
   promoted while it is red — promoting a divergent probe freezes the
   divergence as acceptable.

## 5. Reproduce

```bash
javac -d /tmp/ks apps/probes/LTKeyStoreEngineSweep.java
java -cp /tmp/ks LTKeyStoreEngineSweep > /tmp/ks.hs
cratonvm --jdk-only --java-home "$JAVA_HOME" -cp /tmp/ks LTKeyStoreEngineSweep > /tmp/ks.cv
diff /tmp/ks.hs /tmp/ks.cv          # 24 differing lines

CRATONVM_ENFORCE_NATIVE_SHADOW='sun/security/pkcs12/PKCS12KeyStore,sun/security/provider/DomainKeyStore,sun/security/provider/JavaKeyStore,com/sun/crypto/provider/JceKeyStore' \
  cratonvm --jdk-only --java-home "$JAVA_HOME" -cp /tmp/ks LTKeyStoreEngineSweep > /tmp/ks.arm
diff /tmp/ks.hs /tmp/ks.arm         # 8 differing lines, JKS only
```

---

## 6. The cross-VM matrix: two more defects, and one of them is in the WRITER

Everything above measures one VM writing a store and the same VM reading it
back. A writer and a reader that agree with each other and disagree with the
specification look correct in that arm. `apps/probes/KSInteropWriteRead.java`
asks the cross product instead — each VM writes, each VM reads — with a
`setKeyEntry(alias, key, KEYPW, chain)` whose ENTRY password differs from the
`store(out, PW)` password, which is the case PKCS#12 and JKS both define and
neither of the same-VM arms can distinguish.

It also carries a discriminator the earlier probe lacks. `getAlgorithm()` and
`getFormat()` are what the key OBJECT claims, and this VM's `getKey` returns a
four-slot mirror that answers both from a field — so ciphertext dressed as a
key reads as `RSA/PKCS#8`. `usable` asks the question the claim cannot fake:
does `getEncoded()` parse back as a PKCS#8 private key through
`KeyFactory`? Only real recovered key material does.

MEASURED 2026-09-10, JDK 25.0.3+9, `cratonvm-lt4.exe`, RSA-2048, one
certificate from the JDK's own `cacerts`:

```text
  store written by HotSpot, read by HotSpot          <- the oracle
    JKS/PKCS12/JCEKS   ENTRY pw  RSA/PKCS#8/usable
                       STORE pw  THREW UnrecoverableKeyException
                       WRONG pw  THREW UnrecoverableKeyException

  store written by CRATONVM, read by HotSpot
    JKS/PKCS12/JCEKS   ENTRY pw  THREW UnrecoverableKeyException   <- KS-5
                       STORE pw  RSA/PKCS#8/usable                 <- KS-5
                       WRONG pw  THREW UnrecoverableKeyException

  store written by HotSpot, read by CRATONVM
    JKS                ENTRY pw  RSA/PKCS#8/usable
                       STORE pw  RSA/PKCS#8/NOT-A-KEY              <- KS-1
                       WRONG pw  RSA/PKCS#8/NOT-A-KEY              <- KS-1
    PKCS12, JCEKS      ENTRY pw  RSA/PKCS#8/NOT-A-KEY              <- KS-6
                       STORE pw  RSA/PKCS#8/NOT-A-KEY              <- KS-6
                       WRONG pw  RSA/PKCS#8/NOT-A-KEY              <- KS-6

  store written by CRATONVM, read by CRATONVM
    JKS/PKCS12/JCEKS   every password  RSA/PKCS#8/usable           <- KS-1
```

### KS-5 — the writer protects every private key with the STORE password

`write_pkcs12` and `write_jks_with_magic` encrypt each key with the password
handed to `engineStore`, never with the password the entry was set with:
`native-builtins/src/keystore.rs`'s `pbes2_encrypt(key_der, password)` and
`jks_protect_key(key_der, password)` both take the store password because the
entry has nowhere to carry its own.

Two consequences, and the second is the security one:

* **A keystore this VM writes is not readable by conforming tooling with the
  documented password.** `keytool`, HotSpot, and anything using the JCA gets
  `UnrecoverableKeyException` for the entry password it was told to use, on all
  three store types. It gets the key for the STORE password.
* **The entry password provides no protection.** Anyone holding the store
  password holds every private key in it, which is the property a separate
  entry password exists to deny.

### KS-6 — a HotSpot-written PKCS#12 or JCEKS with a distinct entry password never decrypts, silently

`load_pkcs12` decrypts shrouded key bags with the STORE password and, when that
fails, keeps the encrypted bag as a placeholder `key_der` so alias and chain
pairing still work (the comment at the `unwrap_or_else` says exactly this). The
placeholder is then handed to the application as a `PrivateKey`, for EVERY
password including the correct one, because nothing downstream re-attempts the
decryption with the entry password `getKey` receives. JKS escapes this only
because `keystore_unlock_private_keys` DOES re-attempt with the `getKey`
password — which is why the JKS row above discriminates correctly and merely
fails to throw.

The common deployment (Spring Boot, most `keytool` recipes) uses one password
for both, so this is invisible there and loud nowhere.

### What this changes about §4

`§4.1`'s "narrowly and now" is now **provably** one condition away for JKS: the
password verification already runs and already produces the right answer — the
`ENTRY pw` row is `usable` and the other two are `NOT-A-KEY` — so the entire
defect on that path is that a failed recovery returns the ciphertext instead of
throwing. That is the fix to make first, and the matrix above is its oracle.

### The shape that closes KS-1, KS-5 and KS-6 together

All three are the same missing field. A `PrivateKey` entry carries no record of
the password it was protected WITH, so `engineStore` has nothing to encrypt with
but the store password (KS-5), `engineGetKey` has nothing to verify against
(KS-1), and a bag that did not open at load has nothing to re-attempt with
(KS-6).

```rust
EntryKind::PrivateKey {
    key_der: Vec<u8>,           // as today: plaintext when we have it
    chain: Vec<Vec<u8>>,
    protected: Option<Vec<u8>>, // the envelope the ENTRY password opens
}
```

* `load_jks` / `load_pkcs12` fill `protected` with the envelope they read,
  whether or not the store password opened it. Both already hold those bytes;
  today they drop them on the successful path.
* `engine_set_key_entry` knows its SPI class, so it knows the store type, so it
  can build the envelope in that type's own format — `jks_protect_key` or
  `pbes2_encrypt` — from the ENTRY password it was handed.
* `write_jks_with_magic` / `write_pkcs12` prefer `protected` verbatim. Both
  already have a pass-through arm for an envelope they could not open
  (`is_jks_encrypted_private_key`, the `EncryptedPrivateKeyInfo::parse` probe);
  this makes that arm the normal path instead of the exception.
* `engine_get_key` opens `protected` with the password it was given and throws
  `UnrecoverableKeyException` when it cannot. `is_jks_encrypted_private_key`
  already distinguishes the JKS envelope; a PKCS#8 `PrivateKeyInfo` cannot be
  mistaken for an `EncryptedPrivateKeyInfo` (the former's first element is an
  INTEGER version, the latter's is an `AlgorithmIdentifier` SEQUENCE), so the
  discrimination is exact rather than heuristic.

The messages HotSpot uses are three, and they are chosen by the PROTECTION
SCHEME rather than the store type, which is what the envelope itself carries:
`Cannot recover key` for the JKS key protector, `Given final block not properly
padded. Such issues can arise if a bad key is used during decryption.` for
SunJCE's PBE, and that same text behind `Get Key failed: ` for SunPKCS12.

**What this does NOT close is KS-2/KS-3.** The object handed back is still the
four-slot mirror, so `equals` stays false and `getEncoded` stays whatever the
mirror was given. That needs a real key object, and it is the same "the class's
state has to become real before its shadow can be retired" ordering every other
family in this campaign hit.

---

## 7. KS-1 is FIXED, 2026-09-10, and the matrix says by how much

`engine_get_key` now throws `java.security.UnrecoverableKeyException` when the
entry's material is still inside an envelope, instead of wrapping the
ciphertext in the `PrivateKey` mirror and returning it. `is_encrypted_private_key`
recognises both envelopes this VM can hold — the JKS key protector by its OID,
a PKCS#12 `EncryptedPrivateKeyInfo` by parsing — and
`an_envelope_is_told_from_a_key_exactly` pins the discrimination in the
direction that would hurt: a plaintext PKCS#8 must never read as an envelope,
or every `getKey` with the CORRECT password starts throwing.

MEASURED on `cratonvm-lt6.exe`, the same six stores as §6, HotSpot-written and
read by CratonVM — the arm where this VM is the reader and the specification
says what should happen:

```text
                        HotSpot        before            after
   JKS    ENTRY pw      usable         usable            usable
          STORE pw      throws         NOT-A-KEY         throws
          WRONG pw      throws         NOT-A-KEY         throws
   PKCS12 ENTRY pw      usable         NOT-A-KEY         throws      <- KS-6
   JCEKS  STORE pw      throws         NOT-A-KEY         throws
          WRONG pw      throws         NOT-A-KEY         throws

   rows matching HotSpot          1 of 9   ->   7 of 9
   rows still wrong               8 silent ->   2 LOUD
```

**JKS now matches HotSpot exactly.** The two rows that remain wrong are KS-6's
— a HotSpot-written PKCS#12 or JCEKS whose entry password differs from the
store password never decrypts at all, so the correct password now throws where
HotSpot returns the key. That is a worse *answer* and a better *failure*: it
trades a silent wrong key for a loud refusal, which is the direction this
campaign's own rule points, and it is visible to the application instead of
being discovered when a signature fails to verify on a peer.

**Nothing changed for a store this VM wrote**, because KS-5 is upstream of this
fix: those keys are protected with the STORE password and therefore decrypt at
load, so no envelope survives for `getKey` to refuse. Every `CRATONVM -> CRATONVM`
row in §6 still reads `usable` for all three passwords. KS-5 and KS-6 both wait
on the `protected` field §4 specifies, and `LTKeyStoreEngineSweep` stays
unpromoted until they land: its `getKey with the WRONG password` rows go
through a store this VM wrote, so they are KS-5's rows and this fix does not
move them.

## 8. KS-5 and KS-6 are FIXED, 2026-09-11, and one new residual replaces two old ones

`EntryKind::PrivateKey` gained a `protected: Option<Vec<u8>>` field: the
envelope the ENTRY password builds, kept alongside the plaintext `key_der` the
TLS identity path still needs. `engineSetKeyEntry` and `setEntry` now read the
password argument they previously discarded and wrap the key in it (JKS format
for a `JavaKeyStore`/`JceKeyStore` receiver, PBES2 for a `PKCS12KeyStore` one);
both writers emit that envelope verbatim instead of re-encrypting under the
STORE password; `engine_get_key` checks the ENTRY password against it before
ever reaching KS-1's ciphertext check, which is the in-memory half KS-1 could
not see (an entry set through the API never has ciphertext in `key_der` to
catch). `recover_key_any_scheme` replaces the JKS-only unlock funnel with one
that tries both schemes, so a bag the store password did not open at load time
gets a second chance at `getKey` time under the entry password — that is KS-6.

MEASURED on `cratonvm-lt8.exe` with `apps/probes/KSInteropWriteRead.java`,
which writes each store type with `setKeyEntry(alias, key, KEYPW, chain)`
under store password `PW` (KEYPW != PW) and asks `getKey` three ways —
matching HotSpot 25.0.3+9 exactly means throwing for WRONG and STORE and
returning a usable key for ENTRY:

```text
                                          before KS-5/6   after
   writer=hs  PKCS12  reader=cratonvm     2 of 3          3 of 3   <- KS-6
   writer=cv  PKCS12  reader=cratonvm     3 of 3          3 of 3   (KS-5: no envelope to lose)
   writer=hs  JKS     reader=cratonvm     3 of 3          3 of 3
   writer=cv  JKS     reader=cratonvm     3 of 3          3 of 3
   writer=hs  JCEKS   reader=cratonvm     2 of 3          2 of 3   <- KS-7, see below
   writer=cv  JCEKS   reader=cratonvm     3 of 3          3 of 3   (KS-5)

   rows matching HotSpot exactly (of 9 writer x reader pairs)   7 of 9  ->  8 of 9
```

**PKCS12 is now correct end to end**, both directions: this VM's own writes
protect the entry with the entry password (KS-5) and its own reads recover
either scheme (KS-6). **JKS was already correct** (§7) and is unmoved.

### KS-7: a HotSpot-written JCEKS entry password still does not open

The one cell that did not move: `writer=hs JCEKS reader=cratonvm`, `ENTRY`
password. Before this fix it returned ciphertext wearing a `PrivateKey` mirror
(the same silent-wrong-answer KS-1 closed for the other five cells); after it
throws `UnrecoverableKeyException` for the entry password too, which is a
*different* defect from the one KS-6 fixed, not a survival of it — WRONG and
STORE both now correctly throw, matching HotSpot, and only ENTRY is wrong.

The cause is `spi_wants_pkcs12_envelope`'s own documented choice: it treats
JCEKS as a JKS-format receiver, because that is the format THIS VM's write
side uses for the JCEKS entries IT creates (`write_jks_with_magic` under
`JCEKS_MAGIC`, §2 of the writer's own comment). Real `JceKeyStore` does not
write JKS's `KeyProtector` envelope for a JCEKS entry password — it uses
SunJCE's own `PBEWithMD5AndTripleDES`-family cipher, undocumented outside the
JDK source, with a key-derivation and salt/IV scheme that is neither JKS's
`KeyProtector` nor PKCS#12's `PBES2`/legacy RC2/3DES bags.
`recover_key_any_scheme` tries exactly those two, so a real JCEKS entry
envelope parses as neither and every password refuses.

Closing this needs a third scheme in `recover_key_any_scheme` and
`protect_key_for_store`, reverse-engineered from `com.sun.crypto.provider`'s
`JceKeyStore` (not shipped as source in a normal JDK distribution — it is in
`src.zip` under `com.sun.crypto.provider`, module `jdk.crypto.cryptoki` /
`java.base`'s closed provider jar, readable via `javap` or a decompiler against
the installed JDK). Out of scope for this pass: it is a new cipher to
implement, not a wiring gap like KS-5/KS-6 were.

`apps/probes/LTKeyStoreEngineSweep.java` stays unpromoted (§4 item 4): KS-7
keeps one of its rows red.
