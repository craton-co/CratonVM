# CratonVM

[![CI](https://github.com/craton-co/cratonvm/actions/workflows/ci.yml/badge.svg)](https://github.com/craton-co/cratonvm/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.80%2B-orange.svg)](https://www.rust-lang.org/)

A Java Virtual Machine written in Rust, with a custom x86-64 JIT and an
opt-in automatic GPU fast path for a documented subset of Java kernels.

CratonVM boots against a real JDK when one is present (`JAVA_HOME`,
`CRATONVM_JAVA_HOME`, or `java` on `PATH`) and runs fully standalone when it
isn't — its own Rust implementations of the Java standard library mean no JDK
install, no `rt.jar`, one self-contained binary.

## Highlights

- **Custom x86-64 JIT** — tiered compilation (interpreter → C1 → C2), OSR,
  LICM, bounds-check elimination, AVX2 SIMD, precise stack maps. Experimental
  AArch64 backend.
- **Automatic GPU fast path** — supported pure static array kernels can run on
  NVIDIA GPUs without API changes. Eligibility is intentionally narrow and
  unsupported shapes fall back to CPU (see below).
- **Generational GC** — young/old generations, card table, selective
  promotion. The moving/compacting (Cheney) young gen is the **default**, with
  `CRATONVM_NO_MOVING_YOUNG` as the compatibility opt-out; a cycle that cannot
  prove complete root coverage diverts to the non-moving sweep rather than
  relocating (see [ARCHITECTURE.md](ARCHITECTURE.md#memory-gc-crate)). Opt-in
  region-based G1 (`-XX:+UseG1GC`).
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
- **Provenance instrumentation (`--jdk-only`)** — an internal diagnostic that
  makes every compatibility substitution a counted, attributed event, so the
  gap between "runs on CratonVM" and "runs on the real class bytes" is
  measurable rather than assumed. Not a supported runtime mode; `--real-jdk`
  stays the default (see below).

## Performance

CPU, vs HotSpot JDK 25 C2 — same flags both sides, checksum-verified against
HotSpot on every run (zero mismatches):

| Benchmark                | JDK 25 C2 | CratonVM   | Ratio     |  Growth       |
|--------------------------|-----------|------------|-----------|---------------|
| Arithmetic (2B ops)      | 1,852 ms  | 3,601 ms   | 1.94x     | linear        |
| Fibonacci(44)            | 1,449 ms  | 5,059 ms   | 3.49x     | linear        |
| Sieve (100K × 20K)       | 2,333 ms  | 2,360 ms   | **1.01x** | parity        |
| Matrix 1280×1280         | 2,106 ms  | 2,094 ms   | **0.99x** | parity        |
| Binary Trees (depth 16)‡ | 49 ms     | 259 ms     | 5.29x     | super-linear  |
| Binary Trees (depth 18)‡ | 183 ms    | 1,195 ms   | 6.53x     | super-linear  |
| Binary Trees (depth 20)‡ | 898 ms    | 6,649 ms   | 7.40x     | super-linear  |
| HashMap (1M put/get)‡    | 45 ms     | 553 ms     | 12.29x    | non-monotonic |
| HashMap (10M put/get)‡   | 1,039 ms  | 5,499 ms   | 5.29x     | non-monotonic |
| HashMap (100M put/get)‡  | 11,455 ms | 120,465 ms | 10.52x    | non-monotonic |
| String/Regex (100K)‡     | 54 ms     | 242 ms     | 4.48x     | super-linear  |
| String/Regex (1M)‡       | 138 ms    | 2,320 ms   | 16.81x    | super-linear  |
| String/Regex (10M)‡      | 466 ms    | 23,359 ms  | 50.13x    | super-linear  |

Two rows (Matrix, Sieve) are at parity with HotSpot C2. **Growth** is what the
ratio does as N scales up several-fold *within* one kernel — the point of
showing three sizes per row instead of one is to make that visible instead of
asserting it. `linear` means the ratio stays roughly flat as N grows;
`super-linear` means CratonVM's disadvantage **compounds** with N — its
absolute time grows faster than HotSpot's, not just larger by a fixed factor.
String/Regex shows this most sharply: the ratio nearly triples at each 10x
step in N (4.48x → 16.81x → 50.13x). Binary Trees compounds more gently
(5.29x → 6.53x → 7.40x) as each two-level depth increase roughly quadruples
the node count. HashMap does not follow that pattern here — its ratio moves
12.29x → 5.29x → 10.52x, non-monotonic rather than steadily compounding, most
likely because the 1M run is short enough (553 ms) for fixed per-process
costs to still be a meaningful share of it on both sides.

‡ These nine rows run CratonVM under G1 (`--XX:UseGc G1`; HotSpot already
defaults to G1 on JDK 25) rather than the ZGC collector this project
defaults to. G1 is a large but uneven win here: 7.4-7.6x faster than
ZGC on Binary Trees at every depth measured, a smaller win on
HashMap and small String/Regex, and a measured ~21% **regression** at
String/Regex 10M. Full per-size data, checksums, CV, and the
ZGC-vs-G1 delta are in [BENCHMARK.md](BENCHMARK.md).

GPU offload, vs HotSpot C2 and [TornadoVM](https://github.com/beehive-lab/TornadoVM)
4.0.1 (RTX 2060, N = 2²⁴, warm, full H2D+kernel+D2H round-trip, checksums
bit-identical to HotSpot):

| Kernel                                             | HotSpot C2 | TornadoVM GPU | CratonVM GPU | vs HotSpot | vs TornadoVM |
|----------------------------------------------------|------------|---------------|--------------|------------|--------------|
| Integer div-chain (48 divs/elem)                   | 2,179 ms   | 27 ms         | **7 ms**     | **311x**   | **3.9x**     |
| Double div-chain (64 divs/elem)                    | 1,784 ms   | 135 ms        | **82 ms**    | **21.8x**  | **1.6x**     |
| 128 multiply-adds/elem (data-dependent multiplier) | 1,298 ms   | 26 ms         | **7 ms**     | **185x**   | **3.7x**     |
| Dot-product reduction (int·int → long, x300/elem)  | 1,168 ms   | unimplemented | **2 ms**     | **584x**   | n/a          |
| Ray tracer kernel, 7680×4320 (33.2M px)            | 837.1 ms   | 24.29 ms      | **12.29 ms** | **68x**    | **2.0x**     |

Ray tracer rows are 6 interleaved rounds each (arm order alternated per round
to cancel drift), full H2D+kernel+D2H, checksums bit-identical to HotSpot; the
3840×2160 row is pooled over two independent 6-round passes (12 rounds total,
craton faster in all 12) run 30 minutes apart, which agreed within noise. The
kernel is the reduced proxy (`bench-gpu/RayTracerKernel.java`), documented in
`raytracer-vs-tornadovm-RESOLVED-20260821.md` in the internal tree,
not the full `apps/TornadoVM-Ray-Tracer` app (whose real kernel — reflections,
soft shadows, a skybox — needs dynamic-length scene loops neither engine's
analyzer admits yet). The margin over TornadoVM shrinks with resolution
(2.5x → 2.2x → 2.0x → 1.85x, the last at 11520×6480 / 74.6M pixels, 6/6
rounds) as CratonVM's fixed per-launch cost advantage amortises away,
leaving a smaller but still consistent per-pixel-throughput edge. Every
frame from 160×120 to 11520×6480 is bit-identical to HotSpot's.

Re-verified 2026-09-05 on the same box (RTX 2060, TornadoVM 4.0.1 PTX,
N = 2²⁴). The four non-ray-tracer rows still hold and TornadoVM's integer
div-chain reproduced exactly at 26 ms. Two things that run did change:

- The **dot-product row is now 2 ms**, not 12 — the warp-shuffle reduction
  (one `red.global.add` per warp rather than per thread) landed after the
  original measurement.
- The **double div-chain row had silently stopped reproducing**: it measured
  9,276 ms, slower than HotSpot, because `runtime::offload_jit_gate` asked the
  constant-pool-*free* analyzer, which rejects `ldc2_w` unconditionally, while
  the dispatcher it gates for asks the pool-aware one. That kernel's
  `+ 1.0000001` is an `ldc2_w`; its integer twin's `+ 12345` is a `sipush`
  with no pool entry, which is why one row worked and the other did not.
  Fixed the same day; the row now measures 81 ms.

The CPU columns above are the original idle-box measurements and were **not**
re-taken — that re-run shared the host with an unrelated build, which makes a
CPU baseline pessimistic and every ratio derived from it flattering. The GPU
figures quoted in this note were measured under that same load, so they are
conservative rather than optimistic. The ray-tracer row was not re-run: it
needs `craton-gpu-0.2.0.jar` and a `cratonvm-gpuray` binary that are not
present in this tree.

Unlike TornadoVM, the supported automatic path needs no `@Parallel`
annotations or TaskGraph API — within a deliberately narrow eligibility
subset, not a general promise that arbitrary Java runs on the GPU. Full
results, extra sizes, and methodology:
[BENCHMARK.md](BENCHMARK.md) and [docs/gpu/README.md](docs/gpu/README.md).

## What to build on

Boots, runs, and passes large real-world test suites:

- **Spring / Spring Boot**
- **Tomcat / Netty**
- **Hibernate / Hibernate Reactive**
- **H2 DB / PostgreSQL driver**
- **Apache Commons Math** 
- **Bouncy Castle Java**


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
| `--jar <FILE>` | Execute a JAR (main class from `apps/META-INF/MANIFEST.MF`; classpath from the JAR and its manifest `Class-Path`). |
| `-Xmx<SIZE>`, `--Xmx <SIZE>` | Maximum heap size (`256m`, `1g`, `8g`). Default: 256 MB. |
| `-XX:+UseG1GC` | Use the region-based G1 collector (experimental; generational is the default). |
| `-D<name>=<value>` | Set a Java system property. |
| `--verbose:class` | Class-loading trace to stderr. |
| `--verbose:gc` | GC activity to stderr. |
| `--nojit` | Interpreter only (diagnosis / safety fallback). |
| `--noverify`, `--Xverify <MODE>` | Bytecode verification policy (`none` / `remote` / `all`). |
| `--java-home <PATH>` | Boot from a specific JDK's `java.base`. |
| `--synthetic-jdk` | Force the built-in Rust standard library even when a JDK is detected. |
| `--jdk-only` | Diagnostic strict mode: real JDK class bytes are authoritative. Implies `--real-jdk`, conflicts with `--synthetic-jdk`. May fail where the default passes — see below. |
| `--Xbootclasspath <PATH>` | Override the bootstrap classpath (advanced). |
| `-V`, `--version` / `--help` | Version / full option list. |

Every command above is verified against the current binary. `--gpu` and the
other GPU options require a `--features gpu-driver` build — see
[docs/gpu/README.md](docs/gpu/README.md).

## JDK-only mode (`--jdk-only`)

An **internal diagnostic**, not a supported runtime mode — `--real-jdk`
remains the default. It asks the VM to treat real JDK class bytes as
authoritative and report every compatibility substitution it would otherwise
make silently, so it is *expected* to fail on programs that pass under the
default. A/B a program to see what it depends on:

```bash
./target/release/cratonvm -cp . MyApp                                   # default: --real-jdk
./target/release/cratonvm --jdk-only --jdk-only-report report.json -cp . MyApp
```

Details: [docs/jdk-only-migration.md](docs/jdk-only-migration.md) (operator
guide); the rest of the JDK-only docset is indexed from
[docs/README.md](docs/README.md#what-is---jdk-only-mode).

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
- [docs/jdk-only-migration.md](docs/jdk-only-migration.md) — `--jdk-only`
  operator guide; the rest of the JDK-only docset is indexed from
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

Apache-2.0 — see [LICENSE](LICENSE). CratonVM is an independent
implementation and is not derived from OpenJDK sources; see
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md) and
[TRADEMARKS.md](TRADEMARKS.md).
