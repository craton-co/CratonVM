# bc-java: what is left after the JCA provider-framework fixes

## Status
**OPEN, characterised, not root-caused as one thing.** This page is the
residue of
`docs/internal/fixed-suite-bugs/bug-bcjava-jca-provider-alias-lookup-and-attribution-20260816.md`,
whose four bugs (alias/OID lookup, provider attribution, the VM-fatal
`NotImplemented`, and the three functional-crypto items) are FIXED and
probe-verified against HotSpot 25. Everything below is what the bc-java
`AllTests` sweep still fails on **after** those fixes, measured on Azure host 2
(`azureuser@20.80.105.49`) on 2026-08-16.

Each item here is a separate finding. They are collected on one page because
they were found by one sweep, not because they share a cause.

## How to reproduce, and the oracle

```bash
# runner + classpath: see the memory note `bcjava-suite-runner-and-oracle-classpath`
CRATONVM_BIN=<vm> OUTDIR=/data/<out> bash /data/bcjca-run.sh /data/bcjava-fail-list.txt
MODE=hotspot   OUTDIR=/data/<out-hs> bash /data/bcjca-run.sh /data/bcjava-fail-list.txt
```

Two harness facts matter, and the first sweep had both wrong:

* the classpath must include `apps/bc-java/libs/unboundid-ldapsdk-6.0.8.jar`.
  Without it HotSpot's own `jce.provider.test.AllTests` dies in `<clinit>` with
  `NoClassDefFoundError` in 1 s and reads as "fails on HotSpot too". With it,
  HotSpot passes that class in 113 s of real crypto tests.
* `org.bouncycastle.test.AllTests` needs `-Dtest.java.version.prefix=25`, or it
  fails on BOTH VMs by design.

**The oracle**, with both fixed: of the 24 classes CratonVM failed on
2026-08-16, HotSpot 25 passes **22**. The two it also fails are not VM defects:

| class | HotSpot's own failure |
|---|---|
| `openssl.test.AllTests` | `OutOfMemoryError` in `SCrypt.SMix` at `-Xmx 1g` |
| `pkix.test.AllTests` | `QcType statement was not recognised` + 4 `MissingEntryException` from BC's own resource bundles |

## Residual A — cross-provider key TYPE rejection (ML-DSA, composite)

```
java.security.InvalidKeyException: unknown public key passed to ML-DSA
  at org.bouncycastle.jcajce.provider.asymmetric.mldsa.SignatureSpi.verifyInit
```

BouncyCastle's post-quantum SPIs accept only their OWN key classes
(`BCMLDSAPublicKey`, …). A key that reaches them from anywhere else — a
certificate parsed by the JDK's own `CertificateFactory`, a `KeyFactory` this VM
services natively — is refused. `KeyPairGenerator.getInstance(alg, "BC")` now
returns BouncyCastle's own generator, so keys minted THAT way are fine; the
remaining failures are keys that arrive by another route.

Seen in `cert.test`, `cert.cmp`.

## Residual B — locale / date-format divergence (not JCA)

```
i18n: expected:<Es ist 13:12[ Uhr GMT] am 17.08.2006.>
       but was:<Es ist 13:12[:00 Greenwich Mean Time] am 17.08.2006.>
```

`i18n.test.AllTests` passes on HotSpot. CratonVM formats a German
`DateFormat.MEDIUM` time with the long time-zone display name and a seconds
field where the JDK's CLDR data gives `HH:mm z`. Nothing to do with the JCA;
filed here only because the sweep found it.

The same family accounts for part of `pkix.test`:
`MissingEntryException: Can't find entry CertPathReviewer.certRevoked.text in
resource file org.bouncycastle.pkix.CertPathReviewerMessages` — except that
HotSpot fails those four too, so only the fifth (`QcType`) would be a VM
finding, and it fails on HotSpot as well.

## Residual C — BC-only algorithms this VM's engines do not implement

`getInstance(alg, "BC")` now routes to BouncyCastle for every engine this VM
intercepts, so most of this family closed. What remains are the places a caller
reaches a CratonVM engine with a name only BouncyCastle implements and no
provider named, in an engine that has no chain fallback yet — chiefly
`MessageDigest` (`GOST3411`, `SM3`, `Tiger`, `Whirlpool`, `Skein*`), and named
curves the ASN.1 layer does not know (`Unknown named curve: 1.2.156.10197.1.301`
— SM2, in `tsp.test`).

`KeyPairGenerator`, `KeyFactory`, `Mac` and `SecretKeyFactory` DO have that
fallback now (chain order, only where this VM's own engine cannot serve the
name); `MessageDigest` does not, because its natives would also have to grow the
"is this receiver ours" guard that the other four have — BouncyCastle's digest
classes extend `java.security.MessageDigest` itself, so returning one puts a
foreign receiver under natives registered on that class. See the memory note
`a-base-class-native-shadows-the-overloads-a-provider-subclass-does-not` for why
that guard is not optional.

## Residual D — not yet looked at

* `cert.crmf`: `CRMFException: cannot encode key: cannot encode privateKeyInfo`,
  and `Error finalising cipher` on the CMS enveloped-data path.
* `jce.provider.test.nist`: `CertPathValidatorException` message mismatches and
  `Trust anchor for certification path not found`.
* `mime`: `InvalidCipherTextIOException: Error during cipher finalisation`.
* `pkcs`: `Cipher not initialized for unwrapping` on a path the wrap/unwrap
  delegation added on 2026-08-16 does not cover.

## Note on cost

Routing a named third-party provider to its own SPI runs that provider's
bytecode. Measured with `KeygenCost.java` on this host, `--nojit`: BouncyCastle
RSA-2048 key generation is **1496 ms** against **1202 ms** for this VM's own
generator, and EC P-256 is **210 ms** against **85 ms**. So the routing itself is
cheap; the long per-class wall times in a `--nojit` sweep (`cms.test` > 420 s
against HotSpot's 9 s) are ordinary interpretation of the additional BouncyCastle
code the suite now reaches, not a cost of the routing. Give the runner
`CLASS_TIMEOUT=1500`.
