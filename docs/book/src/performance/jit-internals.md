# JIT Compiler Internals and Optimization

CratonVM's JIT compiler lowers Java bytecode into optimizing native machine code that approaches HotSpot C2 performance across compute-intensive Java benchmarks.

This chapter details the architecture and optimization techniques implemented in the JIT compiler. For operational configuration, see [The JIT Compiler (User Guide)](../user-guide/jit-compiler.md); for subsystem architecture, see [The JIT Compiler Internals](../internals/jit.md).

## Optimization Architecture

The compiler lowers bytecode directly to x86-64 machine code, optimizing register utilization and instruction sequences:

```text
  int fib(int n) {            iload_0                push rbp / mov rbp,rsp
    if (n <= 1) return n;      iconst_1               cmp r12d, 1
    return fib(n-1)            if_icmpgt +5    ->      jg .L1
         + fib(n-2);           ireturn                movsxd rax, r12d ...
  }                            ...                    call fib ; add ; ret
```

The JIT compiler implements optimizations across core execution domains:

| Optimization Layer | Capabilities and Techniques |
|--------------------|-----------------------------|
| **Direct Array Addressing** | Direct heap access for `*aload` and `*astore` bytecodes, compact array layouts (1/2/4/8 bytes per element) with scaled-index (SIB) addressing. |
| **Register Allocation** | Graph-coloring and linear-scan register allocation mapping local variables directly to callee-saved registers, eliminating stack round-trips. |
| **Arithmetic Specialization** | Magic-number division lowering constant integer division and remainder (`/` and `%`) to multiply-and-shift instructions. |
| **Floating-Point Pipeline** | Scalar SSE and AVX instructions for single- and double-precision arithmetic, comparisons, and type conversions. |
| **Loop-Invariant Code Motion (LICM)** | Hoisting loop-invariant array indexing, constant expressions, and field reads into loop preheaders. |
| **Bounds-Check Elimination (BCE)** | Speculative loop-header guards eliminating per-iteration array bounds checks in provably safe loops. |
| **AVX2 Auto-Vectorization** | Vectorizing reduction loops and parallel array operations with 256-bit AVX2 SIMD instructions. |
| **On-Stack Replacement (OSR)** | Mid-method transition from interpreter execution to compiled native code for hot running loops. |
| **Escape Analysis & Scalar Replacement** | Proving object non-escape and replacing heap allocations with scalar stack and register variables. |
| **Polymorphic Inline Caching** | Direct call bridging, monomorphic caches, and hashed vtable tails for interface and virtual dispatch. |

## Compiler Architecture and Emitter Paths

The JIT compiles roughly 140 bytecodes and emits targeted x86-64 instructions: data movement, integer arithmetic, bitwise/shift operations, branch conditions, direct and indirect calls, sign/zero extension, SIB array addressing, scalar SSE floating-point instructions, and AVX2 vector operations.

Two compilation paths cooperate in tiered execution:

- **Tier 1 (Single-Pass Emitter)**: Lowers bytecode directly to machine code in a single traversal. Provides immediate compilation speed and low overhead.
- **Tier 2 (Sea-of-Nodes IR Pipeline)**: Builds an SSA-based intermediate representation graph, performs escape analysis, loop invariant hoisting, and bounds-check elimination, schedules instructions, and allocates registers before final lowering.

## Current Performance Profile

On compute-intensive benchmarks (such as arithmetic loops, Sieve of Eratosthenes, and dense matrix multiplication), CratonVM's optimizing JIT delivers performance competitive with HotSpot C2. Microbenchmarks dominated by deep recursion or extreme object allocation churn are influenced primarily by call-frame overhead and GC throughput. Detailed benchmark comparisons are maintained in [Benchmarks](benchmarks.md).
