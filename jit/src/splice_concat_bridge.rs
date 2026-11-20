// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 4, lane SPLICECAT (`CRATONVM_JIT_SPLICE_CONCAT_BRIDGE`, opt-in):
//! a spliced body's `StringConcatFactory` bridge sites of any arity as
//! [`crate::InlineSite::ir_indy_rows`] rows.
//!
//! Page: `docs/feature-designs/jit-ir-optimizations.md`.
//!
//! Before this, a spliced `invokedynamic` was admitted only as a `(0, L)`
//! constant-word lambda row, so any small callee with `"x=" + v` anywhere in
//! its body kept its call. A concat row is `(callee pc, argument entries > 0,
//! b'L', concat bridge site)`: the VM resolver
//! (`jit_bridge::splice_concat_rows`) admits only a site whose kind tag is
//! `JIT_INDY_SITE_CONCAT`, the pool-built site the compiled body of the callee
//! itself calls. Both tiers then emit the SAME bridge call with the SAME
//! staged operands as that body:
//!
//! * optimizing tier: `IrBuilder::push_indy_bridge_call` builds the
//!   `Op::IndyBridge` (lowered exactly as a method-own bridged site: safepoint
//!   map before the staging, inline-frame row at the return address, the
//!   `i64::MIN` exception edge), and the builder records the node as a call
//!   the splice made (`IrBuilder::note_splice_call`): an operand `toString()`
//!   is arbitrary Java, so a later trap in the same splice may not replay the
//!   invoke, and the successor gets the post-call chain snapshot (the callee
//!   frame with the concat result on its operand stack);
//! * single-pass tier: `x64::inlining`'s `0xba` arm stages the operands and
//!   calls the bridge like the spliced dispatch call, then files the post-call
//!   exit as after any spliced call.
//!
//! The planner counts a concat row as a surviving call
//! ([`concat_row_at`]): a body that may trap after one, or that stores before
//! one, keeps its call rather than costing the caller its build.
//!
//! Opt-in rather than default ON: the change moves where operand
//! `toString()`s and their exceptions run (inside the caller's compiled
//! frame), which only a host run of the probe
//! (`tools/probes/jit/j27/J27SpliceConcatRows.java`) can show matches HotSpot.

/// `CRATONVM_JIT_SPLICE_CONCAT_BRIDGE` (j27 wave 4, lane SPLICECAT; perf,
/// **opt-in**, `=1` enables): may a spliced body's concat bridge site with
/// arguments be an `ir_indy_rows` row, in both tiers? Read per resolution, per
/// rebase and per build (no static); the VM resolver reads the same switch.
pub fn splice_concat_bridge_enabled() -> bool {
    false}

/// May a spliced `ir_indy_rows` row of this shape be rebased? `(0, L)` is the
/// constant-word lambda row (W10L3-1); `(n > 0, L)` is a concat bridge row,
/// under [`splice_concat_bridge_enabled`] only.
pub fn indy_row_shape_admitted(arg_entries: usize, ret_tag: u8) -> bool {
    ret_tag == b'L' && (arg_entries == 0 || splice_concat_bridge_enabled())
}

/// The concat row of `site` at callee pc `pc` (a row with arguments), if any.
pub fn concat_row_at(site: &crate::InlineSite, pc: usize) -> Option<(usize, usize, u8, usize)> {
    site.ir_indy_rows
        .iter()
        .find(|r| r.0 == pc && r.1 > 0)
        .copied()
}

/// Does `site` hold a concat row? Such a body leaves a call behind.
pub fn site_has_concat_rows(site: &crate::InlineSite) -> bool {
    site.ir_indy_rows.iter().any(|r| r.1 > 0)
}

/// Does `op` commit a store a replay would repeat (a field, static or array
/// element write)? A concat row after one keeps the site a call: the builder's
/// fence admits no call after a committed store, and refusing there costs the
/// whole method.
pub fn commits_a_store(op: u8) -> bool {
    matches!(op, 0x4f..=0x56 | 0xb3 | 0xb5)
}

/// The replay planner's (`ir_splice_body_may_trap_after_a_call`) step for
/// `op` at callee pc `pc`, walked in pc order: `None` when a concat row there
/// follows a store the walk has seen (the site keeps its call), else whether
/// the instruction is a concat call. Records a store in `store_seen`.
pub fn planner_concat_step(
    site: &crate::InlineSite,
    pc: usize,
    op: u8,
    store_seen: &mut bool,
) -> Option<bool> {
    let concat = op == 0xba && concat_row_at(site, pc).is_some();
    if concat && *store_seen {
        return None;
    }
    *store_seen |= commits_a_store(op);
    Some(concat)
}

#[cfg(test)]
mod tests;
