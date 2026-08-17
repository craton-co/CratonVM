# bc-java: what was left after the JCA provider-framework fixes — CLOSED

## Status
**CLOSED 2026-08-17** on `fix/bcjava-residuals-20260816`. Every family this page
recorded is either fixed or re-homed to the page that owns its real cause. The
bc-java `AllTests` sweep on Azure host 2 went from

| run | of the 24 classes CratonVM failed on 2026-08-16 |
|---|---|
| CratonVM, `dev` @3ef3eb744 (this page's starting point) | 6 PASS, 17 FAIL, 1 HANG |
| CratonVM, after the JCA fixes below | 16 PASS, 7 FAIL, 1 HANG |
| **CratonVM, after the JIT exception fix as well** | **17 PASS, 6 FAIL, 1 HANG** |
| HotSpot 25, same harness | 22 PASS, 2 FAIL |

Newly passing: `cert.cmp`, `cert.crmf`, `cert.test`, `eac`, `i18n`, `its`,
`mime`, `openssl`, `tsp`, `jce.provider.test.nist` — plus `cms` from 121 failing
methods to 1, `jcajce.provider` from 16 to 2, `pkcs` from 7 to 1.

`openssl` now passes on CratonVM where HotSpot 25 runs out of heap at `-Xmx 1g`
inside `SCrypt.SMix`; that is not a CratonVM defect either way.

## What each residual turned out to be

### Residual 0 — `pqc.jcajce.provider` HANG
Unchanged, and not a correctness defect: the class does real ML-KEM/ML-DSA in
BouncyCastle's pure Java and exceeds the budget where HotSpot takes 271 s. It
has its own page,
bug-bcjava-pqc-lms-hsstests-interpreter-throughput-cliff-20260816-FIXED.md
(this directory), resolved 2026-08-17.

### Residual A — the CMS/PKCS cipher-stream family — FIXED
Not one bug but five, all in `javax.crypto.Cipher`, all of the same species: a
door the JDK declares and this VM did not answer.

* **`init(mode, key, AlgorithmParameters)` dropped the caller's parameters**
  outside the PBES2 arm and recorded an EMPTY IV. Every CMS/CRMF content
  encryptor initialises that way (`EnvelopedDataHelper.createContentCipher`
  rebuilds the parameters from the ASN.1 `AlgorithmIdentifier`), so a CBC
  encrypt ran with no IV and died at `doFinal` inside SunJCE's own `engineInit`
  with `InvalidAlgorithmParameterException: Wrong IV length: must be 16 bytes
  long` — reported two layers up as `CRMFException: cannot process data: Error
  during cipher finalisation`.
* **`init(ENCRYPT_MODE, key)` on an IV-taking mode did not GENERATE an IV**, and
  **`getParameters()` answered null** for everything but PBES2. SunJCE mints a
  random IV and hands it back; the caller is expected to persist it. Without
  that, `SunProviderTest`/`NullProviderTest` wrote an `AlgorithmIdentifier` with
  absent parameters and could not decrypt what they had just encrypted.
* **`doFinal([BI)I`, `doFinal([BII[BI)I`, `updateAAD(ByteBuffer)`,
  `init(int, Certificate[, SecureRandom])` and `getExemptionMechanism()` were
  unregistered**, so they fell through to real `Cipher` bytecode whose
  `checkCipherState()` throws against a receiver whose state lives in
  `CIPHER_TABLE`. `AEADTest.testGCMParameterSpecWithMultipleUpdates` died there.
* **`getInstance(String)` never asked the provider chain.** It consulted a
  hand-written table and refused anything not on it, so
  `Cipher.getInstance("1.2.840.113549.1.12.1.3")` and
  `getInstance("2.16.840.1.101.3.4.4.1")` failed while BouncyCastle serves both.
  It now walks the chain after its own verdict, never before it.
* **`init` forwarded a NULL `SecureRandom` to a delegate SPI.** The JDK passes
  `JCAUtil.getSecureRandom()` even for the two-argument form, and providers
  dereference it: every `RFC3211WrapTest` case was an NPE inside BouncyCastle,
  and `AESTest.wrapTest(2, ...)` — which passes a `FixedSecureRandom` and
  asserts a fixed ciphertext — produced a different answer on every run.

`javax.crypto.Mac` had the same shape twice: `init(Key, AlgorithmParameterSpec)`
never reached a provider-delegated `MacSpi` (so BouncyCastle's own engine
reported `DESede engine not initialised` at the first `update` — the whole
`NewAuthenticatedDataTest` family), `getInstance(String, Provider)` was
unregistered, and `getInstance(String)` refused a name the chain could serve
(`1.3.14.3.2.26`, which is how BC's PKCS#12 MAC calculator asks for HMAC-SHA1).

`PKCS12ParametersGenerator`'s native KDF raised for any digest but SHA-1 where
it should decline to BouncyCastle's own bytecode, and SunJCE's symmetric
`AlgorithmParameters` rows (AES, GCM, DESede, DES, Blowfish, RC2,
ChaCha20-Poly1305, DiffieHellman and the PKCS#5 v1.5 PBE set) were missing from
the service table entirely.

**Residual**: one method, `NewEnvelopedDataTest.testKeyTransDESEDE3Short`, still
fails with `unable to parse internal stream: Error finalising cipher`.

### Residual B — cross-provider key TYPE rejection — FIXED
This VM bound a `Signature`'s SPI eagerly, so a key minted by another provider
reached an SPI that will not take it — SunEC's `ECKeyFactory` requires
`getAlgorithm().equals("EC")` and BouncyCastle's generator mints `"ECDSA"` — and
an algorithm outside this engine's own table was refused outright
(`Signature.verify() could not be performed for Unknown`). Both now fall back
across the provider chain, which is what the JDK's delayed provider selection
(`Signature$Delegate.chooseProvider`) does. `its` and `tsp` pass.

`Signature.getAlgorithm()` also answered this engine's canonical spelling, and
`"Unknown"` for anything off-table; BouncyCastle's `X509SignatureUtil` feeds
that straight back to `AlgorithmParameters.getInstance(..)`. It now echoes the
name the caller asked for.

### Residual C — locale / date-format divergence — FIXED
`TimeZone.getDisplayName` ignored the locale and answered from an English-only
table. It now reads the real CLDR `TimeZoneNames` bundle — the `jdk.localedata`
one for every locale but English, the `java.base` one for English — cached, so
`SimpleDateFormat`'s `z` field (the HTTP `Date`-header path) does not pay a
`ResourceBundle.getBundle` per format. `i18n` passes.

**Known gap this exposed, not fixed here**: the rest of the locale display-name
surface is still English-only. `Locale.getDisplayCountry(Locale.GERMAN)`
answers `US` where HotSpot answers `Vereinigte Staaten`,
`Currency.getDisplayName(Locale.GERMAN)` answers `USD`, and
`ZoneId.getDisplayName(TextStyle.FULL, ..)` answers the zone ID. Date patterns
and month names are correct.

### Residual D — BouncyCastle core, no JCA involved — RE-HOMED
`crypto.test`'s `CipherStreamTest` AEAD tamper check was **a JIT defect**, not a
crypto one, and is FIXED — see the retired page
`fixed-suite-bugs/jit/bug-jit-compiled-body-loses-a-callee-thrown-exception-20260817.md`.
Fixing it exposed the NEXT entry in the same `SimpleTestTest` list, which had
never been reached: `SymmetricConstraintsTest` fails with "no exception!" (on
`--nojit` too, so not a JIT problem), and because it leaves a PROCESS-WIDE
`CryptoServicesRegistrar` constraint set, all 14 `HPKETestVectors` cases after it
fail with "service does not provide 192 bits of security". One defect, fifteen
rows.

`eac`'s `signature test failed` was Residual B and is fixed.

### Residual E — the four not looked at — FIXED or RE-HOMED
* `cert.cmp`'s `CRMFException: cannot encode key` — Residual A, fixed.
* `jce.provider.test.nist`'s CertPath message mismatches — **a JIT defect**, now
  FIXED; the class passes all 286 vectors.
  Four of them (PKITS 4.3.3/4.3.4/4.3.5/4.3.11) were a JCA bug instead:
  `X500Principal` answered the RFC 2253 string for all three name forms and
  compared THAT in `equals`, so two DNs differing only in attribute-name case or
  in runs of spaces were unequal and a CRL could not be matched to its issuer.
  That is fixed (canonical form, `hashCode` consistent with it, `toString` with
  the full keyword map). The remaining 35 are the JIT bug above, reproduced from
  `ProvRevocationChecker.check`, and closed with it.
* `jcajce.provider`'s `SignatureSetParameterTest` — fixed:
  `Signature.getParameters()` / `getParameter(String)` were unregistered and
  fell through to `SignatureSpi`'s `UnsupportedOperationException`.
  `Provider.Service.getAttribute` likewise answered null because the JDK's own
  body reads a map keyed by `java.security`'s private `UString` wrapper.
* `jce.provider.test`'s `AEADTest` items — fixed (the unregistered `doFinal`
  overload).

## What is still open

| class | what is left | owner |
|---|---|---|
| `crypto.test` | 1 real (`SymmetricConstraintsTest`: an expected `CryptoServiceConstraintsException` never raised) + the 14 `HPKETestVectors` it poisons | this page |
| `cms` | 1 (`testKeyTransDESEDE3Short`) | this page |
| `pkcs` | 1 (`testCreateAES256andSHA256`) | this page |
| `jce.provider` | 1 (`AESTest`: GCM does not refuse a repeated key+IV on encrypt) | this page |
| `jcajce.provider` | 2 (`GeneralKeyTest.testDstu4145`, `SignatureSetParameterTest.testSetParameterMidUpdateStillRejected`) | this page |
| `pkix` | 1 CratonVM-only (`IDPRelativeNameTest`, a multi-valued RDN) — the other 5 fail on HotSpot too | this page |
| `pqc.jcajce.provider` | HANG | the PQC throughput page |

`jce.provider.test.nist` and `CipherStreamTest` are gone from this table: both
were the JIT defect, and both pass.

## How to reproduce
```bash
# runner + classpath: /data/bcjca-run.sh, /data/bcjca-classpath.txt on Azure host 2
CVM_JIT_FLAG=" " CRATONVM_BIN=<vm> OUTDIR=/data/<out> CLASS_TIMEOUT=1500 \
  bash /data/bcjca-run.sh /data/bcjava-fail-list.txt
MODE=hotspot OUTDIR=/data/<out-hs> bash /data/bcjca-run.sh /data/bcjava-fail-list.txt
```
The classpath must include `apps/bc-java/libs/unboundid-ldapsdk-6.0.8.jar`, and
`org.bouncycastle.test.AllTests` needs `-Dtest.java.version.prefix=25` or it
fails on BOTH VMs by design. Run with the JIT on: `--nojit` costs about 15x
here and turns ordinary slowness into HANG rows — but see the JIT page, because
`--nojit` is also what makes two of these classes pass.
