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
}
