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

## How to influence the roadmap

File an issue describing the use case, or open a pull request implementing a milestone. See [CONTRIBUTING.md](CONTRIBUTING.md).
