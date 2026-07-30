# CratonVM Roadmap

CratonVM is an experimental Java Virtual Machine in Rust. This roadmap lists
the next milestones in rough priority order. Granular, session-level work
items are tracked internally and are subject to change.

## Status

CratonVM currently runs a wide subset of Java SE 8-25 bytecode on a custom
x86-64 JIT. It is research-grade software; see [SECURITY.md](SECURITY.md) for
production caveats.

## Near-term (next minor release)

- Tighter HotSpot C2 performance parity, with blocking regression budgets for
  HashMap, Binary Trees, and Regex before optimizing broader benchmark totals.
- GC throughput improvements on allocation-heavy workloads (Binary Trees).
  Moving-young collection is now the default (`CRATONVM_NO_MOVING_YOUNG` opts
  out), so its remaining optimizations — the `pointer_map` `FxHashMap`, the
  disabled self-call spill elision, and the unpriced `jit_frame_record` helper —
  are work on the default path, tracked in
  [`docs/moving-young-throughput.md`](docs/moving-young-throughput.md).
- Repeatable framework-throughput qualification for Spring Boot, Quarkus, and
  Micronaut, following [`docs/framework-throughput.md`](docs/framework-throughput.md).
- Complete `java.util.concurrent` parity (ForkJoin, ReentrantReadWriteLock,
  Phaser).
- Bytecode verifier completeness for pre-Java-7 class files.
- AArch64 JIT backend feature parity with x86-64.

The near-term and medium-term lists below are not stretch goals. Pre-Java-7
verifier coverage, `java.util.concurrent` parity, AArch64 parity, real-JDK boot
from a stock `java.base`, JNI function-table and lifecycle coverage, JFR event
coverage, and JEP 261 module semantics are the unresolved holes that currently
define what CratonVM can be used for. Read them as the platform's ceiling, not
as a wish list.

## Medium-term

- Real-JDK boot path stability: load `java.base` JMOD as primary stdlib.
- JNI: full function-table coverage and OnLoad/OnUnload protocol.
- JCK compliance run on Java SE 25 (see [docs/legal.md](docs/legal.md)).
- Concurrent garbage collector (G1 maturity, ZGC experimentation).
- JFR event coverage matching OpenJDK 25.

## Longer-term

- AWT / Swing headful support (current builds are headless-only).
- Module system: full JEP 261 resolution semantics.
- GPU offload — see the dedicated section below.
- Project Panama foreign linker maturity.

## GPU offload

**Validated on real hardware 2026-07-11** (RTX 2060, `--features gpu-driver`):
automatic offload for the documented eligible kernel subset matches HotSpot
checksums bit-for-bit and reaches ~210x over HotSpot C2 and
~3x over TornadoVM's PTX backend on a 48-division-per-element div-chain kernel at n = 2^24
(see [`bench-gpu/results/divchain-comparison-20260711.md`](bench-gpu/results/divchain-comparison-20260711.md)
and [docs/gpu/COMPARISON.md](docs/gpu/COMPARISON.md)). It remains opt-in
(`--features gpu-offload`/`gpu-driver`, `--gpu` at runtime) and CUDA/NVIDIA-only; see
[docs/gpu/README.md](docs/gpu/README.md).

Current product limits:

- Array-returning kernels and device-resident array chaining are not yet
  implemented; scalar reductions and caller-supplied output arrays are.
- Explicit non-default stream selection is only partially wired through the
  Java API.
- Analyzer/lowering coverage remains intentionally narrower than Java:
  non-canonical and nested loop shapes are rejected rather than silently
  offloaded.
- There is no self-hosted hardware CI: the `bench-gpu/` suite is not run on real
  CUDA hardware on every change, so the numbers above are point-in-time
  measurements rather than a continuously enforced budget.

Closed GPU follow-ups, including asynchronous completion and the JIT-caller
admission gate, are retained as
[historical evidence](docs/internal/fixed-suite-bugs/gpu-offload-followups-20260711.md).

Longer-horizon:

- 2D / nested counted loops (the analyzer currently accepts only a single canonical
  `for (i = 0; i < bound; i++)` loop per method).
- Multi-GPU selection beyond a single `--gpu-device` ordinal.
- Float/double reductions, pending a deterministic-summation strategy (naive atomic float
  add is non-associative and would diverge from HotSpot's sequential result).
- OpenCL/SPIR-V/multi-vendor backends are a non-goal: CratonVM's offload path is deliberately
  CUDA-only (see the positioning discussion in [docs/gpu/COMPARISON.md](docs/gpu/COMPARISON.md));
  multi-backend support is TornadoVM's niche, not this project's.

## Success criteria (aspirational targets)

These are directional goals to gauge progress, not guarantees or claims of
current state. CratonVM remains research-grade software; numbers are targets we
are aiming at, and may shift as priorities change.

- **Performance vs HotSpot C2** - narrow the current QuickBench TOTAL gap
  (3.7x default in the 2026-07-08 snapshot, now that back-edge OSR is
  default-on) toward <=1.2x of JDK 25 C2 runtime, with no single QuickBench
  micro above 1.5x. Bring Binary Trees (depth=18), currently 23.7x in the
  2026-07-08 recheck, under 5x through GC throughput work.
- **Bytecode verifier** - reach 100% of the structural/type checks needed to
  verify the pre-Java-7 split-verifier class-file corpus without `--noverify`.
- **JCK / compliance** - target >=90% pass rate on a single chosen JCK area,
  such as `lang` or `vm`, on Java SE 25 as a first compliance milestone
  (subject to the licensing notes in [docs/legal.md](docs/legal.md)).
- **AArch64 JIT** - reach feature parity with the x86-64 backend, validated by
  an identical JIT test suite pass rate across both backends.
- **`java.util.concurrent`** - full parity for ForkJoin, ReentrantReadWriteLock,
  and Phaser, with the corresponding JDK conformance tests passing.
- **Real-JDK boot** - make the `java.base` JMOD load path the default stdlib
  source, booting a stock `java.base` without fallback.

## How to influence the roadmap

File an issue describing your use case, or open a pull request implementing a
milestone. See [CONTRIBUTING.md](CONTRIBUTING.md).
