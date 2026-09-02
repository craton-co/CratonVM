// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Decides whether a Java static method is GPU-eligible.
//!
//! The first cut targets a deliberately narrow class of methods:
//!
//! - Static.
//! - Not synchronized, not native, not abstract, has a Code attribute.
//! - All parameters are primitive scalars or primitive arrays (no
//!   reference-typed parameters, no `Integer[]`).
//! - The return type is a primitive scalar or primitive array, or void.
//! - The bytecode contains no allocation, no method calls, no field
//!   access, no type checks, no monitor ops, no switch tables, no
//!   throw, no exception-handling ranges, no `jsr`/`ret`.
//!
//! Methods that pass become `Eligible(KernelSignature)`. Everything
//! else is `Rejected(Reason)` with a specific reason — Part E uses
//! the reason to log a one-line trace when `--print-gpu-decisions` is
//! on.

use crate::annotations::{AdmissionFlags, AdmissionHint, GridShape, MethodAnnotations};
use crate::signature::KernelSignature;
use cratonvm_reader::attribute::CodeAttribute;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
use cratonvm_reader::field_type::FieldType;
use cratonvm_reader::method::ClassFileMethod;
use cratonvm_reader::method_descriptor::MethodDescriptor;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamKind {
    Void,
    I32,
    I64,
    F32,
    F64,
    I32Array,
    I64Array,
    F32Array,
    F64Array,
    I16Array,
    I8Array,
}

impl ParamKind {
    /// Map a JVM field type to the kernel parameter kind that carries
    /// it, or `None` when this crate will not admit the type at all.
    ///
    /// `pub` because it is the canonical statement of which array
    /// element types the GPU pipeline admits, and the VM's marshaller
    /// is checked against it -- see
    /// `cratonvm_vm::runtime::offload::is_marshallable_array_element`
    /// and the `analyzer_and_marshaller_admit_the_same_arrays` test.
    /// A type admitted here but not marshallable there produces a
    /// kernel that compiles and then silently falls back to the
    /// interpreter, which no differential test can detect.
    pub fn from_field(ft: &FieldType) -> Option<Self> {
        Some(match ft {
            FieldType::Int | FieldType::Boolean | FieldType::Char => ParamKind::I32,
            FieldType::Byte => ParamKind::I32,
            FieldType::Short => ParamKind::I32,
            FieldType::Long => ParamKind::I64,
            FieldType::Float => ParamKind::F32,
            FieldType::Double => ParamKind::F64,
            FieldType::Array(inner) => match inner.as_ref() {
                FieldType::Int => ParamKind::I32Array,
                FieldType::Long => ParamKind::I64Array,
                FieldType::Float => ParamKind::F32Array,
                FieldType::Double => ParamKind::F64Array,
                FieldType::Short => ParamKind::I16Array,
                FieldType::Byte => ParamKind::I8Array,
                _ => return None,
            },
            FieldType::Object(_) => return None,
        })
    }

    pub fn is_array(self) -> bool {
        matches!(
            self,
            ParamKind::I32Array
                | ParamKind::I64Array
                | ParamKind::F32Array
                | ParamKind::F64Array
                | ParamKind::I16Array
                | ParamKind::I8Array
        )
    }

    pub fn is_scalar(self) -> bool {
        matches!(
            self,
            ParamKind::I32 | ParamKind::I64 | ParamKind::F32 | ParamKind::F64
        )
    }
}

// ---------------------------------------------------------------------
// Curated GPU intrinsic table (AUDIT 2026-07-11, intrinsic-table
// follow-up to the `AllowIntrinsicCalls` PHASE1-GUESS gap documented in
// `docs/gpu/annotations.md`).
//
// This is the single source of truth for "which `java/lang/Math` (or
// `java/lang/StrictMath`) static methods can this crate lower to PTX
// bit-exactly". Both `classify_invokestatic` (below, the analyzer side)
// and `lowering::emit::Emitter::invokestatic` (the emitter side)
// resolve through [`resolve_math_intrinsic`] so the two layers can
// never drift apart on which callees are admitted.
// ---------------------------------------------------------------------

/// A curated, Java-exact GPU intrinsic — the closed set of
/// `java/lang/Math`/`java/lang/StrictMath` static methods this crate
/// will lower to PTX. See [`resolve_math_intrinsic`] for the
/// class/name/descriptor table and the exactness rationale, and
/// `lowering::emit::Emitter::invokestatic` for the PTX each variant
/// lowers to.
///
/// Deliberately EXCLUDED from this table (calls to these still reject
/// with `Reason::Invoke` even under `AdmissionHint::AllowIntrinsicCalls`):
///
/// - `sin`/`cos`/`tan`/`exp`/`log`/`log10`/`pow`/`cbrt`/… — every
///   `Math` method whose javadoc allows up to a couple ULPs of
///   platform-dependent slack relative to `StrictMath` (the "the
///   size of the error incurred is 1 or 2 ulps" family). PTX's
///   `.approx` transcendental instructions (`sin.approx.f32`, etc.)
///   are lower precision still and have no `f64` form at all on most
///   architectures; there is no PTX instruction that is provably
///   within Java's error bound for these, so none of them are in this
///   table. (The original Phase 1 spec's five-method list —
///   `sqrt`/`sin`/`cos`/`exp`/`log` — is superseded by this table:
///   `sin`/`cos`/`exp`/`log` are cut for exactly this reason, `sqrt`
///   is kept because it alone in that list is specified as *exactly*
///   rounded, not approximate.)
/// - `toIntExact`/`addExact`/`multiplyExact`/… — these can throw
///   `ArithmeticException`; a GPU kernel body has no lowering for a
///   Java exception.
/// - `round`/`ceil`/`floor`/`rint`/`copySign`/`signum`/`hypot`/… — not
///   yet audited for a bit-exact PTX mapping; left out of this first
///   curated cut rather than guessed at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MathIntrinsic {
    /// `Math.sqrt(double)` / `StrictMath.sqrt(double)` → `sqrt.rn.f64`.
    SqrtF64,
    /// `Math.abs(int)` / `StrictMath.abs(int)` → the explicit
    /// wraparound-safe `shr`/`xor`/`sub` sequence (see
    /// `Emitter::abs_i32`'s doc comment for why this crate does not
    /// rely on PTX's `abs.s32` at the `Integer.MIN_VALUE` boundary).
    AbsI32,
    /// `Math.abs(long)` / `StrictMath.abs(long)` — 64-bit twin of
    /// [`AbsI32`](MathIntrinsic::AbsI32).
    AbsI64,
    /// `Math.abs(float)` / `StrictMath.abs(float)` → `abs.f32` (pure
    /// sign-bit clear — no rounding, no overflow edge case).
    AbsF32,
    /// `Math.abs(double)` / `StrictMath.abs(double)` → `abs.f64`.
    AbsF64,
    /// `Math.min(int,int)` / `StrictMath.min(int,int)` → `setp.lt.s32`
    /// + `selp.s32`.
    MinI32,
    /// `Math.max(int,int)` / `StrictMath.max(int,int)` → `setp.gt.s32`
    /// + `selp.s32`.
    MaxI32,
    /// `Math.min(long,long)` / `StrictMath.min(long,long)`.
    MinI64,
    /// `Math.max(long,long)` / `StrictMath.max(long,long)`.
    MaxI64,
    /// `Math.min(float,float)` / `StrictMath.min(float,float)` — the
    /// NaN-and-signed-zero-correct `setp.nan` + bitwise-OR-of-raw-bits
    /// + `selp` chain (see `Emitter::minmax_f32`'s doc comment).
    MinF32,
    /// `Math.max(float,float)` / `StrictMath.max(float,float)` — twin
    /// of [`MinF32`](MathIntrinsic::MinF32) with the ordered comparison
    /// and the zero-case bitwise op both flipped (`ge`/AND instead of
    /// `le`/OR).
    MaxF32,
    /// `Math.min(double,double)` / `StrictMath.min(double,double)`.
    MinF64,
    /// `Math.max(double,double)` / `StrictMath.max(double,double)`.
    MaxF64,
    /// `Math.fma(float,float,float)` / `StrictMath.fma(float,float,float)`
    /// → `fma.rn.f32` (single-rounding fused multiply-add — exactly
    /// what both classes are specified to compute).
    FmaF32,
    /// `Math.fma(double,double,double)` / `StrictMath.fma(double,double,double)`
    /// → `fma.rn.f64`.
    FmaF64,
    /// `Float.float16ToFloat(short)` -> `cvt.f32.f16`.
    ///
    /// Not a `Math` method, and the only non-`Math` entry in the
    /// table. It earns its place because half-precision weights are
    /// the reason a large model fits in device memory at all, and
    /// because the conversion is EXACT in this direction: every f16
    /// value, including every denormal, NaN and infinity, is
    /// representable in f32, so the hardware instruction and the
    /// JDK method agree bit for bit with no rounding mode to choose.
    Float16ToFloat,
    /// `Math.exp(double)` -> `ex2.approx.f32` of `x * log2(e)`.
    ///
    /// **The one entry in this table that is not bit-exact with the
    /// JDK**, and the only one gated behind an environment variable
    /// (`CRATONVM_GPU_APPROX_MATH=1`) rather than being admitted
    /// whenever the intrinsic hint is set. `ex2.approx.f32` carries
    /// about 2 ULP; `Math.exp` promises 1 ULP and semi-monotonicity
    /// in double precision. Every other transcendental stays
    /// rejected, and this one is rejected too unless the variable is
    /// set, so a kernel cannot acquire an approximate answer by
    /// accident.
    ///
    /// It exists because a sigmoid is the one thing a transformer
    /// feed-forward block needs that cannot be built from the exact
    /// table, and moving just that step back to the host would put a
    /// device round trip in the middle of every layer.
    ExpF64,
}

/// Resolve a static-method callsite — `class_name` in internal form
/// (e.g. `"java/lang/Math"`), `method_name`, and the raw JVM
/// `descriptor` string (e.g. `"(D)D"`) — to a curated [`MathIntrinsic`],
/// or `None` if this crate has no lowering for it.
///
/// ## Why `java/lang/Math` and `java/lang/StrictMath` are both matched
///
/// `sqrt`/`abs`/`min`/`max`/`fma` are exactly the subset of `Math`'s
/// methods whose javadoc requires `Math` and `StrictMath` to compute
/// IDENTICAL results bit-for-bit — unlike `sin`/`cos`/`exp`/`log`/`pow`
/// (excluded from this table entirely; see [`MathIntrinsic`]'s doc
/// comment), where `Math` is explicitly allowed to trade accuracy for
/// speed relative to `StrictMath`:
///
/// - `sqrt(double)`: specified as the correctly-rounded IEEE 754 square
///   root — no "1 ulp" slack clause applies to it the way it does to
///   the transcendentals, so both classes must agree exactly.
/// - `abs`: pure sign-bit manipulation (float/double) or
///   two's-complement negate-if-negative (int/long) — no rounding
///   decision either class could differ on.
/// - `min`/`max`: pure comparison plus NaN/signed-zero selection — no
///   rounding.
/// - `fma`: both classes are specified as "the exact product ... is
///   then rounded once", i.e. a true fused multiply-add — identical
///   contract since `StrictMath.fma` was added (Java 9).
///
/// A wrong/unrecognised `class_name` (anything other than those two),
/// `method_name`, or `descriptor` — including a same-named overload
/// this table doesn't cover, e.g. `Math.min(double,int)` (not a real
/// overload, but the point stands for any descriptor mismatch) —
/// returns `None`. The match is exact on all three fields; there is no
/// fuzzy/partial matching.
/// Whether `CRATONVM_GPU_APPROX_MATH=1` is set, read once per process.
///
/// Off by default, and off is the state in which every admitted
/// intrinsic is bit-exact with the JDK. Turning it on admits
/// `Math.exp` and nothing else; see [`MathIntrinsic::ExpF64`].
fn approx_math_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_GPU_APPROX_MATH")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

pub(crate) fn resolve_math_intrinsic(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<MathIntrinsic> {
    if class_name == "java/lang/Float" {
        // `Float.floatToFloat16` is deliberately absent: the narrowing
        // direction rounds, and PTX `cvt.rn.f16.f32` would have to be
        // proven to match `Math.round`-to-nearest-even at every
        // overflow and denormal boundary before it could be admitted.
        // Widening has no such question.
        return match (method_name, descriptor) {
            ("float16ToFloat", "(S)F") => Some(MathIntrinsic::Float16ToFloat),
            _ => None,
        };
    }
    if class_name != "java/lang/Math" && class_name != "java/lang/StrictMath" {
        return None;
    }
    Some(match (method_name, descriptor) {
        ("sqrt", "(D)D") => MathIntrinsic::SqrtF64,
        ("abs", "(I)I") => MathIntrinsic::AbsI32,
        ("abs", "(J)J") => MathIntrinsic::AbsI64,
        ("abs", "(F)F") => MathIntrinsic::AbsF32,
        ("abs", "(D)D") => MathIntrinsic::AbsF64,
        ("min", "(II)I") => MathIntrinsic::MinI32,
        ("max", "(II)I") => MathIntrinsic::MaxI32,
        ("min", "(JJ)J") => MathIntrinsic::MinI64,
        ("max", "(JJ)J") => MathIntrinsic::MaxI64,
        ("min", "(FF)F") => MathIntrinsic::MinF32,
        ("max", "(FF)F") => MathIntrinsic::MaxF32,
        ("min", "(DD)D") => MathIntrinsic::MinF64,
        ("max", "(DD)D") => MathIntrinsic::MaxF64,
        ("fma", "(FFF)F") => MathIntrinsic::FmaF32,
        ("fma", "(DDD)D") => MathIntrinsic::FmaF64,
        ("exp", "(D)D") if approx_math_enabled() => MathIntrinsic::ExpF64,
        _ => return None,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reason {
    /// The `@GpuKernel(grid = ...)` shape is not one the emitter
    /// implements.
    ///
    /// AUDIT 2026-08-28: `GridShape` was parsed into `MethodAnnotations`
    /// and then read by nothing. Every kernel lowered element-wise
    /// regardless of what it asked for, so `grid = BLOCK_REDUCTION` — a
    /// tree reduction across a thread block, per its Java documentation —
    /// silently compiled to one thread per element with no reduction at
    /// all, and produced wrong answers rather than an error. Rejecting
    /// the shapes the emitter cannot express sends those methods back to
    /// the CPU, where they are correct but slower, which is the only
    /// honest option until the lowerings exist.
    UnsupportedGridShape,
    /// The kernel asks for a launch geometry the emitter cannot produce:
    /// a Y or Z block extent, or dynamic shared memory.
    ///
    /// AUDIT 2026-08-28: like `grid`, these were parsed and discarded.
    /// The emitter has exactly one shape — a 1-D grid of 1-D blocks with
    /// no dynamic shared memory — so a kernel declaring `blockY = 8`
    /// silently ran with `blockY = 1`, and one declaring `sharedBytes`
    /// silently got none. For a kernel written to index by `threadIdx.y`
    /// or to use a shared tile that is not a slower answer, it is a wrong
    /// one. Rejecting sends the method to the CPU until the lowerings
    /// exist.
    UnsupportedLaunchGeometry,
    NonStatic,
    Synchronized,
    NativeOrAbstract,
    NoCode,
    BadDescriptor,
    UnsupportedParamType,
    UnsupportedReturnType,
    Allocation,
    /// `invokestatic` (0xB8) — and every other invoke family opcode
    /// (`invokevirtual`/`invokespecial`/`invokeinterface`/`invokedynamic`,
    /// 0xB6/0xB7/0xB9/0xBA), which never have a GPU lowering at all.
    ///
    /// AUDIT 2026-07-11 (intrinsic table follow-up): `invokestatic`
    /// under `AdmissionHint::AllowIntrinsicCalls` used to be admitted
    /// unconditionally (PHASE1-GUESS — see the removed comment this
    /// replaces) because the analyzer had no way to resolve the
    /// constant-pool callee. `classify_invokestatic` now does the
    /// resolution: with a constant pool available (the
    /// `analyze_with_pool`/`analyze_with_annotations_and_pool` /
    /// `_and_pool` entry points) and the hint set, a callsite that
    /// resolves to one of the [`MathIntrinsic`] table entries via
    /// [`resolve_math_intrinsic`] is admitted; every other
    /// `invokestatic` — including a real `java/lang/Math` method NOT
    /// in the table (`sin`/`cos`/`exp`/`log`/`pow`/…), any call to a
    /// different class, and any `invokestatic` seen through the
    /// CP-free `analyze`/`analyze_with_annotations` entry points
    /// (which have no pool to resolve against) — still rejects with
    /// this same reason, exactly as `Strict` mode always has.
    Invoke,
    FieldAccess,
    Throw,
    Monitor,
    Switch,
    JsrRet,
    TypeCheck,
    HasExceptionHandlers,
    /// `aaload` / `aastore` — we don't allow reference arrays.
    RefArrayOp,
    /// Phase 9 #2 — `aload_0` in a non-static method that is NOT
    /// immediately followed by `getfield` of a primitive-array
    /// field. The only supported receiver-access pattern is
    /// `aload_0; getfield <primitive-array-cp>`; everything else
    /// (passing `this` to another method, storing `this` to a
    /// local, etc.) lacks a GPU lowering today.
    NonStaticReceiverMisuse,
    /// An opcode we haven't enumerated yet — be safe and reject.
    UnknownOpcode(u8),
    /// AUDIT 2026-05-16: the method has at least one array parameter
    /// and a scalar (non-void) return — i.e. it reduces N array
    /// elements to a single scalar (e.g. `dot([I[I)J`, `sum([I)I`).
    /// The current emitter has no block-reduction lowering: every CUDA
    /// thread would race-overwrite the single scalar slot with its own
    /// per-element term, producing silently wrong results. Reject the
    /// shape so the VM falls back to CPU execution until a proper
    /// reduction lowering is implemented.
    ReductionNotImplemented,
    /// AUDIT 2026-05-19: the method has a counted loop (a backward
    /// branch) and a scalar (non-void) return. The counted-loop
    /// lowering dispatches one CUDA thread per loop iteration, but
    /// `scalar_return` writes the result through the single `ret_ptr`;
    /// every thread races to overwrite that one slot with its own
    /// per-iteration value — silently wrong results. The
    /// `ReductionNotImplemented` check above only catches array-in /
    /// scalar-out shapes, so a scalar-in / scalar-out counted loop
    /// (e.g. `(II)I` that loops) slips through. Reject it here until a
    /// guarded single-writer or block-reduction lowering exists.
    CountedLoopScalarReturn,
    /// AUDIT 2026-05-24 (C31): `ldc` (0x12) / `ldc_w` (0x13) /
    /// `ldc2_w` (0x14) load a constant from the constant pool. The
    /// analyzer cannot, without resolving the CP entry, tell whether
    /// the target is a numeric primitive (which a GPU lowering could
    /// in principle materialise as an immediate) or a `String` /
    /// `Class` / `MethodType` / `MethodHandle` / dynamic constant
    /// (which have no GPU representation). The lowering layer has no
    /// dispatch arm for these opcodes either, so admitting them at
    /// the analyzer wastes the analyze→lower round-trip and pollutes
    /// the per-method blacklist with would-be-eligible methods.
    /// Reject upstream until the analyzer learns to resolve the CP
    /// entry or the emitter grows a numeric-only `ldc` arm.
    ///
    /// AUDIT 2026-07-11: this was, in practice, the single most common
    /// eligibility killer — ANY `int` literal outside sipush range
    /// (`|c| > 32767`) or ANY `long`/`float`/`double` literal forced a
    /// blanket reject even in an otherwise-perfect element-wise kernel,
    /// because javac has no `iconst`/`bipush`/`sipush`-style immediate
    /// form for those cases and must fall back to `ldc`/`ldc2_w`. The
    /// analyzer now resolves the CP entry when it has one (see
    /// [`analyze_with_pool`] / [`analyze_with_annotations_and_pool`])
    /// and admits `ldc`/`ldc_w` of an `Integer`/`Float` entry and
    /// `ldc2_w` of a `Long`/`Double` entry — the emitter has a matching
    /// `mov.s32`/`mov.f32`/`mov.s64`/`mov.f64` immediate lowering (see
    /// `lowering/emit.rs`'s `ldc`/`ldc2_w`). `String`/`Class`/
    /// `MethodType`/`MethodHandle`/`Dynamic` entries — and every
    /// `ldc`/`ldc2_w` when no constant pool is available at all (the
    /// plain [`analyze`] / [`analyze_with_annotations`] entry points) —
    /// still reject with this same reason; there is no GPU-representable
    /// immediate form for a reference-typed constant.
    LoadConstant,
    /// Direct annotation API users can pass `MethodAnnotations` with
    /// `gpu_exclude` set; that opt-out takes precedence over any
    /// `gpu_kernel` hint.
    GpuExcluded,
    /// `frem` (0x72) / `drem` (0x73) reject in `Strict` mode (the
    /// default). PTX has no `rem.f32`/`rem.f64` mnemonic; the emitter
    /// lowers them via a div + truncate + fma sequence
    /// (`lowering::emit::Emitter::frem_f32`/`drem_f64` — see the long
    /// AUDIT comment there for the full JLS §15.17.3 derivation) that
    /// is bit-exact only while the quotient magnitude
    /// `|dividend / divisor|` stays within the exactly-representable-
    /// integer range of the type (`< 2^24` for `float`, `< 2^53` for
    /// `double`). Outside that range a single rounded division can
    /// recover the wrong truncated quotient, and the lowering then
    /// returns a value that is wrong by a whole multiple of the
    /// divisor — not a rounding-level error, a flatly incorrect
    /// answer. Because the GPU deopt/failure-flag machinery only
    /// catches bounds/div-zero traps (never wrong VALUES), a
    /// silently-wrong `frem`/`drem` must never be the default.
    ///
    /// `classify`'s `0x72 | 0x73` arm therefore only admits these two
    /// opcodes under
    /// [`AdmissionFlags::approximate_float_remainder`](crate::annotations::AdmissionFlags::approximate_float_remainder).
    ///
    /// AUDIT 2026-09-02: that used to read
    /// `AdmissionHint::AllowDivByZero`, and the reason given for the
    /// reuse was not a semantic one — it was that minting a dedicated
    /// variant "would require editing `annotations.rs`". So one flag
    /// gated two unrelated lowering decisions and three doc comments
    /// existed to keep them untangled. They are separate bits now.
    /// `ALLOW_DIV_BY_ZERO` still sets both, because that is the constant
    /// users have written and its meaning does not change; what is gone
    /// is the coupling in the code that reads it.
    FloatRemainder,
    /// `lcmp` (0x94) / `fcmpl`/`fcmpg` (0x95/0x96) / `dcmpl`/`dcmpg`
    /// (0x97/0x98) push `-1`/`0`/`1`.
    ///
    /// AUDIT 2026-07-11: these opcodes now have a real, unconditionally
    /// bit-exact PTX lowering (`lowering::emit::Emitter::lcmp`/
    /// `cmp_f32`/`cmp_f64` — a `setp` + `selp` chain, with an explicit
    /// `setp.nan` override for the `fcmpl`/`fcmpg`/`dcmpl`/`dcmpg`
    /// NaN-handling rule). Unlike `frem`/`drem`, a compare has no
    /// precision boundary, so `classify`'s `0x94..=0x98` arm admits them
    /// under `Strict` too — this reason is therefore no longer returned
    /// by `classify` and is kept only for API/history stability (a
    /// caller pattern-matching on `Reason` still compiles, and the
    /// variant documents what used to reject here).
    ///
    /// REALITY CHECK: admitting the opcode does NOT make a realistic
    /// javac-emitted method newly offloadable end-to-end. Every real
    /// use of `lcmp`/`fcmp*`/`dcmp*` javac emits is IMMEDIATELY followed
    /// by a single-operand `if<cond>` that consumes the pushed value for
    /// a branch decision (`a < b`, `a > b`, a hand-written 3-way
    /// `compareTo`, …) — javac has no source construct that stores the
    /// raw comparison result without doing so. That following `if*`
    /// still rejects unconditionally in `lowering/emit.rs` ("if-branch
    /// opcode … outside canonical-loop guard position"), so the net
    /// effect of this change is that such a method becomes analyzer-
    /// `Eligible` and then lowering-`Rejected` with a precise message,
    /// instead of analyzer-`Rejected(Compare)` — more precise
    /// diagnostics, no new false eligibility. See
    /// `test_classes/gpu/CompareBranchFusion.java` and the
    /// `compare_branch_fusion_*` tests in `lowering.rs` for the pinned
    /// before/after transition, and the value-producing lowering is
    /// still a correct building block for a future cmp-then-branch
    /// fusion.
    Compare,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OffloadVerdict {
    Eligible(KernelSignature),
    Rejected(Reason),
}

/// Inspect a class-file method and decide whether to offload it.
///
/// Equivalent to [`analyze_with_annotations`] with
/// [`MethodAnnotations::default()`] — i.e. strict, behaves exactly as
/// the analyzer did before the annotation feature landed.
///
/// This entry point has no constant pool, so `ldc`/`ldc_w`/`ldc2_w`
/// always reject with `Reason::LoadConstant` regardless of what they
/// target — see [`analyze_with_pool`] for the CP-aware variant that
/// admits numeric-literal `ldc`.
pub fn analyze(method: &ClassFileMethod) -> OffloadVerdict {
    analyze_with_annotations(method, &MethodAnnotations::default())
}

/// [`analyze`], but resolving `ldc`/`ldc_w`/`ldc2_w` against `cp` (AUDIT
/// C31) so a numeric-literal load (`Integer`/`Float` via `ldc`/`ldc_w`,
/// `Long`/`Double` via `ldc2_w`) is admitted instead of rejected
/// outright. Equivalent to [`analyze_with_annotations_and_pool`] with
/// [`MethodAnnotations::default()`].
pub fn analyze_with_pool(method: &ClassFileMethod, cp: &ConstantPool) -> OffloadVerdict {
    analyze_with_annotations_and_pool(method, &MethodAnnotations::default(), cp)
}

/// Inspect a class-file method with user-supplied annotations that can
/// loosen specific rejection reasons (see §2.4 of the GPU phase-1
/// spec).
///
/// When `annotations` is the default value, this function is identical
/// in behaviour to [`analyze`]. If `annotations.gpu_exclude` is present,
/// the method is rejected before any other checks. When a `GpuKernelAttrs`
/// is present, the associated [`AdmissionHint`] selectively relaxes the
/// bytecode scan:
///
/// - `AdmissionHint::Strict`           — no loosening.
/// - `AdmissionHint::AllowAllocation`  — `newarray` of a primitive
///   component whose size comes from a method parameter is accepted.
/// - `AdmissionHint::AllowDivByZero`   — sets two independent bits (see
///   [`AdmissionFlags`](crate::annotations::AdmissionFlags)):
///   `unguarded_integer_division`, recorded in the [`KernelSignature`]
///   so lowering skips the integer zero-divisor guards, and
///   `approximate_float_remainder`, which admits `frem`/`drem` at all —
///   their div+truncate+fma lowering is bit-exact only for quotients
///   within the type's exactly-representable-integer range, so it needs
///   an explicit opt-in. The two travelled on one flag until
///   2026-09-02 for want of a place to put the second.
/// - `AdmissionHint::AllowIntrinsicCalls` — `invokestatic` is accepted
///   only when the constant-pool callee resolves (via
///   [`resolve_math_intrinsic`]) to one of the curated
///   [`MathIntrinsic`] table entries (`Math`/`StrictMath`
///   `sqrt`/`abs`/`min`/`max`/`fma`); every other `invokestatic` still
///   rejects with `Reason::Invoke`. This supersedes the original
///   Phase 1 spec's five-method list
///   (`sqrt`/`sin`/`cos`/`exp`/`log`) — see [`MathIntrinsic`]'s doc
///   comment for why the transcendentals were cut. Resolution needs a
///   constant pool, so this loosening only has an effect through
///   [`analyze_with_pool`]/[`analyze_with_annotations_and_pool`]; the
///   CP-free entry points below always reject `invokestatic`
///   regardless of the hint.
///
/// No constant pool is available here either, so `ldc`/`ldc_w`/`ldc2_w`
/// still reject unconditionally — see
/// [`analyze_with_annotations_and_pool`].
pub fn analyze_with_annotations(
    method: &ClassFileMethod,
    annotations: &MethodAnnotations,
) -> OffloadVerdict {
    analyze_with_annotations_and_pool_impl(method, annotations, None)
}

/// [`analyze_with_annotations`], but resolving `ldc`/`ldc_w`/`ldc2_w`
/// against `cp` (AUDIT C31) — see [`analyze_with_pool`] for the
/// no-annotations shorthand.
pub fn analyze_with_annotations_and_pool(
    method: &ClassFileMethod,
    annotations: &MethodAnnotations,
    cp: &ConstantPool,
) -> OffloadVerdict {
    analyze_with_annotations_and_pool_impl(method, annotations, Some(cp))
}

/// Shared implementation behind all four `analyze*` entry points above.
/// `cp` is `None` for the CP-free entry points (`analyze` /
/// `analyze_with_annotations`), which keeps their pre-AUDIT-C31
/// behaviour byte-for-byte: `ldc`/`ldc_w`/`ldc2_w` reject unconditionally
/// because there is no constant pool to resolve them against.
fn analyze_with_annotations_and_pool_impl(
    method: &ClassFileMethod,
    annotations: &MethodAnnotations,
    cp: Option<&ConstantPool>,
) -> OffloadVerdict {
    if annotations.gpu_exclude.is_some() {
        return OffloadVerdict::Rejected(Reason::GpuExcluded);
    }

    // Phase 9 #2 — non-static methods are now admitted if their body
    // uses `aload_0` only as the immediate-getfield-receiver
    // pattern. The receiver's accessed fields become extra kernel
    // args (see `KernelSignature::this_field_cps`). Other ways of
    // using `this` (storing it, passing it to another method, etc.)
    // still reject via `Reason::NonStaticReceiverMisuse` from the
    // body scan.
    let is_static = method.is_static();
    if method.is_synchronized() {
        return OffloadVerdict::Rejected(Reason::Synchronized);
    }
    if method.is_native() || method.is_abstract() {
        return OffloadVerdict::Rejected(Reason::NativeOrAbstract);
    }
    let Some(code) = method.code() else {
        return OffloadVerdict::Rejected(Reason::NoCode);
    };
    if !code.exception_table.is_empty() {
        return OffloadVerdict::Rejected(Reason::HasExceptionHandlers);
    }
    let desc = match MethodDescriptor::parse(&method.descriptor) {
        Ok(d) => d,
        Err(_) => return OffloadVerdict::Rejected(Reason::BadDescriptor),
    };
    let mut param_kinds = Vec::with_capacity(desc.parameters.len());
    for p in &desc.parameters {
        match ParamKind::from_field(p) {
            Some(k) => param_kinds.push(k),
            None => return OffloadVerdict::Rejected(Reason::UnsupportedParamType),
        }
    }
    let return_kind = match &desc.return_type {
        None => ParamKind::Void,
        Some(rt) => match ParamKind::from_field(rt) {
            Some(k) => k,
            None => return OffloadVerdict::Rejected(Reason::UnsupportedReturnType),
        },
    };

    // AUDIT 2026-09-02: read the independent SET, not the one-of. See
    // `annotations::AdmissionFlags` for why the enum was the wrong shape
    // and what it cost inside this file — `frem`/`drem` gated on
    // `AllowDivByZero` for no semantic reason, because minting a variant
    // meant editing another module. `AdmissionFlags::from` keeps every
    // existing annotation meaning exactly what it meant.
    let hint = annotations
        .gpu_kernel
        .as_ref()
        .map(|k| k.admit)
        .unwrap_or(AdmissionFlags::STRICT);

    // AUDIT 2026-08-28: honour the declared grid shape, or refuse.
    //
    // Only `Elementwise` has a lowering. `RowPerThread` and
    // `BlockReduction` are parsed, and were then dropped on the floor:
    // the emitter has one path and took it for every kernel. A user who
    // asked for a block reduction got an element-wise kernel that
    // produced wrong results silently — the worst failure mode available,
    // since a rejection would merely have run the method on the CPU.
    if let Some(k) = annotations.gpu_kernel.as_ref() {
        if k.grid != GridShape::Elementwise {
            return OffloadVerdict::Rejected(Reason::UnsupportedGridShape);
        }
        // `block_x` IS honoured — it seeds the occupancy memo in
        // `OffloadCache::lookup_or_compile`. A Y/Z extent or a dynamic
        // shared-memory request has nothing to apply to in a 1-D launch,
        // so asking for one is rejected rather than quietly dropped.
        if k.block_y > 1 || k.block_z > 1 || k.shared_bytes > 0 {
            return OffloadVerdict::Rejected(Reason::UnsupportedLaunchGeometry);
        }
    }

    // AUDIT 2026-05-20: walk the bytecode BEFORE the signature-shape
    // checks (`ReductionNotImplemented` / `CountedLoopScalarReturn`).
    // A method that uses a forbidden opcode — `invokestatic`,
    // `instanceof`, `athrow`, … — must be rejected for THAT specific
    // reason, not the coarse `ReductionNotImplemented` shape reason
    // that also happens to match its `[I…)scalar` descriptor. Running
    // the opcode scan first means `reject_invoke`, `reject_type_check`
    // and friends get their precise reject reason; the shape checks
    // below only ever fire on a method that is otherwise GPU-clean.
    //
    // Single bytecode pass: `scan_bytecode` collects the reject reason,
    // the `this_field_cps` receiver-access CP indices (Phase 9 #2), the
    // loop-trip work estimate, the backward-branch flag, and whether
    // the body matches the dot-product/sum reduction shape — no second
    // walk needed.
    let (this_field_cps, estimated_work, has_backward, is_dot_reduction) =
        match scan_bytecode(code, hint, is_static, cp) {
            Ok(t) => t,
            Err(reason) => return OffloadVerdict::Rejected(reason),
        };
    // A reduction accumulates into a scalar that becomes the return value,
    // so the shape is only meaningful for a scalar-returning method. A
    // void-returning counted loop (e.g. a `map` writing `out[i]`) is never
    // a reduction regardless of its body opcodes; clamp the flag here so
    // `is_reduction` in the emitted signature stays false and the kernel
    // lowers as a per-element map.
    let is_dot_reduction = is_dot_reduction && return_kind.is_scalar();

    // AUDIT 2026-05-22: dot-product / sum reductions are now a
    // supported kernel shape. The `lowering::emit` layer has explicit
    // counted-loop + accumulator + scalar-return lowering (see the
    // `i2l`/`mul.lo.s64`/`add.s64` paths in `lowering/emit.rs` and the
    // `dot_product_lowers_with_long_math` test); the analyzer must
    // agree and admit the shape that lowering exercises. The two shape
    // guards below (`ReductionNotImplemented` / `CountedLoopScalarReturn`)
    // therefore no longer fire for a recognised dot-product reduction —
    // a counted loop over array parameters whose body accumulates into
    // a scalar local that becomes the return value.
    //
    // AUDIT 2026-05-16: an array-in / scalar-out signature is a
    // reduction shape (sum, dot, max, count, …). For shapes the emitter
    // does NOT yet lower, `scalar_return` would write `ret_ptr` from
    // every CUDA thread, racing to overwrite the single scalar — so any
    // array-in / scalar-out method that is NOT the recognised
    // dot-product reduction is still refused and falls back to the CPU.
    //
    // This shape check runs AFTER the opcode scan above so a method
    // that is rejected for a more specific opcode reason keeps that
    // reason.
    if return_kind.is_scalar() && param_kinds.iter().any(|k| k.is_array()) && !is_dot_reduction {
        return OffloadVerdict::Rejected(Reason::ReductionNotImplemented);
    }

    // AUDIT 2026-05-19: a method with a backward branch is a counted
    // loop; the counted-loop lowering runs one CUDA thread per
    // iteration. A scalar (non-void) return is written through the
    // single `ret_ptr` by `scalar_return`, so every thread would race
    // to overwrite that one slot — silently wrong results. The
    // `ReductionNotImplemented` check only fires for array-in /
    // scalar-out shapes; a scalar-in / scalar-out counted loop is not
    // caught there. Reject it so the VM falls back to the CPU.
    //
    // The recognised dot-product reduction is exempt: it is a counted
    // loop with a scalar return that the `lowering::emit` layer
    // supports, so it must remain `Eligible`.
    if has_backward && return_kind.is_scalar() && !is_dot_reduction {
        return OffloadVerdict::Rejected(Reason::CountedLoopScalarReturn);
    }

    OffloadVerdict::Eligible(KernelSignature {
        param_kinds,
        return_kind,
        estimated_work,
        this_field_cps,
        // Merged from the round-9 branch: the analyzer cannot see across
        // the dispatch boundary, so default `false` (cheaper no-sync
        // launch); the launch site flips it to `true` when a host
        // read-back will follow.
        needs_d2h_sync: false,
        // Phase 10 #2 — populated by `lower_method` once the emitter's
        // per-`*astore` `array_param_of` tracking has run. The analyzer
        // doesn't simulate the operand stack, so it can't compute the
        // mask precisely here. Leave `0` and let lowering fill it in
        // before `CompiledKernel` caches the signature.
        writes_param_mask: 0,
        // Same story for the read mask, but the safe default is the
        // OPPOSITE: an unpopulated `reads_param_mask` must read as "every
        // param is read" so a chunked dispatch refuses rather than
        // streams out early on an array the kernel might read. Lowering
        // overwrites it with the precise set.
        reads_param_mask: u64::MAX,
        work_bound: crate::emitter::WorkBound::Unknown,
        // AUDIT 2026-05-24 (C31): propagate the dot-product reduction
        // flag so the lowering layer emits `atom.global.add.<suffix>`
        // instead of a racing plain `st.global.<suffix>` for the scalar
        // return. See `KernelSignature::is_reduction` for the contract.
        is_reduction: is_dot_reduction,
        // Exactly what the field means and nothing else: skip the
        // integer zero-divisor guard. The `frem`/`drem` decision that
        // used to ride on this same flag is now its own bit and is made
        // above, at admission, where it belongs.
        allow_div_by_zero: hint.unguarded_integer_division,
    })
}

/// Walk the bytecode exactly once. Reject on the first forbidden
/// opcode; otherwise return `(this_field_cps, loop-trip estimate,
/// has_backward_branch, is_dot_product_reduction)`.
///
/// `hint` selectively loosens specific rejections — see [`AdmissionHint`]
/// for the policy table. `this_field_cps` is always empty: non-static
/// this-field methods are rejected outright (the emitter has no
/// `getfield` lowering and never binds `this`), so the analyzer and the
/// emitter agree and no round-trip is wasted. The `has_backward` flag
/// lets `analyze` recognise counted-loop shapes without a second pass —
/// the work estimator's branch-direction check needs the same `pc /
/// instruction_size` walk the classifier already performs.
///
/// `is_dot_product_reduction` (the 4th element) is `true` when the body
/// matches the dot-product / sum reduction shape that `lowering::emit`
/// supports: a counted loop (a backward branch) whose body both reads
/// from arrays (an `*aload`) and accumulates with an arithmetic `*add`.
/// `analyze` uses it to exempt that shape from the conservative
/// `ReductionNotImplemented` / `CountedLoopScalarReturn` guards.
///
/// `cp` is the class's constant pool, when the caller has one (AUDIT
/// C31). It is only consulted to resolve `ldc` (0x12) / `ldc_w` (0x13) /
/// `ldc2_w` (0x14) — see [`classify_ldc`] — so a numeric-literal load
/// can be admitted instead of unconditionally rejected. `None` keeps
/// the pre-AUDIT-C31 behaviour: every `ldc`/`ldc_w`/`ldc2_w` rejects.
fn scan_bytecode(
    code: &CodeAttribute,
    hint: AdmissionFlags,
    is_static: bool,
    cp: Option<&ConstantPool>,
) -> Result<(Vec<u16>, usize, bool, bool), Reason> {
    let bytes = &code.code;
    let mut pc = 0usize;
    let mut prev_op: Option<u8> = None;
    // Non-static this-field methods are now rejected outright (the
    // emitter cannot lower them — see the `NonStaticReceiverMisuse`
    // handling in the walk below), so no receiver-access CP indices are
    // ever collected. The field is retained in `KernelSignature` for ABI
    // stability but is always empty.
    let this_field_cps: Vec<u16> = Vec::new();
    let mut has_backward = false;
    // Dot-product reduction shape probes: the body reads array elements
    // (`iaload`/`laload`/`faload`/`daload`/`baload`/`saload`/`caload`,
    // opcodes 0x2E..=0x35 minus `aaload` 0x32) and accumulates with an
    // arithmetic `*add` (`iadd`/`ladd`/`fadd`/`dadd`, 0x60..=0x63).
    let mut body_has_array_load = false;
    let mut body_has_add = false;
    // A per-element array *store* (`iastore`..`sastore`, 0x4F..=0x56 minus
    // `aastore` 0x53 which `classify` already rejects) is the signature of
    // a MAP (`out[i] = f(a[i], …)`), not a reduction. A genuine dot-product
    // / sum reduction accumulates into a scalar local and never writes an
    // array, so an array store disqualifies the reduction shape.
    let mut body_has_array_store = false;
    // Reduction dataflow proof (review jit-cuda-review.md §1 MED): a
    // shape match (counted loop + array load + `*add`) is NOT enough to
    // prove a reduction. A method like `for (i…) { acc += a[i]; } return
    // a[0];` matches the shape but returns an array element, not the
    // accumulator — flagging it `is_reduction` makes the emitter
    // atomically-add the per-thread element into `ret_ptr`, racing the
    // wrong values. To rule that out we require a real loop-carried
    // accumulator: a local that is read, fed into a `*add`, stored back
    // to the SAME slot inside the loop body (`*load S; …; *add; *store
    // S` — the loop re-runs each iteration, giving the loop-carried
    // dependency), AND whose value is what the method returns (`*load S;
    // *return`). We track the most-recent `*load` slot so a following
    // `*add`→`*store` chain can confirm the store target matches a slot
    // that was loaded since the last store; and we remember the slot
    // returned by the trailing `*load S; *return`.
    //
    // This is a deliberate over-approximation in the SAFE direction:
    // false-negatives (missing a genuine reduction → non-atomic serial
    // fallback) are acceptable, false-positives (a wrong `atom.add`) are
    // not. Anything we cannot prove falls back to a plain map / CPU.
    //
    // `ReductionDataflow` collects every slot that has a proven
    // `*load S; ...; *add; *store S` accumulation and the slot of a
    // trailing `*load S; *return`. The two must intersect for the body
    // to be a recognised reduction.
    let mut reduction = ReductionDataflow::default();
    // Literal loop-bound recovery for the work estimate. The canonical
    // counted loop compares the induction variable against its bound with
    // a forward `if_icmp*` (`iload iv; <bound>; if_icmpge exit`). When the
    // bound is a compile-time literal (`iconst_*`/`bipush`/`sipush`) it is
    // the value pushed immediately before that forward comparison.
    // `last_const` tracks the most-recent such push; `literal_bound`
    // captures it at the first forward conditional branch so the work
    // estimate can use the real trip count instead of a flat default.
    let mut last_const: Option<i32> = None;
    let mut literal_bound: Option<i32> = None;

    while pc < bytes.len() {
        let op = bytes[pc];

        if (0x2E..=0x35).contains(&op) && op != 0x32 {
            body_has_array_load = true;
        }
        if (0x60..=0x63).contains(&op) {
            body_has_add = true;
        }
        if (0x4F..=0x56).contains(&op) && op != 0x53 {
            body_has_array_store = true;
        }

        // ── Reduction dataflow tracking ────────────────────────────────
        // Decode the local slot read/written by integer/long/float/double
        // load/store opcodes so we can prove the `acc = acc + x` self-feed
        // and the `*load acc; *return` link.
        reduction.observe(bytes, pc, prev_op);

        // Track literal integer pushes so a forward exit-comparison can
        // recover its bound operand for the work estimate.
        let pushed_const: Option<i32> = match op {
            0x02..=0x08 => Some(op as i32 - 0x03), // iconst_m1..iconst_5
            0x10 if pc + 1 < bytes.len() => Some(bytes[pc + 1] as i8 as i32), // bipush
            0x11 if pc + 2 < bytes.len() => {
                Some(i16::from_be_bytes([bytes[pc + 1], bytes[pc + 2]]) as i32) // sipush
            }
            _ => None,
        };

        // Branch-direction probe (formerly `estimate_work`): a backward
        // branch marks a counted loop.
        if (0x99..=0xA7).contains(&op) || op == 0xC6 || op == 0xC7 {
            if pc + 3 <= bytes.len() {
                let off = i16::from_be_bytes([bytes[pc + 1], bytes[pc + 2]]) as i32;
                if off < 0 {
                    has_backward = true;
                } else if literal_bound.is_none()
                    && (0x9F..=0xA4).contains(&op)
                    && last_const.is_some()
                {
                    // Forward `if_icmp*` (canonical loop-exit family):
                    // the most-recent literal push is the bound operand.
                    literal_bound = last_const;
                }
            }
        } else if op == 0xC8 && pc + 5 <= bytes.len() {
            let off =
                i32::from_be_bytes([bytes[pc + 1], bytes[pc + 2], bytes[pc + 3], bytes[pc + 4]]);
            if off < 0 {
                has_backward = true;
            }
        }

        // Non-static receiver-access pattern handling.
        //
        // The analyzer and the lowering emitter MUST agree on what they
        // accept: a method admitted here only to be rejected by `walk`
        // wastes an analyze→lower round-trip and pollutes the per-method
        // blacklist. The emitter has NO `getfield` (0xB4) dispatch arm
        // and `bind_param_locals` never binds `this` (slot 0 of a
        // non-static method), so the `aload_0; getfield <prim-array>`
        // receiver-access shape can never lower — `walk` always rejects
        // it via the default `UnsupportedNode` arm. Reject it here
        // instead so the two layers stay in sync. (Full `getfield`
        // lowering — binding `this` and resolving the field — is larger
        // scope and not attempted.)
        //
        // For static methods, `aload_0` loads the first array
        // parameter — same as `aload_<n>` for any other slot — and does
        // not interact with `getfield` because static-context `getfield`
        // rejects anyway via `classify`.
        if !is_static && prev_op == Some(0x2A) {
            // `aload_0` loads `this` in a non-static method; neither the
            // getfield receiver-access shape nor any other use of `this`
            // has a GPU lowering today.
            return Err(Reason::NonStaticReceiverMisuse);
        }

        // AUDIT 2026-07-11 (C31 follow-up): `ldc`/`ldc_w`/`ldc2_w` need
        // the constant pool to classify precisely, which `classify`
        // does not have access to. Intercept them here — before the
        // generic classifier, which still conservatively rejects all
        // three opcodes whenever it IS consulted (`cp = None`, or a
        // caller that invokes `classify` directly) — and resolve
        // against `cp` when the caller supplied one.
        let op_class = if op == 0x12 || op == 0x13 || op == 0x14 {
            classify_ldc(bytes, pc, op, cp)
        } else if op == 0xB8 {
            classify_invokestatic(bytes, pc, hint, cp)
        } else {
            classify(op, hint, prev_op)
        };
        match op_class {
            OpClass::Ok => {}
            OpClass::Reject(r) => return Err(r),
        }
        prev_op = Some(op);
        // Carry the literal pushed by this op (if any) into the next
        // iteration so a following forward `if_icmp*` can read it as its
        // bound operand. Any non-pushing op clears it.
        last_const = pushed_const;
        pc += instruction_size(bytes, pc)?;
    }
    // Work estimate: prefer a recovered literal trip bound when available
    // (`for (i = 0; i < N; i++)` with a literal N runs N iterations). Fall
    // back to the flat `1 << 20` only when the loop has a backward branch
    // but no recoverable literal bound (e.g. the bound is an
    // `arraylength` known only at runtime). Straight-line kernels keep
    // their bytecode-length proxy. Stay conservative: clamp a recovered
    // bound to at least 1 and never above the flat default.
    const FLAT_LOOP_WORK: usize = 1 << 20;
    let estimated_work = if has_backward {
        match literal_bound {
            Some(n) if n > 0 => (n as usize).min(FLAT_LOOP_WORK),
            _ => FLAT_LOOP_WORK,
        }
    } else {
        bytes.len().max(1)
    };
    // A dot-product / sum reduction is a counted loop whose body both
    // reads from arrays and accumulates with an arithmetic add. This is
    // the exact shape `lowering::emit` lowers (counted loop + scalar
    // return); recognising it here lets `analyze` admit it instead of
    // rejecting via the conservative reduction shape guards.
    // A per-element array *store* in the body means the loop writes its
    // result element-by-element into an array — that is a MAP
    // (`out[i] = a[i] + b[i]`), not a reduction. Such a method must NOT be
    // flagged `is_reduction`: doing so makes the lowering emit an atomic
    // accumulate into a single scalar slot instead of the per-element
    // store, and the dispatcher declines to launch it (it silently falls
    // back to the CPU). The `!body_has_array_store` guard keeps maps out of
    // the reduction shape; `analyze` additionally gates on a scalar return.
    //
    // Beyond the shape, require a proven accumulator dataflow link (review
    // jit-cuda-review.md §1 MED — "reduction recognition is shape-only"):
    // some slot must be BOTH a proven loop-carried accumulator
    // (`*load S; …; *add; *store S`) AND the slot the method returns
    // (`*load S; *return`). Without this link a body like
    // `for (i…) { acc += a[i]; } return a[0];` matches the syntactic shape
    // yet returns an unrelated array element — flagging it `is_reduction`
    // would emit an `atom.add` of the wrong value. The check is a
    // conservative over-approximation: if we cannot prove the link the
    // method falls back to the non-atomic serial path (false-negatives are
    // safe, false-positives are not).
    let accumulator_feeds_return = reduction.accumulator_feeds_return();
    let is_dot_product_reduction = has_backward
        && body_has_array_load
        && body_has_add
        && !body_has_array_store
        && accumulator_feeds_return;
    Ok((
        this_field_cps,
        estimated_work,
        has_backward,
        is_dot_product_reduction,
    ))
}

#[derive(Default)]
struct ReductionDataflow {
    acc_slots: Vec<u16>,
    returned_slot: Option<u16>,
    loaded_since_store: Vec<u16>,
    add_result_live: bool,
    last_load_slot: Option<u16>,
}

impl ReductionDataflow {
    fn observe(&mut self, bytes: &[u8], pc: usize, prev_op: Option<u8>) {
        let op = bytes[pc];
        if let Some(slot) = load_slot(bytes, pc) {
            // A scalar `*load` after the latest store may feed a later
            // `*add`.
            self.loaded_since_store.push(slot);
            self.last_load_slot = Some(slot);
            self.add_result_live = false;
        } else if is_add_op(op) {
            // `*add` consumes two stack values and leaves the sum on top.
            self.add_result_live = true;
            self.last_load_slot = None;
        } else if let Some(slot) = store_slot(bytes, pc) {
            let is_accumulation = self.add_result_live && self.loaded_since_store.contains(&slot);
            if is_accumulation {
                if !self.acc_slots.contains(&slot) {
                    self.acc_slots.push(slot);
                }
            } else {
                // A plain overwrite invalidates any earlier accumulator
                // proof for this slot.
                self.acc_slots.retain(|&s| s != slot);
            }
            self.clear_expression_candidates();
        } else if (0xAC..=0xAF).contains(&op) {
            // `*return` of a scalar: if it came directly from `*load S`,
            // record S as the returned slot.
            if prev_op.map(is_load_op).unwrap_or(false) {
                self.returned_slot = self.last_load_slot;
            }
            self.add_result_live = false;
        } else if is_reduction_candidate_barrier(op) {
            self.acc_slots.clear();
            self.clear_expression_candidates();
        } else if is_reduction_expression_barrier(op) {
            self.clear_expression_candidates();
        } else {
            // Arithmetic/conversion/array-load opcodes may still be part
            // of `acc = acc + f(a[i])`; they do not clear the loaded-slot
            // candidates, but the top-of-stack add result is no longer
            // live unless the current opcode was itself an add.
            self.add_result_live = false;
            self.last_load_slot = None;
        }
    }

    fn accumulator_feeds_return(&self) -> bool {
        self.returned_slot
            .map(|s| self.acc_slots.contains(&s))
            .unwrap_or(false)
    }

    fn clear_expression_candidates(&mut self) {
        self.loaded_since_store.clear();
        self.add_result_live = false;
        self.last_load_slot = None;
    }
}

fn is_add_op(op: u8) -> bool {
    (0x60..=0x63).contains(&op)
}

fn is_reduction_candidate_barrier(op: u8) -> bool {
    matches!(op, 0x3A | 0x4B..=0x5F)
}

fn is_reduction_expression_barrier(op: u8) -> bool {
    matches!(op, 0x94..=0xA7 | 0xC6..=0xC8)
}

enum OpClass {
    Ok,
    Reject(Reason),
}

/// Classify a single opcode under a given admission hint.
///
/// `prev_op` is the previous opcode in the linear bytecode stream (or
/// `None` at PC 0). It is only consulted by the `AllowAllocation`
/// loosening: the spec requires that the size of an admitted
/// `newarray` come from a method parameter, which we approximate by
/// checking that the immediately preceding instruction is an `iload`
/// family opcode.
fn classify(op: u8, hint: AdmissionFlags, prev_op: Option<u8>) -> OpClass {
    match op {
        // Specific rejects come first.
        0x32 | 0x53 => OpClass::Reject(Reason::RefArrayOp), // aaload, aastore
        0xA5 | 0xA6 => OpClass::Reject(Reason::TypeCheck),  // if_acmpeq, if_acmpne
        0xA8 | 0xA9 | 0xC9 => OpClass::Reject(Reason::JsrRet), // jsr, ret, jsr_w
        0xAA | 0xAB => OpClass::Reject(Reason::Switch),
        // AUDIT 2026-05-24 (C31): `ldc` / `ldc_w` / `ldc2_w` were
        // silently admitted by the permitted band below (0x00..=0x31)
        // even though the emitter has no dispatch arm for them — every
        // such method was analyzed-eligible and then lowering-rejected,
        // wasting work. Reject upstream with the precise reason; see
        // `Reason::LoadConstant`.
        //
        // AUDIT 2026-07-11 (C31 follow-up): this arm is the CP-free
        // fallback. `scan_bytecode`'s walk loop intercepts these three
        // opcodes *before* calling `classify` and routes them to
        // `classify_ldc` instead, which can admit a numeric-literal
        // `ldc`/`ldc2_w` when it has a constant pool to resolve against.
        // This arm stays exactly as it was so `classify` remains correct
        // (conservative) if ever called directly, e.g. from a test.
        0x12 | 0x13 | 0x14 => OpClass::Reject(Reason::LoadConstant),
        // AUDIT 2026-07-11: `frem`/`drem` now have a real PTX lowering
        // (see `lowering::emit::Emitter::frem_f32`/`drem_f64`), but
        // that lowering is only bit-exact for quotients within the
        // exactly-representable-integer range of the type — see
        // `Reason::FloatRemainder` for the full precision analysis.
        // `AllowDivByZero` is reused as the opt-in gate rather than a
        // dedicated hint (see the doc comment on that variant above
        // for why); `Strict` (the default, `prev_op`-independent like
        // every other band here) still rejects unconditionally so a
        // silently-wrong remainder is never the default outcome.
        0x72 | 0x73 if hint.approximate_float_remainder => OpClass::Ok,
        0x72 | 0x73 => OpClass::Reject(Reason::FloatRemainder),
        // AUDIT 2026-07-11: `lcmp`/`fcmpl`/`fcmpg`/`dcmpl`/`dcmpg` used to
        // reject unconditionally with `Reason::Compare` — see that
        // variant's doc comment for the full history. They now have a
        // real PTX lowering (`lowering/emit.rs`'s `lcmp`/`cmp_f32`/
        // `cmp_f64`) that is bit-exact for every input (no precision
        // boundary like `frem`/`drem`), so `Strict` admits them
        // unconditionally too — no hint needed. This is a deliberately
        // explicit arm rather than relying on the `0x54..=0xA4` catch-all
        // band below (which already covers this range) so the intent is
        // documented at the opcode, not implied by a wide band.
        //
        // Admitting the opcode does not, by itself, make any currently-
        // rejected real-world method newly offloadable: every real
        // javac-emitted use pairs the pushed value with an immediately
        // following `if<cond>` branch, and general if-branches outside
        // the canonical-loop guard still reject in `lowering/emit.rs`.
        // See `Reason::Compare`'s doc comment for the full reality-check
        // analysis and the fixture that pins the resulting analyzer-
        // Eligible / lowering-Rejected transition.
        0x94..=0x98 => OpClass::Ok,
        0xB2..=0xB5 => OpClass::Reject(Reason::FieldAccess),
        // Invokes: `invokestatic` (0xB8) needs the constant pool to
        // classify precisely (same reason `ldc`/`ldc_w`/`ldc2_w` get
        // pulled out above `classify` into `classify_ldc`) — see
        // `classify_invokestatic`, which `scan_bytecode`'s walk loop
        // intercepts 0xB8 to before it ever reaches this function. This
        // arm is therefore the CP-free / non-`AllowIntrinsicCalls`
        // fallback, kept so `classify` stays conservative (rejects) if
        // ever called directly, e.g. from a test — mirrors the
        // `0x12 | 0x13 | 0x14 => OpClass::Reject(Reason::LoadConstant)`
        // arm's rationale exactly.
        0xB6..=0xBA => OpClass::Reject(Reason::Invoke),
        // Allocation: `new` (0xBB), `anewarray` (0xBD), and
        // `multianewarray` (0xC5) always reject — `AllowAllocation`
        // does not cover object allocation or reference-component
        // arrays. Only primitive `newarray` (0xBC) is loosened, and
        // only when the size came from `iload <n>` (see prev_op).
        0xBC if hint.allocation && is_iload_family(prev_op) => {
            OpClass::Ok
        }
        0xBB | 0xBC | 0xBD | 0xC5 => OpClass::Reject(Reason::Allocation),
        0xBF => OpClass::Reject(Reason::Throw),
        0xC0 | 0xC1 => OpClass::Reject(Reason::TypeCheck),
        0xC2 | 0xC3 => OpClass::Reject(Reason::Monitor),
        // Permitted bands. Note: `wide` (0xC4) is OK; size handled below.
        //
        // `AdmissionHint::AllowDivByZero` is not an admission loosening:
        // idiv/ldiv/irem/lrem are already part of the supported integer
        // arithmetic band. The hint is copied into `KernelSignature` so
        // lowering can skip its explicit zero-divisor guards.
        0x00..=0x31
        | 0x33..=0x52
        | 0x54..=0xA4
        | 0xA7
        | 0xAC..=0xB1
        | 0xBE
        | 0xC4
        | 0xC6..=0xC8 => OpClass::Ok,
        other => OpClass::Reject(Reason::UnknownOpcode(other)),
    }
}

/// Classify `ldc` (0x12) / `ldc_w` (0x13) / `ldc2_w` (0x14) against the
/// constant pool (AUDIT C31 follow-up, 2026-07-11).
///
/// `ldc`/`ldc_w` reference a single-slot CP entry; the only kinds a GPU
/// immediate can represent are `Integer` and `Float`. `ldc2_w`
/// references a two-slot CP entry (`Long` or `Double`) — per JVMS
/// §6.5, `ldc`/`ldc_w` never target a `Long`/`Double` entry and
/// `ldc2_w` never targets an `Integer`/`Float` one, but the check below
/// enforces that pairing explicitly rather than trusting a
/// (potentially malformed) class file. Every other CP entry kind —
/// `String`, `ClassReference`, `MethodType`, `MethodHandle`,
/// `Dynamic`, … — has no GPU-representable immediate form and rejects
/// with the same [`Reason::LoadConstant`] as before this audit.
///
/// `cp = None` (the CP-free `analyze` / `analyze_with_annotations`
/// entry points) and a truncated/out-of-range operand both fall back to
/// the original unconditional reject — safe-by-default in the same
/// spirit as every other conservative check in this module.
fn classify_ldc(bytes: &[u8], pc: usize, op: u8, cp: Option<&ConstantPool>) -> OpClass {
    let Some(cp) = cp else {
        return OpClass::Reject(Reason::LoadConstant);
    };
    let index = match op {
        0x12 => match bytes.get(pc + 1) {
            Some(&b) => b as u16,
            None => return OpClass::Reject(Reason::LoadConstant),
        },
        0x13 | 0x14 => match (bytes.get(pc + 1), bytes.get(pc + 2)) {
            (Some(&hi), Some(&lo)) => u16::from_be_bytes([hi, lo]),
            _ => return OpClass::Reject(Reason::LoadConstant),
        },
        _ => {
            debug_assert!(false, "classify_ldc called with non-ldc opcode 0x{op:02x}");
            return OpClass::Reject(Reason::LoadConstant);
        }
    };
    let admits = match cp.get(index) {
        Some(ConstantPoolEntry::Integer(_)) | Some(ConstantPoolEntry::Float(_)) => op != 0x14,
        Some(ConstantPoolEntry::Long(_)) | Some(ConstantPoolEntry::Double(_)) => op == 0x14,
        _ => false,
    };
    if admits {
        OpClass::Ok
    } else {
        OpClass::Reject(Reason::LoadConstant)
    }
}

/// Classify `invokestatic` (0xB8) against the admission hint and,
/// when the hint is set, the constant pool (AUDIT 2026-07-11, closing
/// the PHASE1-GUESS gap documented in `docs/gpu/annotations.md`).
///
/// Without `AdmissionHint::AllowIntrinsicCalls` this is identical to
/// `classify`'s catch-all `0xB6..=0xBA` arm: reject with
/// `Reason::Invoke`. With the hint, a constant pool is required to
/// resolve the 2-byte CP index that follows the opcode to a
/// `MethodReference` — no pool (the CP-free `analyze`/
/// `analyze_with_annotations` entry points) means there is nothing to
/// resolve against, so it still rejects; this is the precise behaviour
/// fix over the old code, which admitted blindly regardless of pool
/// availability. When a pool IS available, the callee is resolved via
/// [`resolve_math_intrinsic`]; only a curated-table hit is admitted —
/// everything else (a `java/lang/Math` method not in the table, a call
/// to any other class, a malformed/out-of-range CP index) rejects with
/// `Reason::Invoke`, same as `Strict` mode always has.
fn classify_invokestatic(
    bytes: &[u8],
    pc: usize,
    hint: AdmissionFlags,
    cp: Option<&ConstantPool>,
) -> OpClass {
    if !hint.intrinsic_calls {
        return OpClass::Reject(Reason::Invoke);
    }
    let Some(cp) = cp else {
        return OpClass::Reject(Reason::Invoke);
    };
    let index = match (bytes.get(pc + 1), bytes.get(pc + 2)) {
        (Some(&hi), Some(&lo)) => u16::from_be_bytes([hi, lo]),
        _ => return OpClass::Reject(Reason::Invoke),
    };
    let Some(ConstantPoolEntry::MethodReference {
        class_index,
        name_and_type_index,
    }) = cp.get(index)
    else {
        return OpClass::Reject(Reason::Invoke);
    };
    let Some(class_name) = cp.get_class_name(*class_index) else {
        return OpClass::Reject(Reason::Invoke);
    };
    let Some((method_name, descriptor)) = cp.get_name_and_type(*name_and_type_index) else {
        return OpClass::Reject(Reason::Invoke);
    };
    match resolve_math_intrinsic(class_name, method_name, descriptor) {
        Some(_) => OpClass::Ok,
        None => OpClass::Reject(Reason::Invoke),
    }
}

/// True if `op` is an `iload` family opcode — the cheap proxy for
/// "value-on-stack came from a method parameter" used by the
/// `AllowAllocation` loosening rule.
fn is_iload_family(op: Option<u8>) -> bool {
    match op {
        // `iload` (0x15) + `iload_0..iload_3` (0x1A..=0x1D).
        Some(0x15) | Some(0x1A) | Some(0x1B) | Some(0x1C) | Some(0x1D) => true,
        _ => false,
    }
}

/// True if `op` is a non-reference scalar `*load` opcode (`iload`/`lload`/
/// `fload`/`dload` and their `_0..=_3` short forms). Used by the reduction
/// dataflow check; `aload` (reference) is deliberately excluded because
/// reference locals can never be a numeric accumulator.
fn is_load_op(op: u8) -> bool {
    matches!(op, 0x15..=0x18) || matches!(op, 0x1A..=0x29)
}

/// Decode the local-variable slot read by the scalar `*load` at `pc`, or
/// `None` if the opcode at `pc` is not a scalar load. Handles both the
/// two-byte `iload <index>` family and the one-byte `iload_<n>` forms.
/// Returns `None` on a truncated two-byte operand (defensive — the main
/// walker re-checks bounds via `instruction_size`).
fn load_slot(bytes: &[u8], pc: usize) -> Option<u16> {
    match bytes[pc] {
        // iload/lload/fload/dload <index> — operand is the next byte.
        0x15..=0x18 => bytes.get(pc + 1).map(|&b| b as u16),
        // iload_0..=iload_3 (0x1A..=0x1D).
        0x1A..=0x1D => Some((bytes[pc] - 0x1A) as u16),
        // lload_0..=lload_3 (0x1E..=0x21).
        0x1E..=0x21 => Some((bytes[pc] - 0x1E) as u16),
        // fload_0..=fload_3 (0x22..=0x25).
        0x22..=0x25 => Some((bytes[pc] - 0x22) as u16),
        // dload_0..=dload_3 (0x26..=0x29).
        0x26..=0x29 => Some((bytes[pc] - 0x26) as u16),
        _ => None,
    }
}

/// Decode the local-variable slot written by the scalar `*store` at `pc`,
/// or `None` if the opcode at `pc` is not a scalar store. Mirror of
/// [`load_slot`] for the `istore`/`lstore`/`fstore`/`dstore` families.
fn store_slot(bytes: &[u8], pc: usize) -> Option<u16> {
    match bytes[pc] {
        // istore/lstore/fstore/dstore <index> — operand is the next byte.
        0x36..=0x39 => bytes.get(pc + 1).map(|&b| b as u16),
        // istore_0..=istore_3 (0x3B..=0x3E).
        0x3B..=0x3E => Some((bytes[pc] - 0x3B) as u16),
        // lstore_0..=lstore_3 (0x3F..=0x42).
        0x3F..=0x42 => Some((bytes[pc] - 0x3F) as u16),
        // fstore_0..=fstore_3 (0x43..=0x46).
        0x43..=0x46 => Some((bytes[pc] - 0x43) as u16),
        // dstore_0..=dstore_3 (0x47..=0x4A).
        0x47..=0x4A => Some((bytes[pc] - 0x47) as u16),
        _ => None,
    }
}

/// Byte length of the instruction at `pc`. Returns Err for malformed
/// or for opcodes we did not enumerate.
fn instruction_size(bytes: &[u8], pc: usize) -> Result<usize, Reason> {
    let op = bytes[pc];
    let size = match op {
        // ── 1 byte ──────────────────────────────────────────────────
        0x00..=0x0F   // nop, aconst_null, iconst_*, lconst_*, fconst_*, dconst_*
        | 0x1A..=0x35 // iload_*..aload_3, iaload..saload
        | 0x3B..=0x4E // istore_0..astore_3
        | 0x4F..=0x56 // iastore..sastore
        | 0x57..=0x5F // pop..swap
        | 0x60..=0x83 // arithmetic & shifts (excluding 0x84 iinc)
        | 0x85..=0x93 // conversions, neg
        | 0x94..=0x98 // lcmp, fcmp*, dcmp*
        | 0xAC..=0xB1 // *return
        | 0xBE        // arraylength
        | 0xBF        // athrow  (still 1 byte even though rejected)
        | 0xC2 | 0xC3 // monitor* (1 byte; rejected upstream)
        => 1,
        // ── 2 bytes ─────────────────────────────────────────────────
        0x10 // bipush
        | 0x12 // ldc
        | 0x15..=0x19 // iload..aload
        | 0x36..=0x3A // istore..astore
        | 0xA9 // ret (rejected upstream but length-tagged for safety)
        | 0xBC // newarray
        => 2,
        // ── 3 bytes ─────────────────────────────────────────────────
        0x11 // sipush
        | 0x13 // ldc_w
        | 0x14 // ldc2_w
        | 0x84 // iinc
        | 0x99..=0xA8 // if* + goto + jsr  (jsr rejected upstream)
        | 0xB2..=0xB8 // get/put static/field, invokevirtual/special/static
        | 0xBB // new
        | 0xBD // anewarray
        | 0xC0 // checkcast
        | 0xC1 // instanceof
        | 0xC6 | 0xC7 // ifnull, ifnonnull
        => 3,
        // ── 4 bytes ─────────────────────────────────────────────────
        0xC5 // multianewarray
        => 4,
        // ── 5 bytes ─────────────────────────────────────────────────
        0xB9 // invokeinterface
        | 0xBA // invokedynamic
        | 0xC8 // goto_w
        | 0xC9 // jsr_w (rejected)
        => 5,
        // ── wide prefix ─────────────────────────────────────────────
        0xC4 => {
            let sub = *bytes.get(pc + 1).ok_or(Reason::UnknownOpcode(op))?;
            if sub == 0x84 { 6 } else { 4 }
        }
        // ── variable: switches (rejected, but compute length so a
        //              subsequent reject doesn't desync the walker if
        //              we ever choose to skip-and-continue) ──────────
        0xAA | 0xAB => {
            // We rejected above. Returning 1 keeps the walk from
            // panicking; the caller never observes it because
            // classify() already short-circuited.
            1
        }
        _ => return Err(Reason::UnknownOpcode(op)),
    };
    Ok(size)
}

/// Load a fixture `.class` file and return both the requested method
/// AND the class's constant pool (AUDIT C31 follow-up, 2026-07-11).
///
/// [`crate::test_support::load_method`] only returns the
/// `ClassFileMethod` — the `ClassFile` (and its constant pool) is
/// dropped once the borrow used to force-decode the `Code` attribute
/// ends. The pool-aware `analyze_with_pool` / `lower_method_with_pool`
/// tests need the constant pool to stay alive alongside the method, so
/// this mirrors `load_method`'s loading/force-decode logic but returns
/// the `ConstantPool` too. `pub(crate)` (not `pub(super)`) so
/// `lowering.rs`'s test module can reuse it instead of duplicating the
/// loader a third time.
#[cfg(test)]
pub(crate) fn load_method_with_pool(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> (ClassFileMethod, ConstantPool) {
    let fixture_dir = env!("JIT_CUDA_FIXTURE_DIR");
    let path = std::path::Path::new(fixture_dir).join(format!("{class_name}.class"));
    let bytes = std::fs::read(&path)
        .unwrap_or_else(|e| panic!("failed to read fixture {}: {e}", path.display()));
    let mut class = cratonvm_reader::class_reader::read_class(&bytes)
        .unwrap_or_else(|e| panic!("failed to parse fixture {}: {e:?}", path.display()));
    // See `test_support::load_method` for why only `Code` is
    // force-decoded (and why that has to happen before the constant
    // pool is moved out of `class` below).
    let cp = &class.constant_pool;
    for method in class.methods.iter_mut() {
        for attr in method.attributes.iter_mut() {
            if attr.name() == "Code" {
                let _ = attr.decode(cp);
            }
        }
    }
    let method = class
        .methods
        .into_iter()
        .find(|m| &*m.name == method_name && &*m.descriptor == descriptor)
        .unwrap_or_else(|| {
            panic!(
                "method {method_name}{descriptor} not found in {} \
                 (did the Java source change without recompiling?)",
                path.display()
            )
        });
    (method, class.constant_pool)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotations::{GpuExcludeAttrs, GpuKernelAttrs};
    use crate::test_support::load_method;

    /// Build a `MethodAnnotations` carrying a `GpuKernelAttrs` with the
    /// requested admission hint and all other fields at their `Default`
    /// values. Used by the loosening tests below.
    ///
    /// This relies on `GpuKernelAttrs: Default` (owned by Item 3 — see
    /// the Phase-1 spec §2.3). If that derive is dropped, this helper
    /// is the single place to update.
    fn annotate(admit: AdmissionHint) -> MethodAnnotations {
        MethodAnnotations {
            gpu_kernel: Some(GpuKernelAttrs {
                admit: admit.into(),
                ..GpuKernelAttrs::default()
            }),
            gpu_exclude: None,
        }
    }

    fn track_reduction_ops(bytes: &[u8]) -> ReductionDataflow {
        let mut tracker = ReductionDataflow::default();
        let mut pc = 0usize;
        let mut prev_op = None;
        while pc < bytes.len() {
            tracker.observe(bytes, pc, prev_op);
            prev_op = Some(bytes[pc]);
            pc += instruction_size(bytes, pc).expect("test bytecode must be well-shaped");
        }
        tracker
    }

    #[test]
    fn eligible_vector_add_is_eligible() {
        let method = load_method("EligibleVectorAdd", "vectorAdd", "([I[I[I)V");
        match analyze(&method) {
            OffloadVerdict::Eligible(sig) => {
                assert_eq!(
                    sig.param_kinds,
                    vec![
                        ParamKind::I32Array,
                        ParamKind::I32Array,
                        ParamKind::I32Array
                    ]
                );
                assert_eq!(sig.return_kind, ParamKind::Void);
                // A void-returning per-element map (`out[i] = a[i] + b[i]`)
                // must NOT be flagged as a reduction. If it is, lowering
                // emits an atomic accumulate into a single scalar slot and
                // the dispatcher silently falls back to the CPU instead of
                // launching the per-element kernel.
                assert!(
                    !sig.is_reduction,
                    "void array-writing map misclassified as a reduction"
                );
            }
            v => panic!("expected Eligible, got {v:?}"),
        }
    }

    #[test]
    fn eligible_saxpy_is_eligible() {
        let method = load_method("EligibleSaxpy", "saxpy", "(F[F[F[F)V");
        match analyze(&method) {
            OffloadVerdict::Eligible(sig) => {
                assert_eq!(
                    sig.param_kinds,
                    vec![
                        ParamKind::F32,
                        ParamKind::F32Array,
                        ParamKind::F32Array,
                        ParamKind::F32Array
                    ]
                );
                assert_eq!(sig.return_kind, ParamKind::Void);
                // saxpy (`out[i] = a*x[i] + y[i]`) is a map, not a reduction.
                assert!(!sig.is_reduction, "saxpy map misclassified as a reduction");
            }
            v => panic!("expected Eligible, got {v:?}"),
        }
    }

    #[test]
    fn eligible_dot_product_returns_long() {
        let method = load_method("EligibleDotProduct", "dot", "([I[I)J");
        match analyze(&method) {
            OffloadVerdict::Eligible(sig) => {
                assert_eq!(sig.return_kind, ParamKind::I64);
                // A genuine dot-product / sum reduction (scalar return,
                // accumulates, no array store) must STILL be recognised so
                // lowering emits the atomic-accumulate path. Guards against
                // the `!body_has_array_store` / scalar-return gate
                // over-tightening and dropping real reductions.
                assert!(
                    sig.is_reduction,
                    "genuine scalar-return dot-product reduction no longer recognised"
                );
            }
            v => panic!("expected Eligible, got {v:?}"),
        }
    }

    #[test]
    fn reject_float_remainder_before_lowering() {
        let frem = load_method("FloatRemainder", "fremScalar", "(FF)F");
        assert_eq!(
            analyze(&frem),
            OffloadVerdict::Rejected(Reason::FloatRemainder)
        );

        let drem = load_method("FloatRemainder", "dremScalar", "(DD)D");
        assert_eq!(
            analyze(&drem),
            OffloadVerdict::Rejected(Reason::FloatRemainder)
        );
    }

    // ─── AUDIT 2026-07-11: frem/drem gated admission ────────────────
    //
    // `frem`/`drem` now have a real (if precision-bounded) PTX
    // lowering — see `lowering::emit::Emitter::frem_f32`/`drem_f64`.
    // `Strict` (the default, exercised above) must keep rejecting them
    // unconditionally; `AdmissionHint::AllowDivByZero` is the opt-in
    // that admits them (see `classify`'s `0x72 | 0x73` arm and
    // `Reason::FloatRemainder` for why that hint was reused instead of
    // a dedicated one).

    #[test]
    fn frem_admitted_under_allow_div_by_zero_hint() {
        let method = load_method("FloatRemainder", "fremScalar", "(FF)F");
        let loose = annotate(AdmissionHint::AllowDivByZero);
        match analyze_with_annotations(&method, &loose) {
            OffloadVerdict::Eligible(sig) => {
                assert_eq!(sig.param_kinds, vec![ParamKind::F32, ParamKind::F32]);
                assert_eq!(sig.return_kind, ParamKind::F32);
                assert!(
                    sig.allow_div_by_zero,
                    "AllowDivByZero must reach lowering through KernelSignature \
                     even when it is admitting frem, not an integer div/rem"
                );
            }
            v => panic!("expected Eligible under AllowDivByZero, got {v:?}"),
        }
    }

    #[test]
    fn drem_admitted_under_allow_div_by_zero_hint() {
        let method = load_method("FloatRemainder", "dremScalar", "(DD)D");
        let loose = annotate(AdmissionHint::AllowDivByZero);
        match analyze_with_annotations(&method, &loose) {
            OffloadVerdict::Eligible(sig) => {
                assert_eq!(sig.param_kinds, vec![ParamKind::F64, ParamKind::F64]);
                assert_eq!(sig.return_kind, ParamKind::F64);
            }
            v => panic!("expected Eligible under AllowDivByZero, got {v:?}"),
        }
    }

    /// Other `AdmissionHint` loosenings must NOT accidentally admit
    /// `frem`/`drem` — only `AllowDivByZero` does. Pins the `matches!`
    /// guard in `classify` against a future accidental broadening
    /// (e.g. someone generalising the guard to "any non-Strict hint").
    #[test]
    fn frem_still_rejected_under_unrelated_hints() {
        let method = load_method("FloatRemainder", "fremScalar", "(FF)F");
        for hint in [
            AdmissionHint::AllowAllocation,
            AdmissionHint::AllowIntrinsicCalls,
        ] {
            assert_eq!(
                analyze_with_annotations(&method, &annotate(hint)),
                OffloadVerdict::Rejected(Reason::FloatRemainder),
                "hint {hint:?} must not admit frem — only AllowDivByZero does"
            );
        }
    }

    /// `EligibleFrem.frem([F[F)V` (`out[i] = in[i] % 3.7f` in the
    /// canonical loop) needs BOTH loosenings at once: the pool-aware
    /// `ldc` resolution (for the `3.7f` literal, which has no `fconst`
    /// short form) and `AllowDivByZero` (for the `frem` opcode). Under
    /// `Strict` — even with a constant pool available — it must still
    /// reject on the `frem`, proving the fixture is "only Eligible
    /// under the gating" as intended, not incidentally eligible via
    /// the `ldc` loosening alone.
    #[test]
    fn eligible_frem_fixture_rejected_under_strict_even_with_pool() {
        let (method, cp) = load_method_with_pool("EligibleFrem", "frem", "([F[F)V");
        assert_eq!(
            analyze_with_pool(&method, &cp),
            OffloadVerdict::Rejected(Reason::FloatRemainder)
        );
    }

    #[test]
    fn eligible_frem_fixture_is_eligible_with_pool_and_allow_div_by_zero() {
        let (method, cp) = load_method_with_pool("EligibleFrem", "frem", "([F[F)V");
        let loose = annotate(AdmissionHint::AllowDivByZero);
        match analyze_with_annotations_and_pool(&method, &loose, &cp) {
            OffloadVerdict::Eligible(sig) => {
                assert_eq!(
                    sig.param_kinds,
                    vec![ParamKind::F32Array, ParamKind::F32Array]
                );
                assert_eq!(sig.return_kind, ParamKind::Void);
                assert!(sig.allow_div_by_zero);
            }
            v => panic!("expected Eligible under AllowDivByZero + pool, got {v:?}"),
        }
    }

    // AUDIT 2026-07-11: `lcmp`/`fcmpl`/`fcmpg`/`dcmpl`/`dcmpg` moved from
    // an unconditional `Reject(Reason::Compare)` to unconditional `Ok` —
    // see `classify`'s `0x94..=0x98` arm and `Reason::Compare`'s doc
    // comment for the full rationale (a real, precision-free PTX
    // lowering now exists; the opcodes are no longer rejected at
    // analyze time, only a following non-canonical `if*` branch still
    // rejects, one layer later, in `lowering/emit.rs`).
    #[test]
    fn compare_opcodes_are_admitted_by_analyzer() {
        for op in 0x94..=0x98 {
            match classify(op, AdmissionFlags::STRICT, None) {
                OpClass::Ok => {}
                OpClass::Reject(r) => {
                    panic!("opcode 0x{op:02x} unexpectedly rejected with {r:?}")
                }
            }
        }
    }

    #[test]
    fn reject_allocation() {
        let method = load_method("RejectAllocation", "build", "(I)[I");
        assert_eq!(
            analyze(&method),
            OffloadVerdict::Rejected(Reason::Allocation)
        );
    }

    #[test]
    fn reject_invoke() {
        let method = load_method("RejectInvoke", "outer", "([I)I");
        assert_eq!(analyze(&method), OffloadVerdict::Rejected(Reason::Invoke));
    }

    #[test]
    fn reject_synchronized() {
        let method = load_method("RejectSynchronized", "addAll", "([I)I");
        assert_eq!(
            analyze(&method),
            OffloadVerdict::Rejected(Reason::Synchronized)
        );
    }

    #[test]
    fn reject_ref_array() {
        let method = load_method("RejectRefArray", "sum", "([Ljava/lang/Integer;)I");
        assert_eq!(
            analyze(&method),
            OffloadVerdict::Rejected(Reason::UnsupportedParamType)
        );
    }

    #[test]
    fn reject_switch() {
        let method = load_method("RejectSwitch", "pick", "(I)I");
        assert_eq!(analyze(&method), OffloadVerdict::Rejected(Reason::Switch));
    }

    #[test]
    fn reject_throw() {
        let method = load_method("RejectThrow", "maybeThrow", "(I)V");
        assert_eq!(analyze(&method), OffloadVerdict::Rejected(Reason::Throw));
    }

    #[test]
    fn reject_field_access() {
        let method = load_method("RejectFieldAccess", "read", "()I");
        assert_eq!(
            analyze(&method),
            OffloadVerdict::Rejected(Reason::FieldAccess)
        );
    }

    // ─── Phase 9 #2 — non-static receiver-access pattern ────────────

    /// `NonStaticScale.scaleInPlace(I)V` is a non-static method whose
    /// body reads `this.data` (primitive-array field) via the
    /// `aload_0; getfield <data-cp>` pattern. The lowering emitter has
    /// no `getfield` arm and never binds `this`, so this shape can never
    /// lower; the analyzer must agree and reject it up front (rather than
    /// admit it and waste an analyze→lower round-trip). See the
    /// `NonStaticReceiverMisuse` handling in `scan_bytecode`.
    #[test]
    fn reject_non_static_this_field_pattern() {
        let method = load_method("NonStaticScale", "scaleInPlace", "(I)V");
        assert_eq!(
            analyze(&method),
            OffloadVerdict::Rejected(Reason::NonStaticReceiverMisuse),
            "non-static this-field methods must be rejected — analyzer and \
             emitter must agree, and the emitter cannot lower them",
        );
    }

    /// Sanity check: static methods that still use `aload_0` (to
    /// load their first array parameter) keep working — Phase 9 #2
    /// must not break the static path.
    #[test]
    fn static_methods_still_eligible_after_non_static_relax() {
        let method = load_method("EligibleVectorAdd", "vectorAdd", "([I[I[I)V");
        match analyze(&method) {
            OffloadVerdict::Eligible(sig) => {
                assert!(
                    sig.this_field_cps.is_empty(),
                    "static methods should have no this_field_cps, got {:?}",
                    sig.this_field_cps,
                );
            }
            v => panic!("expected Eligible, got {v:?}"),
        }
    }

    #[test]
    fn reject_type_check() {
        let method = load_method("RejectTypeCheck", "isIntArray", "([I)I");
        assert_eq!(
            analyze(&method),
            OffloadVerdict::Rejected(Reason::TypeCheck)
        );
    }

    // ─── annotation-driven loosening (Phase 1, §2.4) ────────────────

    /// `AllowAllocation` must accept `newarray <primitive>` whose size
    /// is loaded from a method parameter, while `Strict` still
    /// rejects. Uses `RejectAllocation.build(I)[I`, whose bytecode is
    /// literally `iload_0; newarray int; ...` — the canonical pattern
    /// from §2.4.
    #[test]
    fn admit_allocation_loosens_primitive_newarray() {
        let method = load_method("RejectAllocation", "build", "(I)[I");

        // Baseline: strict mode still rejects.
        let strict = annotate(AdmissionHint::Strict);
        assert_eq!(
            analyze_with_annotations(&method, &strict),
            OffloadVerdict::Rejected(Reason::Allocation),
            "Strict must still reject newarray as Allocation"
        );

        // Loosened: the primitive-newarray sourced from `iload_0`
        // (parameter `n`) is accepted, and no other forbidden opcode
        // appears in the method body, so the verdict becomes Eligible.
        let loose = annotate(AdmissionHint::AllowAllocation);
        match analyze_with_annotations(&method, &loose) {
            OffloadVerdict::Eligible(sig) => {
                assert_eq!(sig.param_kinds, vec![ParamKind::I32]);
                assert_eq!(sig.return_kind, ParamKind::I32Array);
            }
            v => panic!("expected Eligible under AllowAllocation, got {v:?}"),
        }
    }

    // ─── AUDIT 2026-07-11: intrinsic-table follow-up ─────────────────
    //
    // The tests below replace the old `admit_intrinsic_loosens_math_sqrt`
    // PHASE1-GUESS placeholder (it asserted only "the verdict changed",
    // using the unrelated `RejectInvoke` fixture, because no real
    // `Math.sqrt` fixture and no CP-aware invoke resolution existed
    // yet). `EligibleMathKernel.java` now gives us real
    // `Math.sqrt`/`abs`/`fma`/`pow` callsites, and
    // `classify_invokestatic`/`resolve_math_intrinsic` actually resolve
    // the constant-pool callee instead of admitting every
    // `invokestatic` blindly.

    /// `sqrtAbsFma` calls `Math.sqrt`/`Math.abs`/`Math.fma` — all three
    /// are in the curated table (see `resolve_math_intrinsic`) — so
    /// under the pool-aware entry point with `AllowIntrinsicCalls` it
    /// must be admitted `Eligible`; under `Strict`, even with the same
    /// pool available, it must still reject with `Reason::Invoke`.
    #[test]
    fn admit_intrinsic_calls_admits_real_math_kernel() {
        let (method, cp) =
            load_method_with_pool("EligibleMathKernel", "sqrtAbsFma", "([F[F[FFFF)V");

        let strict = annotate(AdmissionHint::Strict);
        assert_eq!(
            analyze_with_annotations_and_pool(&method, &strict, &cp),
            OffloadVerdict::Rejected(Reason::Invoke),
            "Strict must still reject Math.sqrt/abs/fma calls"
        );

        let loose = annotate(AdmissionHint::AllowIntrinsicCalls);
        match analyze_with_annotations_and_pool(&method, &loose, &cp) {
            OffloadVerdict::Eligible(sig) => {
                assert_eq!(
                    sig.param_kinds,
                    vec![
                        ParamKind::F32Array,
                        ParamKind::F32Array,
                        ParamKind::F32Array,
                        ParamKind::F32,
                        ParamKind::F32,
                        ParamKind::F32,
                    ]
                );
                assert_eq!(sig.return_kind, ParamKind::Void);
            }
            v => panic!("expected Eligible under AllowIntrinsicCalls + pool, got {v:?}"),
        }
    }

    /// The CP-free entry point (`analyze_with_annotations`, no constant
    /// pool) can never resolve an `invokestatic` target, so even under
    /// `AllowIntrinsicCalls` it must keep rejecting with
    /// `Reason::Invoke`. This is the precise behavioural fix over the
    /// old PHASE1-GUESS code, which admitted ANY `invokestatic` here
    /// regardless of whether a pool was available to prove it safe.
    /// Pins that the CP-free path never regresses back to blind
    /// admission.
    #[test]
    fn admit_intrinsic_calls_without_pool_still_rejects_invoke() {
        let method = load_method("EligibleMathKernel", "sqrtAbsFma", "([F[F[FFFF)V");
        let loose = annotate(AdmissionHint::AllowIntrinsicCalls);
        assert_eq!(
            analyze_with_annotations(&method, &loose),
            OffloadVerdict::Rejected(Reason::Invoke),
            "AllowIntrinsicCalls with no constant pool must not blindly admit invokestatic"
        );
    }

    /// `Math.pow` is deliberately NOT in the curated intrinsic table
    /// (PTX's `.approx` transcendentals don't meet Java's
    /// relative-error contract — see `MathIntrinsic`'s doc comment).
    /// Even under `AllowIntrinsicCalls` WITH a constant pool available,
    /// a callsite that resolves to `Math.pow` must still reject with
    /// `Reason::Invoke` — proving the hint now discriminates by callee
    /// instead of admitting every `invokestatic`.
    #[test]
    fn admit_intrinsic_calls_still_rejects_math_pow() {
        let (method, cp) = load_method_with_pool("EligibleMathKernel", "powRejected", "([D[D)V");
        let loose = annotate(AdmissionHint::AllowIntrinsicCalls);
        assert_eq!(
            analyze_with_annotations_and_pool(&method, &loose, &cp),
            OffloadVerdict::Rejected(Reason::Invoke),
            "Math.pow must not be admitted — it is not in the curated intrinsic table"
        );
    }

    /// White-box coverage of [`resolve_math_intrinsic`] itself — every
    /// table entry, both `java/lang/Math` and `java/lang/StrictMath`.
    #[test]
    fn resolve_math_intrinsic_covers_the_curated_table() {
        use MathIntrinsic::*;
        let cases: &[(&str, &str, &str, MathIntrinsic)] = &[
            ("java/lang/Math", "sqrt", "(D)D", SqrtF64),
            ("java/lang/StrictMath", "sqrt", "(D)D", SqrtF64),
            ("java/lang/Math", "abs", "(I)I", AbsI32),
            ("java/lang/StrictMath", "abs", "(I)I", AbsI32),
            ("java/lang/Math", "abs", "(J)J", AbsI64),
            ("java/lang/Math", "abs", "(F)F", AbsF32),
            ("java/lang/StrictMath", "abs", "(F)F", AbsF32),
            ("java/lang/Math", "abs", "(D)D", AbsF64),
            ("java/lang/Math", "min", "(II)I", MinI32),
            ("java/lang/Math", "max", "(II)I", MaxI32),
            ("java/lang/Math", "min", "(JJ)J", MinI64),
            ("java/lang/Math", "max", "(JJ)J", MaxI64),
            ("java/lang/Math", "min", "(FF)F", MinF32),
            ("java/lang/Math", "max", "(FF)F", MaxF32),
            ("java/lang/Math", "min", "(DD)D", MinF64),
            ("java/lang/Math", "max", "(DD)D", MaxF64),
            ("java/lang/Math", "fma", "(FFF)F", FmaF32),
            ("java/lang/StrictMath", "fma", "(FFF)F", FmaF32),
            ("java/lang/Math", "fma", "(DDD)D", FmaF64),
            ("java/lang/StrictMath", "fma", "(DDD)D", FmaF64),
        ];
        for (class_name, name, desc, expected) in cases {
            assert_eq!(
                resolve_math_intrinsic(class_name, name, desc),
                Some(*expected),
                "expected {class_name}.{name}{desc} to resolve to {expected:?}"
            );
        }
    }

    /// The deliberately-excluded transcendentals/unknown classes/
    /// mismatched descriptors must all resolve to `None`.
    #[test]
    fn resolve_math_intrinsic_excludes_transcendentals_and_unknown_classes() {
        for name in ["sin", "cos", "tan", "exp", "log", "log10", "pow", "cbrt"] {
            assert_eq!(
                resolve_math_intrinsic("java/lang/Math", name, "(D)D"),
                None,
                "{name}(D)D must not be in the curated intrinsic table"
            );
        }
        assert_eq!(
            resolve_math_intrinsic("java/lang/Math", "pow", "(DD)D"),
            None,
            "pow must not be in the curated intrinsic table under any descriptor"
        );
        // Wrong class entirely.
        assert_eq!(
            resolve_math_intrinsic("com/example/Math", "sqrt", "(D)D"),
            None
        );
        // Right class/name, mismatched descriptor — must not fuzzy-match.
        assert_eq!(
            resolve_math_intrinsic("java/lang/Math", "sqrt", "(F)F"),
            None
        );
        assert_eq!(
            resolve_math_intrinsic("java/lang/Math", "min", "(DI)D"),
            None
        );
    }

    #[test]
    fn gpu_exclude_overrides_kernel_for_direct_api_users() {
        let method = load_method("EligibleVectorAdd", "vectorAdd", "([I[I[I)V");
        let annotations = MethodAnnotations {
            gpu_kernel: Some(GpuKernelAttrs::default()),
            gpu_exclude: Some(GpuExcludeAttrs {
                reason: "test opt-out".to_string(),
            }),
        };

        assert_eq!(
            analyze_with_annotations(&method, &annotations),
            OffloadVerdict::Rejected(Reason::GpuExcluded)
        );
    }

    #[test]
    fn allow_div_by_zero_is_carried_to_signature() {
        let method = load_method("EligibleStraightLine", "constReturn", "()I");

        match analyze(&method) {
            OffloadVerdict::Eligible(sig) => assert!(
                !sig.allow_div_by_zero,
                "strict/default analysis must keep divisor-zero guards enabled"
            ),
            v => panic!("expected strict Eligible, got {v:?}"),
        }

        let loose = annotate(AdmissionHint::AllowDivByZero);
        match analyze_with_annotations(&method, &loose) {
            OffloadVerdict::Eligible(sig) => assert!(
                sig.allow_div_by_zero,
                "AllowDivByZero must reach lowering through KernelSignature"
            ),
            v => panic!("expected AllowDivByZero Eligible, got {v:?}"),
        }
    }

    /// A method that is eligible under the existing `analyze` path
    /// must remain eligible when called through
    /// `analyze_with_annotations` with the default (no-annotations)
    /// `MethodAnnotations` — i.e. the new entry-point introduces zero
    /// behavioural drift for un-annotated callers.
    #[test]
    fn strict_default_unchanged() {
        let method = load_method("EligibleVectorAdd", "vectorAdd", "([I[I[I)V");

        let baseline = analyze(&method);
        let with_default = analyze_with_annotations(&method, &MethodAnnotations::default());

        assert_eq!(
            baseline, with_default,
            "MethodAnnotations::default() must not change any verdict"
        );
        assert!(
            matches!(baseline, OffloadVerdict::Eligible(_)),
            "control: vectorAdd must be Eligible without annotations"
        );
    }

    // ─── AUDIT C31 follow-up (2026-07-11): ldc/ldc_w/ldc2_w numeric
    // ─── literal admission ──────────────────────────────────────────
    //
    // Before this fix, ANY int literal outside sipush range
    // (`|c| > 32767`) or ANY long/float/double literal made an
    // otherwise-perfect element-wise kernel ineligible — the single
    // most common real-world eligibility killer. `analyze_with_pool` /
    // `analyze_with_annotations_and_pool` now resolve the CP entry and
    // admit `ldc`/`ldc_w` of `Integer`/`Float`, and `ldc2_w` of
    // `Long`/`Double`. The CP-free `analyze` / `analyze_with_annotations`
    // entry points are unchanged: they still reject every ldc form,
    // since they have no constant pool to resolve against.

    #[test]
    fn ldc_int_literal_is_eligible_with_pool() {
        let (method, cp) = load_method_with_pool("EligibleLdcInt", "scale", "([I[I)V");
        match analyze_with_pool(&method, &cp) {
            OffloadVerdict::Eligible(sig) => {
                assert_eq!(
                    sig.param_kinds,
                    vec![ParamKind::I32Array, ParamKind::I32Array]
                );
                assert_eq!(sig.return_kind, ParamKind::Void);
            }
            v => panic!("expected Eligible, got {v:?}"),
        }
    }

    #[test]
    fn ldc_int_literal_still_rejected_without_pool() {
        // Same fixture, but through the CP-free entry point: it cannot
        // resolve the `ldc` target, so it must keep rejecting exactly
        // like it did before this audit.
        let method = load_method("EligibleLdcInt", "scale", "([I[I)V");
        assert_eq!(
            analyze(&method),
            OffloadVerdict::Rejected(Reason::LoadConstant)
        );
    }

    #[test]
    fn ldc2_w_long_literal_is_eligible_with_pool() {
        let (method, cp) = load_method_with_pool("EligibleLdcLong", "mix", "([J[J)V");
        match analyze_with_pool(&method, &cp) {
            OffloadVerdict::Eligible(sig) => {
                assert_eq!(
                    sig.param_kinds,
                    vec![ParamKind::I64Array, ParamKind::I64Array]
                );
                assert_eq!(sig.return_kind, ParamKind::Void);
            }
            v => panic!("expected Eligible, got {v:?}"),
        }
    }

    #[test]
    fn ldc_float_literal_is_eligible_with_pool() {
        let (method, cp) = load_method_with_pool("EligibleLdcFloat", "fma", "([F[F)V");
        match analyze_with_pool(&method, &cp) {
            OffloadVerdict::Eligible(sig) => {
                assert_eq!(
                    sig.param_kinds,
                    vec![ParamKind::F32Array, ParamKind::F32Array]
                );
                assert_eq!(sig.return_kind, ParamKind::Void);
            }
            v => panic!("expected Eligible, got {v:?}"),
        }
    }

    #[test]
    fn ldc2_w_double_literal_is_eligible_with_pool() {
        let (method, cp) = load_method_with_pool("EligibleLdcDouble", "fma", "([D[D)V");
        match analyze_with_pool(&method, &cp) {
            OffloadVerdict::Eligible(sig) => {
                assert_eq!(
                    sig.param_kinds,
                    vec![ParamKind::F64Array, ParamKind::F64Array]
                );
                assert_eq!(sig.return_kind, ParamKind::Void);
            }
            v => panic!("expected Eligible, got {v:?}"),
        }
    }

    /// A `String` constant-pool entry must never be admitted — not even
    /// once the pool is available — because a `java.lang.String` has no
    /// GPU-representable immediate form. `RejectLdcString.noop()`'s body
    /// is exactly `ldc #<String>; astore_0; return`, so the `ldc` is the
    /// very first instruction the scan sees: whichever reason fires, it
    /// is unambiguously the ldc classification and not, say, the
    /// `astore` of a reference-typed local.
    #[test]
    fn ldc_string_is_rejected_even_with_pool() {
        let (method, cp) = load_method_with_pool("RejectLdcString", "noop", "()V");
        assert_eq!(
            analyze_with_pool(&method, &cp),
            OffloadVerdict::Rejected(Reason::LoadConstant),
            "a String CP entry must stay rejected even when the pool is available"
        );
        // And the CP-free path rejects it too, for the same reason
        // (just less precisely — it can't tell WHAT the entry is).
        assert_eq!(
            analyze(&method),
            OffloadVerdict::Rejected(Reason::LoadConstant)
        );
    }

    // ─── reduction dataflow helpers (review jit-cuda-review.md §1 MED) ──
    //
    // These exercise the slot-decoding helpers that back the
    // accumulator-feeds-return proof directly, without needing a Java
    // fixture (the `test_classes/gpu/` sources live outside this crate's
    // owned-file scope). The end-to-end behaviour — a genuine
    // `return acc` reduction stays `is_reduction`, a `return a[0]`
    // shape-match does not — is covered by `eligible_dot_product_returns_long`
    // plus a future `ReductionFalsePositive` fixture noted in the review.

    #[test]
    fn load_slot_decodes_short_and_wide_forms() {
        // iload_0..=iload_3 (0x1A..=0x1D).
        assert_eq!(load_slot(&[0x1A], 0), Some(0));
        assert_eq!(load_slot(&[0x1D], 0), Some(3));
        // lload_2 (0x20), fload_1 (0x23), dload_3 (0x29).
        assert_eq!(load_slot(&[0x20], 0), Some(2));
        assert_eq!(load_slot(&[0x23], 0), Some(1));
        assert_eq!(load_slot(&[0x29], 0), Some(3));
        // Two-byte `iload <index>` (0x15) / `dload <index>` (0x18).
        assert_eq!(load_slot(&[0x15, 7], 0), Some(7));
        assert_eq!(load_slot(&[0x18, 42], 0), Some(42));
        // Non-load opcodes and reference loads decode to None — an
        // `aload` (0x2A / 0x19) must NOT be mistaken for a scalar
        // accumulator load.
        assert_eq!(load_slot(&[0x2A], 0), None); // aload_0
        assert_eq!(load_slot(&[0x19, 0], 0), None); // aload <index>
        assert_eq!(load_slot(&[0x60], 0), None); // iadd
                                                 // Truncated two-byte operand decodes to None rather than panicking.
        assert_eq!(load_slot(&[0x15], 0), None);
    }

    #[test]
    fn store_slot_decodes_short_and_wide_forms() {
        // istore_0..=istore_3 (0x3B..=0x3E).
        assert_eq!(store_slot(&[0x3B], 0), Some(0));
        assert_eq!(store_slot(&[0x3E], 0), Some(3));
        // lstore_1 (0x40), fstore_2 (0x45), dstore_0 (0x47).
        assert_eq!(store_slot(&[0x40], 0), Some(1));
        assert_eq!(store_slot(&[0x45], 0), Some(2));
        assert_eq!(store_slot(&[0x47], 0), Some(0));
        // Two-byte `istore <index>` (0x36).
        assert_eq!(store_slot(&[0x36, 9], 0), Some(9));
        // astore (reference store) must not decode as a scalar store.
        assert_eq!(store_slot(&[0x4B], 0), None); // astore_0
        assert_eq!(store_slot(&[0x3A, 0], 0), None); // astore <index>
        assert_eq!(store_slot(&[0x36], 0), None); // truncated -> None
    }

    #[test]
    fn is_load_op_excludes_reference_loads() {
        assert!(is_load_op(0x15)); // iload
        assert!(is_load_op(0x1A)); // iload_0
        assert!(is_load_op(0x29)); // dload_3
                                   // aload family is intentionally excluded.
        assert!(!is_load_op(0x19)); // aload
        assert!(!is_load_op(0x2A)); // aload_0
        assert!(!is_load_op(0x2D)); // aload_3
        assert!(!is_load_op(0xAC)); // ireturn
    }

    #[test]
    fn reduction_tracker_keeps_genuine_accumulation() {
        // lload_2; lload_0; ladd; lstore_2; lload_2; lreturn
        let tracker = track_reduction_ops(&[0x20, 0x1E, 0x61, 0x41, 0x20, 0xAD]);
        assert!(
            tracker.accumulator_feeds_return(),
            "load/add/store of the returned slot must still prove a reduction"
        );
    }

    #[test]
    fn reduction_tracker_clears_candidates_on_pop() {
        // lload_2; pop2; lload_0; lload_1; ladd; lstore_2; lload_2; lreturn
        //
        // Without clearing the loaded-slot window at pop2, the later
        // unrelated ladd would inherit stale slot 2 and falsely prove
        // `lstore_2` as an accumulator update.
        let tracker = track_reduction_ops(&[0x20, 0x58, 0x1E, 0x1F, 0x61, 0x41, 0x20, 0xAD]);
        assert!(
            !tracker.accumulator_feeds_return(),
            "pop2 must clear stale reduction candidates"
        );
    }

    #[test]
    fn reduction_tracker_clears_proven_candidate_on_late_pop() {
        // lload_2; lload_0; lload_1; ladd; lstore_2; pop2; lload_2; lreturn
        //
        // The lstore_2 initially looks like an accumulator update because
        // slot 2 was loaded earlier, but the following pop2 proves that
        // old value was still on the stack and did not feed the ladd.
        let tracker = track_reduction_ops(&[0x20, 0x1E, 0x1F, 0x61, 0x41, 0x58, 0x20, 0xAD]);
        assert!(
            !tracker.accumulator_feeds_return(),
            "late pop2 must clear a falsely proven accumulator candidate"
        );
    }

    #[test]
    fn reduction_tracker_drops_overwritten_accumulator_slot() {
        // lload_2; lload_0; ladd; lstore_2; lconst_0; lstore_2; lload_2; lreturn
        let tracker = track_reduction_ops(&[0x20, 0x1E, 0x61, 0x41, 0x09, 0x41, 0x20, 0xAD]);
        assert!(
            !tracker.accumulator_feeds_return(),
            "a non-accumulating store must invalidate the earlier accumulator proof"
        );
    }
}
