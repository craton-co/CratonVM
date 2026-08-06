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
  relocating (see [ARCHITECTURE.md](ARCHITECTURE.md#memory-gc-crate) and
  [moving-young throughput](docs/moving-young-throughput.md)). Opt-in
  region-based G1 (`-XX:+UseG1GC`).
- **Real frameworks run** — Spring, Spring Boot, Tomcat, Hibernate, and H2
  boot and pass large test suites.
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

CPU, vs HotSpot JDK 25 C2, re-measured **2026-08-06** on `dev` (`5428367c0`):
same flags both sides (`-Xmx8g`), one phase per fresh process pinned to one
core, arms **alternated with the order flipped on alternate pairs**, 9 pairs
per phase, no sample discarded, and **every one of the 18 samples per phase**
checksum-verified on both arms — not just the medians, so a mid-series drift
cannot hide behind a matching median. Zero mismatches. All seven phases come
from **one binary in one window**, 1-minute load 1.8–3.8 throughout, with the
series aborted and retried if load left the band mid-run. Full methodology in
[BENCHMARK.md](BENCHMARK.md).

| Benchmark               | JDK 25 C2 | CratonVM  | Ratio     | CV (CratonVM) | was (2026-07) |
|-------------------------|-----------|-----------|-----------|---------------|---------------|
| Arithmetic (2B ops)     | 1,852 ms  | 3,601 ms  | 1.94x     | 0.6% | 2.44x |
| Fibonacci(44)           | 1,449 ms  | 5,059 ms‡ | 3.49x‡    | 0.6% | 2.79x |
| Sieve (100K × 20K)      | 2,333 ms† | 2,360 ms  | **1.01x** | 2.0% | 2.28x |
| Matrix 1280×1280        | 2,106 ms  | 2,094 ms  | **0.99x** | 0.2% | 2.93x |
| HashMap (10M put/get)   | 983 ms    | 2,049 ms  | 2.08x     | 0.6% | 1.75x |
| String/Regex (100K)     | 50 ms     | 200 ms    | 4.00x     | 0.9% | 7.7x  |
| Binary Trees (depth 18) | 176 ms    | 1,700 ms  | 9.66x     | 1.1% | 8.34x |

CratonVM's run-to-run spread is under 1% on five of the seven rows. Ratios are
the durable content; absolute times are this host on this day.

**Two rows are at parity with HotSpot C2** — Matrix (from 2.93x) and Sieve.
**String/Regex moved most this cycle, 5.37x → 4.00x** (274 → 200 ms), the
largest single-row improvement since July. Fibonacci is 3.49x, down from a
5.89x that was a real regression and is now fixed — see ‡. Arithmetic is
unchanged within noise.

HashMap and Binary Trees remain above their July figures, and those July
absolutes were taken on a since-re-provisioned host and were never re-measured
under the current protocol — see BENCHMARK.md before reading the two as
regressions. Fibonacci is deliberately *not* in that sentence any more: it was
checked against a same-host interleaved control and was genuinely a regression.

‡ **Fibonacci briefly reached 5.89x, and that was a real regression.** Unlike
HashMap and Binary Trees it does not need the caveat above — it was confirmed
on *one* host in *one* interleaved window: 4,240 ms on a 2026-07-23 build
against 8,400 ms on `dev`, same JDK, same classes, **1.96x**.

The cause was not a tiering or optimizer decision. Both JIT backends erase the
dead shadow-stack thread fetch from the prologue; the single-pass backend has
jumped over the erased ~46-byte span since June, while the IR backend only
overwrote it with one-byte `NOP`s. So every IR method that published nothing
**retired 46 NOPs on entry, on every invocation** — and `fib`, a two-line
static method entered 2.27e9 times, paid it 2.27e9 times. Fixed 2026-08-05:
**1.74x** recovered against its own parent commit (8 interleaved pairs, user
CPU time, disjoint ranges), all seven phase checksums unchanged
(`perf-02-ir-thread-fetch-nop-sled-FIXED-20260805.md`).

The row above is post-fix and, unlike the two earlier attempts at it, comes
from the same binary and the same window as the other six. It reproduced
independently on the way there: 3.51x measured on the fix branch on 2026-08-05,
3.49x on `5428367c0` here.

The remaining gap to the 2026-07 figure is not the same defect. The
pre-regression single-pass body sits near 2.9x here, so roughly a fifth of the
original gap is still open, and it is precise-root and deopt metadata the IR
tier emits and the single-pass backend did not: the safepoint-id slot the
collector reads to pick an oop map, and the innermost-RBP mirror the stack
walker reads. That is `perf-01`'s open policy question — nothing today compares
an IR body against the C1 body it replaces before keeping it — not a defect to
be fixed by deleting metadata.

† **HotSpot is *bimodal* on Sieve, which is why this row's HotSpot CV is the
worst in the table and why the ratio should be read as parity, not as a
number.** Across an earlier 18-sample characterisation it landed either at
~2,369 ms (10 samples) or ~2,739 ms (8), with nothing in between — one run read
2,386 ms and the next 2,734 ms on an unchanged binary. CratonVM's samples over
the same 18 runs were unimodal (2,276–2,498).

The 2026-08-06 series shows the same shape: eight HotSpot samples fall in
2,279–2,357 and one lands at 2,755, which is the entire reason CV_HotSpot is
5.8% against CratonVM's 2.0%. The median (2,333 ms) therefore sits in the low
mode, and the 1.01x above is a low-mode-vs-CratonVM reading. Had the split gone
the other way the same binaries would have printed something nearer 0.9x. Both
are parity; neither is a 10% claim in either direction.

Sieve was **6.50x** on 2026-08-04 and is not any more. That was a live
regression: `CratonBench.sieve([ZI)I` had begun getting a body from the
optimizing (C2/IR) tier where it previously fell through to the single-pass
backend, and the IR body was 6.4x slower than the C1 body it replaced. Fixed
2026-08-04 — the optimizing tier now declines a method whose loops the
single-pass backend would lower better, and what that backend can do and the IR
tier cannot is enumerated rather than discovered one regression at a time
(`jit/src/x64/single_pass_only.rs`,
`perf-01-sieve-ir-body-slower-than-c1-FIXED-20260804.md`).

GPU offload, vs HotSpot C2 and [TornadoVM](https://github.com/beehive-lab/TornadoVM)
4.0.1 (RTX 2060, N = 2²⁴, warm, full H2D+kernel+D2H round-trip, checksums
bit-identical to HotSpot):

| Kernel                                   | HotSpot C2 | TornadoVM GPU | CratonVM GPU | vs HotSpot | vs TornadoVM |
|------------------------------------------|------------|---------------|--------------|------------|--------------|
| Integer div-chain (48 divs/elem)         | 1,910 ms   | 28 ms         | **9 ms**     | **212x**   | **3.1x**     |
| Double div-chain (64 divs/elem)          | 1,508 ms   | 129 ms        | **91 ms**    | **16.6x**  | **1.4x**     |
| 96 multiply-adds/elem (AVX2 on CPU)      | 8 ms       | 17 ms         | 11 ms        | 0.7x       | 1.5x         |
| Dot-product reduction (int·int → long)   | 7 ms       | unimplemented | 18 ms        | 0.4x       | n/a          |

Unlike TornadoVM, the supported automatic path needs no `@Parallel`
annotations or TaskGraph API. This applies only to the eligibility subset in
the GPU reference; it is not a general promise that arbitrary Java runs on the
GPU. Full results, extra sizes, and counter-cases: [BENCHMARK.md](BENCHMARK.md) and
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
- **JDBC / `java.sql`** is not wired yet — real database support is on the
  roadmap above.
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
