# bc-java, all 53 classes, once per collector

## What this run is

The 53-class bc-java `AllTests` sweep, **one shard**, run three times — once for
each collector CratonVM implements. Previous sweeps only ever ran the default.

The collector is a runtime selector (`-XX:+Use*GC`), not a build feature, so all
three arms run the **same binary**. That is what makes this a comparison rather
than three unrelated runs.

```text
worktree   /data/cvm-gcsweep-20260819   branch perf/bc53-gc-sweep-20260819
binary     /data/vm-gcsweep-20260819.bin   md5 657603e4ce5b91affe557d3d0398c443
built from dev 9aa12c45d
harness    /data/bc53-gc-sweep.sh -> /data/bc53-shard.sh, SHARDS=1, -Xmx 1g,
           JIT ON, CLASS_TIMEOUT=1800 (matches the existing 53-class baseline)
```

**The selectors were verified, not assumed.** `-XX:+UseSerialGC` — a collector
this VM does not implement — prints `unsupported garbage collector ... falling
back to Generational`; the three real selectors print nothing. That negative
control is what rules out a silent fallback making two arms secretly identical.

| collector | PASS | FAIL | HANG |
|---|---:|---:|---:|
| `zgc` (default) | **49** | 2 | 2 |
| `g1` | 48 | 3 | 2 |
| `generational` | 47 | 4 | 2 |

The `zgc` arm reproduces the documented 49/2/2 baseline exactly, which is the
control that makes the other two readable.

## Collector-INDEPENDENT rows

Identical in all three arms, so nothing here is a collector question:

```text
jce.provider.test.AllTests          FAIL   (all three)
pkix.test.AllTests                  FAIL   (all three)
pqc.crypto.test.AllTests            HANG   (all three — 1800s cap)
pqc.jcajce.provider.test.AllTests   HANG   (all three — 1800s cap)
```

Neither `pqc` row is stuck; both are slow past the cap
(bug-bcjava-pqc-53class-20260818.md).

## Collector-DEPENDENT rows, each rerun 3x per collector

A single differing run is a sample, not a measurement. Every differing class was
rerun **three times under every collector** (`/data/gc-confirm.sh`), which is
what separated two real defects from one known flake:

| class | zgc | g1 | generational |
|---|---|---|---|
| `pqc.math.ntru.test` | PASS PASS PASS | **FAIL FAIL FAIL** | PASS PASS PASS |
| `cert.ocsp.test` | PASS PASS PASS | PASS PASS PASS | **FAIL FAIL FAIL** |
| `crypto.test` | PASS PASS PASS | PASS PASS PASS | **PASS HANG HANG** |

### 1. `pqc.math.ntru.test` — deterministic, G1 only

```text
testRqSumZeroFromBytes(org.bouncycastle.pqc.math.ntru.test.PolynomialTest)
java.util.IllegalFormatConversionException: d != java.lang.Object
```

`%d` was handed a bare `java.lang.Object`. That is the fingerprint of the
**W7-84 autobox wrapper becoming visible to Java**: the guard that fires all
over these logs boxes a non-reference value into an `AUTOBOX_CLASS_ID` wrapper
"so the value survives and every collector agrees", and a wrapper read back as
its own type is an `Object`, not an `Integer`. Under G1 something that should
have stayed an `Integer` comes back as the wrapper.

This is the strongest lead of the three: deterministic, 97s to reproduce, one
collector, and the exception names the mechanism.

### 2. `cert.ocsp.test` — deterministic, Generational only

```text
testOCSP: PKIXRevocationTest: Exception:
java.lang.IllegalArgumentException: unexpected object: null
```

Fails in **1 second**, 3/3. A null where an object is expected is the same shape
as the root-collection gap already recorded in
bug-bcjava-53class-residuals-20260817.md, and it being collector-specific is
consistent with that being a GC-lane defect rather than a library one.

### 3. `crypto.test` — NOT deterministic; a known flake, amplified

The sweep recorded FAIL:

```text
testCrypto(SimpleTestTest): 130 -> CipherStreamTest: Unexpected exception CAST5/SIC
```

That is the **already-documented `CipherStreamTest` flake** (~1 run in 4 on the
default collector), not a new defect. The reruns confirm it is not deterministic
under `generational` either — `PASS HANG HANG`, where the two HANGs are the
600s cap in the confirmation harness, not the 1800s suite cap.

So `generational` makes this class both flakier and slower, but the underlying
defect is one this tree already knows about. **Without the repeat runs this row
would have been filed as a third collector-specific defect, which it is not.**

## What this run does NOT say

Arm wall-clock was `zgc` 1h48m, `g1` 1h40m, `generational` 1h20m. **Do not read
that as a collector ranking.** The arms ran sequentially over six hours on a
shared host whose load average fell from ~28 to ~2.5 across them, so the later
arms were measurably advantaged by something that has nothing to do with the
collector. A timing comparison needs interleaved arms on a quiet box; this run
was designed to answer pass/fail, and only that.

## Next

1. **`pqc.math.ntru.test` under G1** is the one to take first — deterministic,
   fast, single-collector, and `d != java.lang.Object` points straight at the
   autobox wrapper leaking into Java-visible state.
2. **`cert.ocsp.test` under Generational** — 1-second deterministic repro of the
   "null where an object should be" family.
3. Neither of these is reachable from the default collector, which is why six
   sweeps of the default never found them.
