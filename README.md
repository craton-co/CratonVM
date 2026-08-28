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
  Hibernate Reactive, Quarkus, H2 Database boot and pass large test suites.
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

CPU, vs HotSpot JDK 25 C2:
same flags both sides (`-Xmx8g`), one phase per fresh process pinned to one
core, arms **alternated with the order flipped on alternate pairs**, 9 pairs
per phase, no sample discarded, and **every one of the 18 samples per phase**
checksum-verified on both arms — not just the medians, so a mid-series drift
cannot hide behind a matching median. Zero mismatches. All seven phases come
from **one binary in one window**, 1-minute load 1.8–3.8 throughout, with the
series aborted and retried if load left the band mid-run. Full methodology in
[BENCHMARK.md](BENCHMARK.md).

| Benchmark               | JDK 25 C2 | CratonVM  | Ratio     | CV (CratonVM) | earlier ratio |
|-------------------------|-----------|-----------|-----------|---------------|---------------|
| Arithmetic (2B ops)     | 1,852 ms  | 3,601 ms  | 1.94x     | 0.6% | 2.44x |
| Fibonacci(44)           | 1,449 ms  | 5,059 ms  | 3.49x     | 0.6% | 2.79x |
| Sieve (100K × 20K)      | 2,333 ms† | 2,360 ms  | **1.01x** | 2.0% | 2.28x |
| Matrix 1280×1280        | 2,106 ms  | 2,094 ms  | **0.99x** | 0.2% | 2.93x |
| HashMap (10M put/get)   | 983 ms    | 2,049 ms  | 2.08x     | 0.6% | 1.75x |
| String/Regex (100K)     | 50 ms     | 200 ms    | 4.00x     | 0.9% | 7.7x  |
| Binary Trees (depth 18) | 176 ms    | 1,700 ms  | 9.66x     | 1.1% | 8.34x |

CratonVM's run-to-run spread is under 1% on five of the seven rows. Ratios are
the durable content; absolute times are this host on this day.

**Two rows are at parity with HotSpot C2** — Matrix and Sieve. Fibonacci sits
at 3.49x, Arithmetic at 1.94x. String/Regex is the row that has moved most.

HashMap and Binary Trees sit above their earlier figures, but those earlier
absolutes were taken on a since-re-provisioned host and were never re-measured
under the current protocol — read BENCHMARK.md before treating either as a
regression.

‡ **Fibonacci carries a known shape.** Both JIT backends erase the dead
shadow-stack thread fetch from the prologue; the single-pass backend jumps over
the erased ~46-byte span, and the IR backend previously overwrote it with
one-byte `NOP`s, so every IR method that published nothing retired 46 NOPs on
entry, on every invocation. That is fixed. The residual gap to the pre-IR
single-pass body (near 2.9x) is precise-root and deopt metadata the IR tier
emits and the single-pass backend did not: the safepoint-id slot the collector
reads to pick an oop map, and the innermost-RBP mirror the stack walker reads.
Nothing today compares an IR body against the C1 body it replaces before
keeping it; that is an open policy question, not metadata to be deleted.

† **HotSpot is *bimodal* on Sieve, which is why this row's HotSpot CV is the
worst in the table and why the ratio should be read as parity, not as a
number.** Across an 18-sample characterisation it lands either at ~2,369 ms or
~2,739 ms, with nothing in between — one run reads 2,386 ms and the next
2,734 ms on an unchanged binary. CratonVM's samples over the same 18 runs are
unimodal (2,276–2,498). In the series above, eight HotSpot samples fall in
2,279–2,357 and one lands at 2,755, which is the entire reason CV_HotSpot is
5.8% against CratonVM's 2.0%. The median (2,333 ms) therefore sits in the low
mode, and the 1.01x above is a low-mode-vs-CratonVM reading. Had the split gone
the other way the same binaries would have printed something nearer 0.9x. Both
are parity; neither is a 10% claim in either direction.

The optimizing tier declines a method whose loops the single-pass backend would
lower better, and what that backend can do and the IR tier cannot is enumerated
in `jit/src/x64/single_pass_only.rs` rather than discovered one regression at a
time.

GPU offload, vs HotSpot C2 and [TornadoVM](https://github.com/beehive-lab/TornadoVM)
4.0.1 (RTX 2060, N = 2²⁴, warm, full H2D+kernel+D2H round-trip, checksums
bit-identical to HotSpot):

| Kernel                                                | HotSpot C2 | TornadoVM GPU | CratonVM GPU | vs HotSpot | vs TornadoVM |
|---------------------------------------------------------|------------|---------------|--------------|------------|--------------|
| Integer div-chain (48 divs/elem)                         | 2,146 ms   | 26 ms         | **11 ms**    | **195x**   | **2.4x**     |
| Double div-chain (64 divs/elem)                          | 1,780 ms   | 128 ms        | **95 ms**    | **18.7x**  | **1.3x**     |
| 128 multiply-adds/elem (data-dependent multiplier)       | 1,300 ms   | 27 ms         | **8 ms**     | **163x**   | **3.4x**     |
| Dot-product reduction (int·int → long, x300/elem)        | 1,172 ms   | unimplemented | **12 ms**    | **98x**    | n/a          |

Unlike TornadoVM, the supported automatic path needs no `@Parallel`
annotations or TaskGraph API. This applies only to the eligibility subset in
the GPU reference; it is not a general promise that arbitrary Java runs on the
GPU. The multiply-add and dot-product rows are GPU wins; BENCHMARK.md's GPU
notes explain how a HotSpot constant-folding artifact makes the CPU side of
those two rows easy to misread. Full results, extra sizes, and methodology notes:
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
| `--jdk-only` | Diagnostic strict mode: real JDK class bytes are authoritative. Implies `--real-jdk`, conflicts with `--synthetic-jdk`. May fail where the default passes — see below. |
| `--Xbootclasspath <PATH>` | Override the bootstrap classpath (advanced). |
| `-V`, `--version` / `--help` | Version / full option list. |

Every command above is verified against the current binary. `--gpu` and the
other GPU options require a `--features gpu-driver` build — see
[docs/gpu/README.md](docs/gpu/README.md).

## JDK-only mode (`--jdk-only`) — internal diagnostic

Where CratonVM cannot yet run the JDK's own code, it substitutes: a native
Rust implementation, or occasionally a fabricated stand-in class. That is what
makes the VM useful today, and it is also why "it runs" and "it runs the real
class bytes" are different claims. `--jdk-only` is the instrument that tells
them apart — it asks the VM to treat real JDK class bytes as authoritative and
to report every substitution it would otherwise have made silently.

**Read the status honestly:**

- It is an **internal diagnostic stage**, not a supported runtime mode, and
  not a compatibility guarantee. The default is and remains `--real-jdk`;
  nothing here changes a default run.
- The current wave is **instrumentation and measurement, not deletion**. Only
  two things are actually enforced today: class fabrication, and the
  *registration* of synthetic-stub natives. The remaining dispatch-side rules
  are **counted and reported**, not yet refused.
- A `--jdk-only` run is **expected to fail on programs that pass under
  `--real-jdk`**. That failure is the measurement, not a regression in your
  program.
- It requires a real JDK image and never falls back.

A/B a program to see what it depends on:

```bash
./target/release/cratonvm -cp . MyApp                                   # default: --real-jdk
./target/release/cratonvm --jdk-only --jdk-only-report report.json -cp . MyApp
```

The report names each violation with the class involved and, where the kind of
violation has them, the method, descriptor, class origin and attempted native
kind. `--dump-class-origins <FILE>` adds the per-class provenance census.

Details: [docs/CONFIG.md](docs/CONFIG.md#jdk-only-mode) (flags and modes),
[docs/jdk-only-migration.md](docs/jdk-only-migration.md) (operator guide),
[docs/feature-designs/jdk-only-mode.md](docs/feature-designs/jdk-only-mode.md)
(the design contract — a proposal, not a statement that a wave has landed).

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
