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

use crate::annotations::{AdmissionHint, MethodAnnotations};
use crate::signature::KernelSignature;
use rustjvm_reader::attribute::CodeAttribute;
use rustjvm_reader::field_type::FieldType;
use rustjvm_reader::method::ClassFileMethod;
use rustjvm_reader::method_descriptor::MethodDescriptor;

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
    fn from_field(ft: &FieldType) -> Option<Self> {
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reason {
    NonStatic,
    Synchronized,
    NativeOrAbstract,
    NoCode,
    BadDescriptor,
    UnsupportedParamType,
    UnsupportedReturnType,
    Allocation,
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
pub fn analyze(method: &ClassFileMethod) -> OffloadVerdict {
    analyze_with_annotations(method, &MethodAnnotations::default())
}

/// Inspect a class-file method with user-supplied annotations that can
/// loosen specific rejection reasons (see §2.4 of the GPU phase-1
/// spec).
///
/// When `annotations.gpu_kernel` is `None`, this function is identical
/// in behaviour to [`analyze`]. When a `GpuKernelAttrs` is present, the
/// associated [`AdmissionHint`] selectively relaxes the bytecode scan:
///
/// - `AdmissionHint::Strict`           — no loosening.
/// - `AdmissionHint::AllowAllocation`  — `newarray` of a primitive
///   component whose size comes from a method parameter is accepted.
/// - `AdmissionHint::AllowDivByZero`   — the analyzer never injects a
///   zero-divisor guard today; this hint is plumbed through for
///   completeness and for Phase-2 lowering to read.
/// - `AdmissionHint::AllowIntrinsicCalls` — `invokestatic` is accepted
///   on the assumption that the lowering layer will handle the
///   intrinsics listed in §2.4 (`Math.sqrt`/`sin`/`cos`/`exp`/`log`).
pub fn analyze_with_annotations(
    method: &ClassFileMethod,
    annotations: &MethodAnnotations,
) -> OffloadVerdict {
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

    let hint = annotations
        .gpu_kernel
        .as_ref()
        .map(|k| k.admit)
        .unwrap_or(AdmissionHint::Strict);

    let this_field_cps = match scan_bytecode(code, hint, is_static) {
        Ok(cps) => cps,
        Err(reason) => return OffloadVerdict::Rejected(reason),
    };

    let estimated_work = estimate_work(code);
    OffloadVerdict::Eligible(KernelSignature {
        param_kinds,
        return_kind,
        estimated_work,
        this_field_cps,
    })
}

/// Walk the bytecode once and reject as soon as we hit a forbidden
/// opcode. `hint` selectively loosens specific rejections — see
/// [`AdmissionHint`] for the policy table.
fn scan_bytecode(
    code: &CodeAttribute,
    hint: AdmissionHint,
    is_static: bool,
) -> Result<Vec<u16>, Reason> {
    let bytes = &code.code;
    let mut pc = 0usize;
    let mut prev_op: Option<u8> = None;
    let mut this_field_cps: Vec<u16> = Vec::new();

    while pc < bytes.len() {
        let op = bytes[pc];

        // Phase 9 #2 — non-static receiver-access pattern handling.
        // For non-static methods, `aload_0` (0x2A) loads `this`. The
        // only supported follow-up is `getfield` (0xB4) of a
        // primitive-array field; anything else (e.g. invokevirtual
        // on this, astore_*) lacks a GPU lowering today.
        //
        // For static methods, `aload_0` loads the first array
        // parameter — same as `aload_<n>` for any other slot — and
        // does not interact with `getfield` because static-context
        // `getfield` rejects anyway via `classify`.
        if !is_static && prev_op == Some(0x2A) {
            if op == 0xB4 {
                // Read the 2-byte CP index following getfield.
                if pc + 2 >= bytes.len() {
                    return Err(Reason::BadDescriptor);
                }
                let cp_index = u16::from_be_bytes([bytes[pc + 1], bytes[pc + 2]]);
                this_field_cps.push(cp_index);
                // Skip the default `classify` rejection of 0xB4 —
                // it's accepted here as the receiver-access shape.
                prev_op = Some(op);
                pc += 3; // getfield is 3 bytes total
                continue;
            } else {
                // `aload_0` followed by something other than
                // getfield — not the supported pattern.
                return Err(Reason::NonStaticReceiverMisuse);
            }
        }

        match classify(op, hint, prev_op) {
            OpClass::Ok => {}
            OpClass::Reject(r) => return Err(r),
        }
        prev_op = Some(op);
        pc += instruction_size(bytes, pc)?;
    }
    Ok(this_field_cps)
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
fn classify(op: u8, hint: AdmissionHint, prev_op: Option<u8>) -> OpClass {
    match op {
        // Specific rejects come first.
        0x32 | 0x53 => OpClass::Reject(Reason::RefArrayOp),    // aaload, aastore
        0xA5 | 0xA6 => OpClass::Reject(Reason::TypeCheck),     // if_acmpeq, if_acmpne
        0xA8 | 0xA9 | 0xC9 => OpClass::Reject(Reason::JsrRet), // jsr, ret, jsr_w
        0xAA | 0xAB => OpClass::Reject(Reason::Switch),
        0xB2..=0xB5 => OpClass::Reject(Reason::FieldAccess),
        // Invokes: the AllowIntrinsicCalls hint loosens `invokestatic`
        // (0xB8) so that the lowering layer can recognise the small set
        // of intrinsics enumerated in §2.4 (Math.sqrt/sin/cos/exp/log).
        //
        // PHASE1-GUESS: a precise check would resolve the 2-byte CP
        // index following 0xB8 and confirm the target is one of the
        // five `java/lang/Math` doubles. The analyzer does not carry a
        // reference to the constant pool today, so we conservatively
        // accept any `invokestatic` under the hint and leave the
        // intrinsic-vs-arbitrary-call distinction to the lowering
        // layer, which will refuse to emit PTX for an unknown callee.
        0xB8 if matches!(hint, AdmissionHint::AllowIntrinsicCalls) => OpClass::Ok,
        0xB6..=0xBA => OpClass::Reject(Reason::Invoke),
        // Allocation: `new` (0xBB), `anewarray` (0xBD), and
        // `multianewarray` (0xC5) always reject — `AllowAllocation`
        // does not cover object allocation or reference-component
        // arrays. Only primitive `newarray` (0xBC) is loosened, and
        // only when the size came from `iload <n>` (see prev_op).
        0xBC if matches!(hint, AdmissionHint::AllowAllocation)
            && is_iload_family(prev_op) =>
        {
            OpClass::Ok
        }
        0xBB | 0xBC | 0xBD | 0xC5 => OpClass::Reject(Reason::Allocation),
        0xBF => OpClass::Reject(Reason::Throw),
        0xC0 | 0xC1 => OpClass::Reject(Reason::TypeCheck),
        0xC2 | 0xC3 => OpClass::Reject(Reason::Monitor),
        // Permitted bands. Note: `wide` (0xC4) is OK; size handled below.
        //
        // `AdmissionHint::AllowDivByZero` is a no-op here: the current
        // analyzer never injects a zero-divisor guard around
        // idiv/ldiv/irem/lrem (opcodes 0x6C/0x6D/0x70/0x71), all of
        // which already fall in the permitted `0x60..=0x83` band. The
        // hint is still threaded through so Phase-2 lowering can pick
        // it up without re-plumbing the analyzer.
        0x00..=0x31 | 0x33..=0x52 | 0x54..=0xA4 | 0xA7 | 0xAC..=0xB1
        | 0xBE | 0xC4 | 0xC6..=0xC8 => OpClass::Ok,
        other => OpClass::Reject(Reason::UnknownOpcode(other)),
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

/// Cheap loop-trip heuristic: if the bytecode contains at least one
/// backward branch, default to "lots of work"; otherwise use bytecode
/// length. The interpreter (Part E) only uses this to skip offload
/// for tiny inputs, so precision is not critical.
fn estimate_work(code: &CodeAttribute) -> usize {
    let bytes = &code.code;
    let mut has_backward = false;
    let mut pc = 0usize;
    while pc < bytes.len() {
        let op = bytes[pc];
        let size = instruction_size(bytes, pc).unwrap_or(1);
        if (0x99..=0xA7).contains(&op) || op == 0xC6 || op == 0xC7 {
            if pc + 3 <= bytes.len() {
                let off = i16::from_be_bytes([bytes[pc + 1], bytes[pc + 2]]) as i32;
                if off < 0 {
                    has_backward = true;
                }
            }
        } else if op == 0xC8 && pc + 5 <= bytes.len() {
            let off = i32::from_be_bytes([
                bytes[pc + 1],
                bytes[pc + 2],
                bytes[pc + 3],
                bytes[pc + 4],
            ]);
            if off < 0 {
                has_backward = true;
            }
        }
        pc += size;
    }
    if has_backward {
        1 << 20
    } else {
        bytes.len().max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotations::GpuKernelAttrs;
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
                admit,
                ..GpuKernelAttrs::default()
            }),
            gpu_exclude: None,
        }
    }

    #[test]
    fn eligible_vector_add_is_eligible() {
        let method = load_method("EligibleVectorAdd", "vectorAdd", "([I[I[I)V");
        match analyze(&method) {
            OffloadVerdict::Eligible(sig) => {
                assert_eq!(
                    sig.param_kinds,
                    vec![ParamKind::I32Array, ParamKind::I32Array, ParamKind::I32Array]
                );
                assert_eq!(sig.return_kind, ParamKind::Void);
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
                    vec![ParamKind::F32, ParamKind::F32Array, ParamKind::F32Array, ParamKind::F32Array]
                );
                assert_eq!(sig.return_kind, ParamKind::Void);
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
            }
            v => panic!("expected Eligible, got {v:?}"),
        }
    }

    #[test]
    fn reject_allocation() {
        let method = load_method("RejectAllocation", "build", "(I)[I");
        assert_eq!(analyze(&method), OffloadVerdict::Rejected(Reason::Allocation));
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
    /// `aload_0; getfield <data-cp>` pattern. Pre-Phase 9 #2 this
    /// rejected as `NonStatic`; now it's `Eligible` with the
    /// getfield CP index recorded in `this_field_cps` (the
    /// marshaller / emitter use it in Phase 9 #2 push 2).
    #[test]
    fn accept_non_static_with_this_field_pattern() {
        let method = load_method("NonStaticScale", "scaleInPlace", "(I)V");
        match analyze(&method) {
            OffloadVerdict::Eligible(sig) => {
                // The body accesses `this.data` multiple times
                // (length read, indexed load, indexed store) — every
                // access goes through `aload_0; getfield <data>`, so
                // we expect ≥ 3 cps. Duplicates are intentional;
                // the marshaller may de-dup.
                assert!(
                    sig.this_field_cps.len() >= 3,
                    "expected ≥3 this_field_cps entries, got {:?}",
                    sig.this_field_cps,
                );
                // Every cp index should be the same field
                // (`NonStaticScale.data`).
                let first = sig.this_field_cps[0];
                for &cp in &sig.this_field_cps {
                    assert_eq!(
                        cp, first,
                        "expected every this_field_cp entry to point at the same field; got {:?}",
                        sig.this_field_cps,
                    );
                }
            }
            v => panic!("expected Eligible for non-static this-access pattern, got {v:?}"),
        }
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
            v => panic!(
                "expected Eligible under AllowAllocation, got {v:?}"
            ),
        }
    }

    /// `AllowIntrinsicCalls` must stop the analyzer from emitting
    /// `Reason::Invoke` for `invokestatic`. We can't construct a
    /// fixture containing the exact `Math.sqrt(D)D` callsite in this
    /// agent's scope, so we fall back to the existing `RejectInvoke`
    /// fixture and assert the *change in behaviour* — strict mode
    /// rejects with `Invoke`, loose mode does not.
    ///
    /// PHASE1-GUESS: when a `RejectMathSqrt`-style fixture lands (a
    /// scalar-in / scalar-out method whose only ineligibility is
    /// `invokestatic java/lang/Math.sqrt(D)D`), this test should
    /// upgrade its loose-mode assertion to `OffloadVerdict::Eligible`.
    #[test]
    fn admit_intrinsic_loosens_math_sqrt() {
        let method = load_method("RejectInvoke", "outer", "([I)I");

        let strict = annotate(AdmissionHint::Strict);
        let strict_verdict = analyze_with_annotations(&method, &strict);

        let loose = annotate(AdmissionHint::AllowIntrinsicCalls);
        let loose_verdict = analyze_with_annotations(&method, &loose);

        // The loosening must change the answer: if strict rejected
        // specifically for `Invoke`, the loose verdict must not be
        // `Rejected(Invoke)`. (Downstream rejections such as
        // ReductionNotImplemented may still fire — that's fine; this
        // test is scoped to the Invoke loosening only.)
        if let OffloadVerdict::Rejected(Reason::Invoke) = strict_verdict {
            assert!(
                !matches!(
                    loose_verdict,
                    OffloadVerdict::Rejected(Reason::Invoke)
                ),
                "AllowIntrinsicCalls must not emit Reason::Invoke; got {loose_verdict:?}"
            );
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
        let with_default =
            analyze_with_annotations(&method, &MethodAnnotations::default());

        assert_eq!(
            baseline, with_default,
            "MethodAnnotations::default() must not change any verdict"
        );
        assert!(
            matches!(baseline, OffloadVerdict::Eligible(_)),
            "control: vectorAdd must be Eligible without annotations"
        );
    }
}
