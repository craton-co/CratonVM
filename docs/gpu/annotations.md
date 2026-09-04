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

Run with `--gpu --print-gpu-decisions` (the flag is
self-sufficient — no separate `RUST_LOG` needed, see
[Diagnostics](#diagnostics)) on a CUDA-equipped box and you will see:

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
| `grid` | `GridShape` | `ELEMENTWISE` | Kernel grid layout. `ELEMENTWISE` matches the analyzer's counted-loop recogniser (one thread per output element). The parser also recognises `ROW_PER_THREAD` and `BLOCK_REDUCTION` (see `GridShape` in [`jit-cuda/src/annotations.rs`](../../jit-cuda/src/annotations.rs)), but only `ELEMENTWISE` is lowered today — the other two are Phase 2/3. |
| `blockX` | `int` | `0` | Threads per block, X dimension. `0` = JVM picks (typically 256). |
| `blockY` | `int` | `0` | Threads per block, Y dimension. `0` = JVM picks (typically 1 for elementwise). |
| `blockZ` | `int` | `0` | Threads per block, Z dimension. `0` = JVM picks (typically 1 for elementwise). |
| `sharedBytes` | `int` | `0` | Per-block shared memory in bytes. The lowering pass currently emits no shared-memory loads, so this is reserved for the (not-yet-lowered) `BLOCK_REDUCTION` and `ROW_PER_THREAD` shapes. |
| `admit` | `AdmissionHint` | `STRICT` | Loosens specific analyzer rejections. See next section. |

The `grid`, `block*`, and `sharedBytes` parameters are *hints* — they
flow into the `LaunchConfig` builder when the kernel is launched. The
`admit` parameter is the only one that affects analyzer verdicts.

## `AdmissionHint` values

| Hint | What it loosens |
|------|-----------------|
| `STRICT` | Nothing (default). The analyzer applies every rule. |
| `ALLOW_ALLOCATION` | Primitive-array `new int[n]` where `n` is derived from a parameter (a `LOAD` of an argument, or `arraylength` of an array argument). The allocation must be the *first* statement of the method and the array must be one of the method's `*astore` targets. No escape analysis. |
| `ALLOW_DIV_BY_ZERO` | Skip the divisor-zero check on `idiv`, `ldiv`, `irem`, `lrem`. The kernel emits no `setp.eq.s32` against zero before the divide. Use only when the caller has already established the divisor is non-zero. **this hint is also reused to gate `frem`/`drem`** (see below) — same "the caller has already established a safe input" contract, applied to IEEE remainder instead of integer division. |
| `ALLOW_INTRINSIC_CALLS` | Loosens the analyzer's rejection of `invokestatic`, and — now — actually resolves and lowers the call. See [`ALLOW_INTRINSIC_CALLS`](#allow_intrinsic_calls) below for the curated table of what's supported. |

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

#### `frem` / `drem` reuse

`ALLOW_DIV_BY_ZERO` also gates `frem` (0x72) and `drem` (0x73) — Java's IEEE
remainder opcodes. In `Strict` mode both still reject unconditionally, same as
before. With `ALLOW_DIV_BY_ZERO` set, the analyzer admits them and the emitter
lowers to a real PTX sequence (`Emitter::frem_f32` / `drem_f64`):

```java
@GpuKernel(admit = AdmissionHint.ALLOW_DIV_BY_ZERO)
public static void wrap(float[] x, float[] period, float[] out) {
    int n = x.length;
    for (int i = 0; i < n; i++) {
        out[i] = x[i] % period[i];
    }
}
```

The hint's name is about integer division by zero, but the underlying
contract it expresses — "the caller has audited this method's numeric edge
cases and accepts the GPU's behavior" — applies just as well to floating
remainder, so it was reused rather than adding a fifth hint for one opcode
pair. The lowering is **exact only for bounded quotients**: like PTX
floating-point division generally, `frem`/`drem` on values whose quotient
would be extremely large loses precision relative to Java's arbitrary-precision
IEEE remainder algorithm. Use it for the common case (period-wrapping,
angle-normalization) where inputs stay in a sane range; do not assume
bit-exact agreement with the CPU interpreter across all possible inputs.

### `ALLOW_INTRINSIC_CALLS`

**Current status (verified against
`jit-cuda/src/analyzer.rs` and `jit-cuda/src/lowering/emit.rs`): this hint is
implemented.** The PHASE1-GUESS gap described in earlier drafts of this
section — the analyzer admitting *any* `invokestatic` without resolving the
constant-pool target, and the emitter having no lowering for `invokestatic`
(`0xB8`) at all — is closed. What happens today:

1. The analyzer's `classify_invokestatic` resolves the constant-pool target
   of every `invokestatic` seen under `AllowIntrinsicCalls` and looks it up in
   a curated table, `analyzer::resolve_math_intrinsic`. A hit is admitted; a
   miss rejects with `Reason::Invoke`, exactly like `Strict` mode. Resolution
   requires a constant pool — the `analyze_with_pool` /
   `analyze_with_annotations_and_pool` entry points; the CP-free `analyze` /
   `analyze_with_annotations` entry points still reject every `invokestatic`
   unconditionally, same as before.
2. `jit-cuda/src/lowering/emit.rs` has a real case for `0xB8` that dispatches
   on the same `resolve_math_intrinsic` table and emits a matching PTX
   sequence for each entry.
3. A method that resolves to something *outside* the table (`sin`, `cos`,
   `exp`, `log`, `pow`, any non-`Math`/`StrictMath` class, or anything seen
   through a CP-free entry point) is still rejected at the analyzer, not
   admitted-then-failed-at-lowering — there is no more silent
   analyzer-says-yes / lowering-says-no split for the covered opcode.

#### Supported table

| Java method | PTX intrinsic | Notes |
|---|---|---|
| `Math.sqrt(double)` / `StrictMath.sqrt(double)` | `sqrt.rn.f64` | Round-to-nearest, matches Java's `sqrt` contract. No `float` overload is in the table — `Math.sqrt(float)` does not exist in the JDK (the JDK API is `double`-only), so there is nothing to add. |
| `Math.abs(int)` / `StrictMath.abs(int)` | `abs.s32` open-coded | Bit-exact including `Integer.MIN_VALUE` (Java's documented "still negative" corner case). |
| `Math.abs(long)` / `StrictMath.abs(long)` | `abs.s64` open-coded | 64-bit twin of the above; same `Long.MIN_VALUE` handling. |
| `Math.abs(float)` / `StrictMath.abs(float)` | `abs.f32` | |
| `Math.abs(double)` / `StrictMath.abs(double)` | `abs.f64` | |
| `Math.min(int,int)` / `Math.max(int,int)` (+ `StrictMath`) | open-coded `setp`/`selp` | |
| `Math.min(long,long)` / `Math.max(long,long)` (+ `StrictMath`) | open-coded `setp`/`selp` | 64-bit twin. |
| `Math.min(float,float)` / `Math.max(float,float)` (+ `StrictMath`) | open-coded `setp`/`selp` | NaN- and signed-zero-correct per Java's `Math.min`/`max` spec (not plain IEEE `min`/`max`, which disagree with Java on `-0.0`/`+0.0` and NaN propagation). |
| `Math.min(double,double)` / `Math.max(double,double)` (+ `StrictMath`) | open-coded `setp`/`selp` | Double twin of the above, same NaN/signed-zero correctness. |
| `Math.fma(float,float,float)` / `StrictMath.fma(float,float,float)` | fused multiply-add | Single rounding, per Java's `fma` contract. |
| `Math.fma(double,double,double)` / `StrictMath.fma(double,double,double)` | fused multiply-add | Double twin. |

`StrictMath` and `Math` both resolve to the same table entry wherever the JDK
defines both — the table doesn't distinguish them because CratonVM's GPU
lowering has one deterministic implementation per operation, not the
separate `Math`-may-use-hardware-intrinsics-but-`StrictMath`-may-not
distinction HotSpot draws on the CPU.

**Deliberately excluded:** `sin`, `cos`, `exp`, `log`, `pow`, and every other
`Math`/`StrictMath` method not listed above. These have no PTX lowering and a
call to one under `ALLOW_INTRINSIC_CALLS` still rejects with `Reason::Invoke`.
The exclusion is a correctness call, not a missing-feature gap: PTX only
offers `.approx` transcendentals (`sin.approx.f32`, `lg2.approx.f32`, etc.),
which trade precision for throughput in a way the JDK's `Math` contract (and
especially `StrictMath`'s stricter one) does not permit. Landing these would
require either accepting a documented precision deviation from Java semantics
or a much more expensive software implementation — neither was in scope for
this pass.

```java
@GpuKernel(admit = AdmissionHint.ALLOW_INTRINSIC_CALLS)
public static void rootArray(double[] src, double[] dst) {
    int n = src.length;
    for (int i = 0; i < n; i++) {
        dst[i] = Math.sqrt(src[i]);
    }
}
```

(This is the `AdmitMathSqrt.java` fixture under
`test_classes/gpu/annotations/`. `jit-cuda/src/lowering.rs`'s
`ptxas_round_trip_math_intrinsics` test round-trips a kernel using this table
through `ptxas` as part of the validation pass, alongside
five other lowering shapes — the reduction epilogue's `ptxas` rejection
(`atom.global.add`'s 2-operand form) is what motivated adding these
round-trip tests in the first place; see [`reductions.md`](reductions.md).)

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

Every `@GpuExclude`-blacklisted method produces one `tracing::debug!`
record, **unconditionally** — this one is not gated by
`--print-gpu-decisions` at all (`vm/src/runtime/offload.rs`,
`lookup_or_compile`'s `gpu_exclude` short-circuit). It carries
`target: "gpu.offload"` and structured fields, not a pre-formatted
message:

```
DEBUG gpu.offload: blacklisted by @GpuExclude class="MyOps" method=3 reason=""
```

(`method` is the numeric method-table index, not a name+descriptor
string; `reason` is whatever string, if any, you passed to
`@GpuExclude(reason = "...")`.) The level is `info`, so it is
suppressed by CratonVM's default tracing filter (`WARN` and above —
see `vm-cli/src/main.rs`). Enable it with `RUST_LOG=gpu.offload=info`
(or any broader directive like `RUST_LOG=info`). `--print-gpu-decisions`
has no effect on this particular line.

> **Changed 2026-09-03.** This line used to be emitted at `debug`, and
> this section used to tell you to set `RUST_LOG=gpu.offload=debug`.
> That could never have worked on a release build: the workspace pins
> `tracing` with `release_max_level_info`, so `debug!` and `trace!`
> expand to no-ops and no `RUST_LOG` value can bring them back. The
> line was not being filtered — it did not exist. It is `info!` now,
> which the filter can actually reach.

#### Before / after

Without `@GpuExclude`, running with `--print-gpu-decisions` alone (no separate `RUST_LOG` is needed — see
[Diagnostics](#diagnostics) below):

```
INFO cratonvm_vm::runtime::offload: gpu offload: MyOps.tinyAdd([I[I[I)V -> Eligible(KernelSignature { ... })
```

With `@GpuExclude`, running with `RUST_LOG=gpu.offload=info` (the
flag above is irrelevant to this line; it fires either way):

```
DEBUG gpu.offload: blacklisted by @GpuExclude class="MyOps" method=3 reason=""
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
| `AdmitMathSqrt.java` | `@GpuKernel(admit = ALLOW_INTRINSIC_CALLS)` calling `Math.sqrt(double)`. Analyzer-eligible and lowered — see [`ALLOW_INTRINSIC_CALLS`](#allow_intrinsic_calls) above for the full curated table. |
| `ExcludedKernel.java` | `@GpuExclude` on an analyzer-eligible method. Cache returns `Blacklisted`. |
| `ExcludedAndKernel.java` | Both `@GpuExclude` and `@GpuKernel` on the same method. Exclude wins. |
| `WarmupTwo.java` | `@EnableGpuAsync(warmup = 2)` over a class with three `@GpuKernel` methods; only the first two are warmed. |

Each is a real `.java` source compiled by `javac` via the
[`jit-cuda/build.rs`](../../jit-cuda/build.rs) build script. There are
no hand-rolled bytecode arrays. Refer to the source files for the
exact method bodies.

## Diagnostics

`--print-gpu-decisions` used to be a silent
no-op unless you separately exported `RUST_LOG=info` — see the "before"
behavior below, which earlier drafts of this doc documented as the permanent
state of the world. It no longer is: `vm-cli/src/main.rs` now has the flag add
its own `cratonvm_vm::runtime::offload=debug` directive to the tracing
`EnvFilter` at startup, so passing just `--print-gpu-decisions` is enough on
its own:

```
cratonvm --gpu --print-gpu-decisions -cp ... MyOps
```

```
INFO cratonvm_vm::runtime::offload: gpu offload: AdmitAllocation.doubled([I)[I -> Eligible(...)
INFO cratonvm_vm::runtime::offload: gpu offload: StrictRejectsAllocation.doubled([I)[I -> Rejected(Allocation)
```

This directive is scoped to exactly the `cratonvm_vm::runtime::offload`
module-path target, so it also makes `@EnableGpuAsync` warmup's summary line
(same target — see below) visible standalone, with no `RUST_LOG` needed. It
does **not** touch the separate `target: "gpu.offload"` tracing target (item 2
below), which is a different mechanism and still needs its own `RUST_LOG`.
`RUST_LOG=info` still works too, and still composes normally with the flag —
nothing above changes how `RUST_LOG` itself behaves, only what
`--print-gpu-decisions` does on top of it.

1. **The `--print-gpu-decisions` flag.** Gates a plain `tracing::info!(...)`
   call in `OffloadCache::lookup_or_compile` that logs one line per analyzer
   verdict (`Eligible(...)` / `Rejected(...)`) under the module's default
   tracing target (`cratonvm_vm::runtime::offload`), *not* `gpu.offload`. The flag itself is sufficient to see these lines —
   no separate `RUST_LOG` required (see above).
2. **The `target: "gpu.offload"` tracing target.** Separate call
   sites (currently: the `@GpuExclude` blacklist record) tag their
   `tracing::debug!` call with `target: "gpu.offload"` and fire
   unconditionally — independent of `--print-gpu-decisions`, and **not**
   covered by the flag's new self-sufficiency (different target string).
   Visibility is still controlled purely by `RUST_LOG`, e.g.
   `RUST_LOG=gpu.offload=info`.

To see everything this page describes at once (including the `gpu.offload`
target), use `RUST_LOG=gpu.offload=info --print-gpu-decisions` — the flag
covers item 1 and warmup below on its own; `RUST_LOG=gpu.offload=info` is
still needed for item 2.

`@EnableGpuAsync` warmup (`OffloadCache::warmup_class`) is a *third*,
separate log source: it always calls `tracing::info!` — once per class
load — under the same default target as item 1
(`cratonvm_vm::runtime::offload`), not `gpu.offload`. The real message shape
is a per-class summary, not a per-method line:

```
INFO cratonvm_vm::runtime::offload: gpu warmup: Hot -> 2/2 eligible methods compiled (considered 3 of 3)
```

or, on a no-device host:

```
INFO cratonvm_vm::runtime::offload: gpu warmup: Hot requested but no device available; skipping
```

Because this shares item 1's target, `--print-gpu-decisions` alone now
surfaces it too; `RUST_LOG=info` (or broader) still works as an alternative.
The `WarmupTwo: warmup = 2; eagerly compiled 2 of 3 @GpuKernel methods` line
shown in even earlier drafts of this page never matched the real message
text.

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
- **`ALLOW_INTRINSIC_CALLS` is now implemented, but the table is curated
  and closed, not "any `Math` method."** The
  analyzer resolves the constant-pool callee via
  `analyzer::resolve_math_intrinsic` and only admits (then lowers) an exact
  hit against the table in [`ALLOW_INTRINSIC_CALLS`](#allow_intrinsic_calls)
  above (`sqrt`(double)/`abs`/`min`/`max`(int/long/float/double)/`fma`(float/double)).
  `sin`/`cos`/`exp`/`log`/`pow` and any non-`Math`/`StrictMath` call still
  reject with `Reason::Invoke` — deliberately: PTX only offers `.approx`
  transcendentals, which would silently trade away precision Java's `Math`/
  `StrictMath` contracts promise. Resolution needs a constant pool
  (`analyze_with_pool` / `analyze_with_annotations_and_pool`); the two
  CP-free entry points still reject every `invokestatic` unconditionally.
- **Warmup is best-effort on missing GPU, but not silent in the logs.**
  On a no-driver box `@EnableGpuAsync` is a functional no-op (no
  exception, class load proceeds), but `warmup_class` always emits an
  unconditional `tracing::info!` line either way (a per-class summary,
  or a "no device available; skipping" line) — visible with either
  `RUST_LOG=info` or, `--print-gpu-decisions`
  alone (shares item 1's tracing target). See [Diagnostics](#diagnostics).
- **Annotations are advisory.** A `@GpuKernel(admit = STRICT)` on a
  method that fails any *other* analyzer rule (synchronized, reference
  array, switch, throw, field access, type check, monitor) is still
  rejected. The hints loosen exactly four named rules; everything else
  is unchanged.
- **No interaction with the JIT.** The CratonVM JIT does not see these
  annotations. They only affect the GPU offload path.
- **`grid`, `block*`, `sharedBytes` are reserved for Phase 2.** The
  current emitter always uses `LaunchConfig::elementwise(n)`. Setting
  `grid = ROW_PER_THREAD` (or `BLOCK_REDUCTION`) or `sharedBytes = 4096`
  on a Phase 1 build is not an error but is ignored.

## Related

- [`README.md`](README.md) — the top-level GPU offload reference.
- [`cuda-oxide-evaluation.md`](cuda-oxide-evaluation.md) — why
  cuda-oxide is not on the critical path.
- [`first-results.md`](first-results.md) — acceptance-criteria
  scaffold for the GPU-equipped verification machine.
- [`test_classes/gpu/annotations/`](../../test_classes/gpu/annotations/) —
  Phase 1 annotation fixtures.
- [`vm/tests/annotations_end_to_end.rs`](../../vm/tests/annotations_end_to_end.rs) —
  Phase 1 integration tests that walk every fixture through the
  offload cache.
