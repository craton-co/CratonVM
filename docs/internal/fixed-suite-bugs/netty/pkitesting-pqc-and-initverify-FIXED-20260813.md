# `CertificateBuilderTest` — PQC umbrella names, and a certificate used as a key

**Status:** ✅ FIXED 2026-08-13. `io.netty.pkitesting.CertificateBuilderTest`
now matches HotSpot JDK 25 **exactly**:

```
HotSpot JDK 25 : found=74 started=74 ok=39 failed=28 aborted=7
CratonVM       : found=74 started=74 ok=39 failed=28 aborted=7
```

and the per-test failing sets are identical **in both directions** — no test
fails on CratonVM that passes on HotSpot, and none the other way. HotSpot fails
35 of the 74 on its own (SLH-DSA, absent from JDK 25; `BCJSSE`; the
`rsa4096`/`rsa8192` aborts), so those 35 stay red on both.

Split out of the batch-11 triage record
(`netty-batch11-inet6-and-sha1-oid-CLOSED-20260812.md`) on 2026-08-12 as the
last of its seven causes. Fixed on the Azure Linux host (`20.80.105.49`) from
`origin/dev` `a16c509d5`.

## The delta was 18, and it was three defects

The page opened with 15 and grouped the two `*OfEveryKeyType` methods; the
measured set on `a16c509d5` is 18. **Take the per-test diff keyed on
METHOD + display name** — the runner prints only the display name, and
`[1] mlKem512` and `[12] mlKem512` are different methods. `probes/` has no
extractor; `docs/internal/.../pki-extract` shape is in the Repro below.

| cause | tests |
| --- | ---: |
| `Signature.initVerify(Certificate)` used the certificate as the key | 5 |
| `ML-DSA` / `ML-KEM` umbrella algorithm names unusable | 12 |
| PKIX path building — a cross-signed intermediate read as a broken chain | 1 |

## 1. A certificate handed to the SPI as the verification key — 5 tests

`Signature.initVerify(Certificate)` was registered against the **same callback**
as `initVerify(PublicKey)`, whose body reads `args[1]` as the key. The
certificate was stored as the verification key and forwarded to the SPI, where
it surfaced as two errors that name the SPI and not the defect:

```
ECDSA  NoSuchMethodError: sun.security.x509.X509CertImpl.getAlgorithm()Ljava/lang/String;
         at sun.security.ec.ECKeyFactory.toECKey(ECKeyFactory.java:98)
         at sun.security.ec.ECDSASignature.engineInitVerify(ECDSASignature.java:346)
EdDSA  InvalidKeyException: Unsupported key type
         at sun.security.ec.ed.EdDSASignature.engineInitVerify(EdDSASignature.java:116)
```

**This page had them as two separate causes** — "a single missing accessor" and
"Ed25519/Ed448 remain genuinely unimplemented". Both readings were wrong.
Ed25519/Ed448 verification was never reached; the SPI rejected the argument
before doing any work. The page's own correction was right and is worth keeping:
`getAlgorithm()` is `java.security.Key`'s, so a call landing on a certificate
can only be a wrong argument, and the fix is to find the returning site rather
than add the method. It guessed `getPublicKey()`; the stack trace puts it one
frame further in, at `initVerify` itself.

All five `createCertIssuedBy*` tests reach it through the same line —
`signature.initVerify(root.getCertificate())` — which is why one argument
accounted for two exception types across two key algorithms.

The fix extracts the key and performs the same KeyUsage refusal
`java.security.Signature.initVerify(Certificate)` does: a certificate whose
KeyUsage extension is present and explicitly denies `digitalSignature` is
refused; a certificate without the extension is allowed.

## 2. The PQC umbrella names — 12 tests

`ML-DSA` and `ML-KEM` are real JDK 25 `KeyPairGenerator` algorithms in their own
right (`sun.security.provider.ML_DSA_Impls$KPG`,
`com.sun.crypto.provider.ML_KEM_Impls$KPG` — both `NamedKeyPairGenerator`
subclasses) whose parameter set is chosen by `initialize(NamedParameterSpec)`.
That is exactly how netty asks:

```java
mlDsa44("ML-DSA", namedParameterSpec("ML-DSA-44"), "ML-DSA-44"),
```

`algo_idx` knew only the PARAMETERISED spellings, so an umbrella request
resolved to -1 and fell through `generateKeyPair` to
`NoSuchAlgorithmException` — while the parameterised names worked perfectly.

**The page's claim that "the KeyPairGenerators are present" was right, and the
place it pointed at was not.** `KeyPairGenerator.getInstance("ML-DSA")` does
succeed; `generateKeyPair()` is what threw. That distinction matters twice over:
`getInstance` succeeding is also why netty's BouncyCastle fallback never ran —
`Algorithms.keyPairGenerator` only falls back when `getInstance` throws.

**Still open, and worth its own page:** `KeyPairGenerator.getInstance` accepts
*any* algorithm name on CratonVM. `getInstance("TOTALLY-BOGUS-ALG")` returns a
generator where HotSpot throws `NoSuchAlgorithmException`. Every caller that
uses a failed `getInstance` to select a provider is silently denied its
fallback.

After the fix, every parameter set matches HotSpot byte-for-byte
(`probes/PqcStepProbe.java` prints the two columns):

| | ML-DSA-44 | ML-DSA-65 | ML-DSA-87 | ML-KEM-512 | ML-KEM-768 | ML-KEM-1024 |
| --- | --- | --- | --- | --- | --- | --- |
| X.509 public key bytes, both VMs | 1334 | 1974 | 2614 | 822 | 1206 | 1590 |

Defaults for an uninitialised umbrella generator are **measured**, not assumed:
HotSpot gives ML-DSA-65 and ML-KEM-768.

## 3. Path building, not path validation — 1 test

`authenticatingCrossSignedCertificate` builds two roots, two intermediates
sharing one subject and one key cross-signed by different roots, and a leaf; it
then trusts only one root. `validate_chain` required
`parsed[i].issuer == parsed[i+1].subject` for every `i` and answered
`chain broken at index 1` — the second intermediate is not a break, it is the
*alternative*. RFC 5280 §6 builds the path from the presented set.

The fix validates the presented order first and keeps its verdict, and only on
failure retries a rebuilt path through the same validation, returning the
ORIGINAL error if that does not fully succeed. So building can turn a rejection
into an acceptance and nothing else. That shape was chosen after the first cut —
which reordered before validating — moved the error two existing security tests
pin (`BrokenChain` → `NoTrustAnchor`, `BadSignature{at:1}` →
`BadSignature{at:0}`). Neither was a weakening; both chains were still rejected.
A security predicate should not have its reported reason drift as a side effect
of an unrelated feature, and the two tests were right to fail.

## Not defects, and left alone

* The **revocation policy** message this page warned about —
  `revocation check failed at index 0 and SOFT_FAIL is not set: no OCSP
  responder URI` — does not appear in the CratonVM-only set at all. The two CRL
  tests that carry it (`validCertificatesWithCrlMustPassValidation`,
  `revokedCertificatesWithCrlMustFailValidation`) **fail on HotSpot too** and
  always did.
* **SLH-DSA** is unavailable on both, and HotSpot JDK 25 has no provider for it.

## Repro

```bash
cd /data/cratonvm/apps/netty-suite-runner
CLS=io.netty.pkitesting.CertificateBuilderTest
CP=$(sed -n 2p common.args)

"$JAVA_HOME/bin/java" -cp "$CP:." -Dcraton.batch=1 CratonRunner $CLS > hs.out
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv-bin> --java-home "$JAVA_HOME" --Xmx 1500m \
    @common.args -Dcraton.batch=1 CratonRunner $CLS > cv.out
```

Key the diff on METHOD + display name — the runner prints only the display
name, which is ambiguous across parameterised methods. The method comes from the
first `at io.netty.pkitesting.CertificateBuilderTest.<method>` frame of each
`@@TESTFAIL` block:

```bash
comm -13 <(rows hs.out) <(rows cv.out)   # CratonVM-only failures
```

`probes/PqcStepProbe.java` (one line per getInstance/initialize/generateKeyPair
step) and `probes/CertKeyProbe.java` (what each cert/key accessor hands back)
are the two that isolate causes 2 and 1 without the suite.
