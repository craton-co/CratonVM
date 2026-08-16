# bc-java: what is left after the JCA provider-framework fixes

## Status
**OPEN, characterised, several families root-caused, NOT one bug.** This page is
the residue of
`docs/internal/fixed-suite-bugs/bug-bcjava-jca-provider-alias-lookup-and-attribution-20260816.md`,
whose four bugs (alias/OID lookup, provider attribution, the VM-fatal
`NotImplemented`, and the three functional-crypto items) are FIXED and
probe-verified against HotSpot 25. Everything below is what the bc-java
`AllTests` sweep still fails on **after** those fixes, measured on Azure host 2
(`azureuser@20.80.105.49`) on 2026-08-16.

Each item is a separate finding. They are collected on one page because one
sweep found them, not because they share a cause.

## Where the numbers stand

| run | of the 24 classes CratonVM failed on 2026-08-16 |
|---|---|
| CratonVM, pristine `dev` | 24 FAIL |
| CratonVM, after the JCA fixes | **6 PASS, 15 FAIL** (`c509`, `cert.path`, `cert.ocsp`, `cert.plants`, `mozilla`, `operator`) |
| **HotSpot 25, same harness** | 22 PASS, 2 FAIL |

The two HotSpot also fails are not VM defects: `openssl.test` is an
`OutOfMemoryError` inside `SCrypt.SMix` at `-Xmx 1g`, and `pkix.test` is
`QcType statement was not recognised` plus four `MissingEntryException`s from
BouncyCastle's own resource bundles.

**No regression**: all 24 classes that passed before the change still pass, and
the in-tree `regression-suite` is 43/43 with `cargo test -p
cratonvm-native-builtins` at 3565/0.

## How to reproduce, and two harness faults that made the old oracle lie

```bash
# runner + classpath: see the memory note `bcjava-suite-runner-and-oracle-classpath`
CVM_JIT_FLAG=" " CRATONVM_BIN=<vm> OUTDIR=/data/<out> CLASS_TIMEOUT=1500 \
  bash /data/bcjca-run.sh /data/bcjava-fail-list.txt
MODE=hotspot OUTDIR=/data/<out-hs> bash /data/bcjca-run.sh /data/bcjava-fail-list.txt
```

* the classpath must include `apps/bc-java/libs/unboundid-ldapsdk-6.0.8.jar`.
  Without it HotSpot's own `jce.provider.test.AllTests` dies in `<clinit>` with
  `NoClassDefFoundError` in 1 s and reads as "fails on HotSpot too". With it,
  HotSpot passes that class in 113 s of real crypto tests — which is what turns
  the AEAD finding from unverifiable into a genuine differential.
* `org.bouncycastle.test.AllTests` needs `-Dtest.java.version.prefix=25`, or it
  fails on BOTH VMs by design.

Run with the JIT on. `--nojit` costs about 15× here (`cms.test`: 116 s with the
JIT, over 1500 s without), which turns ordinary slowness into HANG rows.

## Residual A — the CMS/PKCS cipher-stream family

```
org.bouncycastle.crypto.io.InvalidCipherTextIOException: Error during cipher finalisation
org.bouncycastle.mime.MimeIOException: CMS failure: unable to parse internal stream: Error finalising cipher
java.security.UnrecoverableKeyException: PKCS12 key store mac invalid
```

Seen in `mime`, `openssl`, `cms`, `pkcs`. BouncyCastle's
`CipherInputStream`/`CipherOutputStream` wrap a `javax.crypto.Cipher` and stream
through it; something in the streaming contract (an `update` overload's return
shape, an output-size answer, or a padding boundary) does not match. The
overloads this crate leaves unregistered have been the cause every previous
time — `Cipher.update([BII[B)I`, `updateAAD([BII)V` and `doFinal([BII[B)I` were
each found that way on 2026-08-16 — so start by enumerating what
`javax/crypto/Cipher` still does NOT register.

`Cannot find any provider supporting 1.2.840.113549.1.12.1.3` (PKCS#12
PBEWithSHAAnd3KeyTripleDES) is the same family reached through
`Cipher.getInstance(oid)` with no provider named.

## Residual B — cross-provider key TYPE rejection

```
java.security.InvalidKeyException: unknown public key passed to ML-DSA
java.security.InvalidKeyException: Not an EC key: ECDSA   (sun.security.ec.ECKeyFactory.checkKey)
java.security.InvalidKeyException: cannot identify EdEC/XDH
```

Two directions of one problem, and both now matter because `getInstance(alg,
"BC")` correctly returns BouncyCastle's own objects:

* BouncyCastle's post-quantum and EdDSA SPIs accept only their own key classes,
  so a key that reaches them from a JDK-serviced engine is refused
  (`cert.test`, `cms`).
* SunEC's `ECKeyFactory.checkKey` requires `getAlgorithm().equals("EC")`, and
  BouncyCastle's `ECDSA` generator mints keys whose algorithm is `"ECDSA"` — so
  a BC key handed to this VM's SunEC-backed `Signature` is refused (`its`).
  HotSpot avoids it by never routing that key to SunEC in the first place.

## Residual C — locale / date-format divergence (not JCA)

```
i18n: expected:<Es ist 13:12[ Uhr GMT] am 17.08.2006.>
       but was:<Es ist 13:12[:00 Greenwich Mean Time] am 17.08.2006.>
```

`i18n.test.AllTests` passes on HotSpot. CratonVM formats a German
`DateFormat.MEDIUM` time with the long time-zone display name and a seconds
field where the JDK's CLDR data gives `HH:mm z`. Nothing to do with the JCA.

## Residual D — BouncyCastle core, no JCA involved

```
crypto.test: 130 -> CipherStreamTest: Expected invalid ciphertext after tamper and read : Serpent/CCM
```

`crypto.test.AllTests` is pure BouncyCastle core. The `SICPosition` failure that
used to sit here was a CratonVM native fast-path and is fixed; this one is a
CCM tamper-detection check that does not fire and has not been looked at.
`eac`'s `signature test failed` is likewise unexamined.

## Residual E — the four not yet looked at

`cert.cmp`'s `CRMFException: cannot encode key`, `jce.provider.test.nist`'s
CertPath message mismatches, `jcajce.provider`'s `SignatureSetParameterTest`
(`engineGetParameters` and a `setParameter`-mid-update rejection), and
`jce.provider.test`'s remaining `AEADTest` items.
