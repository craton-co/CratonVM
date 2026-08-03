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

## JDK-only corpus

A second class list, `SUITE=jdk-only`, drives the strict-mode corpus described
in [`docs/feature-designs/jdk-only-mode.md`](../docs/feature-designs/jdk-only-mode.md).
The vector-to-blocker mapping (against
[`docs/jdk-only-runtime-services.md`](../docs/jdk-only-runtime-services.md)),
the determinism rules and the corpus's own known limitations live in
[`jdk-only-coverage.txt`](jdk-only-coverage.txt).

```bash
SUITE=jdk-only CRATONVM_ARGS="--jdk-only" bash regression-suite/run.sh
SUITE=all RELEASES="17 21 25" bash regression-suite/run.sh
```

**Additional env overrides:** `CRATONVM_ARGS="--jdk-only"` (extra launcher args;
empty by default, in which case the CratonVM invocation is unchanged) ·
`SUITE=core|jdk-only|all` (default `core` — the historical set, unchanged) ·
`RELEASES="17 21 25"` (javac `--release` matrix; empty by default, i.e. one
pass with no `--release` flag) · `JDK17=` / `JDK21=` / `JDK25=` (optional
per-release JDK homes; a release with no usable javac is skipped with a
message, not failed).

`regression-suite/modules/` holds a real named module built into
`build-modules/` for `RJdkModule`; `regression-suite/resources/` is staged into
`build/` so `RJdkServices` discovers its providers through a real
`META-INF/services` resource.

## How a class passes

A class **PASSES** when, on CratonVM, it: (1) exits 0, (2) prints its
`PASS <Class> (<n> checks)` line, (3) does not crash, **and** (4) its
deterministic output lines (`PASS …` / `CK …`) are byte-identical to HotSpot's.
HotSpot is the oracle — no golden values are hard-coded; any JIT/GC miscompile
that changes a checksum, or any behavioural divergence, fails the diff. (If
`java` is absent the cross-VM diff is skipped and only the in-VM asserts run.)

The one exception is a vector whose **correct** outcome differs between
compatibility modes. Those get an explicit per-mode golden in
[`expect/`](expect/README.txt), which replaces the HotSpot oracle for that class
in that mode. No such golden is shipped today; the single mode-divergent vector
(`RJdkStrict`) is instead only *scheduled* under `--jdk-only`, and prints a
`SKIP` line with the reason otherwise.

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
| `RLambdaDefaultOverload` | lambda SAM dispatch preserves same-named default overloads, including null arguments |
| `RJitGc` | hot int/long/float/double loops (JIT+OSR), **binary-tree alloc + GC churn**, megamorphic dispatch, array bounds — all checksum-diffed vs HotSpot |
| `RConcurrent` | threads, atomics, locks, `ConcurrentHashMap`, executors, futures, latches — **not in the default set** (see Known gaps) |

### JDK-only corpus (`SUITE=jdk-only`)

| Class | Area |
|-------|------|
| `RJdkHello` | bootstrap, `System` streams and properties, `PrintStream`, `String` incl. interning and a UTF‑16 body |
| `RJdkCollections` | `ArrayList` (incl. `subList` view), `HashMap`/`TreeMap`/`LinkedHashMap`, iterators, streams, `Optional` |
| `RJdkRecords` | records + sealed classes: `Record`/`PermittedSubclasses` attributes, `getRecordComponents`, canonical constructor |
| `RJdkLambdas` | `invokedynamic`, `LambdaMetafactory` (incl. `altMetafactory`), bridges, captured values — **and `Function.identity()`**, a named P0 |
| `RJdkHandles` | `MethodHandle` lookup/adaptation/access checks, `VarHandle` field, static, array and atomic access |
| `RJdkProxy` | proxy generation, invocation handlers, `invokeDefault`, exception wrapping, loader identity and caching |
| `RJdkHidden` | `Lookup.defineHiddenClass`, NESTMATE vs non-nestmate, nestmate private access, nest shape |
| `RJdkReflect` | members, `setAccessible`, the **reflection inflation/accessor** path, annotations, serialization incl. `Externalizable` |
| `RJdkJmx` | `ObjectName` canonicalisation, MBean register/attributes/operations/notifications/queries, platform MXBeans — named P0 |
| `RJdkServices` | class-path `ServiceLoader`: `META-INF/services` discovery, `stream()`, `reload()`, `ServiceConfigurationError` |
| `RJdkModule` | a real named module on `--module-path`: descriptor, reads, exports vs opens, encapsulated resources, module service providers |
| `RJdkExecutors` | fixed pool, futures, cancellation, `invokeAll`/`invokeAny`, thread factory, rejection policies, scheduled executor, interruption, `CompletableFuture` |
| `RJdkForkJoin` | `RecursiveTask`/`RecursiveAction`/`CountedCompleter`, parallel streams, worker exceptions, quiescence |
| `RJdkAqs` | `ReentrantLock` (hold counts, `lockInterruptibly`), `Condition`, `ReentrantReadWriteLock`, `StampedLock`, a custom `AbstractQueuedSynchronizer` |
| `RJdkProcess` | `ProcessHandle` current/parent/children/info/liveness/`onExit`, child process exit code and forcible kill — named P1 |
| `RJdkNio` | `Files`/`Path`, `RandomAccessFile`, `FileChannel` incl. **memory mapping** and locks, buffers, `Selector`, **asynchronous close** |
| `RJdkNet` | DNS, loopback TCP echo, socket options, `SO_TIMEOUT`, close-during-read, loopback UDP |
| `RJdkSecurity` | digest/HMAC/AES‑GCM/PBKDF2 KATs, `SecureRandom` invariants, RSA sign/verify + key encoding, `SSLContext`/`SSLEngine`, provider lookup |
| `RJdkJni` | `ACC_NATIVE` metadata, `java.util.zip` native handles, `System.loadLibrary`, unbound natives, reference identity across GC |
| `RJdkFailure` | missing class, **real `NoSuchMethodError`/`NoClassDefFoundError`** (via a same-length constant-pool patch into a hidden class), missing native, missing module, unsupported platform services |
| `RJdkStrict` | mode-**divergent** probes: no fabricated `org.jboss`/`io.quarkus`/`io.smallrye` classes, no `Function$Identity` stand-in, real `ProcessHandle` bytes, bytecode beats native — **`--jdk-only` only** |

## Extending

Add a `src/RFoo.java` that prints `PASS RFoo (<n> checks)` on success (throw /
`System.exit(1)` on failure; print any cross-VM-verified values on `CK …`
lines), then add `RFoo` to the `CLASSES_CORE` list in `run.sh` (or
`CLASSES_JDKONLY` for a strict-mode vector). Keep each class fast (well under a
second) and deterministic.

**Determinism is not optional** — the suite diffs two VMs byte for byte, so any
wall-clock value, pid, port, host name, absolute path, unsorted hash-map
iteration, unseeded random draw, generated class name or non-ASCII stdout byte
is an immediate false failure. The full rule list, and the reasoning behind
each, is at the bottom of [`jdk-only-coverage.txt`](jdk-only-coverage.txt).

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

## Performance gate (CratonBench) — mandatory

`perf/run-cratonbench-gate.sh` is the **mandatory perf-regression gate** for
any change that touches `vm/`, `jit/`, `jit-api/`, `gc/`, `classloading/`, or
the `native-*` crates. It runs every phase of `bench/CratonBench.java`
(arithmetic, fib, sieve, matrix, hashmap, stringregex, bintrees) as an
isolated fresh process — pinned CPU, `-Xmx8g`, median of 5 reps, no discarded
samples — and enforces:

1. **Exact checksums on every run.** A checksum mismatch is a correctness
   regression and fails the gate outright.
2. **No phase median may exceed its baseline by more than 5%**
   (`perf/cratonbench-baseline-azure-epyc.tsv`). `anchored` baselines carry a
   linked evidence doc (e.g. bintrees = 1,468 ms) and may only be re-anchored
   by a new evidence doc; `provisional` baselines may be re-anchored with a
   normal PR justification (use `--calibrate` on a quiet host).
3. **Never measure under load.** The script refuses (exit 3) when the 1-min
   load average exceeds `--max-load` (default 2.0). A refused run is not a
   pass — rerun on a quiet host.

```bash
# On the Azure EPYC bench host, quiet window:
bash regression-suite/perf/run-cratonbench-gate.sh -Exe /abs/path/to/cratonvm
```

The gate is Linux-bench-host-specific by design (taskset pinning, the
baseline file is per-host). For a new bench host, generate
`perf/cratonbench-baseline-<host>.tsv` with `--calibrate` and pass it via
`--baseline`.

**What the gate is evidence about.** Every run records the optimizing tier's
per-phase reach — `ir_requests` / `ir_admitted` / `ir_bodies` in
`samples.tsv`, one `ir_reach_<phase>` line in `manifest.tsv`, and a summary
line on the console. Across all seven phases the optimizing (C2/IR) tier
produces **two** bodies, so the gate measures the **single-pass** backend, and
a CratonBench delta is not evidence about C2 in either direction — including
"the C2 change did no harm". Note `compiles_c2` is a *different* column and is
not a substitute: it counts compiles whose requested tier was C2, including
every one the optimizing pipeline declined and handed back to the single-pass
backend.

Anchoring a workload whose reach has not been measured, and quoting a
CratonBench delta as a C2 result, are the two things `docs/known-issues/c2/`
says to refuse. `perf/c2-reach.sh` measures any workload's reach in one run;
`bench/CratonBenchC2.java` is a characterised candidate that does reach the
tier, deliberately **not** a gate phase and with no baseline.

> Note (2026-07-24): the bintrees anchor deliberately FAILS on current `dev` —
> an open ~4x bt18 regression (post-`cf3a44e2a`) is being bisected. That is
> the gate doing its job; do not re-anchor the baseline to absorb it.
