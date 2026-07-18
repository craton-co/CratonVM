# CratonVM fast regression suite

A small, fast, deterministic set of Java classes that exercise the VM's
critical paths and **diff CratonVM's output against HotSpot**. It is the quick
"is the VM still healthy?" check that runs in seconds — complementary to the
heavy app-gauntlet (`test-infra/run-all-apps-suites.sh`), which boots real
frameworks and takes minutes.

The classes are weighted toward the areas most prone to silent regression
(and the ones recent fixes touched): JIT/GC codegen, the `java.util`
collections intrinsics, UTF‑16 string handling, serialization round-trips,
JCA crypto, array-store/exception semantics, and reflection/annotations.

## Running

```bash
# Builds target/release/cratonvm.exe first (build-cpu.bat), then:
bash regression-suite/run.sh
```

Output:

```
== compiling regression-suite ==
  RCollections   PASS
  ...
  RJitGc         PASS
---------------------------------------------
REGRESSION SUITE: 8 passed, 0 failed
```

The script exits non-zero if any class fails, so it is CI-ready.

**Env overrides:** `CV=<cratonvm.exe>` · `JDK=<jdk home>` ·
`ONLY="RJitGc RCrypto"` (run a subset) · `TIMEOUT=<seconds>`.

## How a class passes

A class **PASSES** when, on CratonVM, it: (1) exits 0, (2) prints its
`PASS <Class> (<n> checks)` line, (3) does not crash, **and** (4) its
deterministic output lines (`PASS …` / `CK …`) are byte-identical to HotSpot's.
HotSpot is the oracle — no golden values are hard-coded; any JIT/GC miscompile
that changes a checksum, or any behavioural divergence, fails the diff. (If
`java` is absent the cross-VM diff is skipped and only the in-VM asserts run.)

Each source file is self-contained (default package, only JDK classes) and
runnable on its own: `cratonvm -cp build RJitGc`.

## What each class covers

| Class | Area |
|-------|------|
| `RCollections` | ArrayList incl. **`subList` view + `toArray(T[])`**, HashMap, TreeMap (custom comparator), LinkedHashMap order, sets, ArrayDeque, `Iterator.remove`, immutable `List.of` |
| `RStrings` | UTF‑16 code-unit length/`charAt`, `indexOf`/`substring`/`replace`/`split`/`join`, `StringBuilder`, locale-pinned `format`, UTF‑8 round-trip |
| `RNumbers` | int/long overflow & `MIN_VALUE`, shift masking, **`Math.round` JDK‑6430675 edge**, shortest-form `Double.toString`, `BigInteger`/`BigDecimal` |
| `RSerial` | `ObjectOutputStream`/`ObjectInputStream` round-trip: primitive + array + nested + **cyclic self-reference** fields, collections |
| `RCrypto` | SHA‑256/HMAC KATs, AES‑GCM round-trip, **RSA‑2048 OAEP + PKCS1 + sign/verify** |
| `RExceptions` | try/catch/finally, NPE/AIOOBE/CCE/arithmetic, **`ArrayStoreException` + covariant/interface-array stores**, cause chains, try-with-resources |
| `RReflect` | methods/fields/invoke, **runtime annotations (dynamic proxy)**, records + `getRecordComponents`, enums, array reflection |
| `ROptionalClassForName` | `Class.forName` and `ClassLoader.loadClass` throw `ClassNotFoundException` for an optional dependency absent from the classpath |
| `RPrivateLambdaOwner` | private lambda bodies remain bound to their resolved declaring class when a child has the same synthetic lambda name |
| `RJitGc` | hot int/long/float/double loops (JIT+OSR), **binary-tree alloc + GC churn**, megamorphic dispatch, array bounds — all checksum-diffed vs HotSpot |
| `RConcurrent` | threads, atomics, locks, `ConcurrentHashMap`, executors, futures, latches — **not in the default set** (see Known gaps) |

## Extending

Add a `src/RFoo.java` that prints `PASS RFoo (<n> checks)` on success (throw /
`System.exit(1)` on failure; print any cross-VM-verified values on `CK …`
lines), then add `RFoo` to the `CLASSES` list in `run.sh`. Keep each class
fast (well under a second) and deterministic.

## Known gaps (intentionally not asserted)

These are real CratonVM divergences from HotSpot that the suite deliberately
does **not** assert, so the baseline stays green. They are tracked here as a
to-do list — when one is fixed, re-enable the corresponding check:

- **Type-strict wrapper keys** — `Integer(1)`, `Long(1)`, `Short(1)` are not
  distinct `HashMap` keys (they should be).
- **Supplementary-char `indexOf(char)` / `getChars` positioning** — `length()`
  and `charAt()` are correct UTF‑16 code units, but `indexOf(char)` past a
  surrogate pair returns a code-point index.
- **`Math.max`/`min` NaN propagation**.
- **Large-exponent `Double`/`Float.toString`** (e.g. `1e20` shortest form).
- **Fail-fast `ConcurrentModificationException`** detection during iteration.
- **`KeyGenerator.getInstance("AES")`** (and similar key-gen providers).
- **`RConcurrent` / heavy multi-threaded execution** — intermittently hangs in
  cross-thread JIT-frame root scanning at a stop-the-world GC pause (there is a
  `fix/multithread-jit-roots-stw` branch for this). Excluded from the default
  set; run it once the gap is closed with `ONLY="RConcurrent"`.
