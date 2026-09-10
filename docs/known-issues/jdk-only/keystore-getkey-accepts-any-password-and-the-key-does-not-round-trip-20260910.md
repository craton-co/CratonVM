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
