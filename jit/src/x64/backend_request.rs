// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! What one single-pass backend compile is asked to do beyond the bytecode
//! and its resolved side tables.
//!
//! Each of these used to be a thread-local that a door staged before calling
//! `compile_with_param_slots`, which took (cleared) it at entry
//! (`jit-god-functions-and-request-side-channels-FIXED-20260912.md`). A value
//! passed to the call cannot outlive it, so a front end that bails between
//! staging and the call can no longer hand its request to the next method
//! compiled on that thread.

/// The per-compile request for [`super::compile_with_param_slots`].
///
/// `BackendRequest::default()` is the request of a caller with no method
/// identity (the `compile` test wrapper, the loop-unroll fixture): no helper
/// table, no handler table, no register homes. It produces the codegen those
/// callers had when they staged nothing.
#[derive(Clone, Debug, Default)]
pub struct BackendRequest {
    /// The VM's thin direct-call helper addresses and compile-time resolvers.
    pub direct_helpers: crate::DirectHelperTable,
    /// The reader/verifier `max_stack`. `None` keeps the local estimator.
    pub verified_max_stack: Option<usize>,
    /// The method's exception table as `(start_pc, end_pc, handler_pc)`.
    /// `find_bypassable_loop_headers` treats a handler entered from outside a
    /// loop as an external entry into that loop's header.
    pub exception_ranges: Vec<(usize, usize, usize)>,
    /// The same table with catch types, so the backend can emit its own
    /// `catch` blocks: `(start_pc, end_pc, handler_pc, catch type name)`, an
    /// empty name being a catch-all. The names must outlive the compiled code
    /// (`intern_catch_type_name`). Empty means no local handlers.
    pub local_handler_table: Vec<(usize, usize, usize, &'static str)>,
    /// The declaring class id the catch-type names above resolve through.
    pub local_handler_class: u32,
    /// Pure-kernel GPR local homes for a method-entry compile. See
    /// [`super::kernel_reg_locals_enabled`].
    pub kernel_reg_homes: bool,
    /// Pure-kernel GPR local homes for an OSR artifact, which keeps its OSR
    /// entries: the trampoline seeds every local into its assigned register.
    /// Also gated on `CRATONVM_JIT_KERNEL_REG_OSR`.
    pub kernel_reg_homes_osr: bool,
    /// A handler reads locals beyond the incoming parameters, so post-invoke
    /// exceptions keep a precise frame until handler entry.
    pub precise_exception_frames: bool,
    /// May the single-pass backend scalar-replace under
    /// [`Self::precise_exception_frames`]?
    ///
    /// Until 2026-09-22 it could not, unconditionally: `x64/driver.rs` handed
    /// `plan_scalar_replacement` the EMPTY set whenever the flag above was set.
    /// That is what made the backend's entire monitor-relock feature ("Phase C")
    /// unreachable, because javac's mandatory monitor handler sets the flag on
    /// every `synchronized` block ever compiled from Java source.
    ///
    /// It is a separate field rather than a `!precise_exception_frames` test
    /// because the admission has one precondition the backend cannot check for
    /// itself: **the method must not be `ACC_SYNCHRONIZED`.** A reason-9 frame
    /// naming a scalar-replaced object is rebuilt by
    /// `exception_dispatch::materialize_and_relock_precise_frame`, which refuses
    /// a synchronized method for the reason `build_deopt_frame_inner` does — its
    /// method monitor is taken by the invoke path, not by a `monitorenter`, so
    /// it is not a frame-state entry and an elision of it would leave no trace.
    /// A refusal there PROPAGATES the exception, which is a wrong answer for a
    /// method whose handler should have caught it, so the shape must not be
    /// created in the first place.
    ///
    /// `false` also when `CRATONVM_JIT_SCALAR_UNDER_PRECISE_FRAMES=0`, the kill
    /// switch for the whole admission.
    pub allow_scalar_under_precise_frames: bool,
    /// `[start_pc, end_pc)` of the method's protected ranges. A sibling
    /// tail-call inside one would unwind past its handler, so it is suppressed.
    pub protected_ranges: Vec<(u32, u32)>,
    /// When true, the single-pass backend compiles as a pure baseline compiler:
    /// fast, with no speculative passes (EA/scalar replacement, speculative BCE
    /// loop guards, LICM hoists, loop unrolling, and inlining skipped).
    pub baseline_mode: bool,
    /// Bytecode pcs of the `getfield` / `putfield` sites whose field is
    /// declared `volatile`. Every `putfield` pc in this set is followed by
    /// the JMM's StoreLoad fence (`MFENCE`), the instance twin of the fence
    /// the `putstatic` arm has always emitted for a volatile static. A load
    /// needs no fence on x86-64 (TSO loads are acquires); the set still names
    /// the `getfield` sites so no transform ever treats one as invariant.
    ///
    /// Empty = no volatile instance access (or a caller with no field
    /// metadata). Pcs are the method's own; the backend lifts them through
    /// the bytecode loop rewriter with every other pc-keyed table
    /// (`volatile-instance-fields-are-plain-accesses-in-compiled-code`).
    pub volatile_field_pcs: std::collections::HashSet<usize>,
    /// `(holder class id, cp index) -> slot address` for `ldc <String>` /
    /// `ldc <Class>` sites whose resolved constant the VM keeps in a
    /// GC-maintained word (`JitLdcConstant::StringSlot` / `ClassMirrorSlot`).
    /// Keyed by SITE, not pc, so the loop rewriter needs no lifting. Empty =
    /// every site calls its helper.
    pub ldc_slots: std::collections::HashMap<(u32, u16), usize>,
    /// Address of the `JitInvokeInfo` describing THIS method as the callee of
    /// its own raw self-recursive `CALL` sites. `0` = none, which is also what
    /// a caller with no method identity produces.
    ///
    /// ONE per compile, not one per pc: the callee of every self-recursive
    /// site in a method is that same method, so the name/descriptor/kind the
    /// deopt service resolves through are identical at all of them.
    ///
    /// This exists because the self-call sites are the one raw JIT-to-JIT call
    /// shape the planner deliberately gives no `invoke_info` row (it `continue`s
    /// past the registration so the codegen picks its self-recursive arm rather
    /// than the dispatch helper). That left `op_invoke`'s self-call arm with
    /// nothing to hand [`super::Compiler::emit_inline_callee_deopt_check`], so
    /// the arm alone among the backend's raw-CALL routes never gave the
    /// callee's own exception table a look before treating the callee's
    /// `i64::MIN` return as this frame's unwind — see that emission for the
    /// miscompile. Carrying the info separately keeps the routing decision
    /// (which reads `invoke_info`) exactly as it was.
    ///
    /// Stored as an address rather than a `*const JitInvokeInfo` so the request
    /// keeps its auto traits; the pointee is owned by the same
    /// `owned_invoke_infos` vector as every other site's info and outlives the
    /// compile with the artifact.
    pub self_call_invoke_info: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_request_asks_for_nothing() {
        let request = BackendRequest::default();
        assert_eq!(request.direct_helpers, crate::DirectHelperTable::EMPTY);
        assert_eq!(request.verified_max_stack, None);
        assert!(request.exception_ranges.is_empty());
        assert!(request.local_handler_table.is_empty());
        assert!(request.protected_ranges.is_empty());
        assert!(!request.kernel_reg_homes);
        assert!(!request.kernel_reg_homes_osr);
        assert!(!request.precise_exception_frames);
        assert!(!request.baseline_mode);
        assert!(request.volatile_field_pcs.is_empty());
        assert!(request.ldc_slots.is_empty());
    }
}
