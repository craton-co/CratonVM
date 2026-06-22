# How the JIT Got Fast

CratonVM's JIT compiler reached **~1.5× of HotSpot C2** on the QuickBench suite
through a long sequence of optimization rounds, starting from a naive
interpreter that was roughly **97× slower** than the HotSpot interpreter. This
chapter is the engineering story — useful background for anyone tuning workloads
or working on the compiler. For day-to-day JIT operation see [The JIT
Compiler](../user-guide/jit-compiler.md); for the architecture see [The JIT
Compiler internals](../internals/jit.md).

## The arc

```text
  Before optimization   ~97×  slower than the HotSpot interpreter
  Basic JIT             ~1.8× slower
  Register-allocated locals   FASTER than the interpreter
  ...
  Latest                ~1.50× of HotSpot C2 on QuickBench (Fibonacci ~1.31×)
```

The compiler lowers bytecode directly to x86-64 machine code:

```text
  int fib(int n) {            iload_0                push rbp / mov rbp,rsp
    if (n <= 1) return n;      iconst_1               cmp r12d, 1
    return fib(n-1)            if_icmpgt +5    →      jg .L1
         + fib(n-2);           ireturn                movsxd rax, r12d ...
  }                            ...                    call fib ; add ; ret
```

## Optimizations, in the order they landed

The work proceeded in numbered "rounds." The highlights:

| Theme | What it added |
|-------|---------------|
| **Inline array ops** | `newarray`, and the `*aload`/`*astore` families lowered to direct heap accesses. |
| **Register-allocated locals** | The first few locals pinned to callee-saved registers — zero-cost loads. |
| **Magic-number division** | Constant `÷`/`%` lowered to multiply-and-shift, avoiding the slow `IDIV`. |
| **Compact array layouts** | 1/2/4/8 bytes per element by type, addressed with SIB scaling. |
| **SSE float/double pipeline** | All FP arithmetic, comparisons, and ~12 conversions via `ADDSS`/`ADDSD`/`UCOMISS`/`CVT*`. |
| **Object field access** | `getfield`/`putfield`, with a write barrier on reference stores. |
| **Loop-invariant code motion (LICM)** | Hoisting invariant array loads into a loop preheader. |
| **VM context + class checks** | `checkcast`/`instanceof`/`getstatic`/`putstatic` via the VM context pointer. |
| **Compact reference arrays** | `Object[]` stored as raw 8-byte pointers (down from a 16-byte tagged value). |
| **Bounds-check elimination (BCE)** | One length check before a provably safe loop instead of per element, with an out-of-line throw. |
| **Virtual/interface/special calls** | A helper bridge for dynamic dispatch, plus direct calls between compiled methods. |
| **On-Stack Replacement (OSR)** | A hot loop compiled and entered mid-method, transferring interpreter state into the JIT frame. |
| **AVX2 SIMD** | CPUID-gated vectorization (`VPMULLD`/`VPADDD`) of data-parallel reduction loops, with a scalar remainder. |
| **Structure-of-Arrays value layout** | Operand stack and locals split into separate value and tag arrays — less memory, better cache use, tag-based GC scanning. |
| **Loop unrolling, speculative BCE, graph-coloring regalloc** | Unrolling small loop bodies, a speculative loop-header bounds guard, and graph-coloring register allocation across the callee-saved set. |

## What the compiler emits

The JIT compiles ~140 bytecodes and emits a focused x86-64 instruction set:
data movement (`MOV`/`LEA`/`PUSH`/`POP`), integer arithmetic and the magic-div
sequence, bitwise/shift ops, `CMP`/`Jcc`/`CMOV`/`SETcc`, `CALL`/`JMP`/`RET`,
sign/zero-extension, SIB-addressed array access, the SSE scalar-FP and
conversion instructions, and AVX2 vector ops.

Two compilation paths exist:

- A **single-pass emitter** that lowers bytecode directly to machine code (the
  default path — simple, fast to compile).
- An optional **sea-of-nodes IR pipeline** (build → optimize → schedule → lower)
  for methods that qualify, which decouples optimization from instruction
  selection.

See [The JIT Compiler internals](../internals/jit.md) for the structure of both.

## Where the gap remains

Compute-bound code is close to C2. The dominant remaining gap is **allocation
and GC throughput** (the Binary Trees workload), which is a GC-side problem more
than a codegen one — see [Benchmarks](benchmarks.md) and the
[Roadmap](../contributing/roadmap.md).
