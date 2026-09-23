# Roadmap

CratonVM is an experimental JVM, and this roadmap lists the next milestones in
rough priority order. It is directional, not a set of guarantees — priorities
shift as work lands.

## Status

CratonVM runs a wide subset of Java SE 8–25 bytecode on a custom x86-64 JIT. It
is research-grade software — see the [Security Overview](../security/overview.md)
for production caveats.

## Near-term

- Tighter HotSpot C2 performance parity on the QuickBench micros and other
  compute benchmarks.
- GC throughput improvements on allocation-heavy workloads (the Binary Trees
  gap — see [Benchmarks](../performance/benchmarks.md)).
- Fuller `java.util.concurrent` parity (ForkJoin, `ReentrantReadWriteLock`,
  `Phaser`).
- Bytecode-verifier completeness for pre-Java-7 class files.
- AArch64 JIT backend feature parity with x86-64.

## Medium-term

- Real-JDK boot-path stability: load the `java.base` module as the primary
  standard library by default.
- JNI: full function-table coverage and the OnLoad/OnUnload protocol; the
  foreign-thread attach path with safepoint participation.
- A first compliance run on Java SE 25.
- Concurrent garbage collection: maturing the region-based G1 collector into a
  selectable, validated collector.
- JFR event coverage matching a current OpenJDK.

## Longer-term

- Headful AWT / Swing support (current builds are headless-only).
- Full module-system resolution semantics.
- GPU offload maturation (see [GPU Offload](../gpu/overview.md)).
- Project Panama foreign-linker maturity.

## Aspirational targets

These are directional goals to gauge progress, not claims about the current
state:

- **Performance:** narrow the QuickBench total gap toward ≤1.2× of a current
  HotSpot C2, with no single micro above 1.5×, and bring the Binary Trees gap
  under 5× through GC work.
- **Verifier:** reach 100% of the structural/type checks needed to verify the
  pre-Java-7 class-file corpus without `--noverify`.
- **AArch64 JIT:** feature parity with the x86-64 backend, validated by an
  identical JIT test-suite pass rate across both.
- **`java.util.concurrent`:** full parity for ForkJoin,
  `ReentrantReadWriteLock`, and `Phaser`.
- **Real-JDK boot:** make the `java.base` module load path the default
  standard-library source, booting a stock `java.base` without fallback.

## Influencing the roadmap

File an issue describing your use case, or open a pull request implementing a
milestone. See the [Contributing Guide](contributing.md).
