# JIT: a compiled body loses a callee-thrown exception, and the caller reads it as success

## Status
**OPEN, root-caused to a method, not yet to a JIT pass** — found 2026-08-17
while closing the bc-java JCA residuals. Two independent bc-java clusters that
had been filed as crypto/PKIX defects are this one JIT bug: with the JIT on they
fail, with the offending method excluded from compilation they pass, and HotSpot
25 passes both.

This is not a slow path or a policy difference. In both cases an exception that
a callee really threw does not reach the handler the bytecode's exception table
names, and the compiled caller carries on as if the call had returned normally.
For a security suite that means a **tamper check that does not fire** and a
**revocation check that is skipped**.

## The two repros

Both run from `apps/bc-java` with the classpath the bc-java suite runner builds.

### 1. `crypto.test` — AEAD tamper detection does not fire

```bash
<cratonvm-bin> --java-home <jdk25> --Xmx 2g -c "$CP" \
  org.bouncycastle.crypto.test.CipherStreamTest
# CipherStreamTest: Expected invalid ciphertext after tamper and read : Serpent/OCB

CRATONVM_JIT=deny CRATONVM_JIT_DENY=org/bouncycastle/crypto/io/CipherInputStream.nextChunk \
<cratonvm-bin> --java-home <jdk25> --Xmx 2g -c "$CP" \
  org.bouncycastle.crypto.test.CipherStreamTest
# CipherStreamTest: Okay
```

The algorithm named in the failure varies run to run (`Serpent/CCM`,
`Camellia/GCM`, `SEED/EAX`, `Serpent/OCB`) because the test reports the FIRST
failing one and the compile order varies — the defect is not algorithm-specific.

`org.bouncycastle.crypto.io.CipherInputStream.nextChunk` is a loop whose body
calls `finaliseCipher()` OUTSIDE any protected range and `processBytes` INSIDE
one:

```java
while (maxBuf == 0) {
    int read = in.read(inBuf);
    if (read == -1) {
        finaliseCipher();               // throws InvalidCipherTextIOException on a bad MAC
        if (maxBuf == 0) return -1;
        return maxBuf;
    }
    try { maxBuf = aeadBlockCipher.processBytes(...); }
    catch (Exception e) { throw new CipherIOException("Error processing stream ", e); }
}
```

Compiled, `finaliseCipher()`'s throw does not leave `nextChunk`: the method
returns `-1`, `read()` reports EOF, and the caller sees a clean end of stream
over TAMPERED ciphertext.

### 2. `jce.provider.test.nist` — CRL revocation checking is skipped

```bash
# 208 PKITS vectors, run in one JVM
<cratonvm-bin> ... P10 "org.bouncycastle.jce.provider.test.nist.NistCertPathTest2#*"
# 12 FAIL

CRATONVM_JIT_DENY=org/bouncycastle/jce/provider/ProvRevocationChecker.check <same>
# 0 FAIL
<same> --nojit
# 0 FAIL
```

`ProvRevocationChecker.check` has TWO disjoint protected ranges catching the
SAME type, each handler conditionally rethrowing the caught local:

```java
if (hasOption(PREFER_CRLS)) {
    try { crlChecker.check(cert); }
    catch (RecoverableCertPathValidatorException e) {
        if (!hasOption(NO_FALLBACK)) ocspChecker.check(cert); else throw e;
    }
} else {
    try { ocspChecker.check(cert); }
    catch (RecoverableCertPathValidatorException e) {
        if (!hasOption(NO_FALLBACK)) crlChecker.check(cert); else throw e;
    }
}
```

Measured with an instrumented copy of the class on the classpath ahead of the
jar: `PREFER_CRLS` is false and `NO_FALLBACK` is false on EVERY call, so the
correct behaviour is always "OCSP fails, fall back to the CRL check". What the
suite sees instead is the OCSP checker's own
`RecoverableCertPathValidatorException: no OCSP response found for any
certificate` escaping to the caller — i.e. the compiled body behaved as if
`NO_FALLBACK` were set, or never entered the handler at all. The remaining
`RuntimeException: Index did not match: 0 got 3` failures are the same thing
seen through the test's index assertion.

**The instrumented copy does not reproduce it.** Adding two
`System.err.println` calls to `check` makes all 208 pass, which is why the
instrument has to be a classpath shadow and the conclusion has to come from the
`CRATONVM_JIT_DENY` A/B rather than from a print.

## What is NOT the cause

Ruled out by measurement, each in one run:

* **Not GC.** `CRATONVM_DBG_GC_STRESS=262144` and `=1048576` leave the PKITS
  count unchanged.
* **Not heap pressure.** `--Xmx 1g` and `--Xmx 4g` both give 12.
* **Not C2 specifically.** `CRATONVM_JIT_NO_EXC_TABLE_C2=1` still gives 12.
* **Not a side-table collision.** Identity hashes are a monotonic counter
  (`ZgcRealHeap::next_hash`), so the per-object JCA side tables cannot inherit a
  dead object's state.
* **Not repetition.** The same PKITS vector run 40 times in one JVM passes 40
  times; what accumulates is the number of DISTINCT methods that get hot.

## What a fix has to explain

The obvious synthetic shapes do NOT reproduce — both are green on this VM:

* two disjoint try ranges catching one type, throw from the second range;
* a loop calling a throwing method outside the try and a non-throwing one
  inside it.

So the trigger needs something the real methods have and the probes do not:
a deep unwind (the throw is raised several frames below the compiled one), a
handler that runs `invokespecial` on a private method of the same class, or a
particular inlining decision. The next step is `CRATONVM_DBG_EXCFRAME=1` on the
PKITS run filtered to `ProvRevocationChecker`, and
`CRATONVM_DBG_JIT_DISASM` on that one method to see which handler edge the
compiled body actually emits.

## Why it matters beyond bc-java

Both symptoms are silent. Nothing logs, nothing throws, and the caller's
success path runs. A `CipherInputStream` over tampered AEAD data reads to a
clean EOF; a PKIX validation skips revocation and either accepts or rejects for
the wrong reason. Any application that relies on an exception to signal a
security decision is exposed to the same shape.
