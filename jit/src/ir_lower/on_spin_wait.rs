// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 5 (lane JMMHUNT, `CRATONVM_JIT_IR_ON_SPIN_WAIT_BARRIER`, default
//! ON, `=0` kill): `Thread.onSpinWait()` in the optimizing tier as HotSpot's
//! C2 compiles it -- a memory barrier for the compiler that emits one `PAUSE`.
//!
//! # Why
//!
//! The JDK body is empty (`@IntrinsicCandidate public static void
//! onSpinWait() {}`). Under `--jdk-only` no native shadows it, so the
//! optimizing tier SPLICED the empty body: the call vanished from the graph,
//! and with it the only node of a spin loop such as
//!
//! ```java
//! while (!this.ready) Thread.onSpinWait();   // `ready` NOT volatile
//! ```
//!
//! that touched memory. LICM (`ir_optimize::licm`) or the static hoist
//! (`ir_optimize::loop_static_hoist`) then read `ready` once, in the
//! pre-header, and the loop never saw the other thread's store: it spun
//! forever. HotSpot 25 on x86-64 terminates: C2 intrinsifies the call
//! (`LibraryCallKit::inline_onspinwait`, `insert_mem_bar(Op_OnSpinWait)`), and
//! that `MemBar` takes and produces the whole memory state, so the load stays
//! in the loop (and the `MemBar` matches to `PAUSE`). The JMM permits the
//! hoist; HotSpot's Java-visible behaviour does not show it, and code written
//! against HotSpot relies on it.
//!
//! # What
//!
//! * The VM's site resolver keeps the call un-spliced in the optimizing tier
//!   (`jit_bridge::resolve_inline_site_from`, the `on-spin-wait` refusal), so
//!   it stays an `Op::Call`: `MemAccess::Opaque`, a hard LICM barrier, a write
//!   of everything for every load-forwarding pass -- the barrier C2's `MemBar`
//!   is.
//! * [`Lowerer::try_lower_ir_on_spin_wait`] lowers that `Op::Call` as `PAUSE`
//!   and nothing else: no call, no safepoint map (the empty body cannot
//!   collect or throw), the result slot carved for layout uniformity only.
//!   The same shape as `try_lower_ir_reachability_fence`.
//!
//! The single-pass tier needs nothing: it has no field-load hoist across an
//! invoke (its loop hoists refuse every loop holding an `invokestatic`).

use super::{IrType, JitInvokeInfo, Lowerer, NodeId};

/// `CRATONVM_JIT_IR_ON_SPIN_WAIT_BARRIER` (j27 wave 5, lane JMMHUNT; HotSpot
/// parity, **default ON**, `=0` restores the splice of the empty body and the
/// hoist it allowed). Read per site, at compile time; no `static`.
pub fn ir_on_spin_wait_barrier_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_ON_SPIN_WAIT_BARRIER")
}

/// Is this call site `java.lang.Thread.onSpinWait()`?
pub fn is_on_spin_wait_site(class: &str, name: &str, descriptor: &str) -> bool {
    class == "java/lang/Thread" && name == "onSpinWait" && descriptor == "()V"
}

/// `CRATONVM_JIT_IR_ON_SPIN_WAIT_NESTED_BARRIER` (j27 wave 8, lane W8SPL;
/// HotSpot parity, default ON since 2026-10-28 (j27 WARNING flip; =0
/// restores)). The barrier above for a
/// `Thread.onSpinWait()` INSIDE a spliced body too: the VM's resolver refuses
/// the nested splice of the empty body (`jit_bridge::resolve_inline_site_from`,
/// the `on-spin-wait` refusal, then at any `nest_depth`), the row survives as
/// an unbound static call, the planner counts it as bound
/// ([`counts_as_barrier_row`]), the builder builds it as the ordinary
/// `Op::Call` (a hard memory barrier), and [`Lowerer::try_lower_ir_on_spin_wait`]
/// lowers that call to `PAUSE` -- so the unbound row never reaches a call
/// route. Requires the barrier switch itself. Read per decision; no static.
pub fn ir_on_spin_wait_nested_barrier_enabled() -> bool {
    ir_on_spin_wait_barrier_enabled()
        // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_ON_SPIN_WAIT_NESTED_BARRIER")
}

/// Does the unbound row `t` of a spliced body count as bound for the
/// planner's unbindable-call rule? Only the static `Thread.onSpinWait()`, and
/// only while the nested barrier is on: the lowering emits `PAUSE` for it and
/// no call (the twin of `lib.rs` `r22_fence_row::counts_keepalive`).
pub(crate) fn counts_as_barrier_row(t: &crate::InlineInvokeTarget) -> bool {
    t.invoke_kind == 3
        && t.direct_entry.is_none()
        && is_on_spin_wait_site(&t.class_name, &t.method_name, &t.descriptor)
        && ir_on_spin_wait_nested_barrier_enabled()
}

/// `PAUSE` (`F3 90`): the spin-loop hint HotSpot's `OnSpinWait` matches to on
/// x86-64. Touches no register, no flag and no memory.
const PAUSE: [u8; 2] = [0xF3, 0x90];

impl Lowerer<'_> {
    /// `Thread.onSpinWait()` as one `PAUSE` (see the module notes). Called in
    /// the `Op::Call` arm where `try_lower_ir_reachability_fence` is, before
    /// the safepoint map and before a byte of the call. Declines (keeps the
    /// call) for anything but the static no-argument void site.
    pub(super) fn try_lower_ir_on_spin_wait(&mut self, id: NodeId, info_ptr: usize) -> bool {
        if info_ptr == 0 || !ir_on_spin_wait_barrier_enabled() {
            return false;
        }
        // SAFETY: as every other read of an `Op::Call`'s `info_ptr` in
        // `ir_lower` -- a live `JitInvokeInfo` owned by `ir_call_infos`.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        if info.invoke_kind != 3
            || !is_on_spin_wait_site(info.class_name, info.method_name, info.descriptor)
        {
            return false;
        }
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return false;
        };
        // `[ctrl, mem]`: no argument.
        if node.inputs.len() != 2 || node.ty != IrType::Void {
            return false;
        }
        // The slot every `Op::Call` takes, for layout uniformity; nothing
        // writes it (the node is void).
        let _ = self.alloc_slot(id);
        self.buf.emit(&PAUSE);
        if crate::x64::dbg_jitc_enabled() {
            eprintln!("[cratonvm-jitc] ir-on-spin-wait n{id} (PAUSE, no call)");
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::IrBuilder;
    use crate::ir_schedule;
    use crate::JitRuntimeHelpers;
    use std::collections::HashMap;

    const SWITCH: &str = "CRATONVM_JIT_IR_ON_SPIN_WAIT_BARRIER";

    #[test]
    fn j27_jmm_the_site_is_the_exact_static_triple() {
        assert!(is_on_spin_wait_site("java/lang/Thread", "onSpinWait", "()V"));
        assert!(!is_on_spin_wait_site("java/lang/Thread", "yield", "()V"));
        assert!(!is_on_spin_wait_site("java/lang/Thread", "onSpinWait", "(I)V"));
        assert!(!is_on_spin_wait_site("pkg/Thread", "onSpinWait", "()V"));
    }

    #[test]
    fn j27_jmm_the_barrier_switch_is_default_on_with_a_zero_kill() {
        let read = |v: Option<&str>| {
            cratonvm_types::flags::with_thread_overrides(&[(SWITCH, v)], ir_on_spin_wait_barrier_enabled)
        };
        assert!(read(None), "unset is on");
        assert!(!read(Some("0")), "=0 is off");
    }

    /// The call route's helper, as a marker address the generic call route
    /// bakes as `MOV RAX, imm64` (never executed; the ir_lower IC tests'
    /// convention).
    const DISPATCH_MARKER: u64 = 0x1111_2222_3333_4440;

    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    /// `static void f() { Thread.onSpinWait(); }` lowered with the switch as
    /// given: does its code bake the dispatch helper, and does it hold a
    /// `PAUSE`?
    fn lowered(switch: Option<&str>) -> (bool, bool) {
        crate::x64::set_moving_young_override(Some(false));
        // invokestatic #2 ; return
        let code = [0xb8, 0x00, 0x02, 0xb1];
        let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
            class_name: "java/lang/Thread",
            method_name: "onSpinWait",
            descriptor: "()V",
            num_jit_args: 0,
            return_type: b'V',
            invoke_kind: 3, // static
            declaring_class_id: 0,
            owner_class_id: 0,
        }));
        let mut builder = IrBuilder::new(0, 0);
        builder.set_param_types(&[]);
        let mut invoke_info = HashMap::new();
        invoke_info.insert(0usize, (info as *const JitInvokeInfo as usize, 0usize, b'V'));
        builder.set_invoke_info(invoke_info);
        let graph = builder.build(&code, code.len()).expect("IR build of the static call");
        let schedule = ir_schedule::schedule(&graph);
        // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a
        // `usize`, so all-zero is a valid value; the test wires the call route.
        let mut helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        helpers.invoke_dispatch = DISPATCH_MARKER as usize; // Cast: a 64-bit target
        // The call route's other slots, as the IC tests wire them.
        helpers.dispatch_threw = 0x1111_2222_3333_4442;
        helpers.service_callee_deopt = 0x1111_2222_3333_4443;
        let cm = cratonvm_types::flags::with_thread_overrides(&[(SWITCH, switch)], || {
            super::super::lower(&graph, &schedule, 0, 0, &helpers)
        })
        .expect("the call site lowers");
        let code = cm.code_bytes();
        (contains(code, &DISPATCH_MARKER.to_le_bytes()), contains(code, &PAUSE))
    }

    /// The fix: the call is one `PAUSE` and bakes no dispatch. `=0`: the call
    /// as before.
    #[test]
    fn j27_jmm_on_spin_wait_lowers_to_pause_and_the_kill_keeps_the_call() {
        let (dispatches, pause) = lowered(None);
        assert!(!dispatches, "the barrier arm makes no call");
        assert!(pause, "the barrier arm emits PAUSE");
        let (dispatches, _) = lowered(Some("0"));
        assert!(dispatches, "=0 keeps the call to the (empty) body");
    }
}

/// j27 wave 8 (lane W8SPL): the nested barrier row rule.
#[cfg(test)]
mod j27_w8spl_nested_barrier_tests {
    use super::*;

    const NESTED: &str = "CRATONVM_JIT_IR_ON_SPIN_WAIT_NESTED_BARRIER";
    const BARRIER: &str = "CRATONVM_JIT_IR_ON_SPIN_WAIT_BARRIER";

    fn row(kind: u8, class: &str, method: &str, desc: &str) -> crate::InlineInvokeTarget {
        crate::InlineInvokeTarget {
            class_name: class.to_string(),
            method_name: method.to_string(),
            descriptor: desc.to_string(),
            invoke_kind: kind,
            direct_entry: None,
            ..crate::InlineInvokeTarget::default()
        }
    }

    fn counts(t: &crate::InlineInvokeTarget, nested: Option<&str>, barrier: Option<&str>) -> bool {
        cratonvm_types::flags::with_thread_overrides(&[(NESTED, nested), (BARRIER, barrier)], || {
            counts_as_barrier_row(t)
        })
    }

    /// Only the unbound static `onSpinWait()` row counts, only under both
    /// switches (both default ON; nested since the 2026-10-28 j27 WARNING
    /// flip, `=0` kills either); a bound row, another
    /// triple or another kind is not this rule's question.
    #[test]
    fn j27_w8spl_only_the_static_on_spin_wait_row_counts_as_a_barrier() {
        let spin = row(3, "java/lang/Thread", "onSpinWait", "()V");
        assert!(counts(&spin, None, None), "default ON: unset is on");
        assert!(!counts(&spin, Some("0"), None), "=0 is off");
        assert!(counts(&spin, Some("1"), None), "=1 with the default barrier");
        assert!(!counts(&spin, Some("1"), Some("0")), "the barrier's kill wins");
        assert!(!counts(&row(3, "java/lang/Thread", "yield", "()V"), Some("1"), None));
        assert!(!counts(&row(0, "java/lang/Thread", "onSpinWait", "()V"), Some("1"), None));
        let mut bound = spin.clone();
        bound.direct_entry = Some((0x1000, false));
        assert!(!counts(&bound, Some("1"), None));
    }
}
