// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 8, lane W8T1 (STATICFWD21-1, reference half;
//! `CRATONVM_JIT_IR_SPLICE_PUTSTATIC_LOOP_CHAIN`, **opt-in**): may a spliced
//! body that may loop COMMIT its `putstatic` and leave every later trap to the
//! chain snapshots, instead of being refused by the planner as
//! `ir-splice-putstatic-loop`?
//!
//! # The refusal this lifts
//!
//! FG16-3 commits a spliced plain `putstatic` (`IrBuilder::splice_committed_bases`):
//! a trap that resumed at the caller's `invoke` would re-run the body and
//! commit the store twice. The planner (`lib.rs` `append_ir_inline_site`)
//! therefore refuses every body that may loop, because the builder's ROUND-11
//! fence refuses the back edge it reaches after the commit, which costs a
//! whole build. The primitive half (`ir_splice_static_replay`) admits a body
//! whose replay is harmless; a reference store (`sink = new X()`, the alloc
//! kernels) is never harmless to replay, so it stayed refused.
//!
//! # Why the chain route is sound
//!
//! Under the chain fences ([`crate::ir::ir_splice_chain_fences_enabled`],
//! default ON through the chain arm; the planner's `chain_fences` answer, off
//! for an `ACC_SYNCHRONIZED` compiling method) a trap inside a splice resumes
//! the CALLEE at its own bci through the chain snapshot (JVMS frames for every
//! scope), so nothing the body committed is re-run. The builder already
//! admits, after any committed store, an instruction whose chain snapshot it
//! recorded (`relax_fence_by_chain`, the walk-top fence in `IrBuilder::build`)
//! -- backward branches included -- and then marks the graph
//! `splice_chains_required`, so the lowering refuses the compile for any
//! spliced guard without a chain (no flat fall-back that would replay). A
//! spliced back-edge poll is a chain exit or verdict-blind, never a flat
//! re-execute. This is exactly the route a loop body with a committed
//! `putfield` into a caller-visible object already takes (the planner's
//! store-fence walk cannot model a back edge and leaves such a body to the
//! builder); this module only stops the planner from pre-refusing the
//! `putstatic` form of the same shape.
//!
//! The cost of a wrong guess is a builder refusal at the pc with no chain
//! snapshot (a whole build and the W5-2 rebuild without this splice), which is
//! why the switch is opt-in until a host A/B: no answer changes either way.
//!
//! Refused here: a `synchronized` callee (a monitor window publishes no chain:
//! `Lowerer::splice_chain_for_site`), and every compile whose planner answered
//! no chain fences.

/// The opt-in switch (token `ir-splice-putstatic-loop-chain`).
pub(crate) const SWITCH: &str = "CRATONVM_JIT_IR_SPLICE_PUTSTATIC_LOOP_CHAIN";

/// Is the reference half on? Needs FG16-3
/// ([`crate::ir::ir_splice_putstatic_enabled`]), whose rows it admits. Read
/// per use; no static.
pub(crate) fn enabled() -> bool {
    crate::ir::ir_splice_putstatic_enabled() && cratonvm_types::flags::runtime_flag_on(SWITCH)
}

/// The planner's question (`append_ir_inline_site`, before its
/// `ir-splice-putstatic-loop` refusal): may `site`, a body that may loop and
/// commits a plain `putstatic`, be left to the builder's chain fences?
/// `chain_fences` is the planner's answer for this compile (the builder's
/// `splice_chain_fences`). Prints the census line when it admits.
pub(crate) fn planner_admits_chain_loop_site(
    site: &crate::InlineSite,
    caller_pc: usize,
    chain_fences: bool,
) -> bool {
    let admitted = admits(enabled(), chain_fences, site.callee_is_synchronized);
    if admitted && crate::x64::dbg_jitc_enabled() {
        eprintln!("{}", census_line(site, caller_pc));
    }
    admitted
}

/// The one decision: the switch, the compile's chain fences, and no monitor
/// window in the body.
fn admits(on: bool, chain_fences: bool, callee_is_synchronized: bool) -> bool {
    on && chain_fences && !callee_is_synchronized
}

/// `[cratonvm-jitc] ir-splice-putstatic-loop by chain <C>.<m><desc> at caller
/// pc <p>` (`CRATONVM_DBG_JITC=1`): the body the planner handed to the chain
/// fences. A following `IrBuilder::build refused` for the same compile is the
/// body's cost (a pc without a chain snapshot).
fn census_line(site: &crate::InlineSite, caller_pc: usize) -> String {
    format!(
        "[cratonvm-jitc] ir-splice-putstatic-loop by chain {}.{}{} at caller pc {}",
        site.class_name,
        site.method_name,
        site.descriptor,
        crate::ir::inline_site_caller_pc(caller_pc),
    )
}


