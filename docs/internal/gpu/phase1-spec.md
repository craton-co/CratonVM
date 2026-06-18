# Phase 1 implementation spec — annotation-aware GPU offload

This document is the **single source of truth** for the Phase 1
implementation of the `craton.gpu.*` Java library. It is referenced by
each parallel implementation agent. **Read sections 1 and 2 first.**

## 1. Scope

Phase 1 adds *declarative* control over the existing GPU offload
pipeline. It does **not** add async, streams, or futures. Those are
Phase 2/3.

The user-visible deliverable: Java programs can put `@GpuKernel`,
`@GpuExclude`, and `@EnableGpuAsync` on methods/classes to influence
the analyzer's decisions about which methods get offloaded and how.

**Default behaviour is unchanged.** A method with no annotations is
analyzed exactly as today.

## 2. Cross-cutting design contracts (every agent reads this)

These names, signatures, and shapes are **fixed**. Do not deviate.

### 2.1 Java surface — package `craton.gpu`

All classes live under `craton-gpu/src/main/java/craton/gpu/` in the
new `craton-gpu` Cargo crate.

```java
// GridShape.java
package craton.gpu;
public enum GridShape { ELEMENTWISE, ROW_PER_THREAD, BLOCK_REDUCTION }
```

```java
// AdmissionHint.java
package craton.gpu;
public enum AdmissionHint {
    STRICT,
    ALLOW_ALLOCATION,
    ALLOW_DIV_BY_ZERO,
    ALLOW_INTRINSIC_CALLS
}
```

```java
// GpuKernel.java
package craton.gpu;
import java.lang.annotation.*;
@Retention(RetentionPolicy.CLASS)
@Target(ElementType.METHOD)
public @interface GpuKernel {
    GridShape grid()        default GridShape.ELEMENTWISE;
    int       blockX()      default 0;
    int       blockY()      default 0;
    int       blockZ()      default 0;
    int       sharedBytes() default 0;
    AdmissionHint admit()   default AdmissionHint.STRICT;
}
```

```java
// GpuExclude.java
package craton.gpu;
import java.lang.annotation.*;
@Retention(RetentionPolicy.CLASS)
@Target(ElementType.METHOD)
public @interface GpuExclude {
    String reason() default "";
}
```

```java
// EnableGpuAsync.java
package craton.gpu;
import java.lang.annotation.*;
@Retention(RetentionPolicy.CLASS)
@Target(ElementType.TYPE)
public @interface EnableGpuAsync {
    int warmup() default 0;
}
```

**RetentionPolicy.CLASS is mandatory.** The JVM analyzer reads
classfile attribute tables directly; we do NOT want runtime
reflection retention.

### 2.2 Classfile attribute layout

Each annotated method gets a `RuntimeInvisibleAnnotations` attribute
in its `method_info`. Each annotated class gets one in its
`ClassFile`. The agent in charge of parsing (Item 3) consumes these
exactly per JVMS §4.7.16.

Specifically the analyzer needs to recognize annotation type
descriptors:
- `Lcraton/gpu/GpuKernel;`
- `Lcraton/gpu/GpuExclude;`
- `Lcraton/gpu/EnableGpuAsync;`

Enum-valued elements (`grid`, `admit`) use the `e` element_value tag
and produce a `(Lcraton/gpu/GridShape;, ELEMENTWISE)` pair, etc.

### 2.3 Rust surface — `jit_cuda::annotations`

A new module `jit-cuda/src/annotations.rs`, re-exported from
`jit-cuda/src/lib.rs`.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GridShape {
    #[default]
    Elementwise,
    RowPerThread,
    BlockReduction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AdmissionHint {
    #[default]
    Strict,
    AllowAllocation,
    AllowDivByZero,
    AllowIntrinsicCalls,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuKernelAttrs {
    pub grid: GridShape,
    pub block_x: u32,
    pub block_y: u32,
    pub block_z: u32,
    pub shared_bytes: u32,
    pub admit: AdmissionHint,
}

impl Default for GpuKernelAttrs {
    fn default() -> Self {
        Self {
            grid: GridShape::Elementwise,
            block_x: 0,
            block_y: 0,
            block_z: 0,
            shared_bytes: 0,
            admit: AdmissionHint::Strict,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GpuExcludeAttrs {
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MethodAnnotations {
    pub gpu_kernel: Option<GpuKernelAttrs>,
    pub gpu_exclude: Option<GpuExcludeAttrs>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClassAnnotations {
    pub enable_async: Option<EnableAsyncAttrs>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EnableAsyncAttrs {
    pub warmup: u32,
}

/// Parse method-level annotations from RuntimeInvisibleAnnotations.
/// Receives the raw bytes of the method's attribute area + the
/// classfile's constant pool. Returns Default on any I/O error
/// (annotations are advisory; never panic over parsing).
pub fn read_method_annotations(
    method_attributes: &[Attribute],
    cp: &ConstantPool,
) -> MethodAnnotations { ... }

/// Same for class-level annotations.
pub fn read_class_annotations(
    class_attributes: &[Attribute],
    cp: &ConstantPool,
) -> ClassAnnotations { ... }
```

Use the existing `classfile::*` types from `cratonvm-classfile` (the
crate already in the workspace). Use `Attribute` and `ConstantPool`
from there.

### 2.4 Hint behaviour

The existing `jit_cuda::analyzer::classify` rejects methods for
reasons enumerated by `Reason`. With hints, the **specific** rejections
to drop are:

| Hint variant | Reasons dropped (others remain firm) |
| --- | --- |
| `Strict` | none (default) |
| `AllowAllocation` | `Reason::Allocation` for `new <primitive_array>` of known size |
| `AllowDivByZero` | skip the implicit divisor-zero check on `idiv` / `ldiv` |
| `AllowIntrinsicCalls` | `Reason::Invoke` for calls to `java/lang/Math::{sqrt(D)D, sin(D)D, cos(D)D, exp(D)D, log(D)D}` |

`AllowAllocation` is **not** "any new" — only primitive arrays of
size known from a parameter. Object allocation still rejects. This
constraint applies because we have to lower the allocation as a
device-side output buffer, which only works for sized primitive
arrays.

`@GpuExclude` overrides everything: even an annotated `@GpuKernel`
method blacklists if `@GpuExclude` is also present. Order of
precedence: `GpuExclude` > `GpuKernel.admit` > default analyzer.

### 2.5 New top-level analyzer entry point

```rust
// jit-cuda/src/analyzer.rs (additions, not replacements)
pub fn analyze_with_annotations(
    method: &MethodHandle,
    method_annotations: &MethodAnnotations,
) -> OffloadVerdict { ... }

// Backwards compatible: existing call sites work unchanged.
pub fn analyze(method: &MethodHandle) -> OffloadVerdict {
    analyze_with_annotations(method, &MethodAnnotations::default())
}
```

### 2.6 OffloadCache hook

`vm/src/runtime/offload.rs` — `OffloadCache::lookup_or_compile` reads
both method-level and class-level annotations before invoking the
analyzer. When `gpu_exclude.is_some()` it returns
`LookupOutcome::Blacklisted` immediately and **does not** consult the
analyzer.

```rust
// Pseudocode for the addition; do not deviate from the structure.
let class_annotations = read_class_annotations(class.attributes(), class.cp());
let method_annotations = read_method_annotations(method.attributes(), class.cp());

if let Some(exclude) = &method_annotations.gpu_exclude {
    tracing::debug!(reason = %exclude.reason, "GpuExclude blacklist");
    self.blacklist.insert((class_id, method_index));
    return LookupOutcome::Blacklisted;
}

let verdict = analyze_with_annotations(method, &method_annotations);
// ... existing lowering path ...
```

### 2.7 Class-load warmup hook

`EnableGpuAsync(warmup = N)` on a class triggers eager compilation of
up to the first N methods annotated with `@GpuKernel`. The hook
lives in `vm/src/vm/class_load.rs` (or `class_loader.rs` — agent
should locate the right place):

```rust
#[cfg(feature = "gpu-offload")]
pub(crate) fn maybe_warmup_gpu(shared: &SharedVm, class: &ClassFile, class_id: ClassId) {
    if !shared.config.gpu_offload_enabled { return; }
    let class_annotations = jit_cuda::annotations::read_class_annotations(
        class.attributes(), class.cp(),
    );
    let Some(enable) = class_annotations.enable_async else { return; };
    if enable.warmup == 0 { return; }
    shared.offload_cache.warmup_class(class, class_id, enable.warmup as usize);
}
```

And `OffloadCache::warmup_class(class, class_id, max)`:
- iterates methods in class order
- for each method whose annotations include `gpu_kernel`
- calls `lookup_or_compile` to populate the cache
- stops after `max` successful compiles

### 2.8 Where the new crate lives

Workspace `Cargo.toml`: add `craton-gpu` to `members`.

`craton-gpu/Cargo.toml`:
```toml
[package]
name = "craton-gpu"
version = "0.2.0"
edition = "2021"
build = "build.rs"

[lib]
path = "src/lib.rs"

[build-dependencies]
# Add only if absolutely needed.
```

`craton-gpu/src/lib.rs`: a stub that exposes the compiled jar path
via a constant set by build.rs (using `OUT_DIR` / `env!`).

`craton-gpu/build.rs`:
- Finds `javac` on `PATH`.
- Compiles every `.java` under `craton-gpu/src/main/java/` to
  `${OUT_DIR}/classes/`.
- Packages them into `${OUT_DIR}/craton-gpu-annotations.jar` using
  `jar` if available, or just leaves classes loose.
- Emits `cargo:rustc-env=CRATON_GPU_ANNOTATIONS_JAR=...` and
  `cargo:rustc-env=CRATON_GPU_ANNOTATIONS_DIR=...`.
- If `javac` is missing: emit `cargo:warning=javac not found; skipping`
  and create an empty marker file. Don't fail the build.

### 2.9 Test fixture locations

All fixtures go under `test_classes/gpu/annotations/`. Examples
needed:

**Positive (hint admits a method that would otherwise reject):**
- `AdmitAllocation.java` — uses `@GpuKernel(admit = ALLOW_ALLOCATION)`, allocates an output `int[]` from a parameter-known size.
- `AdmitMathSqrt.java` — uses `@GpuKernel(admit = ALLOW_INTRINSIC_CALLS)`, calls `Math.sqrt`.
- `AdmitDivByZero.java` — uses `@GpuKernel(admit = ALLOW_DIV_BY_ZERO)`, has `a[i] / b[i]` without explicit zero-check.

**Negative (annotation forces rejection):**
- `ExcludedKernel.java` — has `@GpuExclude(reason = "branchy")`. Should NOT offload even though the body is eligible.
- `ExcludedAndKernel.java` — has BOTH `@GpuKernel` AND `@GpuExclude`. Exclude wins.

**Defaults (strict mode, baseline behavior):**
- `StrictKernel.java` — `@GpuKernel` with all defaults. Behaves like today's analyzer.
- `StrictRejectsAllocation.java` — `@GpuKernel(admit = STRICT)`, allocates → should still reject.

**Warmup:**
- `WarmupTwo.java` — `@EnableGpuAsync(warmup = 2)` with three `@GpuKernel` methods. After class load, exactly two should be in the cache.

### 2.10 Annotation behavior matrix (canonical reference)

```
Method has         | Body  | Verdict
                   | OK?   |
------------------+-------+-----------------------------------
no annotations    | yes   | Eligible (existing path)
no annotations    | no    | Ineligible (existing analyzer reason)
@GpuKernel        | yes   | Eligible (same as above)
@GpuKernel STRICT | no    | Ineligible (same as above)
@GpuKernel ALLOW_*| was-no| Eligible IF the rejection reason
                          matches the loosened hint; else Ineligible
@GpuExclude       | any   | Blacklisted (LookupOutcome::Blacklisted)
@GpuKernel +      |       |
  @GpuExclude     | any   | Blacklisted (exclude wins)
```

### 2.11 Documentation

`docs/gpu/annotations.md` — user-facing. Sections:
- Quick start
- `@GpuKernel` — table of all parameters, defaults, semantics
- `@GpuExclude` — when to use
- `@EnableGpuAsync` — what `warmup` does, what it doesn't do (no
  async yet; that's Phase 3)
- Examples — for each fixture type above
- Diagnostics — using `--print-gpu-decisions` to see annotation
  effects
- Limitations — what `ALLOW_ALLOCATION` can and can't do, etc.

## 3. Item assignments (one per agent)

Each agent works in a separate worktree. Agents only **write code**
— they do NOT run `cargo build`, `cargo test`, `cargo check`, or any
compiler-driven verification. The orchestrator handles all builds.

### Item 1 — Java annotation sources
**Files to create:**
- `craton-gpu/src/main/java/craton/gpu/GridShape.java`
- `craton-gpu/src/main/java/craton/gpu/AdmissionHint.java`
- `craton-gpu/src/main/java/craton/gpu/GpuKernel.java`
- `craton-gpu/src/main/java/craton/gpu/GpuExclude.java`
- `craton-gpu/src/main/java/craton/gpu/EnableGpuAsync.java`

Use the exact source shown in §2.1. Include a one-line Javadoc
comment per file describing what the annotation/enum does.

### Item 2 — craton-gpu Cargo crate + javac build script
**Files to create:**
- `craton-gpu/Cargo.toml` (per §2.8)
- `craton-gpu/build.rs` (per §2.8 — call `javac`, package jar,
  emit env vars)
- `craton-gpu/src/lib.rs` — stub exposing:
  ```rust
  pub const ANNOTATIONS_JAR: &str = env!("CRATON_GPU_ANNOTATIONS_JAR");
  pub const ANNOTATIONS_DIR: &str = env!("CRATON_GPU_ANNOTATIONS_DIR");
  ```

**File to modify:**
- Root `Cargo.toml` — add `"craton-gpu"` to `[workspace] members`.

### Item 3 — Annotation reader in jit-cuda
**Files to create:**
- `jit-cuda/src/annotations.rs` — types per §2.3 and the two parsing
  functions `read_method_annotations` / `read_class_annotations`.

**Files to modify:**
- `jit-cuda/src/lib.rs` — `pub mod annotations;` and re-exports.

**Dependency to add (if needed):**
- `jit-cuda/Cargo.toml`: add `cratonvm-classfile` dependency on the
  classfile crate already in the workspace. Look at how
  `jit-cuda` already loads methods to confirm correct dep name.

### Item 4 — Wire AdmissionHint into analyzer
**File to modify:**
- `jit-cuda/src/analyzer.rs` — add `analyze_with_annotations` per
  §2.5, modify `classify` to accept `&MethodAnnotations` and
  loosen reasons per §2.4. The existing `analyze` becomes a
  thin wrapper.

Do NOT change the existing tests' expected output for methods
without annotations.

### Item 5 — `@GpuExclude` → Blacklisted in offload.rs
**File to modify:**
- `vm/src/runtime/offload.rs` — `OffloadCache::lookup_or_compile`
  picks up method annotations via the new
  `jit_cuda::annotations::read_method_annotations` and short-circuits
  when `gpu_exclude.is_some()`. Per §2.6.

Also: feed `method_annotations` into the analyzer call:
```rust
let verdict = jit_cuda::analyzer::analyze_with_annotations(
    method, &method_annotations);
```

### Item 6 — `@EnableGpuAsync` warmup at class load
**Files to modify:**
- `vm/src/vm/class_load.rs` (or wherever class loading is) — call
  `maybe_warmup_gpu(...)` per §2.7 once a class is loaded, behind
  `#[cfg(feature = "gpu-offload")]`.

**Files to add:**
- `vm/src/runtime/offload.rs::OffloadCache::warmup_class` per §2.7.

If you cannot find the right class-load callsite, locate it by
grepping for where ClassFile finishes loading. Hint: look for
`class_manager` and where it registers a newly loaded class.

### Item 7 — Test fixtures (positive cases — hints admit)
**Files to create under `test_classes/gpu/annotations/`:**
- `AdmitAllocation.java`
- `AdmitMathSqrt.java`
- `AdmitDivByZero.java`
- `StrictKernel.java`
- `StrictRejectsAllocation.java`

Each file is a small Java class with one or two static methods.
Annotate methods per §2.9.

Update `jit-cuda/build.rs` to compile this new subdirectory too,
since these fixtures depend on the `craton.gpu` annotation
package — use the env vars exported by `craton-gpu/build.rs`
(see §2.8) as the `-cp` for javac.

### Item 8 — Test fixtures (exclude + warmup cases)
**Files to create under `test_classes/gpu/annotations/`:**
- `ExcludedKernel.java`
- `ExcludedAndKernel.java`
- `WarmupTwo.java`

Same javac/classpath constraint as Item 7. Coordinate with Item 7
on `jit-cuda/build.rs` updates — last writer wins, so structure your
edit to be additive.

### Item 9 — `docs/gpu/annotations.md` + integration test scaffold
**Files to create:**
- `docs/gpu/annotations.md` — per §2.11.
- `vm/tests/annotations_end_to_end.rs` — feature-gated integration
  test that:
  1. Creates a `SharedVm` with `gpu_offload_enabled = true`.
  2. Loads each `test_classes/gpu/annotations/*.class` fixture.
  3. Asserts the `OffloadCache::lookup_or_compile` outcome matches
     the expectation table in §2.10.

  This test will not pass on the dev box if the fixtures aren't
  yet compiled (Item 7 / 8). That's OK; the orchestrator handles
  build ordering.

**Files to modify:**
- `docs/gpu/README.md` — add a one-line link to `annotations.md`
  under the "Document map" section.

## 4. Constraints binding all agents

1. **Do not run cargo / javac / jar / any compiler.** Write source
   only. The orchestrator runs all builds.
2. **Do not commit.** The orchestrator commits and merges.
3. **Do not push.** Never.
4. **Match the names in this spec exactly.** Do not rename a
   `GpuKernel` to `GpuOffload`, do not rename `AdmissionHint::Strict`
   to `Mode::Default`, do not invent new variants.
5. **Stay in scope.** If a task you've been given touches a file
   another agent owns, do the minimum on the shared file; the
   orchestrator may need to resolve a merge conflict.
6. **Read this spec start-to-finish before writing.** Sections 2.1,
   2.2, 2.3 are most likely to matter to you.
7. **No new dependencies without justification.** Every `cudarc`,
   `cuda-bridge`, `bytemuck` already in the workspace is reusable.
   `serde` etc. are NOT.
8. **If you must guess, document the guess in a comment so the
   orchestrator sees it.** Prefix with `// PHASE1-GUESS:`.

## 5. Acceptance for Phase 1 (orchestrator-side)

Phase 1 is complete when:

- [ ] `cargo check --workspace` clean
- [ ] `cargo check --workspace --features cratonvm-vm/gpu-offload` clean
- [ ] `cargo test -p jit-cuda` passes existing + new tests for hint behavior
- [ ] `cargo test -p cratonvm-vm --features gpu-offload --lib offload` passes existing + new exclude/warmup tests
- [ ] `cargo test -p cratonvm-vm --features gpu-offload --test annotations_end_to_end` passes
- [ ] The new `craton-gpu-annotations.jar` is reproducibly built by `cargo build -p craton-gpu`
- [ ] `docs/gpu/annotations.md` exists and is linked from `docs/gpu/README.md`

The orchestrator iterates: merge agent branches → build → diagnose
errors → dispatch fix agents → re-build, until acceptance is met.
