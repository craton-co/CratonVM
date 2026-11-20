#[allow(unused_imports)]
use super::*;


/// Interpreter round i2 wave 68, lane L4: `invoke` at a field accessor's
/// declared type links it (`CRATONVM_MH_INVOKE_LINKS_FIELD`), and a `bindTo`
/// fast-mint hit replays the slow path's bound field pre-link
/// (`CRATONVM_MH_BINDTO_FAST_MINT_PRELINK`). The first is default on since
/// the wave-68 triage, the second stays off; the helpers
/// take the switch as a parameter where it matters. The end-to-end rows are
/// `L4W68InvokeLinksField.java` and
/// `L4W68InvokeLinksBench.java`.
#[cfg(test)]
mod i68_l4_invoke_links_and_mint_prelink_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn invoke_is_invoke_exact_only_at_the_declared_type() {
    let decl = Some("(Lp/Cell;)I");
    assert!(invoke_site_is_declared(Some("(Lp/Cell;)I"), decl));
    // An `Object`-typed receiver (the `--compatible` String-receiver row),
    // a widened return, a boxed return: an `asType`, never linked.
    assert!(!invoke_site_is_declared(
        Some("(Ljava/lang/Object;)I"),
        decl
    ));
    assert!(!invoke_site_is_declared(Some("(Lp/Cell;)J"), decl));
    assert!(!invoke_site_is_declared(
        Some("(Lp/Cell;)Ljava/lang/Object;"),
        decl
    ));
    // No site, an empty site, an unknown declared type.
    assert!(!invoke_site_is_declared(None, decl));
    assert!(!invoke_site_is_declared(Some(""), Some("")));
    assert!(!invoke_site_is_declared(Some("(Lp/Cell;)I"), None));
}

#[test]
fn invoke_links_nothing_without_a_declared_type() {
    let mut ctx = MockNativeContext::new();
    let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
    let r = ctx.alloc_object(cell, 4);
    let mh = alloc_method_handle(
        &mut ctx,
        "p/Cell",
        "cellValueZ",
        "(Lp/Cell;)I",
        MH_KIND_GETTER,
    )
    .unwrap();
    ctx.set_field_by_name(mh, "type", Value::Object(None));
    assert!(!mh_declared_type_is_site(&mut ctx, mh, "(Lp/Cell;)I"));
    let args = [Value::Object(Some(r))];
    mh_field_link_try_for_invoke(&mut ctx, mh, &args, Some("(Lp/Cell;)I"));
    mh_field_link_try_for_invoke(&mut ctx, mh, &args, Some(""));
    mh_field_link_try_for_invoke(&mut ctx, mh, &args, None);
    assert!(matches!(ctx.get_field(mh, MH_FIELD_LINK), Value::Int(s) if s & FL_LINKED == 0));
}

/// The bound getter link `mh_field_link_try_bound` writes for slot 2 of a
/// `p/Cell` receiver (`MH_CLASS`'s string stands in for `type`, which the
/// mock does not render), relinked once from a carry of `0`.
fn bound_link_state() -> i32 {
    FL_LINKED | FL_BOUND | ((MH_CLASS as i32) << 16) | 2 | (1 << FL_LINKS_SHIFT)
}

/// A fresh bound getter copy over `p/Cell` bound to `receiver`, unlinked.
fn bound_copy(ctx: &mut MockNativeContext, receiver: ObjectRef) -> ObjectRef {
    let mh = alloc_method_handle(ctx, "p/Cell", "cellValueZ", "()I", MH_KIND_GETTER).unwrap();
    ctx.set_field(mh, MH_BOUND, Value::Object(Some(receiver)));
    mh
}

#[test]
fn the_line_remembers_only_a_plain_bound_link() {
    let mut ctx = MockNativeContext::new();
    let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
    let r = ctx.alloc_object(cell, 4);
    let raw = cell.as_u32() as i32;
    let carry = BoundFieldCarry {
        owner: cell,
        link_state: 0,
    };
    let mh = bound_copy(&mut ctx, r);
    // Nothing linked: nothing to remember.
    assert_eq!(bindto_prelink_memo_of_with(&ctx, mh, carry, true), None);
    let Value::Object(Some(type_obj)) = ctx.get_field(mh, MH_CLASS) else {
        panic!("MH_CLASS holds a string");
    };
    let site_id = field_link_site_id("()I").expect("interned") as i32;
    ctx.set_field(mh, MH_FIELD_LINK_TYPE, Value::Object(Some(type_obj)));
    ctx.set_field(mh, MH_FIELD_LINK_CLASS, Value::Int(raw));
    ctx.set_field(mh, MH_FIELD_LINK_VALUE, Value::Int(0));
    ctx.set_field(mh, MH_FIELD_LINK_SITE, Value::Int(site_id));
    ctx.set_field(mh, MH_FIELD_LINK, Value::Int(bound_link_state()));
    let memo = BoundPrelinkMemo {
        carry_state: 0,
        state: bound_link_state(),
        class_raw: raw,
        site_id,
    };
    assert_eq!(
        bindto_prelink_memo_of_with(&ctx, mh, carry, true),
        Some(memo)
    );
    // Off: nothing.
    assert_eq!(bindto_prelink_memo_of_with(&ctx, mh, carry, false), None);
    // A value check, an unbound link, an owner link: re-derived per hit.
    for bad in [
        bound_link_state() | FL_VALUE_CHECK,
        bound_link_state() & !FL_BOUND,
        bound_link_state() | FL_OWNER_ADMITS,
    ] {
        ctx.set_field(mh, MH_FIELD_LINK, Value::Int(bad));
        assert_eq!(bindto_prelink_memo_of_with(&ctx, mh, carry, true), None);
    }
    // A link made under another `type` object.
    ctx.set_field(mh, MH_FIELD_LINK, Value::Int(bound_link_state()));
    ctx.set_field(mh, MH_FIELD_LINK_TYPE, Value::Object(Some(r)));
    assert_eq!(bindto_prelink_memo_of_with(&ctx, mh, carry, true), None);
}

#[test]
fn a_hit_replays_the_link_for_the_remembered_class_only() {
    let mut ctx = MockNativeContext::new();
    if !mh_field_link_mode_admits(&ctx)
        || !mh_field_link_enabled()
        || crate::nbflags().dbg_mh_dispatch
    {
        return;
    }
    let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
    let stranger = ctx.ensure_class_initialized("p/Str").expect("p/Str");
    let r = ctx.alloc_object(cell, 4);
    ctx.set_field(r, 2, Value::Int(42));
    let raw = cell.as_u32() as i32;
    let carry = BoundFieldCarry {
        owner: cell,
        link_state: 0,
    };
    let site_id = field_link_site_id("()I").expect("interned") as i32;
    let memo = BoundPrelinkMemo {
        carry_state: 0,
        state: bound_link_state(),
        class_raw: raw,
        site_id,
    };

    // No memo, or off: not replayed, nothing written.
    let mh = bound_copy(&mut ctx, r);
    let recv = Value::Object(Some(r));
    assert!(!bindto_fast_mint_replay_prelink_with(
        &mut ctx, mh, recv, carry, None, true
    ));
    assert!(!bindto_fast_mint_replay_prelink_with(
        &mut ctx,
        mh,
        recv,
        carry,
        Some(memo),
        false
    ));
    assert!(matches!(ctx.get_field(mh, MH_FIELD_LINK), Value::Int(s) if s & FL_LINKED == 0));

    // Another carry than the link was derived from: re-derived.
    let minted = BoundFieldCarry {
        owner: cell,
        link_state: field_link_minted_state(2).unwrap(),
    };
    assert!(!bindto_fast_mint_replay_prelink_with(
        &mut ctx,
        mh,
        recv,
        minted,
        Some(memo),
        true
    ));

    // A receiver of another class (a `String`), or one that is not the
    // copy's bound value: re-derived.
    let s = ctx.alloc_object(stranger, 4);
    let other = bound_copy(&mut ctx, s);
    let srecv = Value::Object(Some(s));
    assert!(!bindto_fast_mint_replay_prelink_with(
        &mut ctx,
        other,
        srecv,
        carry,
        Some(memo),
        true
    ));
    assert!(
        matches!(ctx.get_field(other, MH_FIELD_LINK), Value::Int(st) if st & FL_LINKED == 0)
    );
    assert!(!bindto_fast_mint_replay_prelink_with(
        &mut ctx,
        mh,
        srecv,
        carry,
        Some(memo),
        true
    ));

    // The remembered class: the link is written, and it serves the copy.
    if crate::nbflags().dbg_mh_field_link {
        return;
    }
    assert!(bindto_fast_mint_replay_prelink_with(
        &mut ctx,
        mh,
        recv,
        carry,
        Some(memo),
        true
    ));
    assert_eq!(
        ctx.get_field(mh, MH_FIELD_LINK),
        Value::Int(bound_link_state())
    );
    assert_eq!(ctx.get_field(mh, MH_FIELD_LINK_CLASS), Value::Int(raw));
    let got = mh_field_link_fast(&mut ctx, mh, &[], Some("()I"));
    assert!(matches!(got, Some(Ok(Some(Value::Int(42))))));
}
}

/// Interpreter round i2 wave 69, lane L4: the call that writes a field
/// accessor's link is served by it (`CRATONVM_MH_LINKING_CALL_SERVED`, default
/// off), and a `bindTo` fast-mint line keeps a second admitted class
/// (`CRATONVM_MH_BINDTO_FAST_MINT_TWO_CLASSES`, default off). The helpers take
/// the switch as a parameter; the end-to-end rows are
/// `L4W69LinkingCallServed.java` and
/// `L4W69LinkingCallBench.java`.
#[cfg(test)]
mod i69_l4_linking_call_served_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn only_a_written_link_counts() {
    let linked = FL_LINKED | 2 | (1 << FL_LINKS_SHIFT);
    // Nothing before, a link after: written.
    assert!(field_link_was_written(0, linked));
    // A relink raises the count.
    assert!(field_link_was_written(
        linked,
        FL_LINKED | 3 | (2 << FL_LINKS_SHIFT)
    ));
    // A minted slot only (no link), or the word unchanged: not written.
    let minted = field_link_minted_state(2).unwrap();
    assert!(!field_link_was_written(0, minted));
    assert!(!field_link_was_written(minted, minted));
    assert!(!field_link_was_written(linked, linked));
}

/// An UNBOUND instance getter over `p/Cell` slot 2, hand-linked as
/// [`mh_field_link_try`] links it (the mock renders no `MethodType`, so
/// `MH_CLASS`'s string stands in for the `type` object), receiver class
/// `class_raw`, declared type `site`. Returns the handle and its link word.
fn hand_linked_getter(
    ctx: &mut MockNativeContext,
    class_raw: i32,
    site: &str,
) -> (ObjectRef, i32) {
    let mh = alloc_method_handle(ctx, "p/Cell", "cellValueZ", "(Lp/Cell;)I", MH_KIND_GETTER)
        .unwrap();
    let Value::Object(Some(type_obj)) = ctx.get_field(mh, MH_CLASS) else {
        panic!("MH_CLASS holds a string");
    };
    let site_id = field_link_site_id(site).expect("interned") as i32;
    ctx.set_field(mh, MH_FIELD_LINK_TYPE, Value::Object(Some(type_obj)));
    ctx.set_field(mh, MH_FIELD_LINK_CLASS, Value::Int(class_raw));
    ctx.set_field(mh, MH_FIELD_LINK_VALUE, Value::Int(0));
    ctx.set_field(mh, MH_FIELD_LINK_SITE, Value::Int(site_id));
    let state = FL_LINKED | ((MH_CLASS as i32) << 16) | 2 | (1 << FL_LINKS_SHIFT);
    ctx.set_field(mh, MH_FIELD_LINK, Value::Int(state));
    (mh, state)
}

#[test]
fn the_linking_call_is_served_only_through_a_fresh_link_at_the_declared_type() {
    let mut ctx = MockNativeContext::new();
    let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
    let stranger = ctx.ensure_class_initialized("p/Str").expect("p/Str");
    let r = ctx.alloc_object(cell, 4);
    ctx.set_field(r, 2, Value::Int(42));
    let raw = cell.as_u32() as i32;
    let site = Some("(Lp/Cell;)I");
    let (mh, state) = hand_linked_getter(&mut ctx, raw, "(Lp/Cell;)I");
    assert_eq!(field_link_state_word(&ctx, mh), state);
    let args = [Value::Object(Some(r))];
    let door = LinkingDoor::InvokeExact;

    // Off: the door.
    assert!(
        mh_field_link_serve_linking_call_with(&mut ctx, mh, &args, site, 0, door, false)
            .is_none()
    );
    // The attempt wrote nothing (the word it found is the word now): the door.
    assert!(mh_field_link_serve_linking_call_with(
        &mut ctx, mh, &args, site, state, door, true
    )
    .is_none());
    // Another call site than the declared type (a widened return, an
    // `Object` receiver): the door, whatever the strict switch says.
    for other in [
        Some("(Lp/Cell;)J"),
        Some("(Ljava/lang/Object;)I"),
        Some(""),
        None,
    ] {
        assert!(mh_field_link_serve_linking_call_with(
            &mut ctx, mh, &args, other, 0, door, true
        )
        .is_none());
    }
    // A receiver the link does not name, a null receiver, a wrong arity:
    // the door (its cast / message-less NPE / `WrongMethodTypeException`).
    let s = ctx.alloc_object(stranger, 4);
    for bad in [
        vec![Value::Object(Some(s))],
        vec![Value::Object(None)],
        vec![],
        vec![args[0], Value::Int(1)],
    ] {
        assert!(
            mh_field_link_serve_linking_call_with(&mut ctx, mh, &bad, site, 0, door, true)
                .is_none()
        );
    }
    // Freshly written, at the declared type: the link's answer.
    let got = mh_field_link_serve_linking_call_with(&mut ctx, mh, &args, site, 0, door, true);
    assert!(matches!(got, Some(Ok(Some(Value::Int(42))))));
    // `invoke` reads the link only under `CRATONVM_MH_FIELD_LINK_SERVES_INVOKE`.
    let via_invoke = mh_field_link_serve_linking_call_with(
        &mut ctx,
        mh,
        &args,
        site,
        0,
        LinkingDoor::Invoke,
        true,
    );
    if mh_field_link_serves_invoke_enabled() {
        assert!(matches!(via_invoke, Some(Ok(Some(Value::Int(42))))));
    } else {
        assert!(via_invoke.is_none());
    }
}

#[test]
fn an_unlinkable_handle_keeps_the_door() {
    // The mock renders no declared type, so the attempt links nothing and
    // the call is the door's whichever door it came through.
    let mut ctx = MockNativeContext::new();
    let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
    let r = ctx.alloc_object(cell, 4);
    let mh = alloc_method_handle(
        &mut ctx,
        "p/Cell",
        "cellValueZ",
        "(Lp/Cell;)I",
        MH_KIND_GETTER,
    )
    .unwrap();
    ctx.set_field_by_name(mh, "type", Value::Object(None));
    let args = [Value::Object(Some(r))];
    for door in [LinkingDoor::InvokeExact, LinkingDoor::Invoke] {
        assert!(
            mh_field_link_try_and_serve(&mut ctx, mh, &args, Some("(Lp/Cell;)I"), door)
                .is_none()
        );
    }
    assert!(matches!(ctx.get_field(mh, MH_FIELD_LINK), Value::Int(s) if s & FL_LINKED == 0));
}

// ---- `CRATONVM_MH_BINDTO_FAST_MINT_TWO_CLASSES` (wave 69) ----

fn memo(class_raw: i32) -> BoundPrelinkMemo {
    BoundPrelinkMemo {
        carry_state: 0,
        state: FL_LINKED | FL_BOUND | 2,
        class_raw,
        site_id: 0,
    }
}

fn answer_for(class: Option<u32>, owner: ClassId) -> BindToMintAnswer {
    BindToMintAnswer {
        kind: MH_KIND_GETTER,
        type_desc: std::rc::Rc::from("()I"),
        collects: false,
        static_owner: None,
        recv_class: class,
        field_carry: Some(BoundFieldCarry {
            owner,
            link_state: 0,
        }),
        prelink: class.map(|c| memo(c as i32)),
        alt: None,
    }
}

#[test]
fn a_refill_for_another_class_keeps_the_first_as_the_second() {
    let owner = ClassId::new(7);
    let a = answer_for(Some(11), owner);
    let b = answer_for(Some(12), owner);
    // Off, no old line, or the same class: the new answer alone.
    assert_eq!(
        bindto_mint_answer_merged(Some(&a), b.clone(), false).alt,
        None
    );
    assert_eq!(bindto_mint_answer_merged(None, b.clone(), true).alt, None);
    assert_eq!(
        bindto_mint_answer_merged(Some(&b), b.clone(), true).alt,
        None
    );
    // On: B first, A kept with its own pre-link.
    let merged = bindto_mint_answer_merged(Some(&a), b.clone(), true);
    assert_eq!(merged.recv_class, Some(12));
    assert_eq!(merged.prelink, Some(memo(12)));
    assert_eq!(
        merged.alt,
        Some(BindToMintAlt {
            recv_class: 11,
            prelink: Some(memo(11))
        })
    );
    // A third class replaces the older of the two.
    let c = answer_for(Some(13), owner);
    let third = bindto_mint_answer_merged(Some(&merged), c, true);
    assert_eq!(third.recv_class, Some(13));
    assert_eq!(third.alt.map(|alt| alt.recv_class), Some(12));
    // A line that judges no class, or a new answer that judges none: alone.
    let any = answer_for(None, owner);
    assert_eq!(
        bindto_mint_answer_merged(Some(&any), b.clone(), true).alt,
        None
    );
    assert_eq!(
        bindto_mint_answer_merged(Some(&a), any.clone(), true).alt,
        None
    );
    // A shape disagreement (another carry owner, another kind): alone.
    let other_owner = answer_for(Some(11), ClassId::new(8));
    assert_eq!(
        bindto_mint_answer_merged(Some(&other_owner), b.clone(), true).alt,
        None
    );
    let setter = BindToMintAnswer {
        kind: MH_KIND_SETTER,
        ..answer_for(Some(11), owner)
    };
    assert_eq!(bindto_mint_answer_merged(Some(&setter), b, true).alt, None);
}

#[test]
fn either_class_of_a_line_hits_with_its_own_pre_link() {
    let owner = ClassId::new(7);
    let merged = bindto_mint_answer_merged(
        Some(&answer_for(Some(11), owner)),
        answer_for(Some(12), owner),
        true,
    );
    let first = bindto_mint_answer_for_class(merged.clone(), 12).expect("the line's class");
    assert_eq!(first.prelink, Some(memo(12)));
    let second = bindto_mint_answer_for_class(merged.clone(), 11).expect("the second class");
    assert_eq!(second.recv_class, Some(11));
    assert_eq!(second.prelink, Some(memo(11)));
    assert_eq!(second.alt, None);
    assert!(bindto_mint_answer_for_class(merged, 13).is_none());
    // A line that judges no class admits any.
    assert!(bindto_mint_answer_for_class(answer_for(None, owner), 99).is_some());
    // Without a second class, only the line's.
    assert!(bindto_mint_answer_for_class(answer_for(Some(11), owner), 12).is_none());
}
}

/// Interpreter round i2 wave 70, lane L4: `invoke`'s field link reads the
/// declared type once (`CRATONVM_MH_INVOKE_LINK_DECLARED_ONCE`, default off, a
/// pure refactor) and a link's derivation is memoised per source shape
/// (`CRATONVM_MH_FIELD_LINK_DERIVATION_MEMO`, default off). The memo is driven
/// through its test VM override; the end-to-end rows are
/// `L4W70LinkDerivation.java` and
/// `L4W70LinkDerivationBench.java`.
#[cfg(test)]
mod i70_l4_link_derivation_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// Forces the memo on for this thread (an empty memo); off and empty again
/// when dropped.
struct MemoOn;
impl MemoOn {
    fn new() -> Self {
        FIELD_LINK_MEMO_TEST_VM.with(|v| v.set(Some(0x70_4C04)));
        FIELD_LINK_MEMO.with(|m| m.borrow_mut().iter_mut().for_each(|l| *l = None));
        MemoOn
    }
}
impl Drop for MemoOn {
    fn drop(&mut self) {
        FIELD_LINK_MEMO_TEST_VM.with(|v| v.set(None));
        FIELD_LINK_MEMO.with(|m| m.borrow_mut().iter_mut().for_each(|l| *l = None));
    }
}

const SITE: &str = "(Lp/Cell;)I";

/// The link an owner-admitting derivation writes for slot 2 of a getter
/// over `p/Cell` (`MH_CLASS`'s string stands in for the `type` object the
/// mock does not render), first link from an empty word.
fn owner_link_state() -> i32 {
    FL_LINKED | FL_OWNER_ADMITS | ((MH_CLASS as i32) << 16) | 2 | (1 << FL_LINKS_SHIFT)
}

/// Write `state`'s link words on `mh` as a derivation would.
fn hand_link(ctx: &mut MockNativeContext, mh: ObjectRef, state: i32, class_raw: i32) {
    let Value::Object(Some(type_obj)) = ctx.get_field(mh, MH_CLASS) else {
        panic!("MH_CLASS holds a string");
    };
    let site_id = field_link_site_id(SITE).expect("interned") as i32;
    ctx.set_field(mh, MH_FIELD_LINK_TYPE, Value::Object(Some(type_obj)));
    ctx.set_field(mh, MH_FIELD_LINK_CLASS, Value::Int(class_raw));
    ctx.set_field(mh, MH_FIELD_LINK_VALUE, Value::Int(0));
    ctx.set_field(mh, MH_FIELD_LINK_SITE, Value::Int(site_id));
    ctx.set_field(mh, MH_FIELD_LINK, Value::Int(state));
}

/// Back to unlinked (the word the candidate screen found: `0`).
fn unlink(ctx: &mut MockNativeContext, mh: ObjectRef) {
    ctx.set_field(mh, MH_FIELD_LINK, Value::Int(0));
    ctx.set_field(mh, MH_FIELD_LINK_TYPE, Value::Object(None));
    ctx.set_field(mh, MH_FIELD_LINK_CLASS, Value::Int(0));
    ctx.set_field(mh, MH_FIELD_LINK_SITE, Value::Int(0));
}

const FRESH: FieldLinkCandidate = FieldLinkCandidate {
    setter: false,
    old: 0,
    links: 0,
};

#[test]
fn the_memo_keeps_only_owner_instance_links() {
    assert!(field_link_memo_keeps(owner_link_state()));
    assert!(field_link_memo_keeps(FL_LINKED | FL_BOUND | 2));
    assert!(field_link_memo_keeps(FL_LINKED | FL_MINT_SLOT | 2));
    // A static link, a by-name instance link, no link at all.
    assert!(!field_link_memo_keeps(
        FL_LINKED | FL_STATIC | FL_OWNER_ADMITS | 2
    ));
    assert!(!field_link_memo_keeps(FL_LINKED | 2));
    assert!(!field_link_memo_keeps(FL_MINT_SLOT | 2));
    // A bound receiver is one key word, whatever the receiver.
    assert_eq!(field_link_memo_bound_word(Value::Object(None)), 0);
    assert_ne!(field_link_memo_bound_word(Value::Int(5)), u64::MAX - 1);
}

#[test]
fn off_the_memo_is_never_asked() {
    let mut ctx = MockNativeContext::new();
    let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
    let r = ctx.alloc_object(cell, 4);
    let mh =
        alloc_method_handle(&mut ctx, "p/Cell", "cellValueZ", SITE, MH_KIND_GETTER).unwrap();
    let args = [Value::Object(Some(r))];
    // The mock reports VM identity `0`: no key, whatever the switch.
    let got = field_link_memo_replay(&mut ctx, mh, FRESH, &args, Some(SITE), true);
    assert_eq!(got, FieldLinkMemoReplay::Miss(None));
}

#[test]
fn a_remembered_derivation_is_written_for_the_same_shape_and_class_only() {
    let _on = MemoOn::new();
    let mut ctx = MockNativeContext::new();
    let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
    let stranger = ctx.ensure_class_initialized("p/Str").expect("p/Str");
    let r = ctx.alloc_object(cell, 4);
    let mh =
        alloc_method_handle(&mut ctx, "p/Cell", "cellValueZ", SITE, MH_KIND_GETTER).unwrap();
    unlink(&mut ctx, mh);
    let args = [Value::Object(Some(r))];
    let raw = cell.as_u32() as i32;

    // An empty memo: a miss with the key.
    let FieldLinkMemoReplay::Miss(Some(key)) =
        field_link_memo_replay(&mut ctx, mh, FRESH, &args, Some(SITE), false)
    else {
        panic!("a keyed miss");
    };
    // The derivation (by hand) writes an owner link; the fill remembers it.
    hand_link(&mut ctx, mh, owner_link_state(), raw);
    field_link_memo_fill_after(&mut ctx, mh, FRESH, Some(key), &args);
    let linked: Vec<Value> = (MH_FIELD_LINK..=MH_FIELD_LINK_SITE)
        .map(|i| ctx.get_field(mh, i))
        .collect();

    // The next linking call of the same shape and class writes the same words.
    unlink(&mut ctx, mh);
    let got = field_link_memo_replay(&mut ctx, mh, FRESH, &args, Some(SITE), false);
    assert_eq!(got, FieldLinkMemoReplay::Written);
    let replayed: Vec<Value> = (MH_FIELD_LINK..=MH_FIELD_LINK_SITE)
        .map(|i| ctx.get_field(mh, i))
        .collect();
    assert_eq!(format!("{linked:?}"), format!("{replayed:?}"));

    // A receiver of another class, a null receiver, another arity: derived.
    unlink(&mut ctx, mh);
    let s = ctx.alloc_object(stranger, 4);
    for bad in [
        vec![Value::Object(Some(s))],
        vec![Value::Object(None)],
        vec![],
        vec![args[0], Value::Int(1)],
    ] {
        let got = field_link_memo_replay(&mut ctx, mh, FRESH, &bad, Some(SITE), false);
        assert!(matches!(got, FieldLinkMemoReplay::Miss(Some(_))));
        assert!(matches!(ctx.get_field(mh, MH_FIELD_LINK), Value::Int(0)));
    }
    // Judged sites: only the interned declared type.
    for other in [
        Some("(Lp/Cell;)J"),
        Some("(Ljava/lang/Object;)I"),
        Some(""),
        None,
    ] {
        let got = field_link_memo_replay(&mut ctx, mh, FRESH, &args, other, true);
        assert!(matches!(got, FieldLinkMemoReplay::Miss(Some(_))));
    }
    assert_eq!(
        field_link_memo_replay(&mut ctx, mh, FRESH, &args, Some(SITE), true),
        FieldLinkMemoReplay::Written
    );
    // Another prior link word (a relink): another key.
    let relink = FieldLinkCandidate {
        setter: false,
        old: owner_link_state(),
        links: 1,
    };
    let got = field_link_memo_replay(&mut ctx, mh, relink, &args, Some(SITE), false);
    assert!(matches!(got, FieldLinkMemoReplay::Miss(Some(k)) if k != key));
    // A member slot rewritten in place (here `MH_CLASS`, which the mock's
    // link also uses as its `type`): another key, derived.
    unlink(&mut ctx, mh);
    let name = ctx.get_field(mh, MH_NAME);
    let class = ctx.get_field(mh, MH_CLASS);
    ctx.set_field(mh, MH_CLASS, name);
    let got = field_link_memo_replay(&mut ctx, mh, FRESH, &args, Some(SITE), false);
    assert!(matches!(got, FieldLinkMemoReplay::Miss(Some(_))));
    ctx.set_field(mh, MH_CLASS, class);
}

#[test]
fn a_link_the_memo_does_not_keep_fills_nothing() {
    let _on = MemoOn::new();
    let mut ctx = MockNativeContext::new();
    let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
    let r = ctx.alloc_object(cell, 4);
    let mh =
        alloc_method_handle(&mut ctx, "p/Cell", "cellValueZ", SITE, MH_KIND_GETTER).unwrap();
    unlink(&mut ctx, mh);
    let args = [Value::Object(Some(r))];
    let FieldLinkMemoReplay::Miss(Some(key)) =
        field_link_memo_replay(&mut ctx, mh, FRESH, &args, Some(SITE), false)
    else {
        panic!("a keyed miss");
    };
    // A by-name instance link (no owner bit), then nothing written at all.
    let by_name = FL_LINKED | ((MH_CLASS as i32) << 16) | 2 | (1 << FL_LINKS_SHIFT);
    hand_link(&mut ctx, mh, by_name, cell.as_u32() as i32);
    field_link_memo_fill_after(&mut ctx, mh, FRESH, Some(key), &args);
    unlink(&mut ctx, mh);
    field_link_memo_fill_after(&mut ctx, mh, FRESH, Some(key), &args);
    let got = field_link_memo_replay(&mut ctx, mh, FRESH, &args, Some(SITE), false);
    assert!(matches!(got, FieldLinkMemoReplay::Miss(Some(_))));
}

#[test]
fn invoke_links_nothing_without_a_declared_type_either_way() {
    // The mock renders no declared type: neither form of `invoke`'s link
    // writes anything, so the refactor declines exactly where wave 68 did.
    let mut ctx = MockNativeContext::new();
    let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
    let r = ctx.alloc_object(cell, 4);
    let mh =
        alloc_method_handle(&mut ctx, "p/Cell", "cellValueZ", SITE, MH_KIND_GETTER).unwrap();
    ctx.set_field_by_name(mh, "type", Value::Object(None));
    let args = [Value::Object(Some(r))];
    for once in [false, true] {
        for site in [Some(SITE), Some(""), None] {
            mh_field_link_try_for_invoke_with(&mut ctx, mh, &args, site, once, &mut None);
        }
    }
    assert!(matches!(ctx.get_field(mh, MH_FIELD_LINK), Value::Int(s) if s & FL_LINKED == 0));
}
}

/// Interpreter round i3 wave 72, lane L4 (`CRATONVM_MH_SITE_TYPE_TABLE`):
/// the site-type rows of the bound-`MethodType` table. The switch is set
/// explicitly in every test (it is read through a thread override). The
/// end-to-end rows are `L4W72SiteTypeTable.java` and
/// `L4W72JdkOnlyMintBench.java`.


/// Interpreter round i3 wave 73, lane L4: the `MethodHandleNatives` field
/// memo (`CRATONVM_MHN_RESOLVE_MEMO`) and the real field accessor's record
/// (`CRATONVM_MH_REAL_FIELD_RECORD`). Every switch is set explicitly through a
/// thread override. The end-to-end rows are
/// `L4W73FieldNativesMemo.java` and
/// `L4W73JdkOnlyFieldBench.java`.
#[cfg(test)]
mod i73_l4_field_memo_and_record_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::flags::with_thread_overrides;

#[test]
fn the_memo_answers_only_its_own_key_kind_flags_and_vm() {
    let ctx = MockNativeContext::new();
    ctx.set_vm_identity(0x7301);
    let epoch = ctx.gc_collection_count();
    let key = [0x1000, 0x2000, 0x3000];
    let answer = MhnMemoAnswer::Resolve {
        flags: Some(0x0104_0001),
        holder: None,
    };
    mhn_memo_put(&ctx, epoch, MhnMemoKind::Resolve, key, 0x0104_0000, answer);
    assert_eq!(
        mhn_memo_get(&ctx, MhnMemoKind::Resolve, key, 0x0104_0000),
        Some(answer)
    );
    assert_eq!(
        mhn_memo_get(&ctx, MhnMemoKind::Resolve, key, 0x0204_0000),
        None,
        "another requested kind"
    );
    assert_eq!(
        mhn_memo_get(&ctx, MhnMemoKind::StaticBase, key, 0x0104_0000),
        None,
        "another native"
    );
    assert_eq!(
        mhn_memo_get(
            &ctx,
            MhnMemoKind::Resolve,
            [0x1000, 0x2008, 0x3000],
            0x0104_0000
        ),
        None,
        "another name object"
    );
    let other = MockNativeContext::new();
    other.set_vm_identity(0x7302);
    assert_eq!(
        mhn_memo_get(&other, MhnMemoKind::Resolve, key, 0x0104_0000),
        None,
        "another VM"
    );
}

#[test]
fn nothing_is_stored_across_a_collection_or_for_vm_zero() {
    let ctx = MockNativeContext::new();
    ctx.set_vm_identity(0x7303);
    let key = [0x5000, 0x6000, 0];
    let stale_epoch = ctx.gc_collection_count().wrapping_add(1);
    mhn_memo_put(
        &ctx,
        stale_epoch,
        MhnMemoKind::ObjectOffset,
        key,
        0,
        MhnMemoAnswer::Offset(3),
    );
    assert_eq!(mhn_memo_get(&ctx, MhnMemoKind::ObjectOffset, key, 0), None);
    let mock = MockNativeContext::new();
    let epoch = mock.gc_collection_count();
    mhn_memo_put(
        &mock,
        epoch,
        MhnMemoKind::ObjectOffset,
        key,
        0,
        MhnMemoAnswer::Offset(3),
    );
    assert_eq!(
        mhn_memo_get(&mock, MhnMemoKind::ObjectOffset, key, 0),
        None,
        "vm 0"
    );
}

#[test]
fn every_memo_slot_is_in_range() {
    for a in [0usize, 8, 0x1234_5678, usize::MAX] {
        for kind in [
            MhnMemoKind::Resolve,
            MhnMemoKind::ObjectOffset,
            MhnMemoKind::StaticOffset,
            MhnMemoKind::StaticBase,
        ] {
            assert!(mhn_memo_slot(kind, &[a, a.wrapping_mul(3), a ^ 0x55]) < MHN_MEMO_SLOTS);
        }
    }
}
}

/// Interpreter round i3 wave 77, lane L4: the field-finder mint memo
/// (`CRATONVM_MH_FINDER_MINT_MEMO`, default on). Every switch is set
/// explicitly through a thread override. The end-to-end rows are
/// `L4W77FinderMint.java` (behaviour) and
/// `L4W77FinderMintBench.java`.


/// Interpreter round i3 wave 82, lane L4: the converting adapter memo of
/// `asTypeUncached` (`CRATONVM_MH_REAL_AS_TYPE_MEMO`, default off). Switches
/// set explicitly; no default pinned. End to end:
/// `L4W82AsTypeMemo.java` and
/// `L4W72JdkOnlyMintBench.java`.
#[cfg(test)]
mod i82_l4_as_type_memo_tests {
use super::*;
use cratonvm_types::flags::with_thread_overrides;

#[test]
fn only_direct_and_bound_species_receivers_are_admitted() {
    for (name, want) in [
        (
            "java/lang/invoke/DirectMethodHandle",
            Some(AsTypeReceiver::Direct),
        ),
        (
            "java/lang/invoke/DirectMethodHandle$Accessor",
            Some(AsTypeReceiver::Direct),
        ),
        (
            "java/lang/invoke/DirectMethodHandle$Special",
            Some(AsTypeReceiver::Direct),
        ),
        (
            "java/lang/invoke/BoundMethodHandle$Species_L",
            Some(AsTypeReceiver::Bound),
        ),
        (
            "java/lang/invoke/BoundMethodHandle$Species_LLI",
            Some(AsTypeReceiver::Bound),
        ),
        ("java/lang/invoke/BoundMethodHandle", None),
        ("java/lang/invoke/MethodHandleImpl$AsVarargsCollector", None),
        ("java/lang/invoke/MethodHandleImpl$WrappedMember", None),
        ("java/lang/invoke/MethodHandleImpl$CountingWrapper", None),
        ("java/lang/invoke/SimpleMethodHandle", None),
        ("java/lang/invoke/MethodHandle", None),
        ("java/lang/invoke/DirectMethodHandleX", None),
    ] {
        assert_eq!(as_type_memo_receiver_kind(name), want, "{name}");
    }
}

#[test]
fn values_compare_by_identity_and_bits() {
    assert!(as_type_memo_same_value(Value::Int(3), Value::Int(3)));
    assert!(!as_type_memo_same_value(Value::Int(3), Value::Long(3)));
    assert!(as_type_memo_same_value(
        Value::Float(f32::NAN),
        Value::Float(f32::NAN)
    ));
    assert!(!as_type_memo_same_value(
        Value::Double(0.0),
        Value::Double(-0.0)
    ));
    assert!(as_type_memo_same_value(
        Value::Object(None),
        Value::Object(None)
    ));
}

/// Wave 84 (lane L4): the candidate's own receiver answering exactly the
/// prediction mints for that receiver only; another receiver's agreeing
/// answer promotes the key (from either state); any disagreeing answer
/// refuses it; promoted and refused lines are final for the epoch.
#[test]
fn the_second_answer_promotes_self_promotes_or_refuses() {
    use AsTypeState as S;
    for (state, same, predicted, want) in [
        (S::Candidate, true, true, Some(S::PromotedSelf)),
        (S::Candidate, false, true, Some(S::Promoted)),
        (S::Candidate, true, false, Some(S::Refused)),
        (S::Candidate, false, false, Some(S::Refused)),
        (S::PromotedSelf, true, true, Some(S::PromotedSelf)),
        (S::PromotedSelf, false, true, Some(S::Promoted)),
        (S::PromotedSelf, false, false, Some(S::Refused)),
        (S::PromotedSelf, true, false, Some(S::Refused)),
        (S::Promoted, false, true, None),
        (S::Promoted, true, false, None),
        (S::Refused, false, true, None),
    ] {
        assert_eq!(
            as_type_memo_next_state(state, same, predicted),
            want,
            "{state:?} same={same} predicted={predicted}"
        );
    }
}

#[test]
fn every_key_word_moves_the_key_and_the_slot_stays_in_range() {
    let base = AsTypeMemoKey {
        recv_class: 7,
        recv_type: 0x1000,
        recv_form: 0x2000,
        new_type: 0x3000,
    };
    let variants = [
        AsTypeMemoKey {
            recv_class: 8,
            ..base
        },
        AsTypeMemoKey {
            recv_type: 0x1010,
            ..base
        },
        AsTypeMemoKey {
            recv_form: 0x2010,
            ..base
        },
        AsTypeMemoKey {
            new_type: 0x3010,
            ..base
        },
    ];
    assert!(as_type_memo_slot(&base) < AS_TYPE_MEMO_SLOTS);
    for v in variants {
        assert_ne!(v, base);
        assert!(as_type_memo_slot(&v) < AS_TYPE_MEMO_SLOTS);
    }
}
}

/// gcj j1/jt (page
/// `gci-i47w-oz47-jvmti-reflective-constructor-posts-no-vmobjectalloc`): with a
/// JVMTI `VMObjectAlloc` listener armed, both one-call constructor lanes
/// decline before reading the handle, so the handle's own form (whose
/// `Unsafe.allocateInstance` posts the event) runs. The positive control is
/// the probe row (`L1W63JvmtiEvents` `allocRows`, debug build).
#[cfg(test)]
mod gcj1jt_ctor_lane_vm_object_alloc_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{NativeHeapAccess, NativeSystemAccess};
use cratonvm_types::flags::with_thread_overrides;

/// gcj j2/x1: the test above cannot fail -- its receiver is no
/// `DirectMethodHandle$Constructor`, so both lanes decline at their class
/// check with the armed test deleted. This pins the decline itself: in
/// both lanes it precedes the receiver decode, and in the fast lane the
/// memo hit too (a memo hit would instantiate with no event).
#[test]
fn gcj2x1_the_armed_decline_precedes_the_memo_and_the_receiver_decode() {
    let src: &'static str = LANG_INVOKE_SRC;
    for (f, memo) in [
        ("fn mh_real_constructor_fast(", true),
        ("fn mh_real_ctor_widening_serve(", false),
    ] {
        let at = src.find(f).expect(f);
        let end = src[at..].find("\n}\n").expect("fn end");
        let body = &src[at..at + end];
        let armed = body.find("ctx.jvmti_vm_object_alloc_armed()").expect("the decline");
        let decode = body.find("ctx.class_id_of_object(this)").expect("the decode");
        assert!(armed < decode, "{f}: decline before the decode");
        if memo {
            let hit = body.find("ctor_lane_memo_get(key)").expect("the memo");
            assert!(armed < hit, "{f}: decline before the memo hit");
        }
    }
}
}

/// Interpreter round i4 wave 88, lane L4: `CRATONVM_MH_REAL_CTOR_WIDENING_SERVE`
/// (default off) parses as a `truthy_word` switch; no default is pinned.
#[cfg(test)]
mod i88_l4_ctor_widening_serve_tests {
use cratonvm_types::flags::with_thread_overrides;

}

/// Interpreter round i3 wave 82, lane L4: the METHOD finder mint memo
/// (`CRATONVM_MH_METHOD_FINDER_MINT_MEMO`, default off). Every switch is set
/// explicitly through a thread override; no default is pinned. The
/// end-to-end rows are `L4W82MethodFinderMint.java`
/// (behaviour) and `L4W81MethodFinderBench.java`.


/// Interpreter round i3 wave 74, lane L4: the `MethodType.makeImpl` intern memo
/// (`CRATONVM_MH_METHOD_TYPE_INTERN_MEMO`, default off). Every switch is set
/// explicitly through a thread override. The end-to-end rows are
/// `L4W74MethodTypeIntern.java` and
/// `L4W74FinderMintSplitBench.java`.


/// Interpreter round i3 wave 71, lane L4: a finder's field accessor shares its
/// member words (`CRATONVM_MH_FINDER_SHARED_MEMBER_WORDS`, default off) and the
/// linking call is served from what the link attempt admitted
/// (`CRATONVM_MH_LINKING_CALL_DIRECT_SERVE`, default off). Every switch is set
/// explicitly through a thread override. The end-to-end rows are
/// `L4W71FinderLinkServe.java` and
/// `L4W71LinkingCallBench.java`.


/// Interpreter round i2 wave 64, lane L4: a `findSpecial` handle records its
/// dispatch class by id (`CRATONVM_MH_SPECIAL_BY_OWNER_ID`). The helpers take
/// the switch as a parameter; the end-to-end rows are
/// `L4W64SpecialByOwnerId.java`.
#[cfg(test)]
mod i64_l4_special_by_owner_id_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn the_dispatch_owner_is_refc_or_the_callers_superclass_by_id() {
    let mut ctx = MockNativeContext::new();
    let grand = ctx.ensure_class_initialized("p/Grand").expect("p/Grand");
    let mid = ctx.ensure_class_initialized("p/Mid").expect("p/Mid");
    let sub = ctx.ensure_class_initialized("p/Sub").expect("p/Sub");
    ctx.set_superclass(mid, grand);
    ctx.set_superclass(sub, mid);
    // A second loader's `p/Grand`: the name alone cannot tell them apart.
    let grand_b = ctx.declare_second_copy("p/Grand");
    // `refc` itself.
    assert_eq!(
        special_dispatch_owner(&ctx, "p/Grand", "p/Grand", Some(grand), Some(sub)),
        Some(grand)
    );
    assert_eq!(
        special_dispatch_owner(&ctx, "p/Grand", "p/Grand", Some(grand_b), Some(sub)),
        Some(grand_b),
        "the refc mirror's own class, not the name's"
    );
    // The restart at the caller's direct superclass.
    assert_eq!(
        special_dispatch_owner(&ctx, "p/Mid", "p/Grand", Some(grand), Some(sub)),
        Some(mid)
    );
    // Nothing guessed: no id, a name the id does not carry, no class.
    assert_eq!(
        special_dispatch_owner(&ctx, "p/Grand", "p/Grand", None, Some(sub)),
        None
    );
    assert_eq!(
        special_dispatch_owner(&ctx, "p/Mid", "p/Grand", Some(grand), None),
        None
    );
    assert_eq!(
        special_dispatch_owner(&ctx, "p/Other", "p/Grand", Some(grand), Some(sub)),
        None
    );
    assert_eq!(
        special_dispatch_owner(&ctx, "p/Grand", "p/Grand", Some(mid), Some(sub)),
        None
    );
    assert_eq!(
        special_dispatch_owner(&ctx, "", "", Some(grand), Some(sub)),
        None
    );
}

#[test]
fn a_special_handle_records_and_reads_its_owner_only_when_on() {
    let mut ctx = MockNativeContext::new();
    let owner = ctx.ensure_class_initialized("p/Grand").expect("p/Grand");
    // Classless: the slot helpers read no name (and the mock builds no
    // receiver-typed `MethodType`).
    let mh = alloc_method_handle(&mut ctx, "", "m", "()I", MH_KIND_SPECIAL).unwrap();
    mh_record_special_owner_with(&ctx, mh, Some(owner), false);
    assert_eq!(
        mh_special_owner_with(&ctx, mh, true),
        None,
        "off: nothing recorded"
    );
    mh_record_special_owner_with(&ctx, mh, Some(owner), true);
    assert_eq!(mh_special_owner_with(&ctx, mh, true), Some(owner));
    assert_eq!(
        mh_special_owner_with(&ctx, mh, false),
        None,
        "off: nothing read"
    );
    // An `asType` copy keeps it (the member is the same).
    let copy = mh_clone_handle(&mut ctx, mh).expect("copy");
    assert_eq!(mh_special_owner_with(&ctx, copy, true), Some(owner));
    // A narrow handle: neither.
    let narrow = ctx.alloc_object(owner, MH_STATIC_OWNER);
    mh_record_special_owner_with(&ctx, narrow, Some(owner), true);
    assert_eq!(mh_special_owner_with(&ctx, narrow, true), None);
}

/// The orchestrator's fix of `L4W64SpecialByOwnerId` row
/// `special-mid-on-mid`: the recorded `specialCaller` id casts the
/// receiver, refusing another class with HotSpot's message.
#[test]
fn the_special_caller_id_casts_the_receiver() {
    let mut ctx = MockNativeContext::new();
    let mid = ctx.ensure_class_initialized("p/Mid").expect("p/Mid");
    let sub = ctx.ensure_class_initialized("p/Sub").expect("p/Sub");
    // Only a caller id that carries the caller's name is recorded.
    assert_eq!(
        special_caller_owner(&ctx, Some("p/Sub"), Some(sub)),
        Some(sub)
    );
    assert_eq!(special_caller_owner(&ctx, Some("p/Sub"), Some(mid)), None);
    assert_eq!(special_caller_owner(&ctx, Some("p/Sub"), None), None);
    assert_eq!(special_caller_owner(&ctx, None, Some(sub)), None);
    // Recorded and read in `MH_STATIC_SETTLED`, only when on.
    let mh = alloc_method_handle(&mut ctx, "", "m", "()I", MH_KIND_SPECIAL).unwrap();
    mh_record_special_caller_with(&ctx, mh, Some(sub), false);
    assert_eq!(mh_special_caller_with(&ctx, mh, true), None);
    mh_record_special_caller_with(&ctx, mh, Some(sub), true);
    assert_eq!(mh_special_caller_with(&ctx, mh, true), Some(sub));
    assert_eq!(mh_special_caller_with(&ctx, mh, false), None);
    // The verdict: the caller's own instance admitted, a `Mid` refused.
    let s = ctx.alloc_object(sub, 1);
    let m = ctx.alloc_object(mid, 1);
    assert!(matches!(
        special_caller_receiver_verdict(&mut ctx, s, sub),
        Some(None)
    ));
    match special_caller_receiver_verdict(&mut ctx, m, sub) {
        Some(Some(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::ClassCastException { message },
        )))) => assert_eq!(message, "Cannot cast p.Mid to p.Sub"),
        other => panic!("expected the caller cast's refusal: {other:?}"),
    }
}
}

/// Round 12 wave 7, lane mh5: the adapter chain of an `asType` /
/// `explicitCastArguments` copy (`MH_CAST_CHAIN`), one test per conversion row
/// of [`cast_chain_step`], and the door-side changes of the same wave. The
/// mock has no by-name hierarchy walk, so a POSITIVE reference mismatch is
/// always an array (an array is only an `Object`, `Cloneable` or
/// `Serializable`); a class value it cannot contradict must pass. The
/// end-to-end rows are `R12Hunt4MhAsTypeChain` and `R12Mh5CastChainRows`.
#[cfg(test)]
mod r12w7_mh5_cast_chain_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn object(ctx: &mut MockNativeContext, class: &str, payload: Value) -> Value {
    let o = match ctx.new_object(class) {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => panic!("new_object {class}: {other:?}"),
    };
    ctx.set_field(o, 0, payload);
    Value::Object(Some(o))
}

fn int_array(ctx: &mut MockNativeContext) -> Value {
    Value::Object(Some(ctx.new_array(ArrayElementType::Int, 1)))
}

fn mt(ctx: &mut MockNativeContext, desc: &str) -> ObjectRef {
    match build_method_type_from_descriptor(ctx, desc) {
        Ok(Some(mt)) => mt,
        other => panic!("MethodType {desc}: {other:?}"),
    }
}

/// Declare `MethodType`'s `rtype` / `ptypes` so `methodtype_to_descriptor`
/// can read a type back (the mock resolves no field by name otherwise).
fn readable_types(ctx: &mut MockNativeContext) {
    let probe = mt(ctx, "()V");
    let cid = ctx.class_id_of_object(probe);
    let field = |name: &str, descriptor: &str, slot_index: usize| FieldMetadata {
        name: name.to_string(),
        descriptor: descriptor.to_string(),
        access_flags: 0,
        slot_index,
        declaring_class_id: cid,
        is_static: false,
    };
    ctx.set_declared_fields(
        cid,
        vec![
            field("rtype", "Ljava/lang/Class;", 0),
            field("ptypes", "[Ljava/lang/Class;", 1),
        ],
    );
}

fn setup() -> MockNativeContext {
    let mut ctx = MockNativeContext::new();
    for name in ["p/C", "p/Grand", "p/Mid", "p/Sub", "p/Greeter"] {
        let _ = ctx.ensure_class_initialized(name);
    }
    if let Ok(greeter) = ctx.ensure_class_initialized("p/Greeter") {
        ctx.set_is_interface(greeter, true);
    }
    readable_types(&mut ctx);
    ctx
}

fn handle(ctx: &mut MockNativeContext, class: &str, desc: &str, kind: i32) -> ObjectRef {
    match alloc_method_handle(ctx, class, "m", desc, kind) {
        Ok(mh) => mh,
        Err(e) => panic!("alloc {class}: {e:?}"),
    }
}

fn stamp(ctx: &mut MockNativeContext, h: ObjectRef, desc: &str, explicit: bool) -> ObjectRef {
    let t = mt(ctx, desc);
    match mh_with_stamped_type(ctx, Value::Object(Some(h)), t, explicit) {
        Ok(Some(Value::Object(Some(copy)))) => copy,
        other => panic!("stamp {desc}: {other:?}"),
    }
}

fn cce_message(e: &MethodCallFailed) -> Option<String> {
    match e {
        MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::ClassCastException { message, .. },
        )) => Some(message.clone()),
        _ => None,
    }
}

/// The class a `ClassCastException` names as its target (the value's own
/// name, for an array, is the heap's to spell).
fn cce_target(e: &MethodCallFailed) -> Option<String> {
    cce_message(e).and_then(|m| m.rsplit(" to ").next().map(str::to_string))
}

fn is_npe(e: &MethodCallFailed) -> bool {
    matches!(
        e,
        MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::NullPointerException { .. }
        ))
    )
}

fn step(
    ctx: &mut MockNativeContext,
    v: ChainValue,
    from: &str,
    to: &str,
    explicit: bool,
) -> Result<ChainValue, MethodCallFailed> {
    cast_chain_step(ctx, v, from, to, explicit)
}

const OBJ: &str = "Ljava/lang/Object;";

fn plain(v: Value) -> Option<ChainValue> {
    Some(ChainValue::Plain(v))
}

#[test]
fn the_chain_text_round_trips_and_refuses_what_it_cannot_parse() {
    let chain = CastChain {
        bound_refusal: Some("Lp/Sub;".to_string()),
        links: vec![
            CastLink {
                explicit: false,
                desc: "(Lp/Sub;)I".to_string(),
            },
            // A class name may hold `)` and `;`-free punctuation.
            CastLink {
                explicit: true,
                desc: "(La)b;[[J)[Lx/Y;".to_string(),
            },
        ],
    };
    let text = render_cast_chain(&chain);
    assert_eq!(text, "!Lp/Sub;S(Lp/Sub;)IE(La)b;[[J)[Lx/Y;");
    assert_eq!(parse_cast_chain(&text), Some(chain));
    assert_eq!(parse_cast_chain("X()V"), None);
    assert_eq!(parse_cast_chain("S(I"), None);
    assert_eq!(parse_cast_chain("S()"), None);
    let bare = CastChain {
        bound_refusal: Some("I".to_string()),
        links: Vec::new(),
    };
    assert_eq!(parse_cast_chain("!I"), Some(bare));
    assert!(!CastChain::default().is_active());
}

/// Row: reference -> reference. `asType` casts (an array is not a `Sub`),
/// a value the hierarchy cannot contradict and a `null` pass.
#[test]
fn row_reference_to_reference_casts() {
    let mut ctx = setup();
    let arr = int_array(&mut ctx);
    let s = object(&mut ctx, "java/lang/String", Value::Int(0));
    let r = step(&mut ctx, ChainValue::Plain(arr), OBJ, "Lp/Sub;", false);
    assert_eq!(
        r.as_ref().err().and_then(cce_target).as_deref(),
        Some("p.Sub")
    );
    assert_eq!(
        step(&mut ctx, ChainValue::Plain(s), OBJ, "Lp/Sub;", false).ok(),
        plain(s)
    );
    let null = ChainValue::Plain(Value::Object(None));
    assert_eq!(step(&mut ctx, null, OBJ, "Lp/Sub;", false).ok(), Some(null));
}

/// Row: reference -> INTERFACE. `asType` casts; `explicitCastArguments`
/// passes the value "without a cast".
#[test]
fn row_reference_to_interface_is_uncast_only_under_an_explicit_link() {
    let mut ctx = setup();
    let arr = int_array(&mut ctx);
    assert!(step(&mut ctx, ChainValue::Plain(arr), OBJ, "Lp/Greeter;", false).is_err());
    assert_eq!(
        step(&mut ctx, ChainValue::Plain(arr), OBJ, "Lp/Greeter;", true).ok(),
        Some(ChainValue::Plain(arr))
    );
    // A class target still casts under an explicit link.
    assert!(step(&mut ctx, ChainValue::Plain(arr), OBJ, "Lp/Sub;", true).is_err());
}

/// Row: reference -> primitive under `asType`: a `null` is an NPE, a
/// non-widening wrapper and a non-wrapper are a CCE, a widening wrapper
/// converts.
#[test]
fn row_reference_to_primitive_unboxes_or_refuses() {
    let mut ctx = setup();
    let long = object(&mut ctx, "java/lang/Long", Value::Long(2));
    let short = object(&mut ctx, "java/lang/Short", Value::Int(2));
    let s = object(&mut ctx, "java/lang/String", Value::Int(0));
    let null = ChainValue::Plain(Value::Object(None));
    assert!(step(&mut ctx, null, OBJ, "I", false)
        .err()
        .is_some_and(|e| is_npe(&e)));
    assert!(step(&mut ctx, ChainValue::Plain(long), OBJ, "I", false)
        .err()
        .is_some_and(|e| cce_message(&e).is_some()));
    assert!(step(&mut ctx, ChainValue::Plain(s), OBJ, "I", false)
        .err()
        .is_some_and(|e| cce_message(&e).is_some()));
    assert_eq!(
        step(&mut ctx, ChainValue::Plain(short), OBJ, "I", false).ok(),
        Some(ChainValue::Plain(Value::Int(2)))
    );
}

/// Row: reference -> primitive under `explicitCastArguments`: a `null`
/// is the zero, a `Long` narrows, a `Boolean` is 1.
#[test]
fn row_reference_to_primitive_explicitly_casts() {
    let mut ctx = setup();
    let long = object(&mut ctx, "java/lang/Long", Value::Long(0x1_0000_0005));
    let yes = object(&mut ctx, "java/lang/Boolean", Value::Int(1));
    let null = ChainValue::Plain(Value::Object(None));
    assert_eq!(
        step(&mut ctx, null, OBJ, "I", true).ok(),
        plain(Value::Int(0))
    );
    assert_eq!(
        step(&mut ctx, ChainValue::Plain(long), OBJ, "I", true).ok(),
        Some(ChainValue::Plain(Value::Int(5)))
    );
    assert_eq!(
        step(&mut ctx, ChainValue::Plain(yes), OBJ, "I", true).ok(),
        Some(ChainValue::Plain(Value::Int(1)))
    );
}

/// Row: primitive -> primitive. `asType` widens; an explicit link casts
/// (a narrowing, `d2i`, and to `boolean` the low bit).
#[test]
fn row_primitive_to_primitive_widens_or_casts() {
    let mut ctx = setup();
    let three = ChainValue::Plain(Value::Int(3));
    assert_eq!(
        step(&mut ctx, three, "I", "J", false).ok(),
        plain(Value::Long(3))
    );
    let big = ChainValue::Plain(Value::Int(300));
    assert_eq!(
        step(&mut ctx, big, "I", "B", true).ok(),
        plain(Value::Int(44))
    );
    let six = ChainValue::Plain(Value::Int(6));
    assert_eq!(
        step(&mut ctx, six, "I", "Z", true).ok(),
        plain(Value::Int(0))
    );
    let d = ChainValue::Plain(Value::Double(-2.9));
    assert_eq!(
        step(&mut ctx, d, "D", "I", true).ok(),
        plain(Value::Int(-2))
    );
    // A wrapper object standing at a primitive position (the generic
    // door) is its primitive.
    let short = object(&mut ctx, "java/lang/Short", Value::Int(7));
    assert_eq!(
        step(&mut ctx, ChainValue::Plain(short), "S", "J", false).ok(),
        Some(ChainValue::Plain(Value::Long(7)))
    );
}

/// Row: primitive -> reference: boxed to the source's wrapper, whose
/// supertypes are a closed table, then cast.
#[test]
fn row_primitive_to_reference_boxes_then_casts() {
    let mut ctx = setup();
    let five = ChainValue::Plain(Value::Int(5));
    assert_eq!(
        step(&mut ctx, five, "I", "Ljava/lang/Number;", false).ok(),
        Some(ChainValue::Boxed("I", Value::Int(5)))
    );
    let r = step(&mut ctx, five, "I", "Ljava/lang/Long;", true);
    assert_eq!(
        r.as_ref().err().and_then(cce_message).as_deref(),
        Some("Cannot cast java.lang.Integer to java.lang.Long")
    );
    assert!(step(&mut ctx, five, "I", "Lp/Greeter;", false).is_err());
    assert_eq!(
        step(&mut ctx, five, "I", "Lp/Greeter;", true).ok(),
        Some(ChainValue::Boxed("I", Value::Int(5)))
    );
}

/// Row: a box a previous link made, cast again and unboxed again.
#[test]
fn row_a_box_is_judged_by_its_wrapper_class() {
    let mut ctx = setup();
    let boxed = ChainValue::Boxed("I", Value::Int(5));
    assert_eq!(
        step(&mut ctx, boxed, OBJ, "Ljava/lang/Integer;", false).ok(),
        Some(boxed)
    );
    assert!(step(&mut ctx, boxed, OBJ, "Ljava/lang/String;", false).is_err());
    assert_eq!(
        step(&mut ctx, boxed, OBJ, "J", false).ok(),
        plain(Value::Long(5))
    );
    let r = step(&mut ctx, boxed, OBJ, "S", false);
    assert_eq!(
        r.as_ref().err().and_then(cce_message).as_deref(),
        Some("Cannot cast java.lang.Integer to java.lang.Short")
    );
    assert_eq!(
        step(&mut ctx, boxed, OBJ, "S", true).ok(),
        plain(Value::Int(5))
    );
}

/// What the judge cannot follow is never refused.
#[test]
fn what_cannot_be_followed_is_opaque() {
    let mut ctx = setup();
    let raw = ChainValue::Plain(Value::Int(1));
    assert_eq!(
        step(&mut ctx, raw, OBJ, "Lp/Sub;", false).ok(),
        Some(ChainValue::Opaque)
    );
    assert_eq!(
        step(&mut ctx, raw, "V", OBJ, false).ok(),
        Some(ChainValue::Opaque)
    );
    assert_eq!(
        step(&mut ctx, raw, "I", "V", false).ok(),
        Some(ChainValue::Opaque)
    );
    let null = ChainValue::Plain(Value::Object(None));
    assert_eq!(
        step(&mut ctx, null, "I", "J", false).ok(),
        Some(ChainValue::Opaque)
    );
    assert_eq!(
        step(&mut ctx, ChainValue::Opaque, OBJ, "I", false).ok(),
        Some(ChainValue::Opaque)
    );
}

/// `takesObj(Object).asType((Sub)int).asType((Object)int)`: the second
/// stamp keeps the middle `Sub`, and the dispatch judge refuses a non-Sub
/// there although outer and leaf are both `Object`.
#[test]
fn a_stamp_over_a_stamp_keeps_the_middle_type() {
    let mut ctx = setup();
    let h = handle(&mut ctx, "p/C", "(Ljava/lang/Object;)I", MH_KIND_STATIC);
    let c1 = stamp(&mut ctx, h, "(Lp/Sub;)I", false);
    assert_eq!(
        mh_cast_chain_text(&ctx, c1).as_deref(),
        Some("S(Ljava/lang/Object;)I")
    );
    assert_eq!(
        mh_marking_bits(&ctx, c1) & MH_MARK_CAST_CHAIN,
        0,
        "one stamp: no work"
    );
    let c2 = stamp(&mut ctx, c1, "(Ljava/lang/Object;)I", false);
    assert_eq!(
        mh_cast_chain_text(&ctx, c2).as_deref(),
        Some("S(Lp/Sub;)IS(Ljava/lang/Object;)I")
    );
    assert_ne!(mh_marking_bits(&ctx, c2) & MH_MARK_CAST_CHAIN, 0);
    // The receivers are untouched.
    assert_eq!(mh_cast_chain_text(&ctx, h), None);
    assert_eq!(
        mh_cast_chain_text(&ctx, c1).as_deref(),
        Some("S(Ljava/lang/Object;)I")
    );

    let chain =
        parse_cast_chain(&mh_cast_chain_text(&ctx, c2).unwrap_or_default()).unwrap_or_default();
    let outer = Some("(Ljava/lang/Object;)I");
    let arr = int_array(&mut ctx);
    let r = mh_cast_chain_judge_args(&mut ctx, c2, &chain, outer, &[arr]);
    assert_eq!(
        r.err().as_ref().and_then(cce_target).as_deref(),
        Some("p.Sub")
    );
    let s = object(&mut ctx, "java/lang/String", Value::Int(0));
    assert!(mh_cast_chain_judge_args(&mut ctx, c2, &chain, outer, &[s]).is_ok());
    // Another arity (a collector's) is not judged.
    assert!(mh_cast_chain_judge_args(&mut ctx, c2, &chain, outer, &[arr, arr]).is_ok());
}

/// `twiceLong.asType((int)long).explicitCastArguments((long)long)`: the
/// explicit middle link narrows, so the leaf is handed `(long)(int) x`;
/// a chain that changes nothing hands the arguments on as passed.
#[test]
fn a_middle_primitive_conversion_reaches_the_leaf() {
    let mut ctx = setup();
    let h = handle(&mut ctx, "p/C", "(J)J", MH_KIND_STATIC);
    let narrowed = stamp(&mut ctx, h, "(I)J", false);
    let c = stamp(&mut ctx, narrowed, "(J)J", true);
    assert_eq!(mh_cast_chain_text(&ctx, c).as_deref(), Some("E(I)JS(J)J"));
    assert!(
        !mh_is_explicit_cast(&ctx, c),
        "the innermost link is asType's"
    );
    let chain = parse_cast_chain("E(I)JS(J)J").unwrap_or_default();
    let wide = [Value::Long(0x1_0000_0005)];
    let r = mh_cast_chain_judge_args(&mut ctx, c, &chain, Some("(J)J"), &wide);
    assert_eq!(r.ok(), Some(Some(vec![Value::Long(5)])));
    let small = [Value::Long(5)];
    let r = mh_cast_chain_judge_args(&mut ctx, c, &chain, Some("(J)J"), &small);
    assert_eq!(r.ok(), Some(None));
}

/// `retObj.asType((Object)Integer).asType((Object)Object)`: the middle
/// RETURN type is judged on the way out.
#[test]
fn the_middle_return_type_is_judged() {
    let mut ctx = setup();
    let chain = CastChain {
        bound_refusal: None,
        links: vec![
            CastLink {
                explicit: false,
                desc: "(Ljava/lang/Object;)Ljava/lang/Integer;".to_string(),
            },
            CastLink {
                explicit: false,
                desc: "(Ljava/lang/Object;)Ljava/lang/Object;".to_string(),
            },
        ],
    };
    let outer = Some("(Ljava/lang/Object;)Ljava/lang/Object;");
    let arr = int_array(&mut ctx);
    let r = mh_cast_chain_judge_return(&mut ctx, &chain, outer, Ok(Some(arr)));
    assert_eq!(
        r.err().as_ref().and_then(cce_target).as_deref(),
        Some("java.lang.Integer")
    );
    let int = object(&mut ctx, "java/lang/Integer", Value::Int(4));
    let passed = mh_cast_chain_judge_return(&mut ctx, &chain, outer, Ok(Some(int)));
    assert_eq!(passed.ok(), Some(Some(int)));
    let void = mh_cast_chain_judge_return(&mut ctx, &chain, outer, Ok(None));
    assert_eq!(void.ok(), Some(None));
}

/// The explicit marking: a FIRST explicit stamp marks the copy (the leaf's
/// conversion); an explicit stamp over a stamp is a middle link and keeps
/// the receiver's marking; with the switch off every explicit copy is
/// marked and nothing is recorded, as before.
/// `findVirtual(Mid, m, (int)int).asType((Object,int)int).bindTo(v)`: the
/// bound handle carries the chain minus the receiver, and a value the
/// leaf receiver type refuses is recorded as the bound refusal that
/// every invocation raises.
#[test]
fn bind_to_after_as_type_records_the_bound_values_refused_cast() {
    let mut ctx = setup();
    let h = handle(&mut ctx, "p/Mid", "(I)I", MH_KIND_VIRTUAL);
    let wide = stamp(&mut ctx, h, "(Ljava/lang/Object;I)I", false);
    assert_eq!(
        mh_cast_chain_text(&ctx, wide).as_deref(),
        Some("S(Lp/Mid;I)I")
    );
    let arr = int_array(&mut ctx);
    let chain = bound_cast_chain(&mut ctx, wide, arr, MH_KIND_VIRTUAL);
    let expected = CastChain {
        bound_refusal: Some("Lp/Mid;".to_string()),
        links: vec![CastLink {
            explicit: false,
            desc: "(I)I".to_string(),
        }],
    };
    assert_eq!(chain.as_ref(), Some(&expected));
    let s = object(&mut ctx, "java/lang/String", Value::Int(0));
    let passes = bound_cast_chain(&mut ctx, wide, s, MH_KIND_VIRTUAL);
    assert_eq!(passes.and_then(|c| c.bound_refusal), None);
    // An unstamped handle has nothing to carry.
    assert_eq!(bound_cast_chain(&mut ctx, h, arr, MH_KIND_VIRTUAL), None);

    // The dispatch judge raises the recorded refusal against MH_BOUND.
    let bound = handle(&mut ctx, "p/Mid", "(I)I", MH_KIND_VIRTUAL);
    ctx.set_field(bound, MH_BOUND, arr);
    let three = [Value::Int(3)];
    let r = mh_cast_chain_judge_args(&mut ctx, bound, &expected, Some("(I)I"), &three);
    assert_eq!(
        r.err().as_ref().and_then(cce_target).as_deref(),
        Some("p.Mid")
    );
}

/// The door's leaf pass no longer casts to an interface parameter of an
/// `explicitCastArguments` copy; an `asType` copy still does.
/// `mh_type_returns_reference` answers what `mh_type_descriptor`'s
/// return would, including its `MH_DESC` fallback.
#[test]
fn the_cheap_return_check_matches_the_full_descriptor() {
    let mut ctx = setup();
    let h = handle(&mut ctx, "p/C", "(I)I", MH_KIND_STATIC);
    assert!(!mh_type_returns_reference(&mut ctx, h));
    let boxed = stamp(&mut ctx, h, "(I)Ljava/lang/Object;", false);
    assert!(mh_type_returns_reference(&mut ctx, boxed));
    let untyped = handle(&mut ctx, "p/C", "(I)Ljava/lang/String;", MH_KIND_STATIC);
    ctx.set_field_by_name(untyped, "type", Value::Object(None));
    assert!(mh_type_returns_reference(&mut ctx, untyped));
    for mh in [h, boxed, untyped] {
        let full = mh_type_descriptor(&mut ctx, mh).unwrap_or_default();
        assert_eq!(
            mh_type_returns_reference(&mut ctx, mh),
            matches!(
                return_type_desc(&full).as_bytes().first(),
                Some(b'L' | b'[')
            )
        );
    }
}

/// The exact-class fast path of `reference_arg_admitted`.
#[test]
fn an_exact_class_is_admitted_without_a_lookup() {
    let mut ctx = setup();
    let s = object(&mut ctx, "java/lang/String", Value::Int(0));
    assert_eq!(
        reference_arg_admitted(&mut ctx, s, "Ljava/lang/String;"),
        Some(true)
    );
    let arr = int_array(&mut ctx);
    assert_eq!(
        reference_arg_admitted(&mut ctx, arr, "Lp/Sub;"),
        Some(false)
    );
}

/// Round 13 wave 1 (lane mhffm), residual 1 (`R12Mh5ChainResidual` R1):
/// `retLong(int)long.explicitCastArguments((int)int).asType((int)long)`
/// answers `(long)(int) r`; with `CRATONVM_MH_CAST_CHAIN_CONVERTS=0` the
/// untruncated value, as in round 12.
#[test]
fn a_middle_link_narrowing_the_return_is_delivered() {
    let mut ctx = setup();
    let chain = CastChain {
        bound_refusal: None,
        links: vec![
            CastLink {
                explicit: false,
                desc: "(I)I".to_string(),
            },
            CastLink {
                explicit: true,
                desc: "(I)J".to_string(),
            },
        ],
    };
    let r = (5_i64 << 32) | 7;
    let got =
        mh_cast_chain_judge_return(&mut ctx, &chain, Some("(I)J"), Ok(Some(Value::Long(r))));
    assert_eq!(got.ok(), Some(Some(Value::Long(7))));
}

/// Round 13 wave 3 (lane ffm2), the cross-variant return:
/// `retLong.explicitCastArguments((int)int).asType((int)double)` answers
/// `(double)(int) r`. It is delivered in the LEAF's variant (`Long(7)`),
/// which the doors widen to `7.0`; before, the untruncated `Long(r)` went
/// on and was widened to `(double) r`.
#[test]
fn a_middle_link_narrowing_is_delivered_across_value_variants() {
    let mut ctx = setup();
    let chain = CastChain {
        bound_refusal: None,
        links: vec![
            CastLink {
                explicit: false,
                desc: "(I)I".to_string(),
            },
            CastLink {
                explicit: true,
                desc: "(I)J".to_string(),
            },
        ],
    };
    let r = (5_i64 << 32) | -3_i64 as u32 as i64;
    let got =
        mh_cast_chain_judge_return(&mut ctx, &chain, Some("(I)D"), Ok(Some(Value::Long(r))));
    assert_eq!(got.ok(), Some(Some(Value::Long(-3))));
    assert_eq!(
        widen_return_value(Value::Long(-3), b'J', b'D'),
        Some(Value::Double(-3.0))
    );
}

/// A chain answer no leaf-typed value maps to keeps the old answer:
/// `(double)(float) Integer.MAX_VALUE` is `2^31`, which no `int` widens to.
#[test]
fn a_cross_variant_return_with_no_leaf_spelling_is_left_alone() {
    let mut ctx = setup();
    let chain = CastChain {
        bound_refusal: None,
        links: vec![
            CastLink {
                explicit: false,
                desc: "(I)F".to_string(),
            },
            CastLink {
                explicit: false,
                desc: "(I)I".to_string(),
            },
        ],        };
    let got = mh_cast_chain_judge_return(
        &mut ctx,
        &chain,
        Some("(I)D"),
        Ok(Some(Value::Int(i32::MAX))),
    );
    assert_eq!(got.ok(), Some(Some(Value::Int(i32::MAX))));
    assert_eq!(
        chain_return_in_leaf_variant(Value::Double(2147483648.0), "I", "D"),
        None
    );
    assert_eq!(
        chain_return_in_leaf_variant(Value::Double(-3.0), "I", "D"),
        Some(Value::Int(-3))
    );
    // A sub-int leaf never gets a value outside its range.
    assert_eq!(
        chain_return_in_leaf_variant(Value::Long(300), "B", "J"),
        None
    );
}

/// The `invoke` door boxes against the declared return only for a
/// reference call site over a primitive declared return the leaf does not
/// share.
#[test]
fn invoke_boxes_the_declared_return_only_for_that_shape() {
    let obj = Some("()Ljava/lang/Object;");
    assert!(invoke_boxes_declared_return("()J", Some("()D"), obj));
    assert!(!invoke_boxes_declared_return("()J", Some("()J"), obj));
    assert!(!invoke_boxes_declared_return(
        "()J",
        Some("()D"),
        Some("()D")
    ));
    assert!(!invoke_boxes_declared_return(
        "()Ljava/lang/Long;",
        Some("()D"),
        obj
    ));
    assert!(!invoke_boxes_declared_return("()J", None, obj));
    assert!(!invoke_boxes_declared_return("()J", Some("()D"), None));
    // `invokeWithArguments`: its call site is always `Object`.
    assert!(invoke_with_arguments_boxes_declared_return(
        "(I)J",
        Some("(I)D")
    ));
    assert!(!invoke_with_arguments_boxes_declared_return(
        "(I)J",
        Some("(I)J")
    ));
    assert!(!invoke_with_arguments_boxes_declared_return(
        "(I)J",
        Some("(I)Ljava/lang/Object;")
    ));
    assert!(!invoke_with_arguments_boxes_declared_return("(I)J", None));
}

/// Residual 2 (`R12Mh5ChainResidual` R2):
/// `idObj(Object)Object.asType((int)Object).asType((Object)Object)` hands
/// `idObj` an `Integer` made from the caller's `Short`, not the `Short`.
#[test]
fn a_middle_primitive_link_reboxes_the_argument() {
    let mut ctx = setup();
    // Round 13 wave 3 (lane ffm2): a PRIVATE VM identity. The re-box goes
    // through `Integer.valueOf`, whose cache (`lang_math`
    // `integer_cache_table`) and wrapper class-id cache (`alloc_wrapper`)
    // are process-wide tables keyed by `vm_identity()`, which every mock
    // reports as `0` by default. Under the shared `0`, `Integer.valueOf(12)`
    // could hand back a box another test's mock allocated (or a class id
    // only that mock knows), whose class this mock cannot name: the
    // `left: None` failure. The production path is right; one VM, one
    // identity.
    ctx.set_vm_identity(0x13F3_0001);
    let h = handle(
        &mut ctx,
        "p/C",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        MH_KIND_STATIC,
    );
    let chain = CastChain {
        bound_refusal: None,
        links: vec![
            CastLink {
                explicit: false,
                desc: "(I)Ljava/lang/Object;".to_string(),
            },
            CastLink {
                explicit: false,
                desc: "(Ljava/lang/Object;)Ljava/lang/Object;".to_string(),
            },
        ],
    };
    let outer = Some("(Ljava/lang/Object;)Ljava/lang/Object;");
    let short = object(&mut ctx, "java/lang/Short", Value::Int(12));
    let judged = mh_cast_chain_judge_args_rebox(&mut ctx, h, &chain, outer, &[short]);
    let (values, reboxes) = match judged {
        Ok(pair) => pair,
        Err(e) => panic!("the chain refused a Short: {e:?}"),
    };
    assert!(values.is_none());
    assert_eq!(reboxes.len(), 1);
    assert_eq!(reboxes[0].0, 0);
    assert_eq!(reboxes[0].1, DESC_INT);
    assert_eq!(reboxes[0].2, Value::Int(12));
    let (_h, out) = mh_cast_chain_rebox_args(&mut ctx, h, vec![short], &reboxes);
    let boxed = match out.first() {
        Some(Value::Object(Some(o))) => *o,
        other => panic!("no box: {other:?}"),
    };
    assert_ne!(Value::Object(Some(boxed)), short);
    assert_eq!(
        ctx.class_name_of_id(ctx.class_id_of_object(boxed))
            .as_deref(),
        Some("java/lang/Integer")
    );
}
}

/// Interpreter round i1 wave 28, lane L4: an invoker handle checks its call
/// site and its target's type as HotSpot's `Invokers` do, and an
/// `explicitCastArguments` stamp passes a reference to an interface parameter
/// uncast through both doors. Probe:
/// `L4W28InvokerChecks.java`.
#[cfg(test)]
mod w28_invoker_check_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{NativeClassAccess, NativeExceptionAccess, NativeHeapAccess};

fn is_wmte(ctx: &MockNativeContext, err: &MethodCallFailed) -> bool {
    match err {
        MethodCallFailed::ExceptionThrown(o) => {
            ctx.class_name_of_id(ctx.class_id_of_object(*o)).as_deref()
                == Some("java/lang/invoke/WrongMethodTypeException")
        }
        _ => false,
    }
}

fn method_type(ctx: &mut MockNativeContext, desc: &str) -> ObjectRef {
    match build_method_type_from_descriptor(ctx, desc) {
        Ok(Some(mt)) => mt,
        other => panic!("MethodType {desc}: {other:?}"),
    }
}

/// `exactInvoker(t)`: marked exact, `MH_DESC` = `t`, and an `invokeExact`
/// call site on it is judged against its own `(MethodHandle, t...)R`.
/// Without `MH_DESC` (the old shape, whose `type` is `t`) it is not.
#[test]
fn an_exact_invoker_judges_its_call_site_against_its_own_type() {
    let mut ctx = MockNativeContext::new();
    let t = "(Ljava/lang/Object;)Ljava/lang/Object;";
    let ex = alloc_method_handle(&mut ctx, "", "", t, MH_KIND_INVOKER).unwrap();
    let own = method_type(
        &mut ctx,
        "(Ljava/lang/invoke/MethodHandle;Ljava/lang/Object;)Ljava/lang/Object;",
    );
    ctx.set_field_by_name(ex, "type", Value::Object(Some(own)));
    assert!(!mh_is_exact_invoker(&ctx, ex));
    mh_mark_exact_invoker(&ctx, ex);
    assert!(mh_is_exact_invoker(&ctx, ex));
    assert!(!mh_is_varargs_collector(&ctx, ex), "the bits are separate");
    assert!(invoker_has_own_type(&ctx, ex, MH_KIND_INVOKER));
    assert!(!invoker_has_own_type(&ctx, ex, MH_KIND_STATIC));

    let declared = mh_declared_descriptor(&mut ctx, ex).expect("the invoker's own type");
    assert!(
        exact_call_site_refusal(&mut ctx, ex, MH_KIND_INVOKER, &declared, Some(&declared))
            .is_none()
    );
    let short_site = "(Ljava/lang/invoke/MethodHandle;)Ljava/lang/Object;";
    let refusal =
        exact_call_site_refusal(&mut ctx, ex, MH_KIND_INVOKER, short_site, Some(&declared))
            .expect("a site one argument short is refused");
    assert!(is_wmte(&ctx, &refusal));

    ctx.set_field(ex, MH_DESC, Value::Object(None));
    assert!(!invoker_has_own_type(&ctx, ex, MH_KIND_INVOKER));
    assert!(exact_call_site_refusal(
        &mut ctx,
        ex,
        MH_KIND_INVOKER,
        short_site,
        Some(&declared)
    )
    .is_none());
}

/// A null target is HotSpot's helpful NPE, naming the call the invoker's
/// `LambdaForm` makes on it (measured on 25.0.3).
#[test]
fn a_null_target_names_the_call_the_invoker_makes_on_it() {
    let message = |e: MethodCallFailed| match e {
        MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::NullPointerException { message, .. },
        )) => message,
        other => panic!("not an NPE: {other:?}"),
    };
    assert_eq!(
        message(invoker_null_target(true)).as_deref(),
        Some("Cannot invoke \"java.lang.invoke.MethodHandle.type()\" because \"mh\" is null")
    );
    assert_eq!(
        message(invoker_null_target(false)).as_deref(),
        Some(
            "Cannot invoke \"java.lang.invoke.MethodHandle.asType(java.lang.invoke.MethodType)\" \
             because \"mh\" is null"
        )
    );
}

/// The door's boxed answer goes back raw for a primitive target return, so
/// the invoker's own door can still widen it against its call site.
#[test]
fn a_primitive_target_return_goes_back_raw() {
    let mut ctx = MockNativeContext::new();
    let boxed = match ctx.new_object("java/lang/Integer").unwrap() {
        Some(Value::Object(Some(o))) => o,
        other => panic!("Integer: {other:?}"),
    };
    ctx.set_field(boxed, 0, Value::Int(3));
    let b = Some(Value::Object(Some(boxed)));
    assert_eq!(invoker_result_unboxed(&ctx, b, "I"), Some(Value::Int(3)));
    assert_eq!(invoker_result_unboxed(&ctx, b, "J"), Some(Value::Long(3)));
    assert_eq!(invoker_result_unboxed(&ctx, b, "Ljava/lang/Object;"), b);
    assert_eq!(
        invoker_result_unboxed(&ctx, Some(Value::Int(4)), "J"),
        Some(Value::Long(4))
    );
    // `void` -> primitive is the zero, as `asType` makes it.
    assert_eq!(invoker_result_unboxed(&ctx, None, "I"), Some(Value::Int(0)));
    assert_eq!(
        invoker_result_unboxed(&ctx, Some(Value::Object(None)), "D"),
        Some(Value::Double(0.0))
    );
    assert_eq!(invoker_result_unboxed(&ctx, None, "V"), None);
}

/// Stage 1 of the unmount proposal: the door's `MH_TAIL` token, and which
/// leaf returns run transparent under it with which conversion owed.
#[test]
fn a_tail_token_admits_exactly_the_conversions_a_remount_can_make() {
    assert_eq!(door_tail_token("Ljava/lang/Object;"), b'L');
    assert_eq!(door_tail_token("[I"), b'L');
    assert_eq!(door_tail_token("I"), b'I');
    assert_eq!(door_tail_token("V"), b'V');
    assert_eq!(door_tail_token(""), 0);

    // Stage 0 and the identities: nothing owed.
    assert_eq!(tail_leaf_conversion(b'L', b'L'), Some(None));
    assert_eq!(tail_leaf_conversion(b'L', b'['), Some(None));
    assert_eq!(tail_leaf_conversion(b'I', b'I'), Some(None));
    assert_eq!(tail_leaf_conversion(b'V', b'V'), Some(None));
    // A primitive boxed or widened: the remounted frame converts.
    assert_eq!(tail_leaf_conversion(b'L', b'I'), Some(Some(DESC_OBJECT)));
    assert_eq!(tail_leaf_conversion(b'L', b'Z'), Some(Some(DESC_OBJECT)));
    assert_eq!(tail_leaf_conversion(b'J', b'I'), Some(Some(DESC_LONG)));
    assert_eq!(tail_leaf_conversion(b'D', b'F'), Some(Some(DESC_DOUBLE)));
    // Opaque: no tail, a void behind a value, a reference behind a
    // primitive, a narrowing, a boolean widened.
    assert_eq!(tail_leaf_conversion(0, b'L'), None);
    assert_eq!(tail_leaf_conversion(b'L', b'V'), None);
    assert_eq!(tail_leaf_conversion(b'I', b'L'), None);
    assert_eq!(tail_leaf_conversion(b'I', b'J'), None);
    assert_eq!(tail_leaf_conversion(b'I', b'Z'), None);
    assert_eq!(tail_leaf_conversion(b'V', b'I'), None);
}

/// Interpreter round i6 wave 115, lane L4 (`CRATONVM_MH_VOID_TAIL_UNMOUNT`):
/// with the admission, a `void` leaf behind a reference token owes the
/// door's `null` (an `Object` caller return); nothing else moves, and a
/// `void` leaf behind a primitive token stays opaque.
#[test]
fn a_void_leaf_behind_a_reference_owes_null_only_when_admitted() {
    assert_eq!(tail_leaf_conversion_with(b'L', b'V', false), None);
    assert_eq!(
        tail_leaf_conversion_with(b'L', b'V', true),
        Some(Some(DESC_OBJECT))
    );
    assert_eq!(tail_leaf_conversion_with(b'I', b'V', true), None);
    assert_eq!(tail_leaf_conversion_with(b'V', b'V', true), Some(None));
    assert_eq!(tail_leaf_conversion_with(b'L', b'I', true), Some(Some(DESC_OBJECT)));
    assert_eq!(tail_leaf_conversion_with(b'L', b'L', true), Some(None));
    assert_eq!(tail_leaf_conversion_with(0, b'V', true), None);
}

/// A context that keeps no frames (the mock's `continuation_frame_mark`
/// is the trait default) keeps a door opaque when a conversion is owed,
/// and transparent when none is.
#[test]
fn a_leaf_that_owes_a_conversion_needs_a_frame_mark() {
    let ctx = MockNativeContext::new();
    assert!(transparent_leaf_call(&ctx, b'L', "()I", None).is_none());
    let identity = transparent_leaf_call(&ctx, b'I', "()I", None);
    assert!(identity.as_ref().is_some_and(|c| c.owed.is_none()));
    assert_eq!(
        cratonvm_native_api::continuation_pin::transparent_natives(),
        1
    );
    drop(identity);
    assert_eq!(
        cratonvm_native_api::continuation_pin::transparent_natives(),
        0
    );
    assert!(transparent_leaf_call(&ctx, 0, "()Ljava/lang/String;", None).is_none());
}

/// `explicitCastArguments(takesRunner, (Object)String)`: the step to the
/// leaf's INTERFACE parameter is JLS 5.5's, which does not cast, in the
/// `invokeExact` door (round 12's inner cast) and the `invoke` /
/// `invokeWithArguments` doors' leaf pass alike; an `asType` copy casts.
/// The mock gives a positive mismatch only for an array, so the argument
/// is one.
#[test]
fn an_explicit_cast_passes_a_reference_to_an_interface_parameter_uncast() {
    let mut ctx = MockNativeContext::new();
    let runner = ctx
        .ensure_class_initialized("p/Runner")
        .unwrap_or(ClassId::new(0));
    ctx.set_is_interface(runner, true);
    let mh = alloc_method_handle(
        &mut ctx,
        "p/C",
        "m",
        "(Lp/Runner;)Ljava/lang/String;",
        MH_KIND_STATIC,
    )
    .unwrap();
    let widened_desc = "(Ljava/lang/Object;)Ljava/lang/String;";
    let widened = method_type(&mut ctx, widened_desc);
    ctx.set_field_by_name(mh, "type", Value::Object(Some(widened)));
    let arr = ctx.new_array(ArrayElementType::Int, 1);
    let args = [Value::Object(Some(arr))];

    let as_type_exact = invoke_exact_inner_cast_refusal(
        &mut ctx,
        mh,
        MH_KIND_STATIC,
        Some(widened_desc),
        &args,
    );
    assert!(
        as_type_exact.is_some(),
        "an asType copy casts to the interface"
    );
    let as_type_invoke =
        invoke_reference_cast_refusal(&mut ctx, mh, MH_KIND_STATIC, false, false, &args, None);
    assert!(
        as_type_invoke.is_some(),
        "an asType copy casts to the interface"
    );

    mh_mark_explicit_cast(&ctx, mh);
    assert!(invoke_exact_inner_cast_refusal(
        &mut ctx,
        mh,
        MH_KIND_STATIC,
        Some(widened_desc),
        &args
    )
    .is_none());
    assert!(invoke_reference_cast_refusal(
        &mut ctx,
        mh,
        MH_KIND_STATIC,
        false,
        false,
        &args,
        None
    )
    .is_none());
}
}

/// Round 15 wave 1, lane mh: the direct lane's per-thread record memo
/// (`DIRECT_LANE_MEMO`, proposal C14-1), the decoded `invoke` refusal and the
/// lean virtual arm. Mock conventions as `r13w10_callcost4_direct_lane_tests`
/// below (a positive reference mismatch is an array).
#[cfg(test)]
mod r15w1_mh_direct_lane_record_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// A mock with a PRIVATE VM identity (`0` is never memoised) and
/// `MethodType`'s `rtype` / `ptypes` declared.
fn setup(private_vm: bool) -> MockNativeContext {
    static NEXT: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0x1531_0000);
    let mut ctx = MockNativeContext::new();
    if private_vm {
        ctx.set_vm_identity(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    }
    for name in ["p/C", "p/Mid", "p/Sub"] {
        let _ = ctx.ensure_class_initialized(name);
    }
    let probe = mt(&mut ctx, "()V");
    let cid = ctx.class_id_of_object(probe);
    let field = |name: &str, descriptor: &str, slot_index: usize| FieldMetadata {
        name: name.to_string(),
        descriptor: descriptor.to_string(),
        access_flags: 0,
        slot_index,
        declaring_class_id: cid,
        is_static: false,
    };
    ctx.set_declared_fields(
        cid,
        vec![
            field("rtype", "Ljava/lang/Class;", 0),
            field("ptypes", "[Ljava/lang/Class;", 1),
        ],
    );
    ctx
}

fn mt(ctx: &mut MockNativeContext, desc: &str) -> ObjectRef {
    match build_method_type_from_descriptor(ctx, desc) {
        Ok(Some(mt)) => mt,
        other => panic!("MethodType {desc}: {other:?}"),
    }
}

fn handle(
    ctx: &mut MockNativeContext,
    class: &str,
    name: &str,
    desc: &str,
    kind: i32,
) -> ObjectRef {
    match alloc_method_handle(ctx, class, name, desc, kind) {
        Ok(mh) => mh,
        Err(e) => panic!("alloc {class}.{name}{desc}: {e:?}"),
    }
}

fn object(ctx: &mut MockNativeContext, class: &str, payload: Value) -> Value {
    let o = match ctx.new_object(class) {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => panic!("new_object {class}: {other:?}"),
    };
    ctx.set_field(o, 0, payload);
    Value::Object(Some(o))
}

/// The own types of this thread's memo lines filed for `vm`.
fn memoised(vm: usize) -> Vec<String> {
    DIRECT_LANE_MEMO.with(|memo| {
        memo.borrow()
            .iter()
            .filter(|line| line.vm == vm)
            .filter_map(|line| line.record.as_ref().map(|r| r.own_type.clone()))
            .collect()
    })
}

/// A memoised record is re-validated against the handle's field objects on
/// every call: a `type` restamped in place and an `MH_DESC` rewritten in
/// place are both seen at once (a record trusted by the handle's identity
/// would keep answering at `(I)I`).
#[test]
fn a_memoised_record_sees_every_in_place_rewrite_of_its_fields() {
    let mut ctx = setup(true);
    let exact = DirectLaneDoor::InvokeExact;
    let mh = handle(&mut ctx, "p/C", "m", "(I)I", MH_KIND_STATIC);
    for _ in 0..2 {
        let r = mh_direct_lane(&mut ctx, mh, &[Value::Int(1)], Some("(I)I"), exact);
        assert!(r.is_some(), "served at its own type");
    }
    if mh_direct_lane_record_enabled() {
        assert!(
            memoised(ctx.vm_identity()).iter().any(|t| t == "(I)I"),
            "the record is memoised"
        );
    }
    // `type` restamped IN PLACE: the declared type is no longer the own one.
    let long_type = mt(&mut ctx, "(J)I");
    ctx.set_field_by_name(mh, "type", Value::Object(Some(long_type)));
    assert!(mh_direct_lane(&mut ctx, mh, &[Value::Int(1)], Some("(I)I"), exact).is_none());
    // `MH_DESC` rewritten IN PLACE to agree with it: served at the new type
    // only.
    let long_desc = ctx.create_string("(J)I");
    ctx.set_field(mh, MH_DESC, Value::Object(Some(long_desc)));
    assert!(mh_direct_lane(&mut ctx, mh, &[Value::Long(1)], Some("(J)I"), exact).is_some());
    assert!(mh_direct_lane(&mut ctx, mh, &[Value::Int(1)], Some("(I)I"), exact).is_none());
    assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
}

/// `asType` over a handle, as the `asType` native runs it.
fn stamp(ctx: &mut MockNativeContext, h: ObjectRef, desc: &str) -> ObjectRef {
    let t = mt(ctx, desc);
    match mh_with_stamped_type(ctx, Value::Object(Some(h)), t, false) {
        Ok(Some(Value::Object(Some(copy)))) => copy,
        other => panic!("stamp {desc}: {other:?}"),
    }
}

/// `r15w1-mh-exact-invoker-and-cast-chain-share-one-marking-bit`: an
/// `asType` copy of an `exactInvoker` stays exact (the stamp clears the
/// cast-chain bit, which was the exact-invoker bit), and an active cast
/// chain does not read as an exact invoker.
#[test]
fn the_exact_invoker_and_cast_chain_markings_are_separate_bits() {
    let mut ctx = setup(true);
    assert_eq!(MH_MARK_CAST_CHAIN & MH_MARK_EXACT_INVOKER, 0);
    let ex = handle(&mut ctx, "", "", "(I)I", MH_KIND_INVOKER);
    let own = mt(&mut ctx, "(Ljava/lang/invoke/MethodHandle;I)I");
    ctx.set_field_by_name(ex, "type", Value::Object(Some(own)));
    mh_mark_exact_invoker(&ctx, ex);
    let copy = stamp(
        &mut ctx,
        ex,
        "(Ljava/lang/invoke/MethodHandle;Ljava/lang/Integer;)I",
    );
    assert_ne!(Value::Object(Some(copy)), Value::Object(Some(ex)), "a copy");
    assert!(mh_is_exact_invoker(&ctx, ex), "the receiver is untouched");
    assert!(mh_is_exact_invoker(&ctx, copy), "the copy is still exact");
    // Two stamps over a direct handle: an active chain.
    let h = handle(
        &mut ctx,
        "p/C",
        "m",
        "(Ljava/lang/Object;)I",
        MH_KIND_STATIC,
    );
    let c1 = stamp(&mut ctx, h, "(Lp/Sub;)I");
    let c2 = stamp(&mut ctx, c1, "(Ljava/lang/Object;)I");
    assert_ne!(
        mh_marking_bits(&ctx, c2) & MH_MARK_CAST_CHAIN,
        0,
        "an active chain"
    );
    assert!(
        !mh_is_exact_invoker(&ctx, c2),
        "a chain is not an exact invoker"
    );
}

/// A context whose VM identity is `0` (every mock by default, and any
/// wrapper that does not forward it) is served without being memoised.
#[test]
fn a_context_without_a_vm_identity_is_never_memoised() {
    let mut ctx = setup(false);
    assert_eq!(ctx.vm_identity(), 0);
    let mh = handle(&mut ctx, "p/C", "m", "(I)I", MH_KIND_STATIC);
    let r = mh_direct_lane(
        &mut ctx,
        mh,
        &[Value::Int(1)],
        Some("(I)I"),
        DirectLaneDoor::InvokeExact,
    );
    assert!(r.is_some());
    assert!(memoised(0).is_empty(), "{:?}", memoised(0));
}

/// The `invoke` door's reference refusal asked with the record's decoded
/// parameters and class answers exactly what the decoding one answers.
/// No declared type is handed in, so the leaf's own receiver cast (the
/// class the record carries) and parameter casts are the ones judged.
#[test]
fn the_decoded_invoke_refusal_answers_as_the_decoding_one() {
    let mut ctx = setup(true);
    let mh = handle(&mut ctx, "p/Mid", "m", "(Lp/Sub;)I", MH_KIND_VIRTUAL);
    let recv = object(&mut ctx, "p/Mid", Value::Int(0));
    let arr = Value::Object(Some(ctx.new_array(ArrayElementType::Int, 1)));
    let site = Some("(Ljava/lang/Object;Ljava/lang/Object;)I");
    let params = ["Lp/Sub;".to_string()];
    for (row, args) in [
        ("clean", [recv, Value::Object(None)]),
        ("array argument", [recv, arr]),
        ("array receiver", [arr, Value::Object(None)]),
    ] {
        let with = invoke_reference_cast_refusal_with(
            &mut ctx,
            mh,
            MH_KIND_VIRTUAL,
            true,
            false,
            &args,
            site,
            None,
        );
        let decoded = invoke_reference_cast_refusal_decoded(
            &mut ctx,
            mh,
            MH_KIND_VIRTUAL,
            true,
            false,
            &args,
            site,
            None,
            &params,
            Some("p/Mid"),
        );
        assert_eq!(format!("{with:?}"), format!("{decoded:?}"), "{row}");
        assert_eq!(with.is_none(), row == "clean", "{row}: {with:?}");
    }
}

/// `readResolve()Object` missing on the leaf: the neutral answer is the
/// receiver. The hook plays a moving collection inside the call: it
/// re-addresses every pin of the receiver to the object the receiver's
/// field 0 names.
fn missing_read_resolve(
    ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    name: &str,
    desc: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if name != "readResolve" {
        return None;
    }
    if let Value::Object(Some(moved)) = ctx.get_field(receiver, 0) {
        ctx.remap_native_pin_addr_for_test(receiver.as_ptr() as usize, moved.as_ptr() as usize);
    }
    Some(Err(MethodCallFailed::InternalError(VmError::Linkage(
        LinkageError::NoSuchMethodError {
            class_name: "p/Mid".to_string(),
            method_name: name.to_string(),
            method_descriptor: desc.to_string(),
        },
    ))))
}

/// The lean virtual arm answers the receiver where the call left it; the
/// round-13 arm answered its pre-call copy.
#[test]
fn the_lean_virtual_arm_answers_the_receiver_where_the_call_left_it() {
    let mut ctx = setup(true);
    let moved = object(&mut ctx, "p/Mid", Value::Int(0));
    let recv = object(&mut ctx, "p/Mid", moved);
    ctx.set_invoke_virtual_hook(missing_read_resolve);
    let mh = handle(
        &mut ctx,
        "p/Mid",
        "readResolve",
        "()Ljava/lang/Object;",
        MH_KIND_VIRTUAL,
    );
    let r = mh_direct_lane(
        &mut ctx,
        mh,
        &[recv],
        Some("(Lp/Mid;)Ljava/lang/Object;"),
        DirectLaneDoor::InvokeExact,
    );
    let answered = match r {
        Some(Ok(Some(v))) => v,
        other => panic!("the neutral answer: {other:?}"),
    };
    if mh_direct_lane_lean_virtual_enabled() {
        assert_eq!(answered, moved, "the receiver's current address");
    }
    assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
}

/// `r15w1-mh-neutral-serialization-hook-answers-a-stale-receiver`: the
/// ordinary dispatch body's `VIRTUAL` arms (unbound and bound, reached
/// through `mh_dispatch`, not the lane) answer the receiver where the call
/// left it too.
#[test]
fn the_dispatch_body_answers_the_receiver_where_the_call_left_it() {
    let mut ctx = setup(true);
    ctx.set_invoke_virtual_hook(missing_read_resolve);
    let neutral =
        |ctx: &mut MockNativeContext, mh: ObjectRef, extra: &[Value]| match mh_dispatch(
            ctx, mh, extra,
        ) {
            Ok(Some(v)) => v,
            other => panic!("the neutral answer: {other:?}"),
        };
    // Unbound.
    let moved = object(&mut ctx, "p/Mid", Value::Int(0));
    let recv = object(&mut ctx, "p/Mid", moved);
    let mh = handle(
        &mut ctx,
        "p/Mid",
        "readResolve",
        "()Ljava/lang/Object;",
        MH_KIND_VIRTUAL,
    );
    let answered = neutral(&mut ctx, mh, &[recv]);
    if mh_neutral_hook_current_receiver_enabled() {
        assert_eq!(answered, moved, "unbound: the receiver's current address");
    }
    // Bound (`bindTo`'s shape: the receiver in `MH_BOUND`).
    let moved = object(&mut ctx, "p/Mid", Value::Int(0));
    let recv = object(&mut ctx, "p/Mid", moved);
    let bound = handle(
        &mut ctx,
        "p/Mid",
        "readResolve",
        "()Ljava/lang/Object;",
        MH_KIND_VIRTUAL,
    );
    ctx.set_field(bound, MH_BOUND, recv);
    let answered = neutral(&mut ctx, bound, &[]);
    if mh_neutral_hook_current_receiver_enabled() {
        assert_eq!(answered, moved, "bound: the receiver's current address");
    }
    assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
}
}

/// Round 15 wave 4, lane mh2: MH15-1 (the direct lane's `invoke` refusal is
/// provably empty at the exact type, `own_type_aligned`). Mock conventions as
/// `r13w10_callcost4_direct_lane_tests` (a positive reference mismatch is an
/// array).
#[cfg(test)]
mod r15w4_mh2_direct_lane_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn setup() -> MockNativeContext {
    let mut ctx = MockNativeContext::new();
    for name in ["p/C", "p/Mid", "p/Sub"] {
        let _ = ctx.ensure_class_initialized(name);
    }
    let probe = mt(&mut ctx, "()V");
    let cid = ctx.class_id_of_object(probe);
    let field = |name: &str, descriptor: &str, slot_index: usize| FieldMetadata {
        name: name.to_string(),
        descriptor: descriptor.to_string(),
        access_flags: 0,
        slot_index,
        declaring_class_id: cid,
        is_static: false,
    };
    ctx.set_declared_fields(
        cid,
        vec![
            field("rtype", "Ljava/lang/Class;", 0),
            field("ptypes", "[Ljava/lang/Class;", 1),
        ],
    );
    ctx
}

fn mt(ctx: &mut MockNativeContext, desc: &str) -> ObjectRef {
    match build_method_type_from_descriptor(ctx, desc) {
        Ok(Some(mt)) => mt,
        other => panic!("MethodType {desc}: {other:?}"),
    }
}

fn handle(ctx: &mut MockNativeContext, class: &str, desc: &str, kind: i32) -> ObjectRef {
    match alloc_method_handle(ctx, class, "m", desc, kind) {
        Ok(mh) => mh,
        Err(e) => panic!("alloc {class}: {e:?}"),
    }
}

fn object(ctx: &mut MockNativeContext, class: &str) -> Value {
    match ctx.new_object(class) {
        Ok(Some(v @ Value::Object(Some(_)))) => v,
        other => panic!("new_object {class}: {other:?}"),
    }
}

/// `m(Sub, int)int` answers its int plus 100.
fn plus_hundred(
    _ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    name: &str,
    desc: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    match (name, desc, args) {
        ("m", "(Lp/Sub;I)I", [_, Value::Int(x)]) => Some(Ok(Some(Value::Int(x + 100)))),
        _ => None,
    }
}

fn cce_target(r: &Option<MethodCallResult>) -> Option<String> {
    match r {
        Some(Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::ClassCastException { message, .. },
        )))) => message.rsplit(" to ").next().map(str::to_string),
        _ => None,
    }
}

/// The alignment bit: `true` for every shape the lane admits, `false` for
/// a class name whose `;` misaligns the own type's split.
#[test]
fn the_own_type_is_aligned_for_every_real_class_name() {
    let mut ctx = setup();
    // String literals only (`r13_misc10_create_string_ratchet` counts the
    // computed-text `create_string` sites of this crate).
    let name = ctx.create_string("m");
    let rows = [
        (
            ctx.create_string("p/Mid"),
            ctx.create_string("(Lp/Sub;I)I"),
            true,
            true,
        ),
        (
            ctx.create_string("p/Mid"),
            ctx.create_string("()V"),
            true,
            true,
        ),
        (
            ctx.create_string("p/C"),
            ctx.create_string("(Lp/Sub;[IJ)I"),
            false,
            true,
        ),
        (
            ctx.create_string("p/C"),
            ctx.create_string("()I"),
            false,
            true,
        ),
        (
            ctx.create_string("p/a;b"),
            ctx.create_string("(I)I"),
            true,
            false,
        ),
    ];
    for (row, (class, desc, unbound, aligned)) in rows.into_iter().enumerate() {
        let fields = [
            Value::Object(Some(class)),
            Value::Object(Some(name)),
            Value::Object(Some(desc)),
            Value::Object(None),
        ];
        let record = mh_direct_lane_decode(&mut ctx, fields, unbound, None);
        assert_eq!(
            record.map(|r| r.own_type_aligned),
            Some(aligned),
            "row {row}"
        );
    }
}

/// MH15-1's claim, asked of the refusal itself: at site = declared = own
/// type it answers `None` for every argument row -- clean, `null`, and the
/// positive mismatches (an array receiver, an array argument) that the
/// lane's LEAF casts refuse later.
#[test]
fn the_invoke_refusal_is_empty_at_the_exact_type() {
    if !mh_direct_lane_invoke_refusal_skip_enabled() {
        return;
    }
    let mut ctx = setup();
    let mid = object(&mut ctx, "p/Mid");
    let sub = object(&mut ctx, "p/Sub");
    let arr = Value::Object(Some(ctx.new_array(ArrayElementType::Int, 1)));
    let params = ["Lp/Sub;".to_string(), "I".to_string()];
    // Unbound virtual: the receiver leads.
    let virt = handle(&mut ctx, "p/Mid", "(Lp/Sub;I)I", MH_KIND_VIRTUAL);
    let own = "(Lp/Mid;Lp/Sub;I)I";
    for (row, args) in [
        ("clean", [mid, sub, Value::Int(1)]),
        ("null argument", [mid, Value::Object(None), Value::Int(1)]),
        ("array receiver", [arr, sub, Value::Int(1)]),
        ("array argument", [mid, arr, Value::Int(1)]),
    ] {
        for decoded in [true, false] {
            let r = if decoded {
                invoke_reference_cast_refusal_decoded(
                    &mut ctx,
                    virt,
                    MH_KIND_VIRTUAL,
                    true,
                    false,
                    &args,
                    Some(own),
                    Some(own),
                    &params,
                    Some("p/Mid"),
                )
            } else {
                invoke_reference_cast_refusal_with(
                    &mut ctx,
                    virt,
                    MH_KIND_VIRTUAL,
                    true,
                    false,
                    &args,
                    Some(own),
                    Some(own),
                )
            };
            assert!(r.is_none(), "virtual {row} (decoded {decoded}): {r:?}");
        }
    }
    // Static (and a bound virtual's shape: no receiver taken).
    let stat = handle(&mut ctx, "p/C", "(Lp/Sub;I)I", MH_KIND_STATIC);
    let own = "(Lp/Sub;I)I";
    for (row, args) in [
        ("clean", [sub, Value::Int(1)]),
        ("array argument", [arr, Value::Int(1)]),
    ] {
        for (mh, kind, bound) in [(stat, MH_KIND_STATIC, false), (virt, MH_KIND_VIRTUAL, true)]
        {
            let r = invoke_reference_cast_refusal_decoded(
                &mut ctx,
                mh,
                kind,
                kind == MH_KIND_VIRTUAL,
                bound,
                &args,
                Some(own),
                Some(own),
                &params,
                Some("p/Mid"),
            );
            assert!(r.is_none(), "kind {kind} {row}: {r:?}");
        }
    }
}

/// End to end: the lane's `invoke` answers what its `invokeExact` answers
/// at the exact type (the exact door never asked the skipped refusal):
/// the leaf's value, or the leaf cast's `ClassCastException`.
#[test]
fn the_lane_invoke_answers_as_invoke_exact_at_the_exact_type() {
    let mut ctx = setup();
    ctx.set_invoke_virtual_hook(plus_hundred);
    let mid = object(&mut ctx, "p/Mid");
    let sub = object(&mut ctx, "p/Sub");
    let arr = Value::Object(Some(ctx.new_array(ArrayElementType::Int, 1)));
    let virt = handle(&mut ctx, "p/Mid", "(Lp/Sub;I)I", MH_KIND_VIRTUAL);
    let site = Some("(Lp/Mid;Lp/Sub;I)I");
    for (row, args, cce) in [
        ("clean", [mid, sub, Value::Int(5)], None),
        ("array receiver", [arr, sub, Value::Int(5)], Some("p.Mid")),
        ("array argument", [mid, arr, Value::Int(5)], Some("p.Sub")),
    ] {
        let exact = mh_direct_lane(&mut ctx, virt, &args, site, DirectLaneDoor::InvokeExact);
        let invoke = mh_direct_lane(&mut ctx, virt, &args, site, DirectLaneDoor::Invoke);
        // A boxed answer (raw return off) is a fresh box per door.
        if cce.is_some() || mh_direct_lane_raw_return_enabled() {
            assert_eq!(format!("{exact:?}"), format!("{invoke:?}"), "{row}");
        }
        assert_eq!(cce_target(&invoke).as_deref(), cce, "{row}: {invoke:?}");
        if cce.is_none() {
            assert!(matches!(invoke, Some(Ok(Some(_)))), "{row}: {invoke:?}");
        }
    }
    assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
}

fn is_npe(r: &MethodCallResult) -> bool {
    matches!(
        r,
        Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::NullPointerException { .. }
        )))
    )
}

/// `r15w4-mh2-invoke-null-receiver-before-astype`: the `invoke` door's
/// `null` receiver is refused AFTER the call site's
/// `WrongMethodTypeException` and the argument casts, as HotSpot's
/// `asType(callSiteType)` + adapter order has it; with neither, it is
/// still the NPE.
#[test]
fn the_invoke_door_refuses_a_null_receiver_after_the_casts() {
    if !mh_strict_exact_signature() {
        return;
    }
    let mut ctx = setup();
    let after = mh_invoke_null_receiver_after_casts_enabled();
    let arr = Value::Object(Some(ctx.new_array(ArrayElementType::Int, 1)));
    let sub = object(&mut ctx, "p/Sub");
    let null = Value::Object(None);
    // A reference argument the adapter's cast refuses.
    let takes_sub = handle(&mut ctx, "p/Mid", "(Lp/Sub;)I", MH_KIND_VIRTUAL);
    let site = Some("(Lp/Mid;Ljava/lang/Object;)I".to_string());
    let r = mh_invoke_door(&mut ctx, takes_sub, &[null, arr], site.as_deref(), true);
    assert_eq!(is_npe(&r), !after, "cast before the receiver: {r:?}");
    if after {
        assert_eq!(cce_target(&Some(r)).as_deref(), Some("p.Sub"));
    }
    // A call site `asType` cannot convert to (`long` into `int`).
    let takes_int = handle(&mut ctx, "p/Mid", "(I)I", MH_KIND_VIRTUAL);
    let narrow = Some("(Lp/Mid;J)I".to_string());
    let r = mh_invoke_door(
        &mut ctx,
        takes_int,
        &[null, Value::Long(5)],
        narrow.as_deref(),
        true,
    );
    assert!(r.is_err(), "{r:?}");
    assert_eq!(
        is_npe(&r),
        !after,
        "the site's type before the receiver: {r:?}"
    );
    // Nothing else wrong: the receiver's NPE, in both orders.
    let r = mh_invoke_door(&mut ctx, takes_sub, &[null, sub], site.as_deref(), true);
    assert!(is_npe(&r), "{r:?}");
}
}

/// Round 15 wave 4, lane mh3: `bindTo` mints a FIXED-arity handle
/// (`r15w4-mh2-bindto-keeps-varargs-collector-FIXED-20260930.md`, HotSpot ref
/// `R15Mh2BoundVarargs`), the doors refuse its other arities, the ordinary mint
/// shares the source's strings (MH2-3), and the kinds whose dispatch does not
/// take `MH_BOUND` as the leading argument get the INSERT adapter. The handles
/// carry no `type` where a test does not need one (the mock's mirrors are not
/// asked), so each `type` question is answered from `MH_DESC`.
#[cfg(test)]
mod r15w4_mh3_bindto_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

const CAT: &str = "(Ljava/lang/String;[Ljava/lang/String;)Ljava/lang/String;";
const BOUND_CAT: &str = "([Ljava/lang/String;)Ljava/lang/String;";
const SPREAD_SITE: &str = "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;";

fn untyped(ctx: &mut MockNativeContext, class: &str, desc: &str, kind: i32) -> ObjectRef {
    let mh = match alloc_method_handle(ctx, class, "m", desc, kind) {
        Ok(mh) => mh,
        Err(e) => panic!("alloc {class}: {e:?}"),
    };
    ctx.set_field_by_name(mh, "type", Value::Object(None));
    mh
}

fn bind_to(ctx: &mut MockNativeContext, target: ObjectRef, value: Value) -> ObjectRef {
    let mut registry = NativeMethodRegistry::new();
    register_t4_method_handle_invoke(&mut registry);
    let bind = registry
        .find(
            "java/lang/invoke/MethodHandle",
            "bindTo",
            "(Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;",
        )
        .expect("MethodHandle.bindTo should be registered");
    match bind(ctx, &[Value::Object(Some(target)), value]) {
        Ok(Some(Value::Object(Some(h)))) => h,
        other => panic!("bindTo answered {other:?}"),
    }
}

fn is_wmte(ctx: &MockNativeContext, err: &MethodCallFailed) -> bool {
    match err {
        MethodCallFailed::ExceptionThrown(o) => {
            ctx.class_name_of_id(ctx.class_id_of_object(*o)).as_deref()
                == Some("java/lang/invoke/WrongMethodTypeException")
        }
        _ => false,
    }
}

/// The defect: a collector's bind is not a collector, the source keeps its
/// marking, and the bound value is the one captured. With the shared mint
/// the three strings are the source's own objects.
#[test]
fn a_bound_collector_is_fixed_arity_and_shares_the_strings() {
    let mut ctx = MockNativeContext::new();
    let cat = untyped(&mut ctx, "p/C", CAT, MH_KIND_STATIC);
    mh_set_varargs_collector(&ctx, cat, true);
    let dash = Value::Object(Some(ctx.create_string("-")));
    let bound = bind_to(&mut ctx, cat, dash);
    assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
    assert_eq!(
        mh_is_varargs_collector(&ctx, bound),
        !mh_bindto_fixed_arity_enabled()
    );
    assert!(
        mh_is_varargs_collector(&ctx, cat),
        "the source keeps its marking"
    );
    assert_eq!(ctx.get_field(bound, MH_KIND), Value::Int(MH_KIND_STATIC));
    assert_eq!(ctx.get_field(bound, MH_BOUND), dash);
    assert_eq!(mh_read_desc(&ctx, bound).as_deref(), Some(CAT));
    assert_eq!(mh_read_class(&ctx, bound).as_deref(), Some("p/C"));
    if bindto_shared_mint_applies(&ctx, cat, MH_KIND_STATIC) {
        for slot in [MH_CLASS, MH_NAME, MH_DESC] {
            assert_eq!(
                ctx.get_field(bound, slot),
                ctx.get_field(cat, slot),
                "slot {slot}"
            );
        }
    }
}

/// The bound `type` the shared mint builds: the leading parameter dropped
/// for a static-like kind, the receiver-less descriptor (or the stamped
/// type minus its receiver) for a virtual one.
#[test]
fn the_bound_type_drops_what_the_bind_captured() {
    let mut ctx = MockNativeContext::new();
    let cat = untyped(&mut ctx, "p/C", CAT, MH_KIND_STATIC);
    assert_eq!(
        bindto_bound_type_descriptor(&mut ctx, cat, MH_KIND_STATIC, CAT, None),
        BOUND_CAT
    );
    let fmt = "([Ljava/lang/Object;)Ljava/lang/String;";
    let virt = untyped(&mut ctx, "java/lang/String", fmt, MH_KIND_VIRTUAL);
    assert_eq!(
        bindto_bound_type_descriptor(&mut ctx, virt, MH_KIND_VIRTUAL, fmt, None),
        fmt
    );
    assert_eq!(
        bindto_bound_type_descriptor(&mut ctx, virt, MH_KIND_VIRTUAL, fmt, Some("(I)I")),
        "(I)I"
    );
}

/// `asType`'s arity refusal: only a bound, non-collector direct handle at
/// a site of another arity, and only when its `type` is the leaf's minus
/// the capture.
/// `asType`'s raw-descriptor escape reads a bound static handle's
/// `MH_DESC` without the captured parameter, so a longer type is refused
/// (HotSpot: `cannot convert MethodHandle(String[])String to
/// (String,String)String`) where the capture made the arity match.
#[test]
fn a_bound_static_raw_type_drops_the_capture() {
    let mut ctx = MockNativeContext::new();
    let bound = untyped(&mut ctx, "p/C", CAT, MH_KIND_STATIC);
    assert_eq!(
        bound_static_raw_desc(&ctx, bound, CAT),
        None,
        "not bound yet"
    );
    let dash = Value::Object(Some(ctx.create_string("-")));
    ctx.set_field(bound, MH_BOUND, dash);
    let on = mh_bindto_fixed_arity_enabled();
    assert_eq!(
        bound_static_raw_desc(&ctx, bound, CAT),
        on.then(|| BOUND_CAT.to_string())
    );
    if on {
        let refusal = mh_astype_refusal_desc(&mut ctx, bound, BOUND_CAT, SPREAD_SITE);
        assert!(
            refusal.as_ref().is_some_and(|r| is_wmte(&ctx, r)),
            "{refusal:?}"
        );
        // The same arity still converts.
        let object_site = "(Ljava/lang/Object;)Ljava/lang/String;";
        assert!(mh_astype_refusal_desc(&mut ctx, bound, BOUND_CAT, object_site).is_none());
    }
}

// Which kinds `bindTo` hands to the INSERT adapter: every kind whose
// dispatch does not read an object in `MH_BOUND` as the captured leading
// argument, and a second bind of the direct kinds.
// End to end on an adapter: `bindTo` on an INSERT handle wraps it (the
// wrapper's target is the handle itself) instead of overwriting its
// wrapper with the bound value; the inner handle is left as it was.
}

/// Round 15 wave 5, lane mh4: the adapter kinds' arity refusal
/// (`r15w4-mh3-adapter-invoke-has-no-arity-refusal-FIXED-20260930.md`, HotSpot ref
/// `R15Mh3AdapterArity`), `insertArguments` with no values answering its
/// target, and the bound static owner slot
/// (`r15w4-mh3-bindto-static-drops-recorded-owner-FIXED-20260930.md`). The handles
/// carry no `type`, as in `r15w4_mh3_bindto_tests`; `declared` is passed.
#[cfg(test)]
mod r15w5_mh4_adapter_arity_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

const CAT: &str = "(Ljava/lang/String;[Ljava/lang/String;)Ljava/lang/String;";
const BOUND_CAT: &str = "([Ljava/lang/String;)Ljava/lang/String;";
const TWO: &str = "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;";
const ONE: &str = "(Ljava/lang/String;)Ljava/lang/String;";
const THREE: &str =
    "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;";

fn untyped(ctx: &mut MockNativeContext, class: &str, desc: &str, kind: i32) -> ObjectRef {
    let mh = match alloc_method_handle(ctx, class, "m", desc, kind) {
        Ok(mh) => mh,
        Err(e) => panic!("alloc {class}: {e:?}"),
    };
    ctx.set_field_by_name(mh, "type", Value::Object(None));
    mh
}

fn is_wmte(ctx: &MockNativeContext, err: &MethodCallFailed) -> bool {
    match err {
        MethodCallFailed::ExceptionThrown(o) => {
            ctx.class_name_of_id(ctx.class_id_of_object(*o)).as_deref()
                == Some("java/lang/invoke/WrongMethodTypeException")
        }
        _ => false,
    }
}

/// MH3-3: every `MH_KIND_*` this model mints (0 ..= `MH_KIND_TABLE_SWITCH`,
/// contiguous) states what `MH_BOUND` holds; the field accessors by form.
#[test]
fn every_kind_has_a_bound_role() {
    for kind in 0..=MH_KIND_TABLE_SWITCH {
        assert!(
            mh_bound_role(kind, "()V").is_some(),
            "kind {kind} has no role"
        );
    }
    assert_eq!(mh_bound_role(MH_KIND_TABLE_SWITCH + 1, "()V"), None);
    assert_eq!(mh_bound_role(-1, "()V"), None);
    assert_eq!(mh_bound_role(MH_KIND_GETTER, "()I"), Some(BoundRole::Owner));
    assert_eq!(
        mh_bound_role(MH_KIND_GETTER, "(Lp/H;)I"),
        Some(BoundRole::Capture)
    );
    assert_eq!(
        mh_bound_role(MH_KIND_SETTER, "(I)V"),
        Some(BoundRole::Owner)
    );
    assert_eq!(
        mh_bound_role(MH_KIND_SETTER, "(Lp/H;I)V"),
        Some(BoundRole::Capture)
    );
    assert_eq!(
        mh_bound_role(MH_KIND_CONSTRUCTOR, CAT),
        Some(BoundRole::Owner)
    );
    assert_eq!(mh_bound_role(MH_KIND_STATIC, CAT), Some(BoundRole::Capture));
    assert_eq!(mh_bound_role(MH_KIND_INSERT, CAT), Some(BoundRole::Wrapper));
    assert_eq!(
        mh_bound_role(MH_KIND_CONSTANT, "()I"),
        Some(BoundRole::Value)
    );
    assert_eq!(
        mh_bound_role(MH_KIND_LAMBDA_FACTORY, CAT),
        Some(BoundRole::Captures)
    );
    assert_eq!(
        mh_bound_role(MH_KIND_DYNAMIC_INVOKER, CAT),
        Some(BoundRole::CallSite)
    );
    assert_eq!(mh_bound_role(MH_KIND_INVOKER, CAT), Some(BoundRole::Unused));
    // Every adapter kind the arity refusal judges is a wrapper.
    for kind in 0..=MH_KIND_TABLE_SWITCH {
        if mh_adapter_arity_kind(kind) {
            assert_eq!(
                mh_bound_role(kind, CAT),
                Some(BoundRole::Wrapper),
                "kind {kind}"
            );
        }
    }
}

/// The kinds judged: every measured adapter kind, no direct member kind.
#[test]
fn the_adapter_kinds_are_the_measured_combinators() {
    for kind in [
        MH_KIND_INSERT,
        MH_KIND_DROP,
        MH_KIND_FILTER,
        MH_KIND_COLLECT,
        MH_KIND_SPREAD,
    ] {
        assert!(mh_adapter_arity_kind(kind), "kind {kind}");
    }
    for kind in [
        MH_KIND_STATIC,
        MH_KIND_VIRTUAL,
        MH_KIND_SPECIAL,
        MH_KIND_CONSTRUCTOR,
        MH_KIND_INVOKER,
        MH_KIND_IDENTITY,
        MH_KIND_CONSTANT,
    ] {
        assert!(!mh_adapter_arity_kind(kind), "kind {kind}");
    }
}

/// `insertArguments(cat, 0, "-").invoke("a", "b")`, `filterArguments(cat2,
/// 0, upper).invoke("a")`, `dropArguments(cat2, 0, String).invoke(q, a, b,
/// c)`: another arity is refused; the same arity, a collector, an unknown
/// or `()V` type are not judged.
#[test]
fn an_adapter_refuses_another_arity() {
    if !mh_invoke_adapter_arity_enabled() || !mh_strict_exact_signature() {
        return;
    }
    let mut ctx = MockNativeContext::new();
    let insert = untyped(&mut ctx, "__adapter__", CAT, MH_KIND_INSERT);
    let wrapper = Value::Object(Some(ctx.create_string("wrapper")));
    ctx.set_field(insert, MH_BOUND, wrapper);
    match bound_handle_arity_refusal(&mut ctx, insert, TWO, Some(BOUND_CAT)) {
        Some(refusal) => assert!(is_wmte(&ctx, &refusal), "{refusal:?}"),
        None => panic!("a two-argument site over a one-parameter INSERT adapter"),
    }
    let generic = generic_method_type_descriptor(2);
    assert!(bound_handle_arity_refusal(&mut ctx, insert, &generic, Some(BOUND_CAT)).is_some());
    // Its own type, and the same arity (a cast, not this refusal's business).
    assert!(bound_handle_arity_refusal(&mut ctx, insert, BOUND_CAT, Some(BOUND_CAT)).is_none());
    let object_site = "(Ljava/lang/Object;)Ljava/lang/String;";
    assert!(
        bound_handle_arity_refusal(&mut ctx, insert, object_site, Some(BOUND_CAT)).is_none()
    );
    // No `type`, or C21's `()V` stand-in: not judged.
    assert!(bound_handle_arity_refusal(&mut ctx, insert, TWO, None).is_none());
    assert!(bound_handle_arity_refusal(&mut ctx, insert, TWO, Some("()V")).is_none());
    // An explicit collector (`asVarargsCollector` of the adapter) collects.
    mh_set_varargs_collector(&ctx, insert, true);
    assert!(bound_handle_arity_refusal(&mut ctx, insert, TWO, Some(BOUND_CAT)).is_none());
    mh_set_varargs_collector(&ctx, insert, false);
    // FILTER one short, DROP one long; the leaf's `MH_DESC` is not consulted.
    let filter = untyped(&mut ctx, "__adapter__", TWO, MH_KIND_FILTER);
    assert!(bound_handle_arity_refusal(&mut ctx, filter, ONE, Some(TWO)).is_some());
    assert!(bound_handle_arity_refusal(&mut ctx, filter, TWO, Some(TWO)).is_none());
    let dropper = untyped(&mut ctx, "0:1", THREE, MH_KIND_DROP);
    let four = "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)\
                Ljava/lang/String;";
    assert!(bound_handle_arity_refusal(&mut ctx, dropper, four, Some(THREE)).is_some());
    assert!(bound_handle_arity_refusal(&mut ctx, dropper, THREE, Some(THREE)).is_none());
}

/// Memory safety, independent of every switch: a leaf is never entered
/// with fewer arguments than its descriptor has parameters (the VM would
/// read the missing one as the uninitialized-local marker; the w4a
/// `R15Mh3AdapterArity` `filter-short` access violation). A surplus still
/// passes (the VM widens the frame for it).
#[test]
fn a_leaf_is_never_entered_short() {
    let mut ctx = MockNativeContext::new();
    let a = Value::Object(Some(ctx.create_string("a")));
    let b = Value::Object(Some(ctx.create_string("b")));
    match adapt_leaf_args(&mut ctx, &[a], TWO, None, false) {
        Err(e) => assert!(is_wmte(&ctx, &e), "{e:?}"),
        Ok(v) => panic!("a one-argument list reached a two-parameter leaf: {v:?}"),
    }
    assert!(adapt_leaf_args(&mut ctx, &[], ONE, None, false).is_err());
    assert!(adapt_leaf_args(&mut ctx, &[a, b], TWO, None, false).is_ok());
    assert!(adapt_leaf_args(&mut ctx, &[a, b, a], TWO, None, false).is_ok());
    assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
    // End to end, the crash's shape: `filterArguments(cat2, 0, <none>)`
    // dispatched with ONE argument (the doors' refusals bypassed, as with
    // `CRATONVM_MH_INVOKE_ADAPTER_ARITY=0`) reaches the static leaf short.
    let cat2 = untyped(&mut ctx, "p/C", TWO, MH_KIND_STATIC);
    let wrapper = alloc_mh_carrier_holding(
        &mut ctx,
        "__mh_filter_wrapper__",
        &[
            Value::Object(Some(cat2)),
            Value::Object(None),
            Value::Int(0),
        ],
    );
    let filter = untyped(&mut ctx, "__adapter__", TWO, MH_KIND_FILTER);
    ctx.set_field(filter, MH_BOUND, Value::Object(Some(wrapper)));
    match mh_dispatch(&mut ctx, filter, &[a]) {
        Err(e) => assert!(is_wmte(&ctx, &e), "{e:?}"),
        Ok(v) => panic!("the FILTER adapter entered its leaf one argument short: {v:?}"),
    }
}

/// Memory safety (upcall1's patch page): a RAW primitive at a leaf's
/// REFERENCE parameter is boxed (`asType`'s boxing conversion), never laid
/// down as an `int` in a reference local; the site's primitive picks the
/// box when it names one, the raw variant otherwise. Every pin released.
#[test]
fn a_raw_primitive_at_a_reference_parameter_is_boxed() {
    let mut ctx = MockNativeContext::new();
    // The boxes are CANONICAL (`box_value_canonical`: the process-wide
    // `valueOf` caches keyed by `vm_identity()`). Under the shared mock
    // identity `0` this test both read boxes another test's dropped heap
    // left there and left its own for the next test to read (round 15
    // wave 6, lane mh5: `invalid ObjectRef in mock heap` here and in
    // `a_reference_index_follows_unbox_widen`). A private identity.
    ctx.set_vm_identity(0x15F5_0001);
    let boxed_desc = "(Ljava/lang/Integer;Ljava/lang/Integer;)Ljava/lang/Integer;";
    let site = vec!["I".to_string(), "I".to_string()];
    let adapted = match adapt_leaf_args(
        &mut ctx,
        &[Value::Int(7), Value::Int(9)],
        boxed_desc,
        Some(site.as_slice()),
        false,
    ) {
        Ok(v) => v,
        Err(e) => panic!("boxing refused: {e:?}"),
    };
    assert_eq!(adapted.len(), 2);
    for (v, want) in adapted.iter().zip([7, 9]) {
        match *v {
            Value::Object(Some(o)) => {
                assert_eq!(crate::lang_class::unbox_value(&ctx, o), Value::Int(want));
            }
            other => panic!("a raw primitive reached a reference parameter: {other:?}"),
        }
    }
    // No site: the raw variant's wrapper (a `long` is a `Long`).
    match adapt_leaf_args(
        &mut ctx,
        &[Value::Long(3)],
        "(Ljava/lang/Object;)V",
        None,
        false,
    ) {
        Ok(v) => assert!(matches!(v.as_slice(), [Value::Object(Some(_))]), "{v:?}"),
        Err(e) => panic!("{e:?}"),
    }
    // A primitive parameter keeps its raw value; a reference keeps its object.
    let s = Value::Object(Some(ctx.create_string("s")));
    match adapt_leaf_args(
        &mut ctx,
        &[Value::Int(1), s],
        "(ILjava/lang/String;)V",
        None,
        false,
    ) {
        Ok(v) => assert_eq!(v, vec![Value::Int(1), s]),
        Err(e) => panic!("{e:?}"),
    }
    assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
}

/// `mh_dispatch_at_site` arms what the `invoke` door arms: the site
/// without the receiver for an unbound virtual / special handle only.
#[test]
fn the_upcall_site_is_receiver_stripped_for_an_unbound_virtual_handle() {
    let mut ctx = MockNativeContext::new();
    let concat = untyped(&mut ctx, "java/lang/String", ONE, MH_KIND_VIRTUAL);
    assert_eq!(
        mh_receiver_stripped_site(&ctx, concat, TWO).as_deref(),
        Some(ONE)
    );
    let dash = Value::Object(Some(ctx.create_string("-")));
    ctx.set_field(concat, MH_BOUND, dash);
    assert_eq!(
        mh_receiver_stripped_site(&ctx, concat, ONE),
        None,
        "a bound receiver"
    );
    let cat2 = untyped(&mut ctx, "p/C", TWO, MH_KIND_STATIC);
    assert_eq!(
        mh_receiver_stripped_site(&ctx, cat2, TWO),
        None,
        "a static handle"
    );
}

/// `invoke`'s `asType(site)` refuses a wrapper that cannot unbox-and-widen
/// to a primitive parameter (and any other reference/primitive pair
/// `MethodType.canConvert` refuses) with WMTE, before the argument casts:
/// `R15Mh2InvokeExactType` `null-order row3` (a `Long` at an `int`).
/// `insertArguments(t, pos)` with no values is `t` itself (HotSpot's
/// `if (insCount == 0) return target;`), so a virtual target keeps its
/// receiver-leading `type` rather than an adapter typed by `MH_DESC`.
#[test]
fn inserting_no_values_answers_the_target() {
    let mut ctx = MockNativeContext::new();
    let target = untyped(&mut ctx, "java/lang/String", ONE, MH_KIND_VIRTUAL);
    let object = crate::lang_class::reflection_component_id(&mut ctx, "java/lang/Object");
    let empty = ctx.new_ref_array(object, 0);
    let mut registry = NativeMethodRegistry::new();
    register_method_handle_combinator_extras_bridge(&mut registry);
    let insert = registry
        .find(
            "java/lang/invoke/MethodHandles",
            "insertArguments",
            "(Ljava/lang/invoke/MethodHandle;I[Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;",
        )
        .expect("MethodHandles.insertArguments should be registered");
    let answer = match insert(
        &mut ctx,
        &[
            Value::Object(Some(target)),
            Value::Int(0),
            Value::Object(Some(empty)),
        ],
    ) {
        Ok(Some(Value::Object(Some(h)))) => h,
        other => panic!("insertArguments answered {other:?}"),
    };
    assert_eq!(answer == target, mh_invoke_adapter_arity_enabled());
}

/// `findStatic(C, ..).bindTo(x)` keeps the owner the unbound handle
/// recorded in `MH_BOUND` (the bound copy's `MH_BOUND` is `x`).
#[test]
fn a_bound_static_handle_keeps_its_recorded_owner() {
    let mut ctx = MockNativeContext::new();
    let cat = untyped(&mut ctx, "p/C", CAT, MH_KIND_STATIC);
    assert_eq!(
        ctx.get_field(cat, MH_STATIC_OWNER),
        Value::Int(0),
        "minted empty"
    );
    assert_eq!(
        mh_static_owner_in(&ctx, cat, ctx.get_field(cat, MH_BOUND)),
        None
    );
    mh_record_static_owner(&mut ctx, cat, Some(ClassId::new(7)));
    assert_eq!(
        mh_static_owner_in(&ctx, cat, ctx.get_field(cat, MH_BOUND)),
        Some(ClassId::new(7))
    );
    let dash = Value::Object(Some(ctx.create_string("-")));
    let mut registry = NativeMethodRegistry::new();
    register_t4_method_handle_invoke(&mut registry);
    let bind = registry
        .find(
            "java/lang/invoke/MethodHandle",
            "bindTo",
            "(Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;",
        )
        .expect("MethodHandle.bindTo should be registered");
    let bound = match bind(&mut ctx, &[Value::Object(Some(cat)), dash]) {
        Ok(Some(Value::Object(Some(h)))) => h,
        other => panic!("bindTo answered {other:?}"),
    };
    assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
    assert_eq!(ctx.get_field(bound, MH_KIND), Value::Int(MH_KIND_STATIC));
    assert_eq!(ctx.get_field(bound, MH_BOUND), dash);
    let expect = mh_bindto_static_owner_enabled().then(|| ClassId::new(7));
    assert_eq!(mh_static_owner_in(&ctx, bound, dash), expect);
    // An `asType` / `asVarargsCollector` copy keeps it.
    let copy = match mh_clone_handle(&mut ctx, bound) {
        Some(c) => c,
        None => panic!("the bound handle is of this model's layout"),
    };
    assert_eq!(mh_static_owner_in(&ctx, copy, dash), expect);
}
}

