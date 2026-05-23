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
    /// Phase 10 #2 — bit-set of array-parameter indices that the
    /// kernel body writes to (via `iastore`/`lastore`/`fastore`/
    /// `dastore`/`bastore`/`castore`/`sastore`). Bit `i` (LSB-first)
    /// corresponds to `param_kinds[i]`; clear bits are read-only.
    ///
    /// The marshaller uses this to suppress the post-launch D→H
    /// copy + heap write-back for read-only array inputs — closing
    /// the residual TornadoVM performance gap left by the input
    /// residency cache (Phase 10 #1). Before this signal, every
    /// array param was conservatively treated as `inout`, paying a
    /// full D→H copy on every submit even when the kernel never
    /// touched the array (e.g. `a`, `b` in `vectorAdd(a, b, out)`).
    ///
    /// Populated by `lower_method` from the emitter's per-store
    /// `array_param_of` tracking, which already knows precisely
    /// which parameter index each `*astore` targets. The default
    /// (`0` — no params written) is what unit tests construct;
    /// real lowering populates it before the kernel is cached.
    ///
    /// Width: `u64`. Methods with more than 64 array params are
    /// hypothetical; the analyzer caps acceptance well below that.
    /// If a future kernel shape ever exceeds 64 params, callers
    /// should default `writes_param_mask` to "all bits set" so the
    /// conservative writeback behaviour is preserved.
    pub writes_param_mask: u64,
}
