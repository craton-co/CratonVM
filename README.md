# CratonVM

[![CI](https://github.com/craton-co/cratonvm/actions/workflows/ci.yml/badge.svg)](https://github.com/craton-co/cratonvm/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.77%2B-orange.svg)](https://www.rust-lang.org/)

A Java Virtual Machine written entirely in Rust with a custom x86-64 JIT compiler.

**No JDK installation, no `JAVA_HOME`, no `rt.jar` needed** — CratonVM provides its own
synthetic implementations of the Java standard library classes.

## Features

- **Bytecode interpreter** with 140+ fast-path opcodes
- **x86-64 JIT compiler** (~7,200 lines, ~140 bytecodes, 26 optimization rounds)
- **Generational garbage collector** with write barriers
- **Multi-threading** with monitors, locks, and barriers
- **Lambda/invokedynamic** support via LambdaMetafactory
- **3,100+ native method registrations** (java.lang, java.util, java.io, java.time, ...)
- **6,000+ tests** passing, **0** clippy warnings
- **~323,000+ lines** of Rust

### Java Version Support

| Version | Key Features |
|---------|-------------|
| Java 8 | Full language support, lambdas, streams |
| Java 11 | Nest-based access control (JEP 181) |
| Java 17 | Records (JEP 395), sealed classes (JEP 409) |
| Java 21 | Pattern matching for switch, virtual threads, sequenced collections |
| Java 25 | Stream gatherers, scoped values, structured concurrency, class file version 69 |

### Benchmark (vs HotSpot JDK 25 C2)

| Benchmark | JDK 25 C2 | CratonVM | Ratio |
|-----------|-----------|---------|-------|
| Arithmetic 300M | 889 ms | 1,676 ms | 1.89x |
| Fibonacci(42) | 1,876 ms | 2,457 ms | 1.31x |
| Sieve 100K×500 | 324 ms | 510 ms | 1.57x |
| Matrix 500×500 | 351 ms | 518 ms | 1.48x |
| **QuickBench TOTAL** | **3,440 ms** | **5,161 ms** | **1.50x** |
| Binary Trees (depth=18) | 714 ms | 16,657 ms | 23.3x |

*Measured 2026-03-31 on Windows 11, JDK 25.0.1 LTS. Round 26 JIT: OSR, loop unrolling, speculative BCE, graph-coloring regalloc.*

See [docs/PRESENTATION.md](docs/PRESENTATION.md) for the full 26-round JIT optimization journey.

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
| `--java-home <PATH>` | Point to a JDK for boot/ext classpath discovery (advanced). |
| `--nojit` | Disable JIT compilation (interpreter only). |
| `--noverify` | Skip bytecode verification. |

### Examples

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
  no on-screen rendering. **JavaFX** is out of tree (see [docs/javafx-status.md](docs/javafx-status.md)).
- **No `java.sql`/JDBC** — no database connectivity.
- **Limited reflection** — `Class.forName` / `Method.invoke` work; some edge cases unsupported.
- **No custom classloaders** — only the three built-in loaders.
- **Partial JNI** — function table structure exists; limited function implementations.
- **No JAR main-class auto-detection** — you must specify the class name.

## Building from Source

Requires **Rust 1.77+** and optionally **JDK 17+** (for compiling test Java classes).

```bash
git clone https://github.com/craton-co/cratonvm.git
cd cratonvm
cargo build --release -p cratonvm-cli
# Binary at: target/release/cratonvm[.exe]
```

### Running Tests

```bash
# All tests
cargo test --all

# With increased stack for deep recursion tests
RUST_MIN_STACK=8388608 cargo test --all -- --test-threads=4
```

### Linting

```bash
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
```

## Architecture

The workspace has 17 member crates plus a `fuzz` harness (18 Cargo.toml files in total):

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
  craton-gpu/          - GPU offload runtime integration
  classloading/        - Class loading & bytecode verification
  gc/                  - Garbage collectors (semi-space, G1, ZGC)
  jfr/                 - Java Flight Recorder
  vm/                  - Virtual machine runtime
  vm-cli/              - Command-line entry point
  fuzz/                - libfuzzer harness (workspace member, nightly-only)
```

- **Bytecode interpreter** — fast-path dispatch with 140+ opcodes
- **x86-64 JIT compiler** — method compilation with LICM, BCE, AVX2 SIMD, OSR
- **Generational garbage collector** — young/old generation with write barriers and card table
- **Native method registry** — 3,100+ registrations covering the Java SE standard library
- **Synthetic class stubs** — JDK classes provided as native implementations; no `rt.jar` needed

See [ARCHITECTURE.md](ARCHITECTURE.md) for a detailed overview of the codebase structure.
See [BUILD_GUIDE.md](BUILD_GUIDE.md) for detailed build instructions, benchmarking, and project structure.
See [docs/INSTALL.md](docs/INSTALL.md) for binary installation and getting started.
See [docs/CONFIG.md](docs/CONFIG.md) for all configuration options and tuning parameters.
See [docs/embedding.md](docs/embedding.md) for embedding `cratonvm-vm` as a library in a Rust application.
See [docs/gc-tuning.md](docs/gc-tuning.md) for choosing a GC backend, sizing the heap, and diagnosing pauses.
See [docs/PLATFORMS.md](docs/PLATFORMS.md) for the per-feature Linux / Windows / macOS support matrix.
See [ROADMAP.md](ROADMAP.md) for future plans and the performance roadmap.
See [docs/gpu/README.md](docs/gpu/README.md) for the full GPU-offload reference: build modes, CLI flags, architecture, file index, FAQ. The feature is opt-in via Cargo features — the default `cargo build` produces a CPU-only JVM with no GPU code linked.

## Contributing

**Design constraint:** prefer real `.class` files from the JDK and application classpath over synthetic stub classes for application-visible types; see [docs/jvm-no-synthetic-stubs.md](docs/jvm-no-synthetic-stubs.md).

See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines on how to contribute.

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE) for details.

See [TRADEMARKS.md](TRADEMARKS.md) for trademark attributions and notices.
