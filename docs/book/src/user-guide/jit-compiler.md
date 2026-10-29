# The JIT Compiler

CratonVM executes Java bytecode using a high-performance tiered execution strategy: starting in a fast interpreter, profiling execution, and compiling hot methods to optimized native machine code using an advanced Just-In-Time (JIT) compiler.

## Tiered Compilation Architecture

CratonVM employs multi-tiered compilation to achieve both instantaneous startup and peak steady-state performance:

1. **Interpreter**: Methods execute initially in a threaded bytecode interpreter with over 140 fast-path opcodes and inline caching. Execution profiles and invocation counters are gathered continuously.
2. **Tier 1 (Baseline Compilation)**: Methods exceeding the initial compilation threshold (`CRATONVM_JIT_THRESHOLD`, default **500**) are compiled with light optimizations for quick turnaround.
3. **Tier 2 (Optimizing Compilation)**: Frequently invoked methods and hot loops undergo high-level intermediate representation (IR) analysis and deep optimizations.

### On-Stack Replacement (OSR)

Long-running loops trigger **On-Stack Replacement (OSR)** automatically. When a loop inside an interpreted or Tier 1 method executes repeatedly, the JIT compiles the hot loop body into optimized native code and seamlessly transfers thread execution mid-method from the interpreter frame directly into the compiled native frame without waiting for method return. Set `CRATONVM_JIT_OSR=0` to disable OSR for diagnostics.

### Code Cache Management

Compiled machine code resides in an executable memory code cache. The size of the code cache can be bounded using `CRATONVM_JIT_CODE_CACHE_MAX_MB`. If the cache fills, existing compiled code remains active while new methods continue executing in the interpreter.

## Optimization Passes

The optimizing JIT pipeline transforms bytecode into an SSA-based Intermediate Representation (IR) and applies extensive optimization passes:

- **Escape Analysis & Scalar Replacement**: Determines whether objects escape their allocating method or thread. Non-escaping objects are scalarized into registers or stack slots, eliminating heap allocation and GC pressure.
- **Loop-Invariant Code Motion (LICM)**: Hoists invariant calculations, array loads, and constant expressions outside loop headers.
- **Bounds-Check Elimination (BCE)**: Proves array index safety using range analysis and generates speculative loop-header guards, completely removing per-iteration array bounds checks from verified loops.
- **AVX2 SIMD Auto-Vectorization**: Automatically detects data-parallel loops and vectorizes integer and floating-point arithmetic using 256-bit AVX2 vector instructions on supported x86-64 processors.
- **Global Register Allocation**: Uses graph-coloring register allocation across general-purpose and SIMD registers, minimizing stack spills and memory traffic.
- **Constant Folding & Strength Reduction**: Simplifies arithmetic operations, including lowering integer division and modulo with constants to multiply-and-shift operations.
- **Inlining & Polymorphic Inline Caching**: Inlines small methods, field accessors, and monomorphic/bimorphic virtual call sites.

## Precise JIT Stack Maps

For garbage collector safety, the JIT compiler generates precise stack maps for every compilation safepoint. These maps indicate the exact CPU registers and stack slots that hold live object references. This allows the GC (such as ZGC or the Generational collector) to accurately trace root references within native JIT frames without conservative stack scanning. Precise stack maps are enabled by default; `CRATONVM_NO_PRECISE_JIT_MAPS=1` exists only as a diagnostic override.

## Controlling the JIT

| Goal | Mechanism | Default |
|------|-----------|---------|
| Disable JIT entirely (interpreter only) | `--nojit` (or `CRATONVM_DISABLE_JIT=1`) | JIT Enabled |
| Adjust compilation threshold | `CRATONVM_JIT_THRESHOLD=<count>` | `500` |
| Disable On-Stack Replacement | `CRATONVM_JIT_OSR=0` | OSR Enabled |
| Configure Code Cache limit | `CRATONVM_JIT_CODE_CACHE_MAX_MB=<MiB>` | Unbounded / Ergonomic |
| Disable precise stack maps (debug only) | `CRATONVM_NO_PRECISE_JIT_MAPS=1` | Precise Maps Enabled |

### Aggressive Compilation for Benchmarks

To compile methods immediately without waiting for warm-up iterations:

```bash
CRATONVM_JIT_THRESHOLD=1 cratonvm --classpath . MyBenchmark
```

### Isolating Interpreter vs. JIT Issues

To determine whether an unexpected behavior originates from the interpreter or compiled code:

```bash
# Run with JIT (default)
cratonvm --classpath . MyProgram

# Run purely in interpreter
cratonvm --nojit --classpath . MyProgram
```

If an issue occurs only with the JIT enabled, it isolates the problem to a specific compiler optimization pass.

## Architecture Support

The optimizing JIT compiler is fully production-ready on **x86-64** (Linux and Windows). An **AArch64** (ARM64) backend is actively developing. On unsupported architectures, CratonVM falls back transparently to its high-speed bytecode interpreter.

For further internal architecture details, see [The JIT Compiler Internals](../internals/jit.md) and [JIT Optimization Internals](../performance/jit-internals.md).
