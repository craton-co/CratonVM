# JIT architecture and optimization

The JIT combines baseline bytecode compilation with an optimizing Sea-of-Nodes
pipeline. The optimizing path builds and schedules IR, applies guarded
transformations, allocates locations, and lowers to machine code. Compilation
can run on a background worker while the mutator continues interpreting.

## Compiler paths

`jit/src/x64/` owns baseline x86-64 compilation. The optimizing path is centered
on `jit/src/ir.rs`, `ir_optimize.rs`, `ir_schedule.rs`, and `ir_lower.rs`.
`jit/src/x64/isel.rs` defines instruction patterns and machine constraints.
The AArch64 backend is experimental; see [platform support](PLATFORMS.md) and
[known JIT issues](known-issues/jit/).

Compiler reach is conditional. A declared transform or encoder pattern does
not prove a method was admitted, optimized, installed, or entered. Inspect
[compile decisions](observability/jit-compile-decision.md) when diagnosing reach.

## Compilation and OSR policy

The tier manager progresses from interpretation through baseline compilation
and profiling toward the optimizing tier. Background compilation and C1-to-C2
superseding are enabled by default: mutators continue interpreting or executing
a baseline body while a worker compiles its replacement. Publication invalidates
cached targets as required so callers can resolve the replacement safely.

`jit/src/tiered.rs::CompilationPolicy` defaults to 500 invocations for C1 and
20,000 for C2, with a C2 minimum of 1,000. Invocation-based C2 admission requires
both thresholds, so its effective default is 20,000. Eager first-call baseline
compilation and backedge OSR are separate entry paths; these policy numbers are
not a promise that every method waits that long or receives an optimizing body.

OSR is enabled by default. The interpreter's normal per-frame attempt threshold
is 1,000 backedges; the tier policy's 10,000 `osr_threshold` field is not the live
per-frame trigger. A hot loop can enter a published compiled body without waiting
for the enclosing method to return. OSR admission still requires a supported
loop and a validated frame map. See [tiering](feature-designs/wire-tiered-manager.md)
and [OSR entry metadata](feature-designs/jit-osr-entry-metadata.md).

## Bytecode and intrinsic coverage

The x86-64 compiler covers integer and floating-point arithmetic, locals and
operand stacks, arrays, branches and switches, fields, ordinary calls,
allocation, type checks, throws, and monitor operations. Admission depends on
the method shape and backend; unsupported operations can use runtime helpers,
transfer to the interpreter, or prevent compilation. `invokedynamic` has
specialized plans, bridges, and uncommon-trap paths rather than one universal
machine-code implementation.

| Intrinsic family | Examples |
|---|---|
| `Math` / `StrictMath` | Square root, rounding, absolute value, FMA, min/max, high-word multiplication. |
| `Integer` / `Long` | Population count, leading/trailing zeros, byte reversal, rotations, comparisons. |
| Strings | Length, character access, hash, equality, comparison, and search with encoding guards. |
| Arrays | Copy, fill, equality, and admitted primitive sorting paths. |
| Atomics and checksums | Selected atomic operations and CRC32C operations with family-specific guards. |

`jit/src/intrinsic_catalogue.rs` declares intrinsic identities; dispatch and
backend lowering determine which overloads and receiver layouts are admitted.
An enum variant alone does not establish support for every execution policy.
An intrinsic must retain Java null, bounds, exception, overflow, NaN, signed-zero,
and memory-order behavior, with the normal execution path on a failed guard.

## Optimization contracts

- [IR optimization](feature-designs/jit-ir-optimizations.md): folding, GVN, dead
  stores/code, loop transforms, and escape-based scalar replacement preserve
  Java exceptions, memory order, and observable side effects.
- [Inlining](feature-designs/profile-guided-inlining.md): resolve the actual
  callee, retain receiver guards, and publish dependency/deoptimization state.
- [Loops](jit/loop-transforms.md): hoists and bounds elimination need dominance,
  alias, trip-count, and memory-token proofs; transformed loops retain polls.
- [Allocation and references](architecture/inline-allocation-and-reference-publication.md):
  inline allocation and stores obey the active collector's publication rules.
- [Machine locations](feature-designs/jit-machine-level-and-instruction-selection.md):
  frame homes, registers, caller clobbers, parallel copies, and GC/deopt locations
  must agree at every transition. Unsupported shapes use the admitted fallback.
- [AVX-512](feature-designs/avx-512-vector-acceleration.md): emission requires CPU
  and OS support; numerical loop admission and runtime vetoes remain explicit.

## OSR, deoptimization, and code lifetime

OSR entry validates interpreter-to-compiled location maps. Guard failure resumes
at a reconstructable state without repeating side effects. Invalidated artifacts
must not be entered or freed while another thread can execute them.
See [entry metadata](feature-designs/jit-osr-entry-metadata.md),
[deopt/OSR](feature-designs/deopt-osr.md),
[code lifetime](jit/code-cache-lifetime.md), and [lifecycle](jit/code-cache-lifecycle.md).

## Configuration and diagnostics

The [configuration reference](CONFIG.md) inventories flags; defaults are decided
at their read sites. Diagnostic selectors must be read at the same configuration
scope as their owning VM or compilation. Feature presence is not an execution
policy: both `--real-jdk` and `--compatible` target HotSpot behavior.

Use [profiling](PROFILING.md), [compiler metrics](jit/compiler-metrics.md),
[compile decisions](observability/jit-compile-decision.md), and
[differential testing](testing/jit-differential.md) to attribute refusals and
semantic differences. Benchmark commands and measurement requirements are in
[benchmarking](../BENCHMARK.md).
