# CratonVM

[![CI](https://github.com/craton-co/cratonvm/actions/workflows/ci.yml/badge.svg)](https://github.com/craton-co/cratonvm/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.80%2B-orange.svg)](https://www.rust-lang.org/)

A Java Virtual Machine written entirely in Rust with a custom x86-64 JIT compiler.

**Boots against a real JDK when one is present, and runs standalone when it isn't.**
By default CratonVM loads the real `java.base` module from a detected JDK (via `JAVA_HOME`,
`CRATONVM_JAVA_HOME`, or `java` on your `PATH`). When no JDK is found — or when you pass
`--synthetic-jdk` — it falls back to its own synthetic Rust implementations of the Java
standard library, so it can run with **no JDK installation, no `JAVA_HOME`, no `rt.jar`**.

## Features

- **Bytecode interpreter** with 140+ fast-path opcodes
- **x86-64 JIT compiler** (x64 core ~32,000 lines, ~140 bytecodes, 26 optimization rounds; experimental AArch64 backend) with OSR, LICM, bounds-check elimination, AVX2 SIMD, and precise JIT stack maps (default-on)
- **Generational garbage collector** (young/old, write barriers, card table; non-moving sweep + selective promotion default; opt-in region-based G1 via `-XX:+UseG1GC`, experimental; plus a feature-gated ZGC stub)
- **Multi-threading** with monitors, locks, barriers, and virtual threads
- **Lambda/invokedynamic** support via LambdaMetafactory
- **Thousands of native method registrations** (java.lang, java.util, java.io/nio, java.time, java.util.concurrent, JCA crypto, ...)
- **JNI & embedding** — JNI Invocation API, implicit local-reference frames, a GC pin set for critical sections, and a stable C-ABI embedding library (`libcratonvm`) + Rust facade (`cratonvm-embed`)
- **Security hardening** — fail-closed I/O confinement, outbound-network egress policy (cloud-metadata/SSRF block + optional DNS resolution), HTTP request-body caps and anti-smuggling, and an OS-CSPRNG–backed `SecureRandom` (see [Security & sandboxing](#security--sandboxing))
- **Container/cgroup awareness** — cgroup v1/v2 memory & CPU detection + a container-aware default-heap helper (launcher wiring is a documented follow-up; see [docs/CONTAINER.md](docs/CONTAINER.md))
- **Observability** — Java Flight Recorder (JFR), plus advisory `cargo-llvm-cov` coverage reporting (see [docs/COVERAGE.md](docs/COVERAGE.md))
- **GPU offload** (opt-in) — Java bytecode → PTX lowering for CUDA
- A large Rust/Java test corpus plus HotSpot-differential regression tooling; CI is configured for build, fmt, clippy, and test checks, with coverage and difftest currently advisory
- **~880,000 lines** of Rust across 20 workspace crates

### Java Version Support

| Version | Key Features |
|---------|-------------|
| Java 8 | Full language support, lambdas, streams |
| Java 11 | Nest-based access control (JEP 181) |
| Java 17 | Records (JEP 395), sealed classes (JEP 409) |
| Java 21 | Pattern matching for switch, virtual threads, sequenced collections |
| Java 25 | Stream gatherers, scoped values, structured concurrency, class file version 69 |

### Benchmark (vs HotSpot JDK 25 C2)

| Benchmark                          | JDK 25 C2     | CratonVM default | Default ratio |
|-------------------------------------|---------------|-------------------|---------------|
| Arithmetic (2B ops)                 | 2,006 ms      | 4,895 ms          | 2.44x         |
| Fibonacci(44)                       | 1,719 ms      | 4,790 ms          | 2.79x¹        |
| Sieve (100K x 20,000)               | 2,851 ms      | 6,508 ms          | 2.28x         |
| Matrix 1280x1280                    | 2,349 ms      | 6,875 ms          | 2.93x         |
| **QuickBench TOTAL**                | **8,925 ms**  | **23,068 ms**     | **2.58x**     |
| HashMap (10M put/get, isolated)     | 1,471 ms      | 5,488 ms          | 3.73x         |
| String/Regex (100K, isolated)       | 54 ms         | 193 ms            | 3.57x         |
| Binary Trees (depth=18, isolated)   | 176 ms        | 1,468 ms          | 8.34x²        |

*Unless a row-specific footnote says otherwise, all rows were remeasured
2026-07-18 (residuals round) on the Azure Linux benchmark
host (EPYC 9V45, SMT), pinned to logical CPU 13 with `taskset`, as medians of
5 alternating freshly-launched JDK/CratonVM process pairs against Temurin JDK
25.0.3 C2 and a CratonVM candidate at default settings. Checksums matched on
every run. The 10M-HashMap and 100K-String/Regex size-variant rows are
single alternating JDK/CratonVM pairs (added 2026-07-18; checksums
1549999915000000 and 5000050000, exact on both sides — the 10M case
exercises the materialized-map fallback and GC under the default-ON
TLAB-refill triggers). The shared host carried load ~7-10 during this
sweep (vs ~5 for the previous table): both columns inflate together, but CratonVM's
memory-heavy rows inflate more, so rows unchanged by this round (Fibonacci,
Sieve, Matrix) moved within the ±10-15% contention noise band — their
underlying code is identical to the previous measurement. The
host was re-provisioned around 2026-07-14: absolute times are NOT comparable
with the previous table (same-binary A/B showed identical code ~1.7x slower on
the new host for CratonVM's memory-heavy paths while JDK barely moved), so
every row was re-based; ratios are the durable content. The isolated harnesses
(`bench/HashMapOnly.java`, `bench/StringRegexOnly.java`,
`bench/BinTreesClassic.java`, `bench/QuickBenchLong2.java`) are now committed —
bench/ is gitignored, which is how earlier copies kept getting lost.*

¹ Fibonacci is recursion-bound. Its recursive self-call already compiles to
a guarded DIRECT call (no dispatch helper); the remaining gap is register
allocation and recursion inlining, which the template/IR backends do not do
yet.
² Binary Trees at `-Xmx8g` (same flags both sides), measured in a quiet-window
follow-up as seven alternating fresh-process pairs on CPU 13. HotSpot:
175/177/176/176/191/176/175 ms (median 176); CratonVM:
1,479/1,463/1,487/1,466/1,468/1,465/1,474 ms (median 1,468). Every checksum
was `68332206`. This follow-up removes two remaining allocation-path costs:
the JIT refill boundary now lets the non-moving young collector fill 90% of
its active space before its O(heap) sweep (ordinary/moving cycles retain the
50% Cheney-copy headroom), and compact reference `putfield` is default-on,
including inlined constructor bodies. A verifier/resolver-backed first-store
proof emits the two fresh `Node` stores with only compact-layout and old-gen
guards; legacy, old, repeated, and otherwise ambiguous stores retain the
barrier helper. Full evidence:
`docs/internal/performance/binarytrees-half-gap-20260718.md`.

**This round (2026-07-18 residuals,
`docs/internal/performance/halfgap-residuals-20260718.md`):** the
trigger-quarantining young-walk corruption root-caused and fixed (unaligned
young capacity minting off-grid TLAB sizes + free blocks; mark oracle now
fails safe above a truncated walk) — `CRATONVM_TLAB_GC_TRIGGER` default ON
and default-heap Binary Trees no longer wedges. Arithmetic 3.54x→2.44x
(64-bit constant div/rem strength reduction: `ldc2_w` + `lmul/ldiv/lrem`
fusion — pow2 shifts and signed-magic mulhi, probe-verified against HotSpot
across sign/overflow edges). String/Regex 15.6x→10.6x (StringBuilder joins
the exact-receiver object-native cache: `append(I)/(C)/(String)` resolve the
registry once per callsite; plus `ldc`-String wired in the early-compile
path, which had been inserting every string-bearing method into the JIT skip
set). HashMap 5.16x→4.47x (Integer boxing fast path writes the value cell
raw on its freshly-allocated TLAB arm). Also fixed while in the BCE: a
latent soundness hole where variable-stride (`j += i`) loop IVs were
bounds-check-elided with no step-sign/overflow proof — steps are now
provenance-proven and runtime-guarded; a sound inclusive-loop (`<=`) elision
was built and probe-verified, but ships opt-in (`CRATONVM_JIT_INCLUSIVE_BCE`)
because it measured as a net loss on the memory-homed template bodies.

**Previous round (2026-07-17/18, `docs/internal/performance/halfgap-20260717.md`):**
HashMap 7.3x→5.2x (BLOCKGC debug-probe getenv cache — it ran on EVERY native
call; guarded restore of the JIT inline-compact allocation that a Groovy
stale-layout fix had disabled, now safe behind a per-class layout-replace
counter baked into the code; exact-HashMap `put`/`get` thin direct calls with
a wrapper-free Integer-overlay fast path, wired in BOTH compile tiers — the
OSR-tier site is the one benchmark-style once-invoked methods actually use).
String/Regex 24x→15.6x (the OSR-artifact compiler silently bailed on ANY
method containing a string constant — `ldc` String was "not yet wired" — and
permanently OSR-denied it, so `StringRegexOnly.run`'s whole workload ran
interpreted; failure now prints under `CRATONVM_DBG_JITC`). Binary Trees
~110x→58x (second TLAB-remnant wedge: the free list degrades to blocks just
under the refill floor — 10.5M consecutive refill failures with ONE young GC
in a 23s run; fragmentation-floor mini-TLABs re-enable bump allocation, and
the mark phase's exact-object oracle now materializes ranges only for
conservative candidates instead of an ~800MB per-GC Vec of every young
object, with precise heap edges bypassing it entirely — mark phase
2,098→474 ms).

Earlier rounds' full find/fix histories:
[`docs/internal/performance/halfgap-residuals-20260718.md`](docs/internal/performance/halfgap-residuals-20260718.md) (this round),
[`docs/internal/performance/halfgap-20260717.md`](docs/internal/performance/halfgap-20260717.md),
[`docs/internal/performance/hashmap-sieve-half-gap-20260714.md`](docs/internal/performance/hashmap-sieve-half-gap-20260714.md),
[`docs/internal/performance/quickbench-half-gap-3rows-20260713.md`](docs/internal/performance/quickbench-half-gap-3rows-20260713.md),
[`docs/internal/hashmap-native-dispatch-overhead.md`](docs/internal/hashmap-native-dispatch-overhead.md), and
[`docs/internal/performance/string-regex-overallocated-groups-fastpath-20260714.md`](docs/internal/performance/string-regex-overallocated-groups-fastpath-20260714.md).

See [docs/JIT_OPTIMIZATION.md](docs/JIT_OPTIMIZATION.md) for the full 26-round JIT optimization journey.

### Benchmark — GPU offload (vs HotSpot C2 & TornadoVM)

CratonVM can transparently offload eligible static methods over primitive
arrays to an NVIDIA GPU — no annotations, no API, no code changes
(`cargo build --features gpu-driver`, run with `--gpu`). Measured
2026-07-11 on a GeForce RTX 2060 (sm_75) against HotSpot JDK 25 (C2) and
[TornadoVM](https://github.com/beehive-lab/TornadoVM) 4.0.1 (PTX backend,
`@Parallel`/`@Reduce` + TaskGraph API). All timings are warm and include
the full per-call H2D + kernel + D2H round-trip; every row's checksum
matches HotSpot bit-for-bit.

| Benchmark (N = 2²⁴)                              | HotSpot C2 | TornadoVM GPU  | **CratonVM GPU** | vs HotSpot | vs TornadoVM |
|---------------------------------------------------|------------|----------------|-------------------|------------|--------------|
| Integer div-chain (48 unvectorizable divs/elem)    | 1,910 ms   | 28 ms          | **9 ms**          | **212x**   | **3.1x**     |
| Double div-chain (64 divs/elem, IEEE-exact)¹       | 1,508 ms   | 129 ms         | **91 ms**         | **16.6x**  | **1.4x**     |
| 96 multiply-adds/elem (AVX2-vectorized on CPU)     | 8 ms       | 17 ms          | **11 ms**         | 0.7x       | 1.5x         |
| Dot-product reduction (`int·int` → `long`)         | 7 ms       | unimplemented²  | **18 ms**         | 0.4x       | n/a²          |

¹ `FDIV_CHECKSUM` is **bit-exact** between CratonVM-GPU and HotSpot at
every size tested (`div.rn.f64` PTX is IEEE-754 round-to-nearest, same as
x86 `vdivpd`); TornadoVM's checksum diverges slightly from HotSpot's —
its PTX backend doesn't guarantee bit-exact division.

² TornadoVM 4.0.1's PTX backend throws `TornadoInternalError: unimplemented`
on the equivalent `@Reduce`-over-`LongArray` kernel; CratonVM's transparent
reduction dispatch handles a shape TornadoVM's own reduction skeleton
currently can't.

*The two div-chain rows are the "GPU wins big" cases: neither integer nor
double division has a competitive CPU-vectorized form the way multiply/add
does, so the GPU wins on raw parallelism at every size tested (confirmed
2²⁰ through 2²⁶ for the double kernel — see sources below; the ratio holds
steady, it isn't a one-off at this particular N). The other two rows are
the honest counter-cases kept in for the same reason the CPU table above
shows CratonVM losing to JDK, and neither has a scale-up fix: 96-MAD's
AVX2 auto-vectorizer stays competitive with the GPU at every N large enough
to escape millisecond-timer noise (an earlier "GPU wins at 2²²" reading was
that noise, not a real result — see sources), and dot-product's ratio holds
flat from 2²² through 2²⁶ because a single atomic-accumulator cell doesn't
get relatively cheaper with more elements; a real fix needs a proper
tree/shared-memory reduction, tracked as an open item. Checksums verified
identical between CratonVM-GPU and HotSpot on every row. Full results —
more input sizes, `ldc`-constant kernels, cold-start numbers up to N = 2²⁸,
sources, and methodology — are in "GPU offload benchmarks" further down
this file and in [docs/gpu/README.md](docs/gpu/README.md).*

## Quick Start

```bash
cratonvm --classpath path/to/classes MyProgram
```

### 1. Compile your Java source (requires `javac`)

```bash
javac HelloWorld.java
```

### 2. Run with CratonVM

```bash
cargo run --release -p cratonvm-cli -- --classpath . HelloWorld
```

## Command-Line Reference

```
cratonvm-cli [OPTIONS] <CLASS_NAME> [ARGS...]
```

| Option | Description |
|--------|-------------|
| `--classpath <PATH>` / `-c <PATH>` | Directories and JARs to search for `.class` files. Separator: `;` (Windows) or `:` (Unix). |
| `--Xmx <SIZE>` | Maximum heap size (`256m`, `1g`, `4096k`). Default: 256 MB. |
| `--verbose:class` | Print class loading trace to stderr. |
| `--verbose:gc` | Print GC activity to stderr. |
| `--Xbootclasspath <PATH>` | Override bootstrap classpath (advanced). |
| `--java-home <PATH>` | Point to a specific JDK for boot/ext classpath discovery. Forces real-JDK boot from that JDK's `java.base`. |
| `--synthetic-jdk` | Force synthetic JDK mode (built-in Rust stubs) even when a JDK is detected. Useful for hermetic runs or comparing backends. |
| `--nojit` | Disable JIT compilation (interpreter only). |
| `--noverify` | Skip bytecode verification. |

By default the launcher probes the host for a real JDK (`JAVA_HOME`, `CRATONVM_JAVA_HOME`,
or `java` on `PATH`). When a JDK with `jmods/java.base.jmod` or `lib/modules` is found, it
boots from real JDK bytecode; otherwise it falls back to the synthetic stubs. `--java-home`
forces real-JDK boot from the given JDK, while `--synthetic-jdk` forces synthetic mode and
wins over `--java-home`.

### Examples


............
.
.

```bash
# Single class in current directory
cargo run --release -p cratonvm-cli -- --classpath . HelloWorld

# Multiple classpath entries (Unix)
cargo run --release -p cratonvm-cli -- --classpath "src:lib/utils.jar" com.example.Main

# Pass arguments to the Java program
cargo run --release -p cratonvm-cli -- --classpath . MyApp arg1 arg2

# Increase heap for larger programs
cargo run --release -p cratonvm-cli -- --Xmx 1g --classpath . BigProgram
```

## What Works

### Core Language

- Primitive types: `boolean`, `byte`, `char`, `short`, `int`, `long`, `float`, `double`
- Arithmetic, bitwise, and comparison operators
- Control flow: `if/else`, `for`, `while`, `do-while`, `switch`
- Arrays (primitive and reference, including multi-dimensional)
- String concatenation (via `StringBuilder`)
- Exception handling: `try/catch/finally`, `throw`, checked and unchecked
- Classes, interfaces, abstract classes, enums
- Inheritance, method overriding, `super` calls
- `static` methods and fields
- Type casting (`checkcast`, `instanceof`)
- Lambda expressions and method references (`invokedynamic`)
- Records and sealed classes (Java 17)
- Pattern matching for `switch` (Java 21)

### Standard Library

| Package | Classes |
|---------|---------|
| `java.lang` | `Object`, `String`, `StringBuilder`, `System`, `Math`, `Integer`/`Long`/`Double`/`Float`/`Boolean`/`Byte`/`Short`/`Character`, `Enum`, `Throwable`, `Thread`, `Runtime`, `Class` |
| `java.io` | `PrintStream`, `PrintWriter`, `InputStream`/`OutputStream`, `ByteArrayInputStream`/`ByteArrayOutputStream`, `Scanner`, `FileChannel` |
| `java.util` | `ArrayList`, `LinkedList`, `HashMap`, `LinkedHashMap`, `TreeMap`, `HashSet`, `TreeSet`, `ArrayDeque`, `PriorityQueue`, `Vector`, `Stack`, `Arrays`, `Collections`, `Optional`, `StringJoiner`, `Random`, `UUID`, `Properties`, `Base64` |
| `java.util.stream` | `Stream`, `IntStream`, `LongStream`, `DoubleStream`, `Collectors` |
| `java.util.function` | `Function`, `Consumer`, `Predicate`, `Supplier`, `BiFunction`, `BiConsumer`, `Comparator`, etc. |
| `java.util.concurrent` | `ConcurrentHashMap`, `CopyOnWriteArrayList`, `ReentrantLock`, `CountDownLatch`, `Semaphore`, `CyclicBarrier` |
| `java.util.regex` | `Pattern`, `Matcher` |
| `java.nio` | `ByteBuffer` |
| `java.time` | `LocalDate`, `LocalTime`, `Instant`, `Duration` |
| `java.lang.ref` | `WeakReference`, `SoftReference`, `PhantomReference`, `ReferenceQueue` |

### Exceptions (catchable)

`NullPointerException`, `ArithmeticException`,
`ArrayIndexOutOfBoundsException`, `ClassCastException`,
`StackOverflowError`, `IllegalArgumentException`,
`NumberFormatException`, `UnsupportedOperationException`, and more.

## Limitations

- **AWT/Swing** — implemented natively (headless) via the `native-awt` crate;
  no on-screen rendering. **JavaFX** is out of tree (see [docs/internal/javafx-status.md](docs/internal/javafx-status.md)).
- **No `java.sql`/JDBC** — no database connectivity (design sketch in [docs/feature-designs/](docs/feature-designs/)).
- **Limited reflection** — `Class.forName` / `Method.invoke` work; some edge cases unsupported.
- **JNI** — Invocation API + a substantial function table (DefineClass-from-bytes, `Call*Method`/`Call*MethodV`/`Call*MethodA`, global/local refs, array-critical with GC pinning); bare C-varargs `(...)` forms and full foreign-thread attach are still partial.
- **Cryptography** — best-effort; not constant-time everywhere. See [docs/CRYPTO_STATUS.md](docs/CRYPTO_STATUS.md).
- **No JAR main-class auto-detection** — you must specify the class name.

## Security & sandboxing

CratonVM ships JDK-faithful (no extra restrictions) **by default**, plus opt-in
hardening for running less-trusted bytecode or multi-tenant hosting. None of
these are a substitute for OS-level isolation, and the VM is **not** a certified
sandbox — but they close the specific holes the 2026-06 review found:

| Knob | Effect |
|------|--------|
| `CRATONVM_CONFINE_IO` | Fail-closed file-I/O confinement to the CWD + registered roots. |
| `CRATONVM_BLOCK_PRIVATE_NETS` | Deny outbound to loopback/RFC-1918 (the link-local cloud-metadata block, incl. IPv4-mapped IPv6, always runs). |
| `CRATONVM_RESOLVE_OUTBOUND_HOST` | Resolve outbound hostnames and apply the per-IP egress policy (closes DNS-rebind/alias bypass). |
| `CRATONVM_HTTP_MAX_BODY` / `CRATONVM_ZIP_MAX_ENTRY_BYTES` | Request-body and decompression-bomb caps. |
| `CRATONVM_REQUIRE_POLICY` | With a `SecurityManager` and no policy, deny instead of allow-all. |

`SecureRandom` is backed by the OS CSPRNG; the built-in HTTP server rejects
request smuggling (`Transfer-Encoding: chunked` desync) and the HTTP client
strips credentials on cross-host redirects. Full reference and the threat model:
**[docs/SECURITY_HARDENING.md](docs/SECURITY_HARDENING.md)** and
[SECURITY.md](SECURITY.md).

## Building from Source

Requires **Rust 1.80+** and optionally **JDK 17+** (for compiling test Java classes).

```bash
git clone https://github.com/craton-co/cratonvm.git
cd cratonvm
cargo build --release -p cratonvm-cli
# Binary at: target/release/cratonvm[.exe]
```

The optional `java[.exe]` launcher alias is not built by default, so a normal
Cargo install does not shadow the system JDK. Build it only when a tool requires
the launcher basename to be `java`:

```bash
cargo build --release -p cratonvm-cli --features java-bin-alias
```

### Running Tests

```bash
# All tests
cargo test --all

# With increased stack for deep recursion tests
RUST_MIN_STACK=8388608 cargo test --all -- --test-threads=4
```

### Java regression suite

A fast, deterministic suite of Java classes that exercise the VM's critical
paths (JIT/GC, collections, strings, serialization, crypto, exceptions,
reflection) and **diff CratonVM's output against HotSpot**. It runs in
seconds and is the quick "is the VM still healthy?" check — complementary to
the heavier real-app gauntlet in `test-infra/`.

```bash
# Build target/release/cratonvm first, then:
bash regression-suite/run.sh        # → "REGRESSION SUITE: 8 passed, 0 failed"
```

It exits non-zero on any regression (CI-ready). See
[`regression-suite/README.md`](regression-suite/README.md) for what each class
covers, how to add one, and the list of known gaps it intentionally skips.

### Linting

```bash
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
```

## Architecture

The workspace has 20 member crates (the `fuzz` harness is a separate,
standalone workspace, not a member):

```
cratonvm/
  reader/              - .class file parser
  types/               - Shared types (Value, ClassId, ObjectRef)
  native-api/          - NativeContext trait & FD table
  native-builtins/     - java.lang.* native methods
  native-collections/  - java.util.* native methods
  native-io/           - java.io/nio native methods
  native-awt/          - AWT/Swing/Java2D native peers
  jit-api/             - JIT compiler API types
  jit/                 - x86-64 / AArch64 JIT compiler
  jit-cuda/            - Java bytecode -> PTX lowering for GPU offload
  cuda-bridge/         - Thin CUDA Driver API bridge for GPU offload
  craton-gpu/          - Build-time Java annotation sources for GPU offload (@Parallel etc.)
  classloading/        - Class loading & bytecode verification
  gc/                  - GC: generational young/old (moving + non-moving sweep, default), plus an opt-in region-based G1 (selectable via -XX:+UseG1GC, experimental) and a feature-gated ZGC stub; native-root registry + JNI pin set
  jfr/                 - Java Flight Recorder
  vm/                  - Virtual machine runtime
  vm-cli/              - Command-line entry point
  libcratonvm/         - C-ABI shared library for embedding (cdylib/staticlib libjvm substitute)
  cratonvm-embed/      - Semver-stable Rust facade for embedding CratonVM
  difftest/            - HotSpot differential-testing harness

  fuzz/                - libfuzzer harness (separate standalone workspace, nightly-only)
```

- **Bytecode interpreter** — fast-path dispatch with 140+ opcodes
- **x86-64 JIT compiler** — method compilation with LICM, BCE, AVX2 SIMD, OSR
- **Generational garbage collector** — young/old generation with write barriers and card table
- **Native method registry** — 3,100+ registrations covering the Java SE standard library
- **Synthetic class stubs** — JDK classes provided as native implementations for standalone (`--synthetic-jdk` / no-JDK) boot; the default path loads real `java.base` bytecode when a JDK is detected

See [ARCHITECTURE.md](ARCHITECTURE.md) for a detailed overview of the codebase structure.
See [BUILD_GUIDE.md](BUILD_GUIDE.md) for detailed build instructions, benchmarking, and project structure.
See [docs/INSTALL.md](docs/INSTALL.md) for binary installation and getting started.
See [docs/RELEASE_READINESS.md](docs/RELEASE_READINESS.md) for public release, crates.io dry-run, license/notice, and repository readiness checks.
See [docs/CONFIG.md](docs/CONFIG.md) for all configuration options and tuning parameters.
See [docs/SECURITY_HARDENING.md](docs/SECURITY_HARDENING.md) for the sandboxing / egress-policy / crypto-hardening reference.
See [docs/CONTAINER.md](docs/CONTAINER.md) for container/cgroup awareness and resource defaults.
See [docs/COVERAGE.md](docs/COVERAGE.md) for code-coverage tooling (`cargo-llvm-cov`).
See [docs/EMBEDDING.md](docs/EMBEDDING.md) for embedding CratonVM via the C-ABI (`libcratonvm`) or the Rust facade (`cratonvm-embed`).
See [docs/internal/embedding.md](docs/internal/embedding.md) for embedding `cratonvm-vm` as a library in a Rust application.
See [docs/internal/gc-tuning.md](docs/internal/gc-tuning.md) for choosing a GC backend, sizing the heap, and diagnosing pauses.
See [docs/PLATFORMS.md](docs/PLATFORMS.md) for the per-feature Linux / Windows / macOS support matrix.
See [ROADMAP.md](ROADMAP.md) for future plans and the performance roadmap.
See [docs/gpu/README.md](docs/gpu/README.md) for the full GPU-offload reference: build modes, CLI flags, architecture, file index, FAQ. The feature is opt-in via Cargo features — the default `cargo build` produces a CPU-only JVM with no GPU code linked.

## GPU offload benchmarks (RTX 2060, vs TornadoVM)

CratonVM can transparently offload eligible static methods over primitive
arrays to an NVIDIA GPU (`cargo build --features gpu-driver`, then run with
`--gpu` — no annotations, no API, no code changes). Measured 2026-07-11 on a
GeForce RTX 2060 (sm_75) against [TornadoVM](https://github.com/beehive-lab/TornadoVM)
4.0.1 (PTX backend, `@Parallel` + TaskGraph API) and HotSpot JDK 25 (C2).
All timings are warm, include the full per-call H2D + kernel + D2H round-trip,
and every row's checksum matches HotSpot bit-for-bit.

**Integer division chain** — 48 data-dependent `x = x / b[i] + c` steps per
element. x86 has no SIMD integer divide and the divisor is not a constant, so
no CPU JIT can vectorize this shape; the GPU wins on raw parallelism:

| N | CratonVM CPU | HotSpot C2 | **CratonVM GPU** | TornadoVM GPU | GPU vs best CPU |
|---|---|---|---|---|---|
| 2²² | 569 ms | 470 ms | **2 ms** | 7 ms | **235×** |
| 2²⁴ | 2,232 ms | 1,910 ms | **9 ms** | 28 ms | **212×** |
| 2²⁶ | 9,162 ms | 6,735 ms | **33 ms** | 86 ms | **204×** |

**Double-precision division chain** — 64 sequential `x = x / b[i] + c` steps
per element (`GpuFloatDivChain`). The floating-point counterpart of the
integer chain above: `vdivpd` has real but low throughput even under AVX2
(a shared, weakly-pipelined execution unit, unlike multiply/add/FMA), so the
GPU wins here too, and its `div.rn.f64` is IEEE-754 exact — `FDIV_CHECKSUM`
matches HotSpot bit-for-bit at every size (including CratonVM-CPU, which
also matches — the chain is deterministic across all three engines), unlike
TornadoVM's PTX backend (checksum diverges slightly there, e.g.
`2.3711976971862224E8` vs `2.3833420668027386E8` at 2²⁶ — approximate
device math). CratonVM-CPU at 2²⁶ (64M elements × 64 divisions) needed
~99× longer than CratonVM-GPU to confirm this — a good illustration of why
the GPU path exists:

| N | CratonVM CPU | HotSpot C2 | **CratonVM GPU** | TornadoVM GPU | GPU vs HotSpot |
|---|---|---|---|---|---|
| 2²⁰ | 435 ms | 89 ms | **7 ms** | 11 ms | **12.7×** |
| 2²² | 1,856 ms | 400 ms | **23 ms** | 38 ms | **17.4×** |
| 2²⁴ | 6,299 ms | 1,508 ms | **91 ms** | 129 ms | **16.6×** |
| 2²⁶ | 36,082 ms | 5,578 ms | **365 ms** | 482 ms | **15.3×** |

**96 multiply-adds per element** (`GpuWarm.heavy`) — a shape HotSpot C2 *can*
auto-vectorize with AVX2, making it the honest hard case: the GPU still beats
or ties the vectorized CPU and outruns TornadoVM ~2× on the same kernel:

| N | CratonVM CPU | HotSpot C2 | **CratonVM GPU** | TornadoVM GPU | GPU vs CratonVM CPU |
|---|---|---|---|---|---|
| 2²² | 472 ms | 2 ms | **1 ms** | 5 ms | 472× |
| 2²⁴ | 1,955 ms | 8 ms | **11 ms** | 17 ms | 178× |
| 2²⁶ | 7,689 ms | 25 ms | **27 ms** | 51 ms | 285× |

*Does the 96-MAD ratio ever flip in CratonVM-GPU's favor at a smaller N?
Checked directly: at N ≤ 2²² both HotSpot and CratonVM-GPU round to 0-1 ms,
which is `System.nanoTime()`/millisecond-timer noise, not a reproducible
signal (an earlier "2× GPU win at 2²²" reading, visible in the table above,
was exactly this noise — re-measured and it doesn't hold up). At N ≥ 2²⁴ the
ratio settles to ~0.7-0.9× and stays there; scaling further doesn't change
it because both sides scale ~linearly once past the noise floor. The
double-precision division chain above is the honest floating-point
alternative that *does* show a clear, reproducible win at every size.*

Sources: [bench-gpu/results/divchain-comparison-20260711.md](bench-gpu/results/divchain-comparison-20260711.md),
[warm-comparison-20260711.md](bench-gpu/results/warm-comparison-20260711.md),
[float-divchain-20260711.md](bench-gpu/results/float-divchain-20260711.md),
and the cold-start 4-way in [gpu-comparison-20260711.md](bench-gpu/results/gpu-comparison-20260711.md)
(includes N = 2²⁸ / 269M elements: 579 ms on GPU vs 30.8 s CratonVM CPU).
Benchmark sources live in `bench-gpu/` (+ `bench-tornado/` for the TornadoVM twins). Numbers were taken on a
machine with background load; treat CPU baselines as ±25%. Open GPU work is
recorded in [docs/internal/gpu-offload-followups-20260711.md](docs/internal/gpu-offload-followups-20260711.md).

### Update 2026-07-11 (evening)

A second wave of hardware-validated work landed the same day: transparent
offload for **integer/long reduction kernels** (`)I`/`)J`-returning methods —
`sum += a[i] * b[i]` shapes), offload for **`ldc`-sourced constants** (int
literals outside `sipush` range, and any float/double/long literal), a JIT-caller
admission gate that keeps offload-eligible callers interpreted so OSR can no
longer silently degrade offload back to CPU, and a curated `Math`/`StrictMath`
intrinsics table (`sqrt`/`abs`/`min`/`max`/`fma`) under `ALLOW_INTRINSIC_CALLS`.
Full detail in [docs/internal/gpu-offload-followups-20260711.md](docs/internal/gpu-offload-followups-20260711.md)
and [docs/gpu/annotations.md](docs/gpu/annotations.md).

**Reduction dispatch** (`bench-gpu/GpuDotBench.java`, `sum += (long) a[i] * b[i]`
over `int[]`, N = 2²⁴, `DOT_CHECKSUM` bit-exact against HotSpot and an
independent CPU oracle):

| Kernel | CratonVM-CPU | **CratonVM-GPU** | HotSpot C2 |
|---|---|---|---|
| dot-product reduction | 76 ms | **18 ms** | 7 ms |

Honest framing: this kernel is PCIe-bound (small per-element payload, one
scalar out) plus single-cell atomic contention on the accumulator, so the GPU
beats CratonVM's own CPU 4.2× but does **not** beat vectorized HotSpot C2 at
this size — the point isn't winning this particular race, it's completing the
transparent-offload surface for a shape that TornadoVM 4.0.1's PTX backend
currently can't handle at all: the equivalent `@Reduce`-over-`LongArray` kernel
(`bench-tornado/TornadoDotBench.java`) throws
`TornadoInternalError: unimplemented`.

Does scaling N help? No: the ratio holds roughly flat (~3× slower on GPU)
from N=2²² through N=2²⁶ (12 ms → 41 ms GPU vs 4 ms → 19 ms HotSpot at
2²⁴/2²⁶). The kernel is memory-bandwidth-bound on both sides, and every GPU
thread races an atomic add on the *same* accumulator cell — that
contention doesn't improve with more elements. A real win here needs a
proper tree/shared-memory reduction instead of a single atomic cell
(tracked as an open item, not yet built).

**`ldc` constants** (`bench-gpu/GpuLdcBench.java`, a 96-step multiply-add chain
using constants outside `sipush` range so javac emits `ldc` instead of
`sipush`, N = 2²⁴, `SAMPLE` bit-exact against HotSpot):

| Kernel | Before (CPU-bound, `ldc` rejected) | **CratonVM-GPU, warm** |
|---|---|---|
| `ldc`-constant multiply-add chain | ~2,000 ms | **8 ms** |

## Contributing

**Design constraint:** prefer real `.class` files from the JDK and application classpath over synthetic stub classes for application-visible types; see [docs/internal/app-jvm-bugs/jvm-no-synthetic-stubs.md](docs/internal/app-jvm-bugs/jvm-no-synthetic-stubs.md).

See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines on how to contribute.

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE) for details.
Copyright 2024-2026 Craton Software Company. Project ownership and notice
material live in [NOTICE](NOTICE); the root license file intentionally remains
the unmodified Apache License 2.0 text.

See [TRADEMARKS.md](TRADEMARKS.md) for trademark attributions and notices.
