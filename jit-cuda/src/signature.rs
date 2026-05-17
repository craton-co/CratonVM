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
    /// AUDIT 2026-05-17 (round-9 misc CRIT-1): does the launch need the
    /// post-launch event recorded so a subsequent host read-back is
    /// stream-ordered behind the kernel?
    ///
    /// `false` (the default) means the call site can use
    /// `DeviceModule::launch_raw_no_sync`, saving ~3µs of CPU-side
    /// driver overhead per launch. Most JIT-emitted kernels are pure
    /// device-side compute and never feed a `to_host`, so they leave
    /// this `false`. Set to `true` only when the caller knows a
    /// `DeviceBuffer::to_host` will follow without an intervening
    /// `synchronize` or another sync-mode launch.
    pub needs_d2h_sync: bool,
}
