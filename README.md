# CratonVM

[![CI](https://github.com/craton-co/cratonvm/actions/workflows/ci.yml/badge.svg)](https://github.com/craton-co/cratonvm/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.80%2B-orange.svg)](https://www.rust-lang.org/)

A Java Virtual Machine written entirely in Rust, with a custom x86-64 JIT
compiler and transparent GPU offload.

CratonVM boots against a real JDK when one is present (`JAVA_HOME`,
`CRATONVM_JAVA_HOME`, or `java` on `PATH`) and runs fully standalone when it
isn't — its own Rust implementations of the Java standard library mean no JDK
install, no `rt.jar`, one self-contained binary.

## Highlights

- **Custom x86-64 JIT** — tiered compilation (interpreter → C1 → C2), OSR,
  LICM, bounds-check elimination, AVX2 SIMD, precise stack maps. Experimental
  AArch64 backend.
- **Transparent GPU offload** — eligible Java methods run on NVIDIA GPUs with
  **no annotations and no API changes**, and beat TornadoVM on
  division-dominated kernels (see below).
- **Generational GC** — young/old generations, card table, selective
  promotion; opt-in region-based G1 (`-XX:+UseG1GC`).
- **Real frameworks run** — Spring, Spring Boot, Tomcat, Hibernate, and H2
  boot and pass large test suites.
- **Memory-safe by construction** — the interpreter, GC, and JIT are Rust;
  classic VM vulnerability classes are designed out at the language level.
- **JNI & embedding** — JNI Invocation API, a stable C-ABI library
  (`libcratonvm`), and a Rust facade (`cratonvm-embed`).
- **Observability & hardening** — Java Flight Recorder, bytecode
  verification, opt-in I/O confinement and network egress policy
  ([docs/SECURITY_HARDENING.md](docs/SECURITY_HARDENING.md)).

## Performance

CPU, vs HotSpot JDK 25 C2 (same flags both sides, medians of alternating
fresh-process runs — full methodology in [BENCHMARK.md](BENCHMARK.md)):

| Benchmark                         | JDK 25 C2 | CratonVM  | Ratio | Measured |
|-----------------------------------|-----------|-----------|-------|----------|
| Arithmetic (2B ops)               | 2,006 ms  | 4,895 ms  | 2.44x | 07-18 |
| Fibonacci(44)                     | 1,719 ms  | 4,790 ms  | 2.79x | 07-18 |
| Sieve (100K × 20,000)             | 2,851 ms  | 6,508 ms  | 2.28x | 07-18 |
| Matrix 1280×1280                  | 2,349 ms  | 6,875 ms  | 2.93x | 07-18 |
| HashMap (10M put/get)             | 1,039 ms  | 22,077 ms | 21.2x | **07-25** |
| String/Regex (100K)               | 55 ms     | 423 ms    | 7.7x  | **07-25** |
| Binary Trees (depth 18)           | 176 ms    | 1,468 ms  | 8.34x | 07-18 |


GPU offload, vs HotSpot C2 and [TornadoVM](https://github.com/beehive-lab/TornadoVM)
4.0.1 (RTX 2060, N = 2²⁴, warm, full H2D+kernel+D2H round-trip, checksums
bit-identical to HotSpot):

| Kernel                                   | HotSpot C2 | TornadoVM GPU | CratonVM GPU | vs HotSpot | vs TornadoVM |
|------------------------------------------|------------|---------------|--------------|------------|--------------|
| Integer div-chain (48 divs/elem)         | 1,910 ms   | 28 ms         | **9 ms**     | **212x**   | **3.1x**     |
| Double div-chain (64 divs/elem)          | 1,508 ms   | 129 ms        | **91 ms**    | **16.6x**  | **1.4x**     |
| 96 multiply-adds/elem (AVX2 on CPU)      | 8 ms       | 17 ms         | 11 ms        | 0.7x       | 1.5x         |
| Dot-product reduction (int·int → long)   | 7 ms       | unimplemented | 18 ms        | 0.4x       | n/a          |

Unlike TornadoVM, CratonVM needs no `@Parallel` annotations or TaskGraph
API — plain Java methods offload transparently — and its GPU division is
IEEE-754 bit-exact with HotSpot. Full results, extra sizes, and the honest
counter-cases: [BENCHMARK.md](BENCHMARK.md) and
[docs/gpu/README.md](docs/gpu/README.md).

## What Runs Today

Boots, runs, and passes large real-world test suites:

- **Spring / Spring Boot**
- **Tomcat** (servlets, NIO, WebSocket, HTTP/2)
- **Hibernate**
- **H2** (embedded SQL database)

**Coming soon — the reactive stack:** Netty, Quarkus, Hibernate Reactive,
and real database support (JDBC drivers over live network connections).

## Java Version Support

| Version | Key features |
|---------|--------------|
| Java 8  | Full language support, lambdas, streams |
| Java 11 | Nest-based access control |
| Java 17 | Records, sealed classes |
| Java 21 | Pattern matching for switch, virtual threads, sequenced collections |
| Java 25 | Stream gatherers, scoped values, structured concurrency |

## Quick Start

Build (Rust 1.80+):

```bash
cargo build --release -p cratonvm-cli
```

Compile and run a class:

```bash
javac HelloWorld.java
./target/release/cratonvm -cp . HelloWorld
```

Run a JAR:

```bash
./target/release/cratonvm --jar app.jar
```

Bigger heap, G1 collector, program arguments:

```bash
./target/release/cratonvm -Xmx4g -XX:+UseG1GC -cp "classes:lib/util.jar" com.example.Main arg1 arg2
```

## Command-Line Reference

```
cratonvm [OPTIONS] <CLASS_NAME> [ARGS]...
cratonvm [OPTIONS] --jar <FILE.jar> [ARGS]...
```

| Option | Description |
|--------|-------------|
| `-c`, `-cp`, `--classpath <PATH>` | Directories and JARs to search. Separator: `:` (Unix) / `;` (Windows). |
| `--jar <FILE>` | Execute a JAR (main class from `META-INF/MANIFEST.MF`; classpath from the JAR and its manifest `Class-Path`). |
| `-Xmx<SIZE>`, `--Xmx <SIZE>` | Maximum heap size (`256m`, `1g`, `8g`). Default: 256 MB. |
| `-XX:+UseG1GC` | Use the region-based G1 collector (experimental; generational is the default). |
| `-D<name>=<value>` | Set a Java system property. |
| `--verbose:class` | Class-loading trace to stderr. |
| `--verbose:gc` | GC activity to stderr. |
| `--nojit` | Interpreter only (diagnosis / safety fallback). |
| `--noverify`, `--Xverify <MODE>` | Bytecode verification policy (`none` / `remote` / `all`). |
| `--java-home <PATH>` | Boot from a specific JDK's `java.base`. |
| `--synthetic-jdk` | Force the built-in Rust standard library even when a JDK is detected. |
| `--Xbootclasspath <PATH>` | Override the bootstrap classpath (advanced). |
| `-V`, `--version` / `--help` | Version / full option list. |

Every command above is verified against the current binary. `--gpu` and the
other GPU options require a `--features gpu-driver` build — see
[docs/gpu/README.md](docs/gpu/README.md).

## Limitations

- **AWT/Swing** are headless (no on-screen rendering); JavaFX is out of tree.
- **JDBC / `java.sql`** is not wired yet — real database support is on the
  roadmap above.
- **Reflection and JNI** cover the common paths; some edge cases
  (C-varargs JNI forms, full foreign-thread attach) are partial.
- **Cryptography** is best-effort and not constant-time everywhere — see
  [docs/CRYPTO_STATUS.md](docs/CRYPTO_STATUS.md).

## Documentation

- [ARCHITECTURE.md](ARCHITECTURE.md) — crate layout and subsystem design
- [BENCHMARK.md](BENCHMARK.md) — benchmark methodology and full results
- [docs/GC.md](docs/GC.md) — collectors, barriers, heap layout
- [docs/JIT_OPTIMIZATION.md](docs/JIT_OPTIMIZATION.md) — the JIT optimization journey
- [docs/gpu/README.md](docs/gpu/README.md) — GPU offload reference
- [docs/EMBEDDING.md](docs/EMBEDDING.md) — embedding CratonVM in your application
- [docs/SECURITY_HARDENING.md](docs/SECURITY_HARDENING.md) — sandboxing and hardening
- [docs/CONFIG.md](docs/CONFIG.md) — configuration reference

## Building & Testing

```bash
cargo build --release          # full workspace
cargo test --workspace         # unit + integration tests
bash regression-suite/run.sh   # fast HotSpot-differential regression suite
```

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Architectural orientation lives in
[ARCHITECTURE.md](ARCHITECTURE.md); the fast regression suite and the
performance gate (`regression-suite/`) are the merge gates.

## License

Apache-2.0 — see [LICENSE](LICENSE). CratonVM is an independent
implementation and is not derived from OpenJDK sources; see
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md) and
[TRADEMARKS.md](TRADEMARKS.md).
