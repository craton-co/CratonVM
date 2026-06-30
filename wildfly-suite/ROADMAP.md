# CratonVM Roadmap

CratonVM is an experimental Java Virtual Machine in Rust. This roadmap lists the next milestones in rough priority order. Granular work items live in `docs/internal/` and are subject to change.

## Status

CratonVM currently runs a wide subset of Java SE 8–25 bytecode on a custom x86-64 JIT. It is research-grade software — see [SECURITY.md](SECURITY.md) for production caveats.

## Near-term (next minor release)

- Tighter HotSpot C2 performance parity on QuickBench, Fannkuch, N-Body
- GC throughput improvements on allocation-heavy workloads (Binary Trees)
- Complete `java.util.concurrent` parity (ForkJoin, ReentrantReadWriteLock, Phaser)
- Bytecode verifier completeness for pre-Java-7 class files
- AArch64 JIT backend feature parity with x86-64

## Medium-term

- Real-JDK boot path stability: load `java.base` JMOD as primary stdlib
- JNI: full function-table coverage and OnLoad/OnUnload protocol
- JCK compliance run on Java SE 25 (see [docs/legal.md](docs/legal.md))
- Concurrent garbage collector (G1 maturity, ZGC experimentation)
- JFR event coverage matching OpenJDK 25

## Longer-term

- AWT / Swing headful support (current builds are headless-only)
- Module system: full JEP 261 resolution semantics
- GPU offload (opt-in, see [docs/gpu/README.md](docs/gpu/README.md))
- Project Panama foreign linker maturity

## Success criteria (aspirational targets)

These are directional goals to gauge progress, not guarantees or claims of current
state. CratonVM remains research-grade software; numbers are targets we are aiming
at, and may shift as priorities change.

- **Performance vs HotSpot C2** — narrow the QuickBench TOTAL gap from the current
  ~1.5x toward ≤1.2x of JDK 25 C2 runtime, with no single QuickBench micro above
  1.5x. Aim to bring Binary Trees (depth=18), currently ~23x, under 5x through GC
  throughput work.
- **Bytecode verifier** — reach 100% of the structural/type checks needed to verify
  the pre-Java-7 (split-verifier) class-file corpus without `--noverify`.
- **JCK / compliance** — target ≥90% pass rate on a single chosen JCK area (e.g.
  `lang` or `vm`) on Java SE 25, as a first compliance milestone (subject to the
  licensing notes in [docs/legal.md](docs/legal.md)).
- **AArch64 JIT** — reach feature parity with the x86-64 backend (matching opcode
  coverage and optimization rounds), validated by an identical JIT test suite pass
  rate across both backends.
- **`java.util.concurrent`** — full parity for ForkJoin, ReentrantReadWriteLock,
  and Phaser, with the corresponding JDK conformance tests passing.
- **Real-JDK boot** — make the `java.base` JMOD load path the default stdlib
  source (replacing synthetic stubs), booting a stock `java.base` without fallback.

## How to influence the roadmap

File an issue describing the use case, or open a pull request implementing a milestone. See [CONTRIBUTING.md](CONTRIBUTING.md).
