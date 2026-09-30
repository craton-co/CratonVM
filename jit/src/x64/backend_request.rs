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
    /// One bit per [`Self::exception_ranges`] entry, in the same (table)
    /// order: `true` for a catch-all (`catch_type == 0`). A catch-all ends
    /// the JVM's handler search for the pcs it covers (JVMS §2.10), so no
    /// later entry is reachable from them; the taken-monitor analysis
    /// (`x64/deopt_stubs.rs` `taken_monitor_slots_per_pc`) stops there
    /// (interpreter round i1 wave 27, lane L2). A length that differs from
    /// `exception_ranges` (the default: empty) is read as "no entry is a
    /// catch-all", which only adds edges — the direction that answers less.
    pub exception_catch_all: Vec<bool>,
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
    /// the JMM's StoreLoad fence (`LOCK ADD [RSP], 0`), the instance twin of the fence
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
    /// Every `Integer.valueOf` / `Function.apply` / `checkcast Double` /
    /// `Double.doubleValue` run in this method is proven by its constant pool
    /// ([`crate::lambda_int_to_double_runs_proven`]), so the `invokestatic` arm
    /// may fold a run into one `jit_lambda_int_to_double` call. `false` (the
    /// default) folds nothing. Replaces the `org/elasticsearch/tdigest/Dist`
    /// method-name test (interpreter round i1 wave 10).
    pub lambda_int_to_double_proven: bool,
    /// This compile is an OSR-tier artifact: it runs inside the activation of
    /// the thread's innermost interpreter frame, so an admitted back-edge poll
    /// leaves on the slow path's [`crate::SAFEPOINT_VERDICT_INNERMOST_FRAME`]
    /// verdict ("this frame must run interpreted now": a JVMTI
    /// interpreter-only event or a JDWP request came into force while the loop
    /// ran; any non-zero verdict until wave 17). Only the slow path changes; the fast
    /// path is byte-identical. See `Compiler::mode_exit_target`.
    ///
    /// `false` (the default, every method-entry compile): since interpreter
    /// round i1 wave 15 the same polls are admitted, but leave only on
    /// [`crate::SAFEPOINT_VERDICT_POLLING_BODY`], the answer about the body
    /// that polled (wave 17; before it, about every method); the VM's stash
    /// sinks take such an exit uncharged
    /// (`jvmti_events::exit_left_for_the_interpreter`).
    pub mode_exit_polls: bool,
    /// A debugger may read this compile's locals (the VM runs a JDWP agent;
    /// `CompileRequest::debugger_observes_locals`). Every definitely assigned
    /// local then keeps its home until its next store, and every deopt
    /// snapshot describes it even where it is dead, as HotSpot does under
    /// `can_access_local_variables` (`regalloc::allocate_registers_keeping_assigned`,
    /// `build_frame_state_at`; interpreter round i1 wave 21, lane L3). A mode
    /// exit whose resumed frame would still show one as `0` is refused, and
    /// the body keeps running compiled (`Compiler::frame_hides_an_assigned_local`;
    /// wave 19, lane L1). `false` (the default) compiles exactly as before.
    pub debugger_observes_locals: bool,
    /// The parameter region's per-slot kinds, from the method descriptor
    /// ([`crate::param_slot_kind_tags`]): the width source for a primitive
    /// parameter the method never loads or stores, which the backend's
    /// load/store kind scan (`bce::classify_local_kinds`) leaves `Unknown`.
    /// Read only under [`Self::debugger_observes_locals`], and only for a slot
    /// the scan left `Unknown` (`bce::seed_unread_parameter_kinds`): such a
    /// parameter is kept alive and definitely assigned from bci 0, so without
    /// a kind every snapshot described it `Undefined` and every mode exit of
    /// the method was refused (interpreter round i1 wave 22, lane L1;
    /// `interpreter-L1-an-unread-primitive-parameter-has-no-kind-in-single-pass-snapshots-FIXED-20260926.md`).
    /// Empty (the default, and every compile without a debugger) seeds
    /// nothing.
    pub param_slot_tags: Vec<u8>,
    /// Round 13 wave 4 (lane sync2): compile this `ACC_SYNCHRONIZED` INSTANCE
    /// method's method-entry body as a SELF-LOCKING body -- the prologue enters
    /// the receiver's monitor and every exit of the body releases it, as
    /// HotSpot compiles a synchronized method -- instead of the wrapped entry
    /// whose monitor a door supplies. Set only by the method-entry door for a
    /// method `crate::self_lock_admits_method` admits; the backend re-checks
    /// what only the finished body can tell (`x64/driver.rs`,
    /// `self_lock_exits_are_closed`) and refuses the compile otherwise, and
    /// the door then compiles the ordinary wrapped entry. `false` (the
    /// default, every OSR and eager door) compiles exactly as before. The OSR
    /// door's request may carry `true`; the backend ignores it there.
    ///
    /// Round 13 wave 6 (lane sync3): no poll of such a body leaves for the
    /// interpreter (`x64/safepoint.rs` `holds_self_lock_no_frame_names`), so
    /// a method with a loop can be admitted
    /// (`CRATONVM_JIT_SELF_LOCKING_SYNC_LOOPS`).
    pub self_lock_receiver: bool,
    /// Round 13 wave 10 (lane sync6): for a `static synchronized` method under
    /// [`Self::self_lock_receiver`], the VM's GC-maintained word holding its
    /// class's `Class` mirror -- the monitor (JVMS 2.11.10) --
    /// (`crate::CompileRequest::self_lock_mirror_slot`). The body then locks
    /// the mirror it reads from this word, through a scratch frame word
    /// refreshed before every use (`x64/frames.rs`
    /// `emit_self_lock_refresh_mirror_word`), instead of local 0. `0` (the
    /// default, and every instance method) is the receiver-locking body.
    pub self_lock_mirror_slot: usize,
}

impl BackendRequest {
    /// Stage the method's exception table: [`Self::exception_ranges`],
    /// [`Self::exception_catch_all`] and [`Self::protected_ranges`], in table
    /// order, replacing whatever they held. An empty table stages three empty
    /// vectors (the default).
    ///
    /// The ONE staging every door that calls
    /// [`super::compile_with_param_slots`] with a method's table shares: the
    /// method-entry door (`single_pass_tier`), the OSR door
    /// (`jit_bridge::compile_osr_body`) and the eager first-call door
    /// (`interpreter.rs` `execute`). The third staged none of them until
    /// interpreter round i1 wave 28 (lane L2), so every backend analysis that
    /// takes handler edges from the table (the local-oop masks, the bypassable
    /// loop headers, the taken-monitor analysis) ran on a method with a
    /// `catch` as if it had none
    /// (`docs/internal/fixed-bugs/interpreter-L2-the-eager-first-call-door-compiles-a-handler-method-without-its-exception-table-FIXED-20260929.md`).
    /// [`Self::precise_exception_frames`] is the door's own decision (the
    /// method-entry and eager doors ask `crate::handler_resume_requires_precise_locals`
    /// and `crate::precise_handler_frame_blocking_site`; the OSR door asks for
    /// it for every table), and is not set here.
    pub fn stage_exception_table(
        &mut self,
        table: &[cratonvm_reader::attribute::ExceptionTableEntry],
    ) {
        // Widening: classfile pcs are u16.
        self.exception_ranges = table
            .iter()
            .map(|e| (e.start_pc as usize, e.end_pc as usize, e.handler_pc as usize))
            .collect();
        self.exception_catch_all = table.iter().map(|e| e.catch_type == 0).collect();
        self.protected_ranges = table
            .iter()
            .map(|e| (u32::from(e.start_pc), u32::from(e.end_pc)))
            .collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Interpreter round i1 wave 28, lane L2: the shared staging fills the
    /// three table fields in table order, the catch-all bit from
    /// `catch_type == 0`, and leaves `precise_exception_frames` alone.
    #[test]
    fn the_exception_table_is_staged_in_table_order() {
        use cratonvm_reader::attribute::ExceptionTableEntry;
        let table = [
            ExceptionTableEntry {
                start_pc: 2,
                end_pc: 9,
                handler_pc: 12,
                catch_type: 7,
            },
            ExceptionTableEntry {
                start_pc: 2,
                end_pc: 12,
                handler_pc: 20,
                catch_type: 0,
            },
        ];
        let mut request = BackendRequest::default();
        request.stage_exception_table(&table);
        assert_eq!(request.exception_ranges, vec![(2, 9, 12), (2, 12, 20)]);
        assert_eq!(request.exception_catch_all, vec![false, true]);
        assert_eq!(request.protected_ranges, vec![(2, 9), (2, 12)]);
        assert!(!request.precise_exception_frames);
        request.stage_exception_table(&[]);
        assert!(request.exception_ranges.is_empty());
        assert!(request.exception_catch_all.is_empty());
        assert!(request.protected_ranges.is_empty());
    }

    #[test]
    fn the_default_request_asks_for_nothing() {
        let request = BackendRequest::default();
        assert_eq!(request.direct_helpers, crate::DirectHelperTable::EMPTY);
        assert_eq!(request.verified_max_stack, None);
        assert!(request.exception_ranges.is_empty());
        assert!(request.exception_catch_all.is_empty());
        assert!(request.local_handler_table.is_empty());
        assert!(request.protected_ranges.is_empty());
        assert!(!request.kernel_reg_homes);
        assert!(!request.kernel_reg_homes_osr);
        assert!(!request.precise_exception_frames);
        assert!(!request.baseline_mode);
        assert!(request.volatile_field_pcs.is_empty());
        assert!(request.ldc_slots.is_empty());
        assert!(!request.mode_exit_polls);
        assert!(!request.debugger_observes_locals);
        assert!(request.param_slot_tags.is_empty());
        assert!(!request.self_lock_receiver);
        assert_eq!(request.self_lock_mirror_slot, 0);
    }
}
