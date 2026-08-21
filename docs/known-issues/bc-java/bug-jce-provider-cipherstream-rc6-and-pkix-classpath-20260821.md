# The two bc-java `FAIL` classes: one harness gap, one real defect

The 53-class sweep leaves two classes failing identically on both arms of a
fix/base A/B. They look alike in the results table and are nothing alike.

| # | class | verdict |
|---|---|---|
| 028 | `org.bouncycastle.jce.provider.test.AllTests` | **VM defect** — HotSpot passes |
| 040 | `org.bouncycastle.pkix.test.AllTests` | **harness gap** — HotSpot fails identically |

## 040 — not a VM defect: the classpath omits pkix's resources

Five failures, all resource-bundle lookups:

```text
1) CheckNameConstraintsTest.testPKIXCertPathReviewer
     MissingEntryException: Can't find entry CertPathReviewer.noValidCrlFound.text
     in resource file org.bouncycastle.pkix.CertPathReviewerMessages
2) CheckNameConstraintsTest.testNameConstraintsAppliedToLeaf   (notPermittedEmail.text)
3) PKIXCertPathReviewerCrlReasonTest.testOutOfRangeReasonCodePkixReviewer (certRevoked.text)
4) PKIXCertPathReviewerCrlReasonTest.testBeyondIntReasonCode              (certRevoked.text)
1) QcStatementReviewerTest.testModernQcStatementsAreRecognised
     AssertionFailedError: QcType statement was not recognised
```

All three named keys **are present** in
`pkix/build/resources/main/org/bouncycastle/pkix/CertPathReviewerMessages.properties`
(650 lines). The bundle is simply not on the classpath. Every other module's
resources directory is:

```text
 1  pkix/build/classes/java/main
 2  pkix/build/classes/java/test
 3  pkix/src/test/resources          <- test resources, not main
 6  prov/build/resources/main        <- prov has one
10  core/build/resources/main        <- core has one
    …                                    pkix/build/resources/main is ABSENT
```

Measured on the same host, same fixture:

| classpath | HotSpot | CratonVM |
|---|---|---|
| shipped `/data/bcjca-classpath.txt` | 4 errors, 1 failure | 4 errors, 1 failure |
| + `pkix/build/resources/main` | **OK (19 tests)** | **OK (19 tests)** |

So all five are the missing bundle, including the `QcType` one, which reads the
same file. Nothing here is a VM divergence, and the fix is one classpath entry.

This is the second omission found in this same classpath file — the first was
`unboundid-ldapsdk`, which made `jce.provider.test` die in `<clinit>` on HotSpot
and read as "fails on HotSpot too". Same file, same shape, opposite direction:
that one hid a real defect, this one invented five.

**Not applied here.** `/data/bcjca-classpath.txt` is a shared fixture and two
long sweeps were reading it when this was found; changing it under them would
corrupt their results.

## 028 — a real defect: an empty transformation reaches the AES dispatch

```text
HotSpot:   OK (1 test)
CratonVM:  index 18 CipherStreamTest2: Unexpected exception RC6/CTR/NoPadding
```

Run standalone, the cause is CratonVM's own guard:

```text
java.lang.IllegalStateException: Cipher dispatch reached the AES path for
transformation '' (family None); `classify_transformation` admitted a name this
arm cannot compute. Refusing to encrypt with a substitute algorithm.
    at javax.crypto.CipherInputStream.getMoreData(CipherInputStream.java:155)
    at javax.crypto.CipherInputStream.read(CipherInputStream.java:281)
    at CipherStreamTest2.testWriteRead / testWriteReadEmpty / testModes
```

The guard is in `native-builtins/src/jca/cipher.rs:3787`, and it is doing its
job — refusing to compute AES for something that is not AES. **Its stated
diagnosis is wrong, though**, and that is the lead:

> Not reachable through `Cipher.getInstance`, which now refuses every name
> outside the table — so reaching it means the admission table and this dispatch
> have drifted apart, not that a user asked for something odd.

The transformation is not an odd name that slipped past the admission table. It
is **empty**. `algo` comes from `state.algorithm.clone()` (line 3548), i.e. the
native cipher state's own record of what it was constructed for — so this object
reached `update`/`doFinal` with a state that never had a transformation written
into it. The admission table is not implicated; nothing was admitted.

RC6 is a Bouncy Castle algorithm, not a JDK one, so the `Cipher` here comes from
BC's provider SPI. The shape to check first is the one already recorded in
`a-base-class-native-shadows-the-overloads-a-provider-subclass-does-not`: a named
provider's engine returns its own SPI object, and that object still meets
CratonVM's base-class natives on the overloads it does not override. If BC's SPI
performed the `init` and a CratonVM base-class native serviced the stream read,
the native's `state.algorithm` would be exactly what it is — empty.

### One thing that does not fit yet

Run directly, `CipherStreamTest2` prints that stack trace and then reports
`CipherStreamTest2: Okay`; under the `SimpleTestTest` JUnit wrapper the same
condition is a failure at index 18. Whether the standalone path swallows the
exception or exercises a different mode list is not established, and it matters
for building a minimal repro — do not assume the standalone run is the same
event until that is checked.

## What is not claimed

The 028 mechanism above is a hypothesis with one piece of direct evidence (the
empty `state.algorithm`) and one piece of circumstantial support (RC6 being
BC-provided). It has not been confirmed by instrumenting the init path, and the
standalone/harness discrepancy is unexplained.
