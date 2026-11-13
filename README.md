# CratonVM

[![CI](https://github.com/craton-co/cratonvm/actions/workflows/ci.yml/badge.svg)](https://github.com/craton-co/cratonvm/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.88%2B-orange.svg)](https://www.rust-lang.org/)

A Java Virtual Machine written in Rust, with a custom x86-64 JIT and an
opt-in automatic GPU fast path for a documented subset of Java kernels.

CratonVM is built from the ground up to execute authentic OpenJDK class libraries
directly, treating real JDK bytecode as authoritative. It boots directly against a host JDK
(`JAVA_HOME`, `CRATONVM_JAVA_HOME`, or `java` on `PATH`), and features an engineered
compatible optimization layer (`--compatible`) with native standard library
implementations for ultra-fast startup and standalone execution.

## Highlights

- **Custom x86-64 JIT** — tiered compilation (interpreter → C1 → C2), OSR,
  LICM, bounds-check elimination, AVX2 SIMD, precise stack maps. Experimental
  AArch64 backend.
- **Automatic GPU fast path** — supported pure static array kernels can run on
  NVIDIA GPUs (CUDA), AMD GPUs (HIP/ROCm), or OpenCL devices without API changes. Eligibility is intentionally narrow and
  unsupported shapes fall back to CPU (see below).
- **ZGC by default.** The default collector is a memory-backed ZGC (`-XX:+UseZGC`).
  It features thread-local allocation buffers (TLABs) that the JIT allocates from
  inline, parallel marking by default (`CRATONVM_ZGC_PARMARK`, `=0` disables), and
  parallel sliding compaction (kill switch `CRATONVM_ZGC_RELOCATE=0`).
  - Default execution runs stop-the-world (STW parallel mark + STW parallel compaction).
  - Concurrent marking (`CRATONVM_ZGC_CONCURRENT=1`, `CRATONVM_ZGC_CONC_START=<percent>|auto`)
    and generational mode (`CRATONVM_ZGC_GENERATIONAL=1`) are opt-in. Concurrent marking uses an
    SATB pre-write barrier rather than a load barrier.
  - The colored-pointer load barrier is built but not yet armed in production.
  - `-XX:+UseGenerationalGC` selects the young/old collector with a Cheney
    moving young generation; a cycle that cannot prove complete root coverage
    falls back to a non-moving sweep.
  - Region-based G1 is experimental and opt-in (`-XX:+UseG1GC`).
  - See [docs/GC.md](docs/GC.md) and [ARCHITECTURE.md](ARCHITECTURE.md#memory-gc-crate).
- **Real frameworks run** — Spring, Spring Boot, Tomcat, Netty, Hibernate,
  Hibernate Reactive, H2 Database, Postgres driver boot and pass large test suites.
- **Rust implementation** — Rust removes many ambient memory hazards, but the
  VM, JIT, GC, FFI, I/O, AWT, CUDA, and JFR contain reviewed and still-being-
  audited `unsafe` regions. See [SECURITY.md](SECURITY.md).
- **JNI & embedding** — JNI Invocation API, a stable C-ABI library
  (`libcratonvm`), and a Rust facade (`cratonvm-embed`).
- **Observability & hardening** — Java Flight Recorder, bytecode
  verification, opt-in I/O confinement and network egress policy
  ([docs/SECURITY_HARDENING.md](docs/SECURITY_HARDENING.md)).

## Performance

CPU, vs HotSpot JDK 25 C2 — same flags both sides, checksum-verified against
HotSpot on every run (zero mismatches):

| Benchmark               | JDK 25 C2 | CratonVM  | Ratio     | Growth        |
|------------------------|-----------|-----------|-----------|---------------|
| Arithmetic (2B ops)    | 2,208 ms  | 2,506 ms  | 1.13x     | linear        |
| Fibonacci(44)          | 1,741 ms  | 540 ms    | **0.31x** | linear        |
| Sieve (100K × 20K)     | 2,758 ms  | 2,492 ms  | **0.90x** | linear        |
| Matrix 1280×1280       | 2,508 ms  | 2,451 ms  | **0.98x** | linear        |
| Binary Trees (depth 20)| 1,561 ms  | 2,709 ms  | 1.74x     | non-monotonic |
| HashMap (50M put/get)  | 3,523 ms  | 3,491 ms  | **0.98x** | non-monotonic |
| String/Regex (10M)     |   689 ms  | 1,042 ms  |  1.51x    | linear        |

Full per-size data, checksums, CV, and the ZGC-vs-G1 delta are in [BENCHMARK.md](BENCHMARK.md).

> [!NOTE]
> Real-world enterprise applications show an observed **3x–10x** performance gap compared to HotSpot, reflecting full-stack framework overhead (dynamic proxy dispatch, class hierarchy scanning, reflection, and multi-threaded storage engines). Detailed benchmark evidence and analysis are in [PERFORMANCE.MD](PERFORMANCE.MD).

GPU offload, vs HotSpot C2 and [TornadoVM](https://github.com/beehive-lab/TornadoVM)
5.2 (PTX backend, RTX 2060, N = 2²⁴, warm, full H2D+kernel+D2H round-trip, checksums
bit-identical to HotSpot):

| Kernel                                             | HotSpot C2 | TornadoVM GPU | CratonVM GPU | vs HotSpot | vs TornadoVM |
|----------------------------------------------------|------------|---------------|--------------|------------|--------------|
| Integer div-chain (48 divs/elem)                   | 2,179 ms   | 19 ms         | **10 ms**    | **218x**   | **1.9x**     |
| Double div-chain (64 divs/elem)                    | 1,784 ms   | 117 ms        | **96 ms**    | **18.6x**  | **1.2x**     |
| 128 multiply-adds/elem (data-dependent multiplier) | 1,298 ms   | 17 ms         | **9 ms**     | **144x**   | **1.9x**     |
| Dot-product reduction (int·int → long, x300/elem)  | 1,168 ms   | unimplemented | **2 ms**     | **584x**   | n/a          |
| Dot-product reduction (long·long → long, x300/elem)| 1,241 ms   | 25 ms         | **2 ms**     | **620x**   | **12.5x**    |
| Integer div-chain (192 divs/elem)                  | 14,246 ms  | 28 ms         | **15 ms**    | **950x**   | **1.9x**     |
| Ray tracer kernel, 7680×4320 (33.2M px)            | 837.1 ms   | 27.89 ms      | **18.47 ms** | **45x**    | **1.5x**     |
| GPULlama3 Llama-3.2-1B F16 decode (tok/s)          | 11.9 tok/s | 20.2 tok/s    |**38.2 tok/s**| **3.2x**   | **1.9x**     |


Unlike TornadoVM, the supported automatic path needs no `@Parallel`
annotations or TaskGraph API — within a deliberately narrow eligibility
subset, not a general promise that arbitrary Java runs on the GPU. Full
results, extra sizes, and methodology:
[BENCHMARK.md](BENCHMARK.md) and [docs/gpu/README.md](docs/gpu/README.md).

## What to build on

Boots, runs, and passes large real-world test suites:

- **Spring 7.1 / Spring Boot 4.2 / Quarkus 4.0**
- **Tomcat 12.0 / Netty 4.2**
- **Hibernate ORM 7.4 / Hibernate Reactive 5.0**
- **H2 DB 2.4 / PostgreSQL driver 42.7**
- **Apache Commons Math 4.0**
- **Bouncy Castle Java 1.86**


## Java Version Support

| Version | Key features |
|---------|--------------|
| Java 8  | Full language support, lambdas, streams |
| Java 11 | Nest-based access control |
| Java 17 | Records, sealed classes |
| Java 21 | Pattern matching for switch, virtual threads, sequenced collections |
| Java 25 | Stream gatherers, scoped values, structured concurrency |

## Quick Start

Build (Rust 1.88+):

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
cratonvm [OPTIONS] -jar <FILE.jar> [ARGS]...
cratonvm [OPTIONS] -m <MODULE>[/<CLASS_NAME>] [ARGS]...
```

| Option | Description |
|--------|-------------|
| `-c`, `-cp`, `-classpath`, `--class-path`, `--classpath <PATH>` | Directories and JARs to search. Separator: `:` (Unix) / `;` (Windows). |
| `-jar <FILE>`, `--jar <FILE>` | Execute a JAR (main class from `META-INF/MANIFEST.MF`; classpath from the JAR and its manifest `Class-Path`). |
| `-m`, `--module <MODULE>[/<CLASS>]` | Resolve and execute the main class of a modular application. |
| `-p`, `--module-path <PATH>` | Module search path for modular JARs or exploded modules. |
| `-Xms<SIZE>`, `--Xms <SIZE>` | Initial heap commit size (e.g. `16m`, `512m`). Supported across all collectors; reserves `-Xmx` and commits initial heap. |
| `-Xmx<SIZE>`, `--Xmx <SIZE>` | Maximum heap size (`256m`, `1g`, `8g`). Default: 1/4 of physical RAM, or of the container memory limit, clamped to 256 MB..4 GB. The cap is `CRATONVM_DEFAULT_HEAP_MAX_MB`; `CRATONVM_DEFAULT_HEAP_ERGONOMICS=0` restores a fixed 256 MB. |
| `-Xss<SIZE>`, `--Xss <SIZE>` | Default thread stack size (`2m`, `64m`). Expands carrier stack if larger than 8 MB default. |
| `-XX:+UseZGC` | Use ZGC (memory-backed, STW/concurrent marking, compacting; **default collector**). |
| `-XX:+UseG1GC` | Use the region-based G1 collector (experimental; ZGC is the default). |
| `-XX:+UseGenerationalGC` | Use the generational (young/old, Cheney young gen) collector instead of ZGC. |
| `-D<name>=<value>` | Set a Java system property. |
| `-verbose:class`, `--verbose:class` | Class-loading trace to stderr. |
| `-verbose:gc`, `--verbose:gc` | GC activity to stderr. |
| `--nojit`, `-Xint` | Interpreter only (diagnosis / safety fallback). |
| `--noverify`, `-noverify`, `--Xverify <MODE>` | Bytecode verification policy (`none` / `remote` / `all`). |
| `--java-home <PATH>` | Boot from a specific JDK's `java.base`. |
| `--synthetic-jdk` | Force the built-in Rust standard library (~5,200 stubs) even when a JDK is detected. Implies `--compatible`. Requires `synthetic-jdk` feature. |
| `--real-jdk` | Strict mode: authentic OpenJDK class bytes are authoritative. **This is the launcher default**; the flag states it explicitly. Conflicts with `--compatible` and `--synthetic-jdk`. (Alias: `--jdk-only`). |
| `--fast-boot`, `--compatible` | Fast-boot compatible mode: real JDK class library with fast-boot optimizations (synthetic acceleration bridges, lazy core bootstrap) for sub-second cold starts. |
| `--Xbootclasspath <PATH>` | Override the bootstrap classpath (advanced). Append with `-Xbootclasspath/a:<PATH>`. |
| `--diff-hotspot` | Run differentially against a reference JDK and report the first divergence in stdout / stderr / exit status. |
| `--real-jdk-report <FILE>` | Write Real-JDK policy violation and counter census to JSON (alias: `--jdk-only-report`). |
| `--dump-class-origins <FILE>` | Write class-origin census JSON (provenance of every loaded class). |
| `-V`, `--version`, `-version` | Print product version and execution mode. |
| `-h`, `-help`, `--help` | Print standard JDK launcher help text. |
| `--cratonvm-help` | Print CratonVM-specific options, execution modes, and diagnostic flags. |

Every command above is verified against the current binary. `--gpu` and the
other GPU options require a GPU-enabled build (`--features gpu-driver` for NVIDIA CUDA,
`--features gpu-driver-hip` for AMD HIP/ROCm, or `--features gpu-driver-opencl` for OpenCL) — see
[docs/gpu/README.md](docs/gpu/README.md).

## Foundational JDK Execution and Compatible Acceleration Layer

By foundation, CratonVM is architected to load and execute authentic OpenJDK
class libraries directly (`java.base` and core modules) — real JDK class bytes
are authoritative. In this standard mode (the default), no class is fabricated
without genuine bytecode, and the runtime executes standard OpenJDK bytecode faithfully.

The compatible mode (`--compatible`, alias `--fast-boot`) was engineered specifically as an
optimization layer to trigger fast cold-start loading and specialized throughput
execution paths, providing high-performance synthetic acceleration bridges.
Synthetic stubs exist to accelerate cold start and bypass complex initialization
paths in compatible mode, while foundational execution runs real bytecode.

Every embedding entry point (`VmConfig::default`, `libcratonvm`, `cratonvm-embed`) defaults
to compatible mode deliberately, providing immediate cold-start and throughput acceleration
to host applications without requiring CLI flags.

You can select the operating mode depending on your application needs:

```bash
./target/release/cratonvm -cp . MyApp                       # default: direct OpenJDK execution (Real-JDK mode)
./target/release/cratonvm --compatible -cp . MyApp          # compatibility and startup optimization layer
./target/release/cratonvm --real-jdk-report report.json -cp . MyApp # Real-JDK violation and counter report
./target/release/cratonvm --dump-class-origins origins.json -cp . MyApp # class-origin census
```

The report works in either mode: under `--compatible` it provides a census of what
strict direct execution would handle differently.

Details: [docs/feature-designs/real-jdk-mode.md](docs/feature-designs/real-jdk-mode.md) (operator
guide); the rest of the Real-JDK docset is indexed from
[docs/README.md](docs/README.md#what-is---real-jdk-mode).

## Limitations

- **AWT/Swing** are headless (no on-screen rendering); JavaFX is out of tree.
- **Reflection and JNI** cover the common paths; some edge cases
  (C-varargs JNI forms, full foreign-thread attach) are partial.
- **Cryptography** is best-effort and not constant-time everywhere — see
  [docs/CRYPTO_STATUS.md](docs/CRYPTO_STATUS.md).

## Documentation

- [Complete manual](docs/book/src/SUMMARY.md) — installation, use,
  compatibility, security, performance, operations, embedding, architecture,
  contributing, and reference
- [Documentation index](docs/README.md) — map of maintained standalone and
  manual references
- [ARCHITECTURE.md](ARCHITECTURE.md) — crate layout and subsystem design
- [BENCHMARK.md](BENCHMARK.md) — benchmark methodology and full results
- [Performance tuning](docs/book/src/performance/tuning.md) — measurement-first
  tuning workflow
- [Deployment and operations](docs/book/src/operations/deployment.md) —
  packaging, sizing, security boundaries, rollout, and rollback
- [Incident response](docs/book/src/operations/incident-response.md) —
  crash/hang/OOM/wrong-result diagnostic runbook
- [Runtime lifecycle](docs/book/src/internals/runtime-lifecycle.md) and
  [runtime contracts](docs/book/src/internals/runtime-contracts.md)
- [docs/GC.md](docs/GC.md) — collectors, barriers, heap layout
- [docs/JIT_OPTIMIZATION.md](docs/JIT_OPTIMIZATION.md) — the JIT optimization journey
- [docs/gpu/README.md](docs/gpu/README.md) — GPU offload reference
- [docs/EMBEDDING.md](docs/EMBEDDING.md) — embedding CratonVM in your application
- [docs/SECURITY_HARDENING.md](docs/SECURITY_HARDENING.md) — sandboxing and hardening
- [docs/CONFIG.md](docs/CONFIG.md) — configuration reference
- [docs/feature-designs/real-jdk-mode.md](docs/feature-designs/real-jdk-mode.md) — `--real-jdk`
  operator guide and design contract; the rest of the Real-JDK docset is indexed from
  [docs/README.md](docs/README.md)

## Building & Testing

```bash
cargo build --release          # full workspace
cargo test --workspace         # unit + integration tests
bash regression-suite/run.sh   # fast HotSpot-differential regression suite
```

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Architectural orientation lives in
[ARCHITECTURE.md](ARCHITECTURE.md); required merge signals are defined by
[release readiness](docs/RELEASE_READINESS.md) and the CI workflows.

## License

GPL-2.0-only WITH Classpath-exception-2.0 — see [LICENSE](LICENSE). CratonVM is an independent
implementation and is not derived from OpenJDK sources; see
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md) and
[TRADEMARKS.md](TRADEMARKS.md).
