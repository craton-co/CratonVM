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

use crate::signature::KernelSignature;
use cratonvm_reader::attribute::CodeAttribute;
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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OffloadVerdict {
    Eligible(KernelSignature),
    Rejected(Reason),
}

/// Inspect a class-file method and decide whether to offload it.
pub fn analyze(method: &ClassFileMethod) -> OffloadVerdict {
    if !method.is_static() {
        return OffloadVerdict::Rejected(Reason::NonStatic);
    }
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

    // AUDIT 2026-05-16: an array-in / scalar-out signature is a
    // reduction shape (sum, dot, max, count, …). The emitter's
    // `scalar_return` lowering writes the value through `ret_ptr`
    // from every CUDA thread, so each thread races to overwrite the
    // single scalar with its per-element term — silently wrong
    // results. Until a proper block-reduction lowering exists, refuse
    // the shape and let the VM run the method on the CPU.
    if return_kind.is_scalar() && param_kinds.iter().any(|k| k.is_array()) {
        return OffloadVerdict::Rejected(Reason::ReductionNotImplemented);
    }

    // AUDIT 2026-05-17 (Fix 7): single bytecode pass that collects
    // both the reject reason (formerly `scan_bytecode`) and the
    // loop-trip heuristic (formerly `estimate_work`). Walking the
    // bytecode twice was pure waste — the work estimator's branch-
    // direction check only needs the same `pc / instruction_size`
    // walk the classifier already performs.
    let estimated_work = match scan_and_estimate(code) {
        Ok(w) => w,
        Err(reason) => return OffloadVerdict::Rejected(reason),
    };
    OffloadVerdict::Eligible(KernelSignature {
        param_kinds,
        return_kind,
        estimated_work,
        // AUDIT 2026-05-17 (round-9 misc CRIT-1): the analyzer cannot
        // see across the dispatch boundary to know whether the launch
        // will be followed by a host read-back. Default `false` so the
        // common pure-device kernel path takes the cheaper no-sync
        // entry point; the launch site flips this to `true` only when
        // it knows a `to_host` will follow.
        needs_d2h_sync: false,
    })
}

/// Walk the bytecode exactly once. Reject on the first forbidden
/// opcode; otherwise return the loop-trip estimate.
fn scan_and_estimate(code: &CodeAttribute) -> Result<usize, Reason> {
    let bytes = &code.code;
    let mut pc = 0usize;
    let mut has_backward = false;
    while pc < bytes.len() {
        let op = bytes[pc];
        match classify(op) {
            OpClass::Ok => {}
            OpClass::Reject(r) => return Err(r),
        }
        // Branch-direction probe lifted from the old `estimate_work`.
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
        pc += instruction_size(bytes, pc)?;
    }
    Ok(if has_backward { 1 << 20 } else { bytes.len().max(1) })
}

enum OpClass {
    Ok,
    Reject(Reason),
}

fn classify(op: u8) -> OpClass {
    match op {
        // Specific rejects come first.
        0x32 | 0x53 => OpClass::Reject(Reason::RefArrayOp),    // aaload, aastore
        0xA5 | 0xA6 => OpClass::Reject(Reason::TypeCheck),     // if_acmpeq, if_acmpne
        0xA8 | 0xA9 | 0xC9 => OpClass::Reject(Reason::JsrRet), // jsr, ret, jsr_w
        0xAA | 0xAB => OpClass::Reject(Reason::Switch),
        0xB2..=0xB5 => OpClass::Reject(Reason::FieldAccess),
        0xB6..=0xBA => OpClass::Reject(Reason::Invoke),
        0xBB | 0xBC | 0xBD | 0xC5 => OpClass::Reject(Reason::Allocation),
        0xBF => OpClass::Reject(Reason::Throw),
        0xC0 | 0xC1 => OpClass::Reject(Reason::TypeCheck),
        0xC2 | 0xC3 => OpClass::Reject(Reason::Monitor),
        // Permitted bands. Note: `wide` (0xC4) is OK; size handled below.
        0x00..=0x31 | 0x33..=0x52 | 0x54..=0xA4 | 0xA7 | 0xAC..=0xB1
        | 0xBE | 0xC4 | 0xC6..=0xC8 => OpClass::Ok,
        other => OpClass::Reject(Reason::UnknownOpcode(other)),
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

// AUDIT 2026-05-17 (Fix 7): `estimate_work` was folded into
// `scan_and_estimate` above so the bytecode is walked exactly once
// per call to `analyze`. The previous version walked the bytes twice
// (once to classify, once to count backward branches) — pure waste on
// methods with thousands of bytecodes.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::load_method;

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
    fn reject_reduction_dot_product() {
        // AUDIT 2026-05-16: previously named `eligible_dot_product_returns_long`.
        // Array-in / scalar-out methods are reduction-shaped; the
        // current emitter can't express a block reduction, so we
        // reject and let the CPU run them.
        let method = load_method("EligibleDotProduct", "dot", "([I[I)J");
        assert_eq!(
            analyze(&method),
            OffloadVerdict::Rejected(Reason::ReductionNotImplemented)
        );
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

    #[test]
    fn reject_type_check() {
        let method = load_method("RejectTypeCheck", "isIntArray", "([I)I");
        assert_eq!(
            analyze(&method),
            OffloadVerdict::Rejected(Reason::TypeCheck)
        );
    }
}
