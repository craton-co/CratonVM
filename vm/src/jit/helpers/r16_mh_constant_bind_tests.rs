use super::*;
use cratonvm_native_builtins::lang_invoke::MhCompiledBind;

/// A VM identity no other test uses (the interning table is per thread).
const VM: usize = 0x16C0_0001;

fn bind(class: &str, name: &str, desc: &str, kind: u8) -> MhCompiledBind {
    MhCompiledBind {
        invoke_kind: kind,
        class_id: 0x16C1,
        class: class.to_string(),
        name: name.to_string(),
        desc: desc.to_string(),
        bound: false,
    }
}

/// One interned info per (class, name, descriptor, kind), with the slot
/// count and return tag a compiled site of that call would carry.
#[test]
fn a_bound_target_info_is_interned_and_shaped_as_a_compiled_site() {
    let st = mh_bind_leaf_info(VM, &bind("p/C", "m", "(IJ)D", 3)).expect("static info");
    let again = mh_bind_leaf_info(VM, &bind("p/C", "m", "(IJ)D", 3)).expect("static info");
    assert!(std::ptr::eq(st, again), "interned");
    assert_eq!(
        (st.num_jit_args, st.return_type, st.invoke_kind),
        (2, b'D', 3)
    );
    // The target's own class is the call's class context (the callee-resume
    // fix, `r16-mh-constant-bind-callee-resume-refused-FIXED-20261001.md`).
    assert_eq!((st.declaring_class_id, st.owner_class_id), (0x16C1, 0));
    // Another class id (another loader's copy of `p/C`) or another VM is
    // another info.
    let mut other = bind("p/C", "m", "(IJ)D", 3);
    other.class_id = 0x16C2;
    let o = mh_bind_leaf_info(VM, &other).expect("other class");
    assert!(!std::ptr::eq(st, o));
    assert_eq!(o.declaring_class_id, 0x16C2);
    let v2 = mh_bind_leaf_info(VM + 1, &bind("p/C", "m", "(IJ)D", 3)).expect("other vm");
    assert!(!std::ptr::eq(st, v2));
    let vi = mh_bind_leaf_info(VM, &bind("p/C", "m", "(IJ)D", 0)).expect("virtual info");
    assert!(!std::ptr::eq(st, vi), "the kind is part of the key");
    assert_eq!(vi.num_jit_args, 3, "the receiver is a slot");
    let r = mh_bind_leaf_info(VM, &bind("p/C", "r", "()Lp/C;", 3)).expect("ref info");
    assert_eq!(r.return_type, b'L');
}

/// Round 16 wave 8 (lane mh): a bound (`bindTo`) virtual site passes as
/// many words as its target takes -- the handle's word becomes the
/// receiver's -- which `mh_constant_bind_redirect` checks before
/// `mh_constant_bind_call` splices; the receiver is the `MH_BOUND` guard word.
#[test]
fn a_bound_virtual_target_takes_the_sites_word_count() {
    let mut b = bind("p/C", "v", "(IJ)I", 0);
    b.bound = true;
    let info = mh_bind_leaf_info(VM, &b).expect("virtual info");
    // The site `(IJ)I` on the handle: the handle and two values.
    assert_eq!(info.num_jit_args, 1 + 2);
    assert!(info.num_jit_args <= MH_BIND_MAX_SPLICED_ARGS);
    // An object's guard word is its address, so it can be spliced as is.
    let addr = 0x7_0000_0040usize;
    let obj = unsafe { ObjectRef::from_raw(addr as *mut u8) };
    assert_eq!(mh_bind_slot_key(Value::Object(Some(obj))), addr as u64);
}

/// Guard words: an `Int` never equals a reference or null.
#[test]
fn slot_keys_keep_ints_apart_from_references() {
    assert_eq!(mh_bind_slot_key(Value::Object(None)), 0);
    assert_ne!(mh_bind_slot_key(Value::Int(0)), 0);
    assert_ne!(mh_bind_slot_key(Value::Int(8)), 8);
    assert_ne!(
        mh_bind_slot_key(Value::Int(1)),
        mh_bind_slot_key(Value::Int(2))
    );
}

/// Round 16 wave 10 (lane mhfloor, MH16-8): the direct entry takes an
/// ordinary bound target and leaves to the dispatch ladder every target a
/// name-decided ladder arm may answer before its compiled-entry memos.
#[test]
fn the_direct_entry_leaves_name_decided_arms_to_the_ladder() {
    let st = mh_bind_leaf_info(VM, &bind("p/D", "m", "(I)I", 3)).expect("static info");
    let vi = mh_bind_leaf_info(VM, &bind("p/D", "v", "(I)I", 0)).expect("virtual info");
    assert!(
        mh_bind_direct_leaf_eligible(st),
        "an ordinary static target"
    );
    assert!(
        mh_bind_direct_leaf_eligible(vi),
        "an ordinary virtual target"
    );
    let integer = mh_bind_leaf_info(
        VM,
        &bind("java/lang/Integer", "valueOf", "(I)Ljava/lang/Integer;", 3),
    )
    .expect("integer info");
    assert!(
        !mh_bind_direct_leaf_eligible(integer),
        "the boxing cache's site"
    );
    for name in ["getResource", "getResources", "getResourceAsStream"] {
        let r = mh_bind_leaf_info(
            VM,
            &bind("p/L", name, "(Ljava/lang/String;)Ljava/lang/Object;", 0),
        )
        .expect("resource info");
        assert!(!mh_bind_direct_leaf_eligible(r), "the null-name NPE arm");
    }
    // JIT round 19 wave 1 (lane mhiface): an interface target is eligible;
    // the fill decides whether its site goes direct
    // (`CRATONVM_JIT_MH_CONSTANT_BIND_DIRECT_INTERFACE`).
    let iface = mh_bind_leaf_info(VM, &bind("p/D", "i", "(I)I", 2)).expect("interface info");
    assert!(
        mh_bind_direct_leaf_eligible(iface),
        "an interface target behind the redirect's receiver guard"
    );
    let special = mh_bind_leaf_info(VM, &bind("p/D", "s", "(I)I", 4)).expect("kind 4 info");
    assert!(
        !mh_bind_direct_leaf_eligible(special),
        "a kind the bind does not mint"
    );
}


/// MHF-1: the shape and the guard together name every synthetic slot of
/// a bound handle once, and the shape's words are distinct from the
/// guard's (a shape match re-reads no guard slot as a shape slot).
#[test]
fn the_shape_slots_extend_the_guard_slots() {
    use cratonvm_native_builtins::lang_invoke::{
        MH_COMPILED_BIND_GUARD_SLOTS, MH_COMPILED_BIND_SHAPE_SLOTS,
    };
    for slot in MH_COMPILED_BIND_SHAPE_SLOTS {
        assert!(!MH_COMPILED_BIND_GUARD_SLOTS.contains(&slot));
    }
    let shape = MhBindShape {
        class_id: 7,
        width: 26,
        type_slot: 0,
        words: [0; 5],
    };
    assert_eq!(shape.words.len(), MH_COMPILED_BIND_SHAPE_SLOTS.len() + 1);
}


fn conv(unbox_int: u32, box_int_return: bool) -> MhBindConv {
    MhBindConv {
        type_slot: 0,
        type_word: 0x7_0000_0100,
        unbox_int,
        box_int_return,
        integer_class_id: 0x16C3,
        value_slot: 0,
    }
}

/// Round 16 wave 15 (lane mh6; MHB-1): a converting call's member words
/// are the site's words after the handle, unboxed exactly where the
/// conversion says; a word that will not unbox, or a call wider than the
/// buffer, is the door's.
#[test]
fn a_converting_call_unboxes_only_its_marked_words() {
    let boxed = 0x7_0000_0040i64;
    let unbox = |w: i64| (w == boxed).then_some(-42i64);
    let out = mh_bind_convert_args(&conv(0b10, true), &[5, boxed, boxed], unbox)
        .expect("the marked word unboxes");
    assert_eq!(&out.words[..3], &[5, -42, boxed], "word 2 passes through");
    assert!(out.box_int_return);
    // A null, or any word the unbox refuses, sends the call to the door.
    assert!(mh_bind_convert_args(&conv(0b1, false), &[0], unbox).is_none());
    assert!(mh_bind_convert_args(&conv(0b1, false), &[0x7_0000_0080], unbox).is_none());
    // Nothing marked: the words as they are.
    let plain = mh_bind_convert_args(&conv(0, true), &[1, 2], unbox).expect("plain");
    assert_eq!(&plain.words[..2], &[1, 2]);
    // Wider than the buffer.
    let wide = [boxed; MH_BIND_MAX_SPLICED_ARGS + 1];
    assert!(mh_bind_convert_args(&conv(0b1, false), &wide, unbox).is_none());
}

/// Round 16 wave 15 (lane mh6; MHB-1): the leaf of a converting bind is
/// the member's own info -- the one an exact bind of the member interns --
/// so a converting site's words match its slot count, and its dispatch
/// memos are the member's.
#[test]
fn a_converting_bind_calls_the_members_own_info() {
    let st = mh_bind_leaf_info(VM, &bind("p/E", "twice", "(I)I", 3)).expect("static info");
    // The site `(Object)Object` on the handle: the handle and one value.
    assert_eq!(st.num_jit_args + 1, 2);
    assert_eq!(st.return_type, b'I', "boxed by the helper, not the leaf");
    let again = mh_bind_leaf_info(VM, &bind("p/E", "twice", "(I)I", 3)).expect("again");
    assert!(std::ptr::eq(st, again));
}
