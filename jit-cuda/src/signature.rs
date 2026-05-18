//! Kernel signature: the GPU-side view of a Java method's parameter
//! and return types.

use crate::analyzer::ParamKind;

/// What an offload-eligible method looks like to the kernel launcher.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KernelSignature {
    /// One entry per declared Java parameter (in source order).
    pub param_kinds: Vec<ParamKind>,
    /// Java return type. `ParamKind::Void` for `void` methods.
    pub return_kind: ParamKind,
    /// Rough work estimate (see analyzer). The interpreter uses this
    /// to decide whether the round-trip overhead is worth it for
    /// small inputs.
    pub estimated_work: usize,
    /// Phase 9 #2 — constant-pool indices of `this.<field>` accesses
    /// the body makes via the `aload_0; getfield <cp_index>` pattern.
    /// Empty for static methods (the standard `getfield` opcode is
    /// rejected outside the receiver-access pattern). For non-static
    /// methods this is the ordered list of fields the marshaller must
    /// extract from the receiver and pass as extra kernel args; the
    /// lowering layer maps each `aload_0; getfield <cp_index>` pair
    /// to the matching arg slot.
    ///
    /// Duplicates are preserved in body-encounter order; the
    /// dispatcher / emitter can de-dup if it wants but that's
    /// optimisation. The simplest implementation passes each access
    /// as a distinct kernel arg.
    pub this_field_cps: Vec<u16>,
}
