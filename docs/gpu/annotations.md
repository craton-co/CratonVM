# GPU offload annotations — Phase 1 reference

CratonVM ships a small set of Java annotations that let library and
application authors steer the GPU offload pipeline without changing
JVM flags. This document describes Phase 1 of that surface:
`@GpuKernel`, `@GpuExclude`, and `@EnableGpuAsync`.

The annotations are advisory inputs to the analyzer in
[`jit-cuda/src/analyzer.rs`](../../jit-cuda/src/analyzer.rs) and the
cache in [`vm/src/runtime/offload.rs`](../../vm/src/runtime/offload.rs).
They never *force* a method onto the GPU — the analyzer still has the
final say on whether bytecode can be safely lowered. What they do is:

1. Loosen specific analyzer rejections that the user has audited
   (`@GpuKernel(admit = ...)`).
2. Forbid offload for a method that would otherwise be eligible
   (`@GpuExclude`).
3. Eagerly compile a class's kernels at class-load time
   (`@EnableGpuAsync(warmup = N)`).

Everything else — the kernel parameter convention, the PTX output, the
no-driver fall-through path, the `--gpu*` CLI flags — is unchanged from
the GPU offload reference in [`README.md`](README.md).

## Quick start

The annotations are bundled with the JVM via the `craton-gpu` crate.
There is nothing to install: any `.class` file produced by `javac`
against a classpath that includes `craton-gpu-annotations.jar` (shipped
in the JVM distribution) carries the annotations through to runtime,
where the offload cache reads them.

```java
import craton.gpu.GpuKernel;

public final class VectorAdd {
    @GpuKernel
    public static void add(int[] a, int[] b, int[] out) {
        int n = a.length;
        for (int i = 0; i < n; i++) {
            out[i] = a[i] + b[i];
        }
    }
}
```

Run with `--gpu --print-gpu-decisions` on a CUDA-equipped box and you
will see:

```
gpu offload: VectorAdd.add([I[I[I)V -> Eligible(...)
```

A default CPU-only build (no `gpu` feature) silently ignores the
annotation and runs the method on the interpreter exactly as before.

## `@GpuKernel`

`@GpuKernel` marks a method as a candidate for GPU offload. On its own
(no parameters) it is equivalent to the analyzer's default behaviour:
the method is eligible iff it passes every analyzer rule.

The parameters let the user override grid shape, block dimensions,
shared-memory budget, and — most importantly — selectively loosen the
analyzer rules they have audited.

### Parameter table

| Parameter | Type | Default | Meaning |
|-----------|------|---------|---------|
| `grid` | `GridShape` | `ELEMENTWISE` | Kernel grid layout. `ELEMENTWISE` matches the analyzer's counted-loop recogniser (one thread per output element). Future shapes (`TILED_2D`, `REDUCTION`) are Phase 2/3. |
| `blockX` | `int` | `0` | Threads per block, X dimension. `0` = JVM picks (typically 256). |
| `blockY` | `int` | `0` | Threads per block, Y dimension. `0` = JVM picks (typically 1 for elementwise). |
| `blockZ` | `int` | `0` | Threads per block, Z dimension. `0` = JVM picks (typically 1 for elementwise). |
| `sharedBytes` | `int` | `0` | Per-block shared memory in bytes. The lowering pass currently emits no shared-memory loads, so this is reserved for `REDUCTION` and `TILED_2D` shapes. |
| `admit` | `AdmissionHint` | `STRICT` | Loosens specific analyzer rejections. See next section. |

The `grid`, `block*`, and `sharedBytes` parameters are *hints* — they
flow into the `LaunchConfig` builder when the kernel is launched. The
`admit` parameter is the only one that affects analyzer verdicts.

## `AdmissionHint` values

| Hint | What it loosens |
|------|-----------------|
| `STRICT` | Nothing (default). The analyzer applies every rule. |
| `ALLOW_ALLOCATION` | Primitive-array `new int[n]` where `n` is derived from a parameter (a `LOAD` of an argument, or `arraylength` of an array argument). The allocation must be the *first* statement of the method and the array must be one of the method's `*astore` targets. No escape analysis. |
| `ALLOW_DIV_BY_ZERO` | Skip the divisor-zero check on `idiv`, `ldiv`, `irem`, `lrem`. The kernel emits no `setp.eq.s32` against zero before the divide. Use only when the caller has already established the divisor is non-zero. |
| `ALLOW_INTRINSIC_CALLS` | Allow `invokestatic` to `java/lang/Math` for the five double overloads: `sqrt(D)D`, `sin(D)D`, `cos(D)D`, `exp(D)D`, `log(D)D`. The emitter rewrites these to the PTX intrinsics `sqrt.rn.f64`, `sin.approx.f64`, `cos.approx.f64`, `ex2.approx.f64` (with a `mul` by `ln 2`), and `lg2.approx.f64` (with a `mul` by `ln 2`) respectively. |

The hints are independent — each one only loosens its own rule. A
method that allocates *and* calls `Math.sqrt` needs
`admit = AdmissionHint.ALLOW_ALLOCATION_AND_INTRINSIC_CALLS` ... no, in
Phase 1 the parameter takes a single enum value. Methods that need
multiple loosenings will be supported via an `EnumSet` parameter in
Phase 2; for Phase 1 a method whose body needs two relaxations is not
expressible as a single `@GpuKernel` annotation and must be split.

### `STRICT` (default)

Identical to the analyzer's default verdict. Use when the method body
is already shaped to fit the analyzer's rules.

```java
@GpuKernel
public static void scale(float[] x, float k) {
    int n = x.length;
    for (int i = 0; i < n; i++) {
        x[i] = x[i] * k;
    }
}
```

### `ALLOW_ALLOCATION`

Loosens `Reason::Allocation` for primitive arrays whose length comes
from a parameter. Useful when the caller does not pre-allocate an
output buffer.

```java
@GpuKernel(admit = AdmissionHint.ALLOW_ALLOCATION)
public static int[] doubled(int[] src) {
    int n = src.length;
    int[] out = new int[n];
    for (int i = 0; i < n; i++) {
        out[i] = src[i] * 2;
    }
    return out;
}
```

The analyzer would normally reject `new int[n]` with
`Reason::Allocation`. With `ALLOW_ALLOCATION` it admits the method
provided the `newarray` opcode targets a primitive type and the size
operand is loadable from a parameter slot. No general escape analysis
runs; arrays allocated for use as scratch space *inside* a loop are
still rejected.

### `ALLOW_DIV_BY_ZERO`

Skips the divisor-zero check on integer division opcodes. The CPU
interpreter throws `ArithmeticException` on `idiv` / `ldiv` /
`irem` / `lrem` with a zero divisor; the GPU kernel cannot raise Java
exceptions. By default the analyzer rejects methods that contain these
opcodes when the divisor is data-dependent. With `ALLOW_DIV_BY_ZERO`
the analyzer admits them and the emitter generates no zero-check.

```java
@GpuKernel(admit = AdmissionHint.ALLOW_DIV_BY_ZERO)
public static void normalise(int[] x, int[] divisor, int[] out) {
    int n = x.length;
    for (int i = 0; i < n; i++) {
        // Caller guarantees divisor[i] != 0.
        out[i] = x[i] / divisor[i];
    }
}
```

A runtime zero divisor in this kernel produces undefined PTX behaviour
(typically a NaN-like sentinel or a 0; depends on the architecture).
Use only when the caller has independently established the invariant.

### `ALLOW_INTRINSIC_CALLS`

Loosens `Reason::Invoke` for the five `java.lang.Math` double
overloads listed above. Other `Math` methods (`abs`, `min`, `max`,
`pow`, `tan`, `floor`, `ceil`, `round`) are *not* admitted in Phase 1
even with this hint — they would each need an emitter case.

```java
@GpuKernel(admit = AdmissionHint.ALLOW_INTRINSIC_CALLS)
public static void rsqrt(double[] x, double[] out) {
    int n = x.length;
    for (int i = 0; i < n; i++) {
        out[i] = 1.0 / Math.sqrt(x[i]);
    }
}
```

The emitter lowers `Math.sqrt(d)` to `sqrt.rn.f64`. Precision matches
PTX defaults: `sqrt.rn` is IEEE-correct; `sin.approx` / `cos.approx` /
`ex2.approx` / `lg2.approx` are *not* (they have an architecture-
dependent ULP bound; consult the PTX ISA notes for the target SM).
Methods that require strict CPU-equivalent precision should not use
this hint.

## `@GpuExclude`

`@GpuExclude` blacklists a method even if every analyzer rule would
otherwise admit it. The cache inserts the method into its permanent
blacklist on first reach and never re-analyzes it.

Useful for "borderline" methods where the offload round-trip cost
exceeds the speedup, or where the user has measured the CPU to be
faster on the workload's typical input size.

```java
public final class MyOps {
    @GpuExclude
    public static void tinyAdd(int[] a, int[] b, int[] out) {
        // Analyzer would admit this, but n is always < 32 in practice
        // and the marshal cost dominates. Keep it on the CPU.
        int n = a.length;
        for (int i = 0; i < n; i++) {
            out[i] = a[i] + b[i];
        }
    }
}
```

`@GpuExclude` wins over `@GpuKernel` if both are present on the same
method — see `ExcludedAndKernel.java` in the test fixtures. The
combined annotations are not an error; they let one declaration site
say "this method is *intended* as a GPU kernel but is exempt today",
which preserves the intent for a future audit.

### Diagnostic output

With `--print-gpu-decisions`, every `@GpuExclude`-blacklisted method
produces one `tracing::debug!` line:

```
gpu.offload  DEBUG  MyOps.tinyAdd([I[I[I)V -> Blacklisted (annotation: @GpuExclude)
```

The `target` field is `gpu.offload` and the level is `debug` so the
line is suppressed by default. Enable it via `RUST_LOG=gpu.offload=debug`
or `--print-gpu-decisions` (which raises the level to `info` for the
GPU subsystem only).

#### Before / after

Without `@GpuExclude` and with `--print-gpu-decisions`:

```
gpu.offload  INFO   MyOps.tinyAdd([I[I[I)V -> Eligible(KernelSignature { ... })
```

With `@GpuExclude`:

```
gpu.offload  DEBUG  MyOps.tinyAdd([I[I[I)V -> Blacklisted (annotation: @GpuExclude)
```

## `@EnableGpuAsync`

`@EnableGpuAsync` is a *class-level* annotation. When the class is
loaded, the offload cache eagerly compiles up to `warmup` methods that
carry `@GpuKernel` (in declaration order). This trades startup cost
for first-call latency: the first invocation of a warm kernel skips
the analyze + lower + load steps.

```java
@EnableGpuAsync(warmup = 2)
public final class Hot {
    @GpuKernel
    public static void add(int[] a, int[] b, int[] out) { /* ... */ }

    @GpuKernel
    public static void mul(int[] a, int[] b, int[] out) { /* ... */ }

    @GpuKernel
    public static void scale(float[] x, float k) { /* ... */ }
    // ^ NOT warmed (only the first 2 are).
}
```

Warmup is best-effort and silent on failure. If the host has no GPU,
or `cuda_bridge::probe` returns `NoDriver`, or analyzer-rejects any of
the marked methods, the affected entries are simply not added to the
cache and the class load proceeds normally. No exception is raised.

### "Async" is misleading today

Phase 1 ships **warmup only**. The name `@EnableGpuAsync` is forward-
looking:

- Phase 2 will add a `streams = N` parameter that lets the JVM run
  multiple kernels on independent CUDA streams (overlapping memcpy and
  launch with CPU work).
- Phase 3 will add `Future`-returning entry points so a caller can
  fire a kernel and continue on the CPU.

Until then, `warmup = N` is the only knob that does anything. Treat
`@EnableGpuAsync` on a class with no `@GpuKernel` methods as a no-op.

## Examples

The Phase 1 test fixtures live under
[`test_classes/gpu/annotations/`](../../test_classes/gpu/annotations/).
Each fixture exercises one annotation or admission hint in isolation:

| Fixture | Demonstrates |
|---------|--------------|
| `StrictKernel.java` | Bare `@GpuKernel` with no parameters; analyzer treats it as `STRICT`. |
| `StrictRejectsAllocation.java` | `@GpuKernel` (STRICT) on a method that allocates → `OffloadVerdict::Ineligible(Reason::Allocation)`. |
| `AdmitAllocation.java` | `@GpuKernel(admit = ALLOW_ALLOCATION)` on the same shape; analyzer admits the `newarray`. |
| `AdmitDivByZero.java` | `@GpuKernel(admit = ALLOW_DIV_BY_ZERO)` on a method whose loop body divides by a data-dependent value. |
| `AdmitMathSqrt.java` | `@GpuKernel(admit = ALLOW_INTRINSIC_CALLS)` calling `Math.sqrt(double)`. |
| `ExcludedKernel.java` | `@GpuExclude` on an analyzer-eligible method. Cache returns `Blacklisted`. |
| `ExcludedAndKernel.java` | Both `@GpuExclude` and `@GpuKernel` on the same method. Exclude wins. |
| `WarmupTwo.java` | `@EnableGpuAsync(warmup = 2)` over a class with three `@GpuKernel` methods; only the first two are warmed. |

Each is a real `.java` source compiled by `javac` via the
[`jit-cuda/build.rs`](../../jit-cuda/build.rs) build script. There are
no hand-rolled bytecode arrays. Refer to the source files for the
exact method bodies.

## Diagnostics

`--print-gpu-decisions` is the single diagnostic flag. It promotes the
`gpu.offload` tracing target from `debug` to `info`, surfacing one log
line per analyzer verdict and one per cache decision:

```
gpu.offload  INFO   AdmitAllocation.doubled([I)[I -> Eligible(...)  (admit=ALLOW_ALLOCATION)
gpu.offload  INFO   StrictRejectsAllocation.doubled([I)[I -> Rejected(Allocation)  (admit=STRICT)
gpu.offload  INFO   ExcludedKernel.add([I[I[I)V -> Blacklisted (annotation: @GpuExclude)
gpu.offload  DEBUG  WarmupTwo: warmup = 2; eagerly compiled 2 of 3 @GpuKernel methods
```

The `target = "gpu.offload"` filter applies to every annotation-driven
decision so the user can ignore the rest of the JVM's logging with a
single `RUST_LOG=gpu.offload=info` or `--print-gpu-decisions`.

## Limitations

Phase 1 is intentionally narrow. The boundaries below are not bugs;
they are deliberate cut points that keep the Phase 1 surface
auditable.

- **`ALLOW_ALLOCATION` is shape-locked.** Only `newarray`/`anewarray`
  of a *primitive* component type with a size operand that traces
  unambiguously back to a parameter (a direct `iload` or a
  `parameter.arraylength`) is admitted. Conditional, computed, or
  loop-internal allocations are rejected with `Reason::Allocation`
  even when the annotation is present. No escape analysis.
- **`ALLOW_DIV_BY_ZERO` does not insert a guard.** A zero divisor at
  runtime gives architecture-defined PTX behaviour. The annotation is
  a *contract*: the caller asserts the divisor is non-zero. There is
  no fallback to the CPU on a runtime zero — the kernel completes
  with whatever value PTX produces.
- **`ALLOW_INTRINSIC_CALLS` covers exactly five methods.** Adding new
  intrinsics requires (a) an emitter case in
  `jit-cuda/src/lowering/emit.rs` and (b) an analyzer admission case
  in `jit-cuda/src/analyzer.rs`. The annotation parameter does not
  unlock arbitrary `Math.*` calls.
- **Warmup is best-effort and silent on missing GPU.** On a no-driver
  box `@EnableGpuAsync` is a no-op. There is no diagnostic unless
  `--print-gpu-decisions` is enabled, in which case one `debug` line
  per warmup attempt is logged.
- **Annotations are advisory.** A `@GpuKernel(admit = STRICT)` on a
  method that fails any *other* analyzer rule (synchronized, reference
  array, switch, throw, field access, type check, monitor) is still
  rejected. The hints loosen exactly four named rules; everything else
  is unchanged.
- **No interaction with the JIT.** The CratonVM JIT does not see these
  annotations. They only affect the GPU offload path.
- **`grid`, `block*`, `sharedBytes` are reserved for Phase 2.** The
  current emitter always uses `LaunchConfig::elementwise(n)`. Setting
  `grid = TILED_2D` or `sharedBytes = 4096` on a Phase 1 build is not
  an error but is ignored.

## Related

- [`README.md`](README.md) — the top-level GPU offload reference.
- [`plan.md`](plan.md) — execution plan and per-part status.
- [`cuda-oxide-evaluation.md`](cuda-oxide-evaluation.md) — why
  cuda-oxide is not on the Phase 1 critical path.
- [`first-results.md`](first-results.md) — acceptance-criteria
  scaffold for the GPU-equipped verification machine.
- [`test_classes/gpu/annotations/`](../../test_classes/gpu/annotations/) —
  Phase 1 annotation fixtures.
- [`vm/tests/annotations_end_to_end.rs`](../../vm/tests/annotations_end_to_end.rs) —
  Phase 1 integration tests that walk every fixture through the
  offload cache.
