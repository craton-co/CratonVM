// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

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

    /// Bit-set of parameter indices the kernel READS element-wise.
    ///
    /// Mirror of `writes_param_mask`. The chunked writeback commits each
    /// chunk into the Java array as its event fires, which is before the
    /// bounds-failure flag has been read; that is only safe for an array
    /// the kernel does not also read, so `writes & !reads` is the set a
    /// chunked dispatch may stream out early. A caller that cannot supply
    /// this should default it to "all bits set", which refuses chunking.
    pub reads_param_mask: u64,

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

    /// Annotation policy propagated from `@GpuKernel(admit =
    /// ALLOW_DIV_BY_ZERO)`. Despite the name, this single flag gates two
    /// independent lowering decisions — see the "AllowDivByZero-reuse"
    /// note on `analyzer::classify`'s `0x72 | 0x73` arm and the doc
    /// comment on `analyzer::Reason::FloatRemainder` for the full "why
    /// one hint, not two" rationale:
    ///
    /// - Integer `idiv`/`ldiv`/`irem`/`lrem`: when true, lowering skips
    ///   only the explicit divisor-zero deopt guard. Other guards, such
    ///   as signed-minimum divided by `-1`, remain in force.
    /// - `frem`/`drem` (AUDIT 2026-07-11): the analyzer only admits
    ///   these two opcodes at all under this hint — under `Strict` they
    ///   still reject with `Reason::FloatRemainder` before ever reaching
    ///   lowering. The div+truncate+fma identity
    ///   (`lowering::emit::Emitter::frem_f32`/`drem_f64`) is bit-exact
    ///   only while the quotient magnitude `|dividend / divisor|` stays
    ///   within the type's exactly-representable-integer range (`<
    ///   2^24` for `float`, `< 2^53` for `double`); this flag is the
    ///   opt-in that accepts that approximation, reused rather than a
    ///   dedicated `AdmissionHint` variant because it is already the
    ///   "accept looser numeric edge-case semantics for a
    ///   division-family opcode" opt-in and `frem`/`drem` are literally
    ///   the floating counterparts of `irem`/`lrem`.
    ///
    /// `lcmp`/`fcmpl`/`fcmpg`/`dcmpl`/`dcmpg` do NOT consult this flag —
    /// unlike `frem`/`drem`, their `setp`+`selp` lowering
    /// (`lowering::emit::Emitter::lcmp`/`cmp_f32`/`cmp_f64`) is
    /// unconditionally bit-exact, so the analyzer admits them under
    /// `Strict` with no gating hint at all (see
    /// `analyzer::Reason::Compare`'s doc comment).
    pub allow_div_by_zero: bool,
}
