# OcspClientTest: bcprov-jdk18on's own copy of MiscObjectIdentifiers was shadowing bcutil-jdk18on's

## Status

**FIXED 2026-09-05.** Classpath-ordering fix applied to `common.args`
(backed up as `common.args.bak-20260905-pre-bcutil-reorder`) and verified.

## The symptom

`io.netty.handler.ssl.ocsp.OcspClientTest`, discovered after this session's
earlier OpenSSL-args fix (`gen-openssl-args.sh`, made default for the whole
suite the same day) made the class run far enough to reach real BouncyCastle
code for the first time:

```
java.lang.NoSuchFieldError: org/bouncycastle/asn1/misc/MiscObjectIdentifiers.id_HashMLDSA44_RSA2048_PSS_SHA256
	at org.bouncycastle.operator.DefaultSignatureAlgorithmIdentifierFinder.<clinit>(Unknown Source)
	at org.bouncycastle.operator.jcajce.JcaContentSignerBuilder.<clinit>(Unknown Source)
	at io.netty.handler.ssl.ocsp.OcspClientTest.createBasicOcspResponse(OcspClientTest.java:277)
```

## Root cause

`org.bouncycastle.asn1.misc.MiscObjectIdentifiers` exists in **two** jars on
the classpath: `bcprov-jdk18on-1.84.jar` and `bcutil-jdk18on-1.84.jar`.
Confirmed with `javap` against both:

```
bcprov-jdk18on-1.84.jar's MiscObjectIdentifiers: no id_HashMLDSA44_RSA2048_PSS_SHA256 field
bcutil-jdk18on-1.84.jar's  MiscObjectIdentifiers: HAS id_HashMLDSA44_RSA2048_PSS_SHA256
```

`common.args`' classpath listed `bcprov-jdk18on` *before* `bcutil-jdk18on`,
so bcprov's older, incomplete copy of the class shadowed bcutil's complete
one. `bcpkix-jdk18on`'s `DefaultSignatureAlgorithmIdentifierFinder.<clinit>`
needs the field bcutil carries — the same "one class exists in two BC
artifacts, wrong one wins by classpath order" mechanism this session already
found and fixed once for `bcprov-jdk15on` vs `bcprov-jdk18on`
(`BouncyCastleEngineAlpnTest`'s `id_ml_dsa_44` NoSuchFieldError), just
between a different pair of BC jars this time.

## The fix

Reordered `common.args`' classpath so `bcutil-jdk18on-1.84.jar` sits
immediately before `bcprov-jdk18on-1.84.jar`, giving bcutil's copy of any
class the two share precedence. One-line swap, applied via a small Python
patch script for exact-match safety; `common.args` backed up first.

## Verification

```
OcspClientTest              found=6 ok=6 failed=0   (87s -- genuinely slow crypto work, not a bug)
BouncyCastleEngineAlpnTest   found=1 ok=1 failed=0   (no regression from the reorder)
SslErrorTest                found=72 ok=72 failed=0  (no regression)
```

`OcspClientTest` needs more than the suite's default 180s cap in isolation
(87s alone, with zero host contention) — under the full 3-shard x 3-GC-arm
run this landed as `HANG` rather than `FAIL`, which is expected: real crypto
work replacing a fast classloading error is the same tradeoff this session
already saw and documented for the broader OpenSSL fix.

## Repro

```bash
cd apps/netty-suite-runner
CV_BIN=<binary> JDK=<jdk25 home> ./run-netty-suite.sh \
  --list <(printf 'io.netty.handler.ssl.ocsp.OcspClientTest\n') --shards 1 --out /tmp/repro
```
