// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 4 (lane OSRSUPER; P2 of
//! `docs/feature-designs/jit-osr-exit-and-recompile.md`):
//! the single-pass OSR door's guarded pass may splice a receiver body whose
//! only calls are DIRECT-BOUND `super.m()` calls.
//!
//! The shape is NIO's covariant setter override,
//! `ByteBuffer.position(I)ByteBuffer { super.position(p); return this; }`:
//! one `invokespecial Buffer.position(I)Buffer`, not `<init>`, of a class other
//! than the body's own. `plan_osr_door_guarded_splices` used to hand the VM's
//! receiver resolver no direct binder, so the resolver refused every
//! call-carrying body ("neither spliced nor direct-bound") and the planner
//! logged `CalleeUnresolved`.
//!
//! Why the old rule was "no binder" (git history, read only): the
//! method-entry doors' comment (`facd6c8ff8`, 2026-08-18) says the binder
//! closure "is declared further down this function, and a spliced body reached
//! through a receiver guard is planned before it exists. The consequence is a
//! REFUSAL, never a downgrade". "Downgrade" is a spliced call emitted through
//! the blind `jit_invoke_dispatch` helper (measured as a 3.5x pessimisation);
//! the rule keeps a call-carrying body from being spliced with an unbound
//! call. Lane mega copied the `None` to the single-pass OSR door (`24b8dc53b2`,
//! round 16 wave 13), where the door's static binder (`osr_splice_bind`)
//! already exists before the guarded pass runs. So binding does not break the
//! rule; leaving a call unbound would, and [`body_calls_only_bound_super`]
//! refuses exactly that.
//!
//! Soundness of the bound call inside a guarded body:
//!  * selection: `invokespecial` of a superclass method selects from the
//!    caller's superclass whatever the receiver's class is (JVMS 6.5); the VM
//!    resolver already classifies every such site (`classify_invoke_site`:
//!    refuse, constant-pool class, or substituted owner) before it binds;
//!  * deopt frames: the call is emitted by the same splice walk as a call in a
//!    statically bound OSR splice (`push_inline_scope` per variant, the
//!    `JitInvokeInfo` service copy for an exceptional return);
//!  * redefinition: the bound entry goes to the OSR artifact's
//!    `_direct_callee_entries` (`intern_osr_door_splice_targets` interns the
//!    variants' targets), so withdrawing the super method's body evicts the
//!    OSR body; the receiver body itself carries its `SpeculatedReceiver`
//!    dependency; a new subclass misses the exact class-id guard into the
//!    unchanged dispatch.
//!
//! Opt-in ([`osr_guarded_super_bind_enabled`]): this is the first single-pass
//! guarded variant body with a call in it, and nothing here could be built or
//! measured.

use crate::{InlineInvokeTarget, InlineSite, OsrSpliceCandidate};

/// The VM's receiver-class body resolver, `(receiver class id, cp class, name,
/// descriptor)`, as `plan_osr_door_guarded_splices` takes it.
pub type ReceiverResolve<'a> = &'a dyn Fn(u32, &str, &str, &str) -> Option<InlineSite>;

/// `CRATONVM_JIT_OSR_GUARDED_SUPER_BIND` (j27 wave 4, lane OSRSUPER; **default
/// OFF**, opt-in because it is unbuilt and unmeasured; `=1` on): the single-pass
/// OSR door's guarded pass retries a receiver body the plain resolver refused
/// with the door's direct binder, and keeps it only when every call left in it
/// is a bound `super.m()` ([`body_calls_only_bound_super`]). Read once per OSR
/// compile, live.
pub fn osr_guarded_super_bind_enabled() -> bool {
    false}

/// The body a receiver of class `cid` splices at candidate `c`: the plain
/// resolver's answer when it has one (unchanged behaviour), else, with
/// `super_bound` (the binder-carrying resolver, `None` when the switch is off),
/// its answer when [`body_calls_only_bound_super`] accepts it.
pub(crate) fn osr_guarded_receiver_body(
    plain: ReceiverResolve<'_>,
    super_bound: Option<ReceiverResolve<'_>>,
    cid: u32,
    c: &OsrSpliceCandidate<'_>,
) -> Option<InlineSite> {
    if let Some(site) = plain(cid, c.class_name, c.method_name, c.descriptor) {
        return Some(site);
    }
    let bound = super_bound?;
    let site = bound(cid, c.class_name, c.method_name, c.descriptor)?;
    let accepted = body_calls_only_bound_super(&site);
    if crate::x64::dbg_jitc_enabled() {
        eprintln!(
            "[cratonvm-jitc] osr-splice-guarded-super-bind {} {}.{}{} receiver={cid} \
             body={}.{}{} calls={}",
            if accepted { "ADMITTED" } else { "REFUSED" },
            c.class_name,
            c.method_name,
            c.descriptor,
            site.class_name,
            site.method_name,
            site.descriptor,
            describe_calls(&site),
        );
    }
    accepted.then_some(site)
}

/// Does every call `site` (and every body nested into it) still makes go
/// through a raw `CALL` to a bound `super.m()`? A nested pc is not a call (its
/// body is spliced in turn, and checked the same way). A static exit row
/// (`unloaded_static_call_pcs`) is refused like any other non-super call.
pub(crate) fn body_calls_only_bound_super(site: &InlineSite) -> bool {
    site.unloaded_static_call_pcs.is_empty()
        && site
            .invoke_targets
            .iter()
            .filter(|(pc, _)| !is_nested_pc(site, *pc))
            .all(|(pc, t)| is_bound_super_call(site, *pc, t))
        && site.nested_sites.iter().all(|n| {
            body_calls_only_bound_super(&n.site)
                && n
                    .bimorphic_second
                    .as_deref()
                    .is_none_or(|b| body_calls_only_bound_super(&b.site))
        })
}

fn is_nested_pc(site: &InlineSite, pc: usize) -> bool {
    site.nested_sites.iter().any(|n| n.callee_pc == pc)
}

/// An `invokespecial` (opcode `0xb7`, kind 1: not a private `invokevirtual`
/// pinned to kind 1) of a method other than `<init>` named on a class other
/// than the body's own (so a superclass: JVMS 4.10.1.9 admits only the
/// current class, a superclass or a direct superinterface, and the VM
/// resolver refuses the interface form), with a compiled entry. An unbound
/// one is the downgrade the rule forbids.
fn is_bound_super_call(site: &InlineSite, pc: usize, t: &InlineInvokeTarget) -> bool {
    t.invoke_kind == 1
        && site.callee_code.get(pc) == Some(&0xb7)
        && t.method_name != "<init>"
        && t.class_name != site.class_name
        && t.direct_entry.is_some()
}

/// `pc:kind:class.method[bound|unbound]` per call, for the debug line.
fn describe_calls(site: &InlineSite) -> String {
    let rows: Vec<String> = site
        .invoke_targets
        .iter()
        .filter(|(pc, _)| !is_nested_pc(site, *pc))
        .map(|(pc, t)| {
            format!(
                "{pc}:{}:{}.{}{}[{}]",
                t.invoke_kind,
                t.class_name,
                t.method_name,
                t.descriptor,
                if t.direct_entry.is_some() { "bound" } else { "unbound" }
            )
        })
        .collect();
    format!("[{}] nested={}", rows.join(","), site.nested_sites.len())
}

#[cfg(test)]
mod tests;
