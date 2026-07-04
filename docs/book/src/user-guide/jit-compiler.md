# The JIT Compiler

CratonVM executes bytecode in an interpreter, then compiles hot methods to
native x86-64 machine code with its own just-in-time (JIT) compiler. For most
workloads this is transparent and automatic — but it's worth knowing how to
control it when measuring performance or isolating a bug. This chapter is about
*operating* the JIT; for the engineering story see [How the JIT Got
Fast](../performance/jit-internals.md) and [The JIT
Compiler internals](../internals/jit.md).

## How compilation kicks in

- Each method has an invocation counter. When it reaches the **warmup
  threshold** (`CRATONVM_JIT_THRESHOLD`, default **500**), the method becomes
  eligible for compilation.
- Long-running loops can trigger **On-Stack Replacement (OSR)** by default: a
  hot loop is compiled and execution transfers from the interpreter into the
  compiled code mid-method, without waiting for the method to be re-entered.
  Set `CRATONVM_JIT_OSR=0` to disable this path for diagnosis.
- Compiled code is kept in a code cache. When the cache cap is reached, new
  methods stay interpreted.

The JIT targets **x86-64 only**. On other architectures it is automatically
disabled and the interpreter runs everything. (An AArch64 backend exists but is
partial.)

## Controlling the JIT

| Goal | How |
|------|-----|
| Disable the JIT entirely (interpreter only) | `--nojit` (or `CRATONVM_DISABLE_JIT=1`) |
| Change the warmup threshold | `CRATONVM_JIT_THRESHOLD=<n>` (`0` is clamped to `1`) |
| Disable hot-loop OSR | `CRATONVM_JIT_OSR=0` |
| Bound the code cache | `CRATONVM_JIT_CODE_CACHE_MAX_MB=<MiB>` (`0` = unbounded) |

### Lower the threshold to compile sooner

```bash
# Compile methods aggressively (useful for short benchmarks)
CRATONVM_JIT_THRESHOLD=1 cratonvm --classpath . MyBench
```

### Disable the JIT to isolate behavior

If a program produces wrong output or appears to hang, comparing with `--nojit`
quickly tells you whether the issue is in the interpreter or the JIT:

```bash
cratonvm --nojit --classpath . MyProgram
```

If the problem disappears under `--nojit`, it points at the JIT (and is worth a
bug report — see [Troubleshooting](troubleshooting.md)).

## What the JIT optimizes

The compiler applies a broad set of optimizations, including:

- **Register-allocated locals** (graph-coloring allocation across callee-saved
  registers).
- **Magic-number division** — constant `÷`/`%` lowered to multiply-and-shift.
- **Loop-invariant code motion (LICM)** — hoisting invariant array loads out of
  loops.
- **Bounds-check elimination (BCE)**, including a speculative loop-header guard
  that removes per-element checks from provably safe loops.
- **AVX2 SIMD vectorization** of data-parallel reduction loops (when the CPU
  supports AVX2).
- **Loop unrolling** for small loop bodies.
- **Inlined field and array access**, virtual/interface call bridges, and
  cross-method static calls.

## Precise JIT stack maps

CratonVM records **precise JIT stack maps** (which registers/slots hold live
object references at each safepoint), so the garbage collector can find roots in
compiled frames accurately. This is **on by default**. The opt-out
`CRATONVM_NO_PRECISE_JIT_MAPS` reverts to an older conservative stack scan and
exists only for diagnosing GC-root coverage issues — you should not need it.

## Measuring JIT performance

Always benchmark with a `--release` build, run multiple times, and take the
median. See [Profiling](../performance/profiling.md) for tooling and method, and
[Benchmarks](../performance/benchmarks.md) for representative results against
HotSpot.
