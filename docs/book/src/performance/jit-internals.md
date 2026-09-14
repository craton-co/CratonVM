# How the JIT Got Fast

CratonVM's JIT compiler went through a long sequence of optimization rounds,
starting from a naive interpreter that was roughly **97x slower** than the
HotSpot interpreter. An early snapshot reached about **1.5x of
HotSpot C2** on QuickBench; that figure is historical, not the current benchmark
claim. For current measurements, see [Benchmarks](benchmarks.md).

This chapter is the engineering story, useful background for anyone tuning
workloads or working on the compiler. For day-to-day JIT operation see
[The JIT Compiler](../user-guide/jit-compiler.md); for the architecture see
[The JIT Compiler internals](../internals/jit.md).

## The arc

```text
  Before optimization   ~97x  slower than the HotSpot interpreter
  Basic JIT             ~1.8x slower
  Register-allocated locals   FASTER than the interpreter
  ...
  March 2026 R26        ~1.50x of HotSpot C2 on QuickBench
  Current snapshot      see Benchmarks; OSR-enabled loops are mixed, Fibonacci and Binary Trees lag
```

The compiler lowers bytecode directly to x86-64 machine code:

```text
  int fib(int n) {            iload_0                push rbp / mov rbp,rsp
    if (n <= 1) return n;      iconst_1               cmp r12d, 1
    return fib(n-1)            if_icmpgt +5    ->      jg .L1
         + fib(n-2);           ireturn                movsxd rax, r12d ...
  }                            ...                    call fib ; add ; ret
```

## Optimizations, in the order they landed

The work proceeded in numbered "rounds." The highlights:

| Theme | What it added |
|-------|---------------|
| **Inline array ops** | `newarray`, and the `*aload`/`*astore` families lowered to direct heap accesses. |
| **Register-allocated locals** | The first few locals pinned to callee-saved registers: zero-cost loads. |
| **Magic-number division** | Constant `/` and `%` lowered to multiply-and-shift, avoiding the slow `IDIV`. |
| **Compact array layouts** | 1/2/4/8 bytes per element by type, addressed with SIB scaling. |
| **SSE float/double pipeline** | FP arithmetic, comparisons, and conversions via SSE scalar instructions. |
| **Object field access** | `getfield`/`putfield`, with a write barrier on reference stores. |
| **Loop-invariant code motion (LICM)** | Hoisting invariant array loads into a loop preheader. |
| **VM context + class checks** | `checkcast`/`instanceof`/`getstatic`/`putstatic` via the VM context pointer. |
| **Compact reference arrays** | `Object[]` stored as raw 8-byte pointers instead of a 16-byte tagged value. |
| **Bounds-check elimination (BCE)** | One length check before a provably safe loop instead of per element, with an out-of-line throw. |
| **Virtual/interface/special calls** | A helper bridge for dynamic dispatch, plus direct calls between compiled methods. |
| **On-Stack Replacement (OSR)** | A hot loop compiled and entered mid-method by default; `CRATONVM_JIT_OSR=0` disables it for diagnosis. |
| **AVX2 SIMD** | CPUID-gated vectorization of data-parallel reduction loops, with a scalar remainder. |
| **Structure-of-Arrays value layout** | Operand stack and locals split into separate value and tag arrays for better cache behavior and GC scanning. |
| **Loop unrolling, speculative BCE, graph-coloring regalloc** | Unrolling small loop bodies, a speculative loop-header bounds guard, and graph-coloring register allocation. |

## What the compiler emits

The JIT compiles roughly 140 bytecodes and emits a focused x86-64 instruction
set: data movement, integer arithmetic and magic division, bitwise and shift
ops, branches, calls, sign/zero extension, SIB-addressed array access, scalar
SSE floating-point instructions, and AVX2 vector ops.

Two compilation paths exist:

- A **single-pass emitter** that lowers bytecode directly to machine code. This
  is the default path: simple and fast to compile.
- An optional **sea-of-nodes IR pipeline** (build -> optimize -> schedule ->
  lower) for methods that qualify, decoupling optimization from instruction
  selection.

See [The JIT Compiler internals](../internals/jit.md) for the structure of both.

## Where the gap remains

Current measurements show the OSR-enabled Arithmetic, Sieve, and Matrix kernels
near HotSpot C2, but recursive Fibonacci remains call-heavy and slow, and Binary
Trees is dominated by allocation and GC throughput. See [Benchmarks](benchmarks.md)
and the [Roadmap](../contributing/roadmap.md).
