# JIT-only zero-length array — `ArrayIndexOutOfBoundsException: Index 0 out of bounds for length 0`

## Status
**OPEN.** Deterministic, JIT-only, reproducible in a self-contained
default-package probe in under a second. Found 2026-08-01 while establishing why
`org.h2.test.unit.TestMemoryEstimator` fails on cratonvm.

## Severity
**HIGH.** It is a silent wrong-length array reaching application code, and it is
the real blocker on lifting the `org/h2/` JIT ban (`vm/src/jit/skip_list.rs`,
`HIB-LONGTAIL.1`), whose stated lift condition is "once `TestMemoryEstimator`
passes lifted".

## Symptom

```
java.lang.ArrayIndexOutOfBoundsException: Index 0 out of bounds for length 0
    at ExactEstimatorProbe.testPageEstimator(ExactEstimatorProbe.java:84)
```

| arm | result |
| --- | --- |
| HotSpot jdk-25, 3 rounds | clean |
| cratonvm **JIT**, 3 rounds | **fails 3/3, deterministic** |
| cratonvm **`--nojit`**, 3 rounds | clean |

## Two things that will mislead you

**1. cratonvm reports the wrong exception TYPE.** Running H2's own
`TestMemoryEstimator`, cratonvm's stack rendering says

```
Exception in thread "main" java/lang/NullPointerException
    at org/h2/test/unit/TestMemoryEstimator.testPageEstimator(TestMemoryEstimator.java:77)
```

The real exception is an `ArrayIndexOutOfBoundsException`, at a different line.
Chasing the reported NPE means hunting a null `Integer` — autoboxing, `aastore`,
GC — that does not exist. **Get a JDK-formatted `printStackTrace` out of your
own probe before trusting cratonvm's exception type or line attribution.** That
mis-report is itself worth fixing and may be a separate defect.

**2. A near-miss probe passes.** `PageStorageProbe` mirrors the failing inner
loop exactly — generic `T[] createStorage` through a covariant bridge, boxed
`aastore`, unboxing `aaload`, then the same real `MemoryEstimator` array
overload — and runs **20 000 rounds clean**. `ArrayLenProbe`, which checks
`storage.length != pageSz` directly on every iteration with `createStorage`
confirmed JIT-compiled (`[JIT_COMPILED] ArrayLenProbe$TestDataType.createStorage(I)[Ljava/lang/Integer;`),
also passes 20 000 rounds. **A reduced probe that passes does not exonerate the
shape.** Copy the method verbatim first; reduce afterwards.

## Where it is narrowed to

The failing method is (H2's `TestMemoryEstimator.testPageEstimator`, copied
verbatim into the probe):

```java
int pageSz;
for (int i = 0; i < size; i += pageSz) {          // increment reads a var
    pageSz = random.nextInt(48) + 1;              // ...assigned in the BODY
    Integer[] storage = dataType.createStorage(pageSz);
    for (int k = 0; k < pageSz; k++) { storage[k] = ...; }
    int y = MemoryEstimator.estimateMemory(stat, dataType, storage, pageSz);
}                                                  //  ^ indexes storage[0]
```

`storage.length` and the `count` argument disagree, so `pageSz` is read
inconsistently across its three sites — or `storage` is not the array that was
just created. Ruled out so far:

* `createStorage` returning a wrong-length array (checked directly, 20 000
  iterations, JIT-compiled — clean);
* `pageSz < 1` (checked, never observed);
* the inner fill loop running the wrong number of times (checked, clean).

**The structural difference between the passing probes and the failing method is
the loop shape**: `for (i = 0; i < size; i += pageSz)` keeps `pageSz` live
across the back-edge and reads it at three sites, where the passing probes used
a plain `i++`. That is the next thing to test.

## Reproduction

`ExactEstimatorProbe.java` (default package, so no package ban applies; needs
only `h2/target/classes` for `MemoryEstimator` + `BasicDataType`):

```bash
javac -cp <h2>/target/classes -d probe ExactEstimatorProbe.java
<cratonvm> --java-home <jdk25> --Xmx 1g -c "<h2>/target/classes:probe" ExactEstimatorProbe 3
<cratonvm> --java-home <jdk25> --Xmx 1g --nojit -c "<h2>/target/classes:probe" ExactEstimatorProbe 3   # clean
```

Probe sources: `docs/internal/repros/h2-insert-scale-20260731/`.

## Why the skip-list's account of `TestMemoryEstimator` is wrong

`vm/src/jit/skip_list.rs` records it as "PASS 3/3 banned vs FAIL 3/3 lifted —
H2's statistical `MemoryEstimator` computes a wrong average once org/h2 is
compiled". Measured 2026-08-01, all four parts are wrong:

* it fails in **both** arms (ban active and lifted) — it is not ban-linked;
* the average is fine; the recorded `err=0.290` **passes** its own `< 0.3`
  bound. The failing assertion was `pct <= 7` at `TestMemoryEstimator.java:61`;
* **HotSpot fails it too**, intermittently (`err=0.1305` against its own
  `< 0.12` bound) — it is a statistical test on an *unseeded* `Random`;
* the actual failure is this AIOOBE, which is not statistical at all.

Note this ban's "sole blocker" slot has now held three different wrong answers:
the TreeMap ctor-drop (fixed 2026-07-31), "wrong average", and this. Re-derive
the blocker before trusting the comment.

## Related
* `docs/known-issues/h2/bug-h2-testmultithread-concurrent-insert-throughput-timeout.md`
  — the investigation this fell out of.
* `java.util.Random.nextGaussian()`'s discarded partner, fixed in the same
  change, which is why `TestMemoryEstimator`'s *statistics* now match HotSpot's.
