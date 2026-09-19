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
`ONLY="RJitGc RCrypto"` (run a subset) · `TIMEOUT=<seconds>` ·
`STRICT_COVERAGE=1` (make an unscheduled vector a failure — see
[Coverage census](#coverage-census)).

## JDK-only corpus

A second class list, `SUITE=jdk-only`, drives the strict-mode corpus described
in [`docs/feature-designs/jdk-only-mode.md`](../docs/feature-designs/jdk-only-mode.md).
The vector-to-blocker mapping (against
[`docs/known-issues/jdk-only/runtime-services-blocker-inventory.md`](../docs/known-issues/jdk-only/runtime-services-blocker-inventory.md)),
the determinism rules and the corpus's own known limitations live in
[`jdk-only-coverage.txt`](jdk-only-coverage.txt).

```bash
SUITE=jdk-only CRATONVM_ARGS="--jdk-only" bash regression-suite/run.sh
SUITE=all RELEASES="17 21 25" bash regression-suite/run.sh
```

**Additional env overrides:** `CRATONVM_ARGS="--jdk-only"` (extra launcher args;
empty by default, in which case the CratonVM invocation is unchanged) ·
`SUITE=core|jdk-only|all` (default `core` — the historical set, unchanged;
`jdk-only` runs the corpus *instead of* core, `all` runs both; an unrecognised
value is a hard error) · `JDK_ONLY=1` (additive: core *plus* the corpus) ·
`RELEASES="17 21 25"` (javac `--release` matrix; empty by default, i.e. one
pass with no `--release` flag) · `JDK17=` / `JDK21=` / `JDK25=` (optional
per-release JDK homes; a release with no usable javac is skipped with a
message, not failed).

> **`SUITE` was documented here for months before `run.sh` implemented it**
> (fixed 2026-08-07). Until then `SUITE=jdk-only bash run.sh` and
> `SUITE=all RELEASES=…` quietly ran the CORE list and reported green — a
> documented invocation whose result said nothing about the corpus it named.
> Only the `CRATONVM_ARGS="--jdk-only"` spelling ever selected the corpus,
> because that one is matched separately. Any green `SUITE=` result recorded
> before that date should be re-run.

`regression-suite/modules/` holds a real named module built into
`build-modules/` for `RJdkModule`; `regression-suite/resources/` is staged into
`build/` so `RJdkServices` discovers its providers through a real
`../apps/META-INF/services` resource.

`regression-suite/modules-overlay/` is a SECOND javac pass, run over
`build-modules/` on a plain classpath after the module is compiled. It exists
because javac refuses to compile a `provides` clause whose provider declares a
`provider()` returning a non-subtype of the service -- which is exactly the
shape `ServiceLoader.loadProvider` carries a RUNTIME check for, and therefore
exactly the shape a negative vector has to reproduce. `compile_modules`
ground-truths the overlay with `javap` and fails the build if it did not land,
because a missing overlay would fail RJdkModule on BOTH VMs and read as a VM
defect.

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

### Things that count as FAILURE, not as absence

The suite's job is to be un-fool-able, so every ambiguity resolves to failure:

- a **class-list entry with no `src/*.java`** — `LIST ERROR`, one failure each.
  This used to be filtered out in silence, so renaming a vector deleted its
  coverage and only lowered the "N passed" count, which nobody diffs;
- a **run that schedules nothing** (`ONLY=" "`, an emptied list, `SUITE=jdk-only`
  against a checkout with no `RJdk*` sources) — hard error, exit 3. "0 passed,
  0 failed" must never exit green;
- an **unrecognised `SUITE` value** — hard error, exit 3, never a fall-back to
  core;
- a **compile failure** (suite or module) — exit 3, or one failure per
  `--release` level in matrix mode;
- a **timeout** — `timeout` returns 124, which is a non-zero rc, which is a
  FAIL;
- a **`RELEASES=` run where every level was skipped** — exit 3.

Absent HotSpot is the one genuine weakening left, and the runner now says so
out loud (`NOTE: no HotSpot at … — cross-VM output diff SKIPPED`): checks
(1)–(3) still run, but the byte-for-byte diff that catches a miscompiled
checksum does not.

### Coverage census

`run.sh` compiles **all** of `src/*.java` but runs only what a class list
names, so a vector can exist, compile, and never execute — coverage that isn't.
That is how `RJdkPhaser` (240 checks) and `RJdkFieldModule` (75 checks) arrived
inert. The runner now cross-checks the two sets on every run:

- a `src/*.java` in no list and not in `UNREGISTERED_CLASSES` → `COVERAGE
  WARNING` by default, `COVERAGE ERROR` (a failure) under `STRICT_COVERAGE=1`.
  It is a warning by default only because vectors land here from several
  branches at once and the lane that lands next must not inherit someone
  else's red. **CI should set `STRICT_COVERAGE=1`.**
- an `UNREGISTERED_CLASSES` entry whose source is gone → `LIST ERROR`, a
  failure, so the exemption list cannot rot.

## What each class covers

| Class | Area |
|-------|------|
| `RCollections` | ArrayList incl. **`subList` view + `toArray(T[])`**, HashMap, TreeMap (custom comparator), LinkedHashMap order, sets, ArrayDeque, `Iterator.remove`, immutable `List.of` |
| `RStrings` | UTF‑16 code-unit length/`charAt`, `indexOf`/`substring`/`replace`/`split`/`join`, `StringBuilder`, locale-pinned `format`, UTF‑8 round-trip |
| `RNumbers` | int/long overflow & `MIN_VALUE`, shift masking, **`Math.round` JDK‑6430675 edge**, shortest-form `Double.toString`, `BigInteger`/`BigDecimal` |
| `RSerial` | `ObjectOutputStream`/`ObjectInputStream` round-trip: primitive + array + nested + **cyclic self-reference** fields, collections |
| `RCrypto` | SHA‑256/HMAC KATs, AES‑GCM round-trip, **RSA‑2048 OAEP + PKCS1 + sign/verify** |
| `RSslEndpointIdentification` | raw `SSLEngine` endpoint identification (`CVE-2018-8034` shape): a handshake to a host the server's certificate does not name must be refused even with an accepting `TrustManager`, and the same check must not reject a handshake to the name the certificate actually carries |
| `RExceptions` | try/catch/finally, NPE/AIOOBE/CCE/arithmetic, **`ArrayStoreException` + covariant/interface-array stores**, cause chains, try-with-resources |
| `RReflect` | methods/fields/invoke, **runtime annotations (dynamic proxy)**, records + `getRecordComponents`, enums, array reflection |
| `ROptionalClassForName` | `Class.forName` and `ClassLoader.loadClass` throw `ClassNotFoundException` for an optional dependency absent from the classpath |
| `RPrivateLambdaOwner` | private lambda bodies remain bound to their resolved declaring class when a child has the same synthetic lambda name |
| `RLambdaDefaultOverload` | lambda SAM dispatch preserves same-named default overloads, including null arguments |
| `RJitGc` | hot int/long/float/double loops (JIT+OSR), **binary-tree alloc + GC churn**, megamorphic dispatch, array bounds — all checksum-diffed vs HotSpot |
| `RJitStringLayout` | two general x64-backend codegen defects found behind the H2 `org/h2/` JIT ban |
| `RJitArrayTypecheck` | BUG-JIT-ARRAY-INSTANCEOF-20260726: the JIT typecheck helper honoured only a *positive* array-descriptor answer |
| `RArraysMismatch` | `Arrays.mismatch`/`equals`/`compare` over every primitive array type, asserted only **after** the helper behind them is JIT-compiled |
| `RExecutorShutdown` | BUG-EXEC-SHUTDOWN-INTERRUPTS-RUNNING-TASK-20260726: `shutdown()` is orderly — only *idle* workers are interrupted |
| `RBlockingQueue` | synthetic blocking-queue natives must not be applied to real JDK queue objects (a four-slot side layout the bytecode cannot see) |
| `RChmKeySetView` | `ConcurrentHashMap.newKeySet()` must return a real `KeySetView`, not a plain `HashSet` |
| `RChannelInterrupt` | BUG-NIO-NULL-INTERRUPTOR-20260726 (part 1) + the blocked-reader-never-wakes defect in the `java.net.Socket` stream path |
| `RSocketChannelInterrupt` | the same pair on the NIO socket path |
| `RAtomicArray` | atomicity of `AtomicInteger`/`Long`/`ReferenceArray`, whose operations CratonVM serves from natives over a plain Java array |
| `RDirectBufferElem` | per-element `DirectByteBuffer` `get`/`put`, served from `native-io/src/direct_buffer.rs` rather than real-JDK bytecode |
| `RMapResizeGc` | HIB-MAPRESIZE-STALE.1: the native `HashMap.put` resize walk under GC pressure |
| `RMapGcStress` | every map native that walks a bucket chain must keep the chain rooted across the Java callbacks it dispatches |
| `RForNameGcStress` | `Class.forName(name, init, loader)` must keep the name `String` and the loader rooted across `loader.loadClass` |
| `ROverlaySystemGcStress` | the in-place old-gen sweep must not free an object the same cycle just promoted |
| `RFileTimes` | file and ZIP-entry timestamp round-trips — the Spring Boot `jarmode-tools` extract pipeline reduced to its timestamp steps |
| `RNioNoFollow` | `LinkOption.NOFOLLOW_LINKS` in `Files.write*` / `new*Stream` / `open` varargs when the final component is a symlink |
| `RSyncMethodJit` | an `ACC_SYNCHRONIZED` method must keep excluding after the JIT compiles it (compiled bodies carry no monitor prologue) |
| `RFieldSiteCache` | the per-thread resolved-**field** site cache must never answer one field reference with another site's answer |
| `RMethodSiteCache` | the per-thread resolved-**method** site cache must never answer one call site with another site's descriptor |
| `RDataInputFastPull` | `DataInputStream` typed reads must observe the same stream position as everything else on the underlying stream |

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
| `RJdkFieldModule` | the JPMS half of `Field.get`/`set` and the typed `getInt`/`setLong` family: `exports` vs `opens`, public field of an unexported package, public field of a non-public class, cross-module protected read from a subclass — every ALLOW row paired with a DENY row |
| `RJdkJmx` | `ObjectName` canonicalisation, MBean register/attributes/operations/notifications/queries, platform MXBeans — named P0 |
| `RJdkServices` | class-path `ServiceLoader`: `../apps/META-INF/services` discovery, `stream()`, `reload()`, `ServiceConfigurationError` |
| `RJdkModule` | a real named module on `--module-path`: descriptor, reads, exports vs opens, encapsulated resources, module service providers -- including the two ILLEGAL `provider()` factory shapes, which must raise `ServiceConfigurationError` from `iterator()` and `stream()` alike |
| `RJdkExecutors` | fixed pool, futures, cancellation, `invokeAll`/`invokeAny`, thread factory, rejection policies, scheduled executor, interruption, `CompletableFuture` |
| `RJdkForkJoin` | `RecursiveTask`/`RecursiveAction`/`CountedCompleter`, parallel streams, worker exceptions, quiescence |
| `RJdkAqs` | `ReentrantLock` (hold counts, `lockInterruptibly`), `Condition`, `ReentrantReadWriteLock`, `StampedLock`, a custom `AbstractQueuedSynchronizer` |
| `RJdkPhaser` | `Phaser` against its real `volatile long state`: `arriveAndDeregister` lowering parties and unarrived together, the terminated phase as `phase \| MIN_VALUE` (not `-1`), inertness after termination, tiering, `onAdvance` on a real subclass |
| `RJdkProcess` | `ProcessHandle` current/parent/children/info/liveness/`onExit`, child process exit code and forcible kill — named P1 |
| `RJdkProcessStreams` | `Process`/`Runtime.exec` stdin/stdout/stderr are live pipes, not stub streams: byte round-trip through stdin→stdout, stdout/stderr kept separate by default, `redirectErrorStream` merging, and the real child exit code |
| `RJdkNio` | `Files`/`Path`, `RandomAccessFile`, `FileChannel` incl. **memory mapping** and locks, buffers, `Selector`, **asynchronous close** |
| `RJdkNet` | DNS, loopback TCP echo, socket options, `SO_TIMEOUT`, close-during-read, loopback UDP |
| `RJdkSecurity` | digest/HMAC/AES‑GCM/PBKDF2 KATs, `SecureRandom` invariants, RSA sign/verify + key encoding, `SSLContext`/`SSLEngine`, provider lookup |
| `RJdkJni` | `ACC_NATIVE` metadata, `java.util.zip` native handles, `System.loadLibrary`, unbound natives, reference identity across GC |
| `RJdkFailure` | missing class, **real `NoSuchMethodError`/`NoClassDefFoundError`** (via a same-length constant-pool patch into a hidden class), missing native, missing module, unsupported platform services |
| `RJdkStrict` | mode-**divergent** probes: no fabricated `org.jboss`/`io.quarkus`/`io.smallrye` classes, no `Function$Identity` stand-in, real `ProcessHandle` bytes, bytecode beats native — **`--jdk-only` only** |

### Unregistered vectors (`UNREGISTERED_CLASSES` in `run.sh`)

These exist under `src/` and are scheduled by **no** list. They are named
explicitly so "not scheduled" stays distinguishable from "forgotten"; the
coverage census fails if one of them disappears, and warns about any *other*
unscheduled vector.

| Class | Why it is not scheduled |
|-------|-------------------------|
| `RConcurrent` | Heavy multi-threaded execution; trips the documented cross-thread JIT-frame root-scan gap (see Known gaps) and flakes. Run with `ONLY="RConcurrent"`. |
| `RPriorityQueueGc` | Needs **both** `--nojit` **and** `--Xmx 64m` — a live JIT frame downgrades the young generation to a non-moving sweep, under which the stale reference still resolves and the defect hides; the small heap is what makes a collection happen inside the native at all. With `--nojit` alone the vector **passes on a broken VM**. Wired into `class_cv_args`. |
| `RTreeRangeGc` | Needs a small heap (`--Xmx 64m`) or no collection happens during the range-view walk at all. It must **not** get `--nojit`: it reproduces with the JIT on, so registering it keeps the compiling configuration under test. Wired into `class_cv_args`. |

> Both GC vectors *were* registered and validated FAIL-then-PASS under this
> runner when their fixes landed (`6cd01bcba`, `b2e13e441`). A later `run.sh`
> merge resolution silently discarded the registrations and the argument hook —
> that is why they are unregistered today, not because they were never run.

Both GC vectors pass on HotSpot 25 with byte-identical output over repeated
runs, so the vectors themselves are sound; what is missing is a CratonVM run
under the arguments their own doc comments already assume the suite supplies.
Registering them is a task for a lane that can build and run the VM. The
per-vector **CratonVM-only** argument hook they need is `class_cv_args()` in
`run.sh`, kept deliberately separate from `class_args()`: `class_args` is
handed to HotSpot too, and a `--nojit` there would make the oracle exit
non-zero, return empty key lines, and fail the cross-VM diff for a reason that
has nothing to do with the VM.

## Extending

Add a `src/RFoo.java` that prints `PASS RFoo (<n> checks)` on success (throw /
`System.exit(1)` on failure; print any cross-VM-verified values on `CK …`
lines), then add `RFoo` to the **`CORE_CLASSES`** list in `run.sh` (or
**`JDKONLY_CLASSES`** for a strict-mode vector). Keep each class fast (well
under a second) and deterministic.

**Adding the source is not adding the vector.** `run.sh` globs `src/*.java`
into `javac` but runs only what a list names, so a file that is not in a list
compiles on every run and never executes. If a vector genuinely should not be
scheduled, put it in `UNREGISTERED_CLASSES` with a reason instead — the
coverage census treats anything in neither place as a defect.

Which list:

- **`CORE_CLASSES`** is the default green baseline. A vector goes here only
  once it is known to pass on CratonVM. Adding a never-run vector here turns
  the plain `bash run.sh` red for everyone who lands next.
- **`JDKONLY_CLASSES`** is scheduled only under `SUITE=jdk-only`/`all`,
  `JDK_ONLY=1`, or `CRATONVM_ARGS="--jdk-only"`, and is documented as
  *expected* to fail where `--real-jdk` passes. A vector written to expose a
  gap that is not fixed yet belongs here, where its red is informative rather
  than blocking.

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
- **Receiver-typed `protected` field access** (JLS 6.6.2.1) — a subclass caller
  may read an inherited `protected` field only *through a receiver of its own
  type*. HotSpot throws `IllegalAccessException` for
  `buf.get(new ByteArrayOutputStream())` from a subclass; CratonVM's
  `caller_may_access_member` takes no receiver and allows it. `RJdkFieldModule`
  documents this and deliberately does **not** assert it — the divergence is
  masked today by the module gate that vector is fixing, so closing the module
  gap unmasks it.
- **`RELEASES="17 …"` does not compile** — `run.sh` compiles all of
  `src/*.java` in one `javac` call, so one vector above the level takes the
  whole level down. Today that is `RChmKeySetView`
  (`Executors.newVirtualThreadPerTaskExecutor()`, Java 21; `ExecutorService`
  in try-with-resources, Java 19). Levels 21 and 25 are clean. This is counted
  as a failure, not skipped.

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
produces **three** bodies, so the gate measures the **single-pass** backend, and
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

## Bridge ratchet (unadjudicated `Bridge` natives) — gate

`bridge-ratchet.sh` is the second non-Java gate hosted here, and the only one
that measures the *native registry* rather than execution. Contract §1.5
defines a `NativeKind::Bridge` as what an `ACC_NATIVE` method binds to;
**10,084 of the 10,844 `Bridge` registrations have no `ACC_NATIVE` target**,
every one of them inherited its kind from an ambient `set_category`, and until
this gate nothing stopped that number rising. See
the retired `native-kind-is-ambient-and-defaults-to-syntheticstub` write-up.

```bash
JAVA_HOME=/path/to/jdk25 sh regression-suite/bridge-ratchet.sh
sh regression-suite/bridge-ratchet.sh --selftest    # hermetic: no VM, no JDK
```

It boots the VM against a real JDK image, takes the schema-3 census
(`--explain-jdk-only --dump-native-registry`, whose `image_declaring_method`
column is what makes "adjudicated?" a machine-readable fact), and scores it
against [`scripts/baselines/jdk-only-bridge-ratchet.json`](../scripts/baselines/jdk-only-bridge-ratchet.json).

**Why it lives here and not in `native-builtins/tests/`** — beside
`stub_ratchet.rs`, which it is modelled on: the question needs a real JDK image
at measurement time and `cargo test` has none. The alternative, committing a
census artefact and asserting over it in a unit test, was rejected: an
11,909-row snapshot keyed to one JDK build rots, and a rotting baseline is
worse than none.

It asserts three things, with `SLACK = 0`:

1. `bridge.without_acc_native <= baseline` — the ratchet.
2. `bridge.shadows_bytecode <= baseline` — the same ratchet on the subgroup
   that has already produced a defect. A `Bridge` shadowing concrete bytecode
   reaches §7 step 3's decline, which used to fall through to
   `UnsatisfiedLinkError` instead of to the bytecode; that is how `--jdk-only`
   came to be unable to start a thread.
3. `total_rows >= 8_000` — a **collapse detector, not a measurement**, exactly
   as in `stub_ratchet.rs`'s `essential_registry_is_populated`. Do not cite it
   as a fact about the registry's size.

**The baseline is keyed by `<jdk-feature>/<os>`,** and a census with no
matching entry is a refusal (exit 2), never a pass. `image_declaring_method`
describes one runtime image, and the registrars are platform-conditional — a
Linux baseline scoring a Windows census is the "silently combines two different
worlds" defect [`scripts/jdk-only-census.sh`](../scripts/jdk-only-census.sh)'s
header exists to not repeat.

**Exit codes:** `0` pass · `1` the gate fired · `2` refused to adjudicate (never
a pass) · `3` a prerequisite is missing.

**In CI it is blocking**, in `build-and-test`'s ubuntu leg beside the
synthetic-stub ratchet — that leg is the `25/linux` key the committed baseline
covers, and all three non-zero exits fail the job. The advisory `jdk-only`
matrix runs it too, across JDK 21/25 × ubuntu/windows, where the three legs with
no committed baseline report themselves ungated rather than green.

**The guard is shown to fail on every run.** `--selftest` runs first,
hermetically, and injects an unadjudicated `Bridge` into a synthetic census
that the gate must reject — plus an *adjudicated* one it must accept, so the
gate is not simply always-red. A guard never shown to fail is decoration; three
shipped inert in this feature. The one-off injection into the real registrar
that the lane doc asks for is recorded in
`L6-unadjudicated-bridge-ratchet-DONE-20260805.md`.

**Re-freezing.** A count that goes *down* passes and prints an instruction; it
is never absorbed automatically, because a slack-free ratchet left at the old
number silently re-admits exactly that many new unadjudicated rows.

```bash
JAVA_HOME=/path/to/jdk25 sh regression-suite/bridge-ratchet.sh \
    --update-baseline --note "L5: native-io migrated to register_with_kind"
```
