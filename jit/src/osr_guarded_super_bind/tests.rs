use super::*;
use crate::{profile, NestedInlineSite, OsrSplicePlan};


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
    );    }