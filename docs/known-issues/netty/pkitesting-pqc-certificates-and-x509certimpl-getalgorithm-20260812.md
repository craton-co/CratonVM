# `CertificateBuilderTest` — PQC certificates, and a `getAlgorithm()` on the wrong receiver

**Status:** OPEN (2026-08-12). Split out of
[the batch-11 triage record](../../internal/fixed-suite-bugs/netty-batch11-inet6-and-sha1-oid-CLOSED-20260812.md)
when that page's other six causes were closed. This is the whole of what is
left, re-measured on `dev` `e48ebe9d0` — the batch-11 page's own numbers for it
are stale in both directions.

## The delta is 15 tests, not 12

`io.netty.pkitesting.CertificateBuilderTest`, one class per VM, same classpath:

```
HotSpot JDK 25 : found=74 started=74 ok=39 failed=28 aborted=7
CratonVM       : found=74 started=74 ok=21 failed=46 aborted=7
```

HotSpot fails 35 of these itself — SLH-DSA (absent on JDK 25), `BCJSSE`, and the
`rsa4096`/`rsa8192` aborts — so the raw 18-test gap overstates CratonVM. The
per-test diff (CratonVM's failing set minus HotSpot's) is **15**:

```
[9] mlDsa44   [10] mlDsa65   [11] mlDsa87                 — 3
[12] mlKem512 [13] mlKem768  [14] mlKem1024               — 3
[1]  mlKem512 [2]  mlKem768  [3]  mlKem1024               — 3
createCertIssuedBySameAlgorithm()                          — 1
createCertIssuedByDifferentAlgorithmEcp256vsRsa2048()      — 1
createCertIssuedByDifferentAlgorithmEcp384vsEcp256()       — 1
createCertIssuedByDifferentAlgorithmEd25519vEcp256()       — 1
createCertIssuedByDifferentAlgorithmEd448vEcp256()         — 1
authenticatingCrossSignedCertificate()                     — 1
```

## What is already closed, so nobody re-measures it

**The KeyPairGenerators are present.** The batch-11 page recorded "9 × `ML-DSA
KeyPairGenerator not available`, 3 × `ML-KEM …`". That is no longer true —
`KeyPairGenerator.getInstance` succeeds for `ML-DSA`, `ML-KEM` **and**
`SLH-DSA` on CratonVM, and SLH-DSA is available here while HotSpot JDK 25
throws `NoSuchAlgorithmException` for it. The failures moved from key generation
into **certificate construction and verification**; two of the nine PQC rows
still report `NoSuchAlgorithmException: ML-DSA KeyPairGenerator not available`
from a *different* provider lookup path, so the surface is not uniformly wired.

**The signature-verification family is wider than it was.** Closing batch-11's
cause 2 added SHA-1/384/512-with-RSA and ECDSA-with-SHA-384/512 to
`x509_manager::verify_one_signature`. It did **not** move any test on this
class, so `createCertIssuedByDifferentAlgorithmEcp384vsEcp256` is not blocked on
the ECDSA-384 verify path. Ed25519/Ed448 remain genuinely unimplemented
(`OID_SIG_ED25519` is routed to `NotImplemented` deliberately), which covers two
of the six chain tests but not the other four.

## The cheapest thread to pull: `X509CertImpl.getAlgorithm()`

```
java.lang.NoSuchMethodError: sun.security.x509.X509CertImpl.getAlgorithm()Ljava/lang/String;
```

The batch-11 page called this "a single missing accessor, and the cheapest item
on this page". **It is not a missing accessor.** JDK 25's `X509CertImpl` has no
`getAlgorithm()` at all:

```
$ javap -p --module java.base sun.security.x509.X509CertImpl | grep -i getAlg
  public java.lang.String getSigAlgName();
```

so HotSpot would raise the same error if it ever dispatched there — and it does
not. `getAlgorithm()` is `java.security.Key.getAlgorithm()`. A call that lands on
`X509CertImpl` is therefore a **receiver confusion**: something that should be
returning a `Key` (almost certainly `X509CertImpl.getPublicKey()`, or a
`KeyPair`/`PrivateKey` accessor on the pkitesting path) is handing back the
*certificate* instead. Adding the method would paper over that; find the
returning site instead.

That makes it the right entry point anyway — it is one wrong return value, and
it plausibly sits under several of the six chain tests.

## Not to be confused with

* the **revocation policy** message that also appears in this log —
  `CertificateException: revocation check failed at index 0 and SOFT_FAIL is not
  set: no OCSP responder URI (no PKIXRevocationChecker override and no AIA
  extension)`. That is CratonVM's own PKIX checker refusing a chain HotSpot
  accepts, and it is a policy decision, not a crypto gap;
* `chain broken at index 1`, which is a path-building answer and needs the
  chain dumped before it means anything.

Both are inside this class's remaining delta and neither has been isolated.

## Repro

```bash
cd /data/cratonvm/apps/netty-suite-runner
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv-bin> --java-home "$JAVA_HOME" --Xmx 1500m \
    @common.args -Dcraton.batch=1 CratonRunner io.netty.pkitesting.CertificateBuilderTest
```

Take the per-test diff, never the raw counts — HotSpot fails 35 of the 74 on its
own, and every previous reading of this class has been inflated by them:

```bash
# CratonVM-only failures
comm -13 <(sort -u hotspot-testfails.txt) <(sort -u cratonvm-testfails.txt)
```
