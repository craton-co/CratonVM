// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 5 (lane JMMHUNT, `CRATONVM_JIT_IR_ON_SPIN_WAIT_BARRIER`, default
//! ON, `=0` kill): `Thread.onSpinWait()` in the optimizing tier as HotSpot's
//! C2 compiles it -- a memory barrier for the compiler that emits one `PAUSE`.
//!
//! # Why
//!
//! The JDK body is empty (`@IntrinsicCandidate public static void
//! onSpinWait() {}`). Under `--real-jdk` no native shadows it, so the
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
    true
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



// j27 wave 8 (lane W8SPL): the nested barrier row rule.

