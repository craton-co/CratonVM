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

| Benchmark                  | JDK 25 C2     | CratonVM default | Default ratio |
|----------------------------|---------------|-------------------|---------------|
| Arithmetic (2B ops)        | 2,384 ms      | 10,845 ms         | 4.55x         |
| Fibonacci(44)               | 3,229 ms      | 7,785 ms          | 2.41x         |
| Sieve (100K x 20,000)      | 5,417 ms      | 19,714 ms         | 3.64x         |
| Matrix 1280x1280           | 2,234 ms      | 14,928 ms         | 6.68x         |
| **QuickBench TOTAL**       | **13,664 ms** | **53,272 ms**     | **3.90x**     |
| Binary Trees (depth=18)    | 400 ms        | 12,003 ms         | 30.0x         |

*Measured 2026-07-10 on the primary Windows dev box (hybrid P/E-core CPU, pinned to
the 16 P-core logical processors via `ProcessorAffinity` — single-threaded benchmarks
otherwise get scheduled onto slower E-cores, which skews results) against JDK 25 C2
and a CratonVM release build off `dev` at `13011cffd` **plus** a same-day fix restoring
the guarded-inline-getfield JIT fast path to its intended default-on state (see below).
All rows measured in a single combined run (`bench/QuickBenchLong2.java`, a further
rescale of the QuickBench suite: Arithmetic 1.8B → 2B, Fibonacci fib(41)×5 → fib(44)
single call, Sieve 16,700 → 20,000 reps, Matrix 1230×1230 → 1280×1280); Binary Trees is
the same kernel run as the suite's 5th test, not a separate process, so its ratio here
(30.0x) is not directly comparable to a fresh-process `bintrees18` run (which measured
~13x on this same build — see the JIT optimization doc) — running it back-to-back after
four other allocation-heavy kernels leaves more GC/heap pressure resident. Checksums
verified identical between CratonVM and JDK on every kernel. `CRATONVM_JIT_OSR`
back-edge OSR is default-on; `guarded_inline_getfield_enabled()` (default-on again as of
this fix) is the single largest lever in this table — a same-day intermediate commit
had flipped it back to an opt-in checked-helper path after finding a real SIGSEGV on an
Elasticsearch vector-query workload, which cost ~35-40% throughput on getfield-heavy
kernels; it was restored to default-on as a deliberate trade-off (the crash is a narrow,
specific repro, not a general hazard) while the root cause is investigated separately —
see [docs/JIT_OPTIMIZATION.md](docs/JIT_OPTIMIZATION.md) for the full history and the
known workaround (`CRATONVM_JIT_GETFIELD_HELPER=1`) if you hit that crash signature.*

See [docs/JIT_OPTIMIZATION.md](docs/JIT_OPTIMIZATION.md) for the full 26-round JIT optimization journey.

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

## Contributing

**Design constraint:** prefer real `.class` files from the JDK and application classpath over synthetic stub classes for application-visible types; see [docs/internal/app-jvm-bugs/jvm-no-synthetic-stubs.md](docs/internal/app-jvm-bugs/jvm-no-synthetic-stubs.md).

See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines on how to contribute.

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE) for details.
Copyright 2024-2026 Craton Software Company. Project ownership and notice
material live in [NOTICE](NOTICE); the root license file intentionally remains
the unmodified Apache License 2.0 text.

See [TRADEMARKS.md](TRADEMARKS.md) for trademark attributions and notices.
