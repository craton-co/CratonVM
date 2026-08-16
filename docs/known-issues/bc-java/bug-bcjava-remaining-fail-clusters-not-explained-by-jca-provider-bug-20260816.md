# bc-java: remaining FAIL clusters not explained by the JCA provider alias/attribution bug

## Status
**OPEN, mixed confidence, root causes NOT identified** — found 2026-08-16
running bc-java's `AllTests` suites under CratonVM on Azure, differential-
verified against real HotSpot JDK 25. This is a pointer/triage doc covering
FAIL classes left over after accounting for:
* the JCA provider alias-lookup/attribution/native-bypass cluster
  (`bug-bcjava-jca-provider-alias-lookup-and-attribution-20260816.md` —
  Bugs A/B/C, explains most of the original 24-class FAIL list: `cert.c509`,
  `cert.cmp`, `cert.ocsp`, `cms`, `eac`, `its`, `openssl`, `pkcs`, `pkix`,
  `tsp`, `jcajce.provider`, `operator` — confirmed here via
  `NoSuchAlgorithmException: 1.2.840.113549.1.1.1 KeyFactory not available`
  in `operator.test.AllTests`, same Bug A shape);
* the already-flagged, not-yet-root-caused Bug D items in that same doc
  (AEAD encrypt failure, SIC range-check, ML-DSA key rejection);
* `org.bouncycastle.test.AllTests` — confirmed **not** CratonVM-specific:
  fails on HotSpot too, for an unrelated harness-configuration reason
  (`testAssertExpectedJVM`: the `test.java.version.prefix` system property
  isn't set by this ad-hoc classpath-based harness — it's normally set by
  BC's own Ant/Gradle build). No CratonVM action needed here.

Each item below is a distinct failure *signature*, not yet individually
root-caused — grouped only where the evidence directly suggests a shared
cause. Filed together as a triage pointer rather than five separate
under-evidenced docs, consistent with time spent investigating each so far.

## Cluster 1: "Error during cipher finalisation" — cert.crmf, mime (2 of 24)
```
cert.crmf.test.AllTests:  CRMFException: cannot process data: Error during cipher finalisation
                          CRMFException: Cannot parse decrypted data: Error finalising cipher
mime.test.AllTests:       InvalidCipherTextIOException: Error during cipher finalisation
                          MimeIOException: CMS failure: ... Error finalising cipher
```
Both wrap a lower-level `doFinal()`/cipher-finalization failure inside
higher-level CRMF encrypted-value building and S/MIME enveloped-data
decryption respectively. The generic "Error during cipher finalisation"
wording (BC's own wrapper message, not a JCA-lookup exception) suggests
this may share a root cause with the already-flagged, separately-filed
"AEAD encrypt with additional data failed" item in
`bug-bcjava-jca-provider-alias-lookup-and-attribution-20260816.md`'s Bug D
section (also a cipher `doFinal`-stage failure) — **not confirmed**, only
flagged as the natural first thing to check given the matching shape.

## Cluster 2: certificate/key "unsupported encoding" — cert.path, mozilla (2 of 24)
```
cert.path.test.AllTests: CertificateEncodingException: unsupported encoding
mozilla.test.AllTests:   InvalidKeyException: error encoding public key
```
Both are ASN.1/X.509 **encoding** failures (not parsing/decoding) on
certificate or public-key objects, in two unrelated call paths (CertPath
validation vs. Netscape SPKAC signed-public-key-and-challenge). Possibly a
shared ASN.1 DER-encoding code path issue; not confirmed — no shared stack
frame identified between the two failure sites yet.

## Cluster 3: cert.test.AllTests — two independent failures in one class
```
testFrodoKEM(PQCCertTest): IllegalArgumentException: Unknown signature type requested: ML-DSA
testSimpleTests(AttrCertTest): principal[0] for entity names don't match
```
The `ML-DSA` one is very likely the **same** issue as the already-flagged
`cert.plants.test.AllTests` ML-DSA `InvalidKeyException` Bug D item in the
JCA cluster doc — both are BC rejecting/mishandling an ML-DSA key or
signature-type identifier, reached from two different test classes. Worth
merging investigation once someone picks up that Bug D item.

The `AttrCertTest` X.500 principal-name-comparison failure looks unrelated
— an `X500Name`/`X500Principal` equality or encoding-order difference
causing two representations of "the same" distinguished name to compare
unequal. Not investigated further here.

## Cluster 4: cert path validation policy divergence — jce.provider.test.nist (1 of 24)
```
NistCertPathReviewerTest.testUserNoticeQualifierTest18: path rejected when should be accepted
```
A PKIX cert-path validation policy decision (accept/reject) differs between
CratonVM and HotSpot for at least one NIST PKITS test vector. Could be a
downstream effect of the JCA provider-lookup cluster (if a policy/algorithm
lookup silently fails and the validator conservatively rejects instead of
throwing), or a genuinely separate cert-path-validation logic difference —
not distinguished here. Only one of presumably many `NistCertPathReviewerTest`
methods was inspected; worth checking whether this is an isolated case or
part of a wider pattern within that one class before concluding anything.

## Not covered here
`pqc.jcajce.provider.test.AllTests` (`AIMerTest.testNamedKeyPairGenLocked`:
"no exception" — expected an exception that wasn't thrown) was seen to
**complete** with a normal JUnit failure report in this pass, which is a
different shape from the process-crashing "not implemented" native panic
described for this same class in Bug C of the JCA cluster doc — meaning
this class has **at least two independent CratonVM-specific problems**
(a graceful assertion failure on some runs/algorithms, a hard native panic
on others). Not reconciled or further investigated in this pass.

## Next steps
* Pick Cluster 1 (cipher finalization) first — it has the clearest existing
  lead (Bug D's AEAD failure) and touches real-world-relevant code paths
  (CMS/S-MIME encryption).
* For Cluster 3's ML-DSA half, merge with the existing `cert.plants` Bug D
  investigation rather than treating as separate.
* For `pqc.jcajce.provider.test.AllTests`, first separate its failures by
  which specific algorithm/test method crashes the process (Bug C) vs which
  merely fails a JUnit assertion (this doc), since those are evidently two
  different bugs currently conflated under one class name.

## Repro
```bash
cd apps/bc-java
source <toolchain env>
CP="$(cat bcjava-classpath.txt)"
<cratonvm-bin> --java-home <jdk25-home> --nojit --Xmx 1g -c "$CP" \
  junit.textui.TestRunner org.bouncycastle.cert.crmf.test.AllTests
# or: mime.test.AllTests / cert.path.test.AllTests / mozilla.test.AllTests /
#     cert.test.AllTests / jce.provider.test.nist.AllTests
```
