//! j27 wave 4 (lane OSRSUPER; P2 of
//! `docs/known-issues/jit/j27-small-call-frame-arms-and-osr-super-bind-proposal-20261028.md`):
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
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_OSR_GUARDED_SUPER_BIND")
}

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
mod tests {
    use super::*;
    use crate::{profile, NestedInlineSite, OsrSplicePlan};

    const SWITCH: &str = "CRATONVM_JIT_OSR_GUARDED_SUPER_BIND";
    const GUARDED: &str = "CRATONVM_JIT_OSR_SPLICE_GUARDED";

    fn target(kind: u8, class: &str, method: &str, bound: bool) -> InlineInvokeTarget {
        InlineInvokeTarget {
            class_name: class.to_string(),
            method_name: method.to_string(),
            descriptor: "(I)Ljava/nio/Buffer;".to_string(),
            num_jit_args: 2,
            return_type: b'L',
            invoke_kind: kind,
            declaring_class_id: 419,
            direct_entry: bound.then_some((0x1000, false)),
            ..InlineInvokeTarget::default()
        }
    }

    /// `ByteBuffer.position(I)`: 0 aload_0; 1 iload_1; 2 <op> #5; 5 pop;
    /// 6 aload_0; 7 areturn; two padding bytes. The call at pc 2 is `op`
    /// against `t`.
    fn setter(op: u8, t: InlineInvokeTarget) -> InlineSite {
        InlineSite {
            callee_code: vec![0x2a, 0x1b, op, 0, 5, 0x57, 0x2a, 0xb0, 0, 0],
            callee_code_len: 8,
            callee_max_locals: 2,
            callee_num_args: 2,
            callee_is_static: false,
            return_type: b'L',
            class_name: "java/nio/ByteBuffer".to_string(),
            class_id: 419,
            method_name: "position".to_string(),
            descriptor: "(I)Ljava/nio/ByteBuffer;".to_string(),
            invoke_targets: vec![(2, t)],
            ..InlineSite::default()
        }
    }

    fn super_setter() -> InlineSite {
        setter(0xb7, target(1, "java/nio/Buffer", "position", true))
    }

    #[test]
    fn a_bound_super_call_is_the_only_call_admitted() {
        assert!(body_calls_only_bound_super(&super_setter()));
        // Unbound: the downgrade the rule forbids.
        let unbound = setter(0xb7, target(1, "java/nio/Buffer", "position", false));
        assert!(!body_calls_only_bound_super(&unbound));
        // A bound static call, a constructor, the body's own private method,
        // and a private `invokevirtual` pinned to kind 1 are not `super.m()`.
        let stat = setter(0xb8, target(3, "java/nio/Buffer", "position", true));
        let ctor = setter(0xb7, target(1, "java/nio/Buffer", "<init>", true));
        let own = setter(0xb7, target(1, "java/nio/ByteBuffer", "position", true));
        let pinned = setter(0xb6, target(1, "java/nio/Buffer", "position", true));
        for refused in [&stat, &ctor, &own, &pinned] {
            assert!(!body_calls_only_bound_super(refused), "{refused:?}");
        }
        // A static exit row is a call that is not a bound super call.
        let mut exit = super_setter();
        exit.unloaded_static_call_pcs.push(2);
        assert!(!body_calls_only_bound_super(&exit));
    }

    #[test]
    fn nested_bodies_are_held_to_the_same_rule() {
        let nest = |inner: InlineSite| {
            let mut outer = setter(0xb7, target(1, "java/nio/Buffer", "position", false));
            outer.nested_sites.push(NestedInlineSite {
                callee_pc: 2,
                guard_class_id: 0,
                site: inner,
                bimorphic_second: None,
            });
            outer
        };
        // The nested pc's own (unbound) fallback target is not a call.
        assert!(body_calls_only_bound_super(&nest(super_setter())));
        let static_inside = setter(0xb8, target(3, "java/nio/Buffer", "position", true));
        assert!(!body_calls_only_bound_super(&nest(static_inside)));
    }

    fn candidate() -> OsrSpliceCandidate<'static> {
        OsrSpliceCandidate {
            pc: 1,
            invoke_kind: 0,
            class_name: "java/nio/ByteBuffer",
            method_name: "position",
            descriptor: "(I)Ljava/nio/ByteBuffer;",
        }
    }

    #[test]
    fn the_plain_answer_wins_and_the_bound_one_is_filtered() {
        let c = candidate();
        let leaf = |_: u32, _: &str, _: &str, _: &str| Some(InlineSite::default());
        let none = |_: u32, _: &str, _: &str, _: &str| -> Option<InlineSite> { None };
        let bound_super = |_: u32, _: &str, _: &str, _: &str| Some(super_setter());
        let bound_static = |_: u32, _: &str, _: &str, _: &str| {
            Some(setter(0xb8, target(3, "java/nio/Buffer", "position", true)))
        };
        let asked = std::cell::Cell::new(false);
        let tripwire = |_: u32, _: &str, _: &str, _: &str| -> Option<InlineSite> {
            asked.set(true);
            None
        };
        assert_eq!(
            osr_guarded_receiver_body(&leaf, Some(&tripwire as ReceiverResolve<'_>), 419, &c),
            Some(InlineSite::default())
        );
        assert!(!asked.get(), "a body the plain resolver admits is not re-resolved");
        assert_eq!(osr_guarded_receiver_body(&none, None, 419, &c), None);
        assert_eq!(
            osr_guarded_receiver_body(&none, Some(&bound_super as ReceiverResolve<'_>), 419, &c),
            Some(super_setter())
        );
        assert_eq!(
            osr_guarded_receiver_body(&none, Some(&bound_static as ReceiverResolve<'_>), 419, &c),
            None
        );
    }

    /// The planner end to end: the OSR'd loop `0 aload_0; 1 invokevirtual #4;
    /// 4 goto 1; 7 return`, monomorphic on class 419, whose plain resolver
    /// refuses the call-carrying setter. Off: refused `callee-unresolved`, as
    /// before. On: planned, guarded on 419, the body keeps its bound call.
    #[test]
    fn the_switch_admits_the_nio_setter_shape() {
        const CODE: [u8; 10] = [0x2a, 0xb6, 0, 4, 0xa7, 0xff, 0xfd, 0xb1, 0, 0];
        let mut prof = profile::MethodProfile::default();
        prof.receivers
            .insert(1, [(419u32, 1000u32)].into_iter().collect());
        let run = |switch: &str| {
            cratonvm_types::flags::with_thread_overrides(
                &[(GUARDED, Some("1")), (SWITCH, Some(switch))],
                || {
                    let mut plan = OsrSplicePlan::default();
                    let plain = |_: u32, _: &str, _: &str, _: &str| -> Option<InlineSite> { None };
                    let bound = |_: u32, _: &str, _: &str, _: &str| Some(super_setter());
                    let namer = |cid: u32| Some(format!("p/C{cid}"));
                    crate::plan_osr_door_guarded_splices_bound(
                        &mut plan,
                        &CODE,
                        8,
                        ("p/Main", "main", "([Ljava/lang/String;)V"),
                        &[],
                        false,
                        Some(&prof),
                        Some(1),
                        &[candidate()],
                        &namer,
                        &plain,
                        Some(&bound as ReceiverResolve<'_>),
                    );
                    plan
                },
            )
        };
        let off = run("0");
        assert!(off.guard_variants.is_empty());
        assert_eq!(off.tally.refusal_count("callee-unresolved"), 1);
        let on = run("1");
        let variants = on.guard_variants.get(&1).cloned().unwrap_or_default();
        assert_eq!(variants.iter().map(|(cid, _)| *cid).collect::<Vec<_>>(), vec![419]);
        assert_eq!(variants[0].1.invoke_targets[0].1.direct_entry, Some((0x1000, false)));
        assert!(on
            .inlined_methods
            .iter()
            .any(|(c, m, _)| c == "p/C419" && m == "position"));
    }
}
