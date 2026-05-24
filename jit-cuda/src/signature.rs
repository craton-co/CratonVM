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

    /// AUDIT 2026-05-24 (C31): the analyzer recognised this method as a
    /// dot-product / sum reduction (counted loop, array load, arithmetic
    /// `*add`, scalar return). The lowering layer turns the per-thread
    /// `*return` into an `atom.global.add.<suffix>` against `ret_ptr`
    /// instead of a racing plain `st.global.<suffix>`, so every thread's
    /// partial contribution accumulates correctly into the single output
    /// slot. The host marshaller MUST pre-zero `*ret_ptr` before launch
    /// for the atomic accumulation to land on the correct identity
    /// (`0` for sum/dot — matches Java initialiser of the accumulator).
    ///
    /// `false` for non-reduction shapes (element-wise stores into output
    /// arrays, void returns, straight-line scalar returns where every
    /// thread computes the same value and racing on `ret_ptr` is benign).
    pub is_reduction: bool,
}
