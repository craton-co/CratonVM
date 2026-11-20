use super::*;

/// Wave 53, lane L4: the refusal record is taken only by the door whose
/// exception it names, and any take drops it.
#[test]
fn the_astype_refusal_record_is_taken_by_its_own_exception_only() {
    // SAFETY: never dereferenced; non-null and 8-aligned, as `from_raw`
    // requires (the record compares identities only).
    let exc = |bits: usize| unsafe { cratonvm_types::ObjectRef::from_raw(bits as *mut u8) };
    let record = |e| VhAsTypeRefusal {
        exception: e,
        coordinates: vec!["[I".to_string(), "I".to_string()],
        value: "I".to_string(),
    };
    assert_eq!(take_vh_astype_refusal(exc(0x1000)), None, "nothing noted");
    note_vh_astype_refusal(record(exc(0x1000)));
    assert_eq!(
        take_vh_astype_refusal(exc(0x1000)),
        Some(record(exc(0x1000)))
    );
    assert_eq!(take_vh_astype_refusal(exc(0x1000)), None, "taken once");
    note_vh_astype_refusal(record(exc(0x2000)));
    assert_eq!(
        take_vh_astype_refusal(exc(0x1000)),
        None,
        "another exception's"
    );
    assert_eq!(
        take_vh_astype_refusal(exc(0x2000)),
        None,
        "dropped by that take"
    );
}

/// TAKEN, not peeked — the property the whole channel rests on.
#[test]
fn the_call_site_descriptor_is_taken_once() {
    assert_eq!(take(), None, "a fresh thread has nothing armed");
    arm("(Ljava/lang/Object;)Ljava/lang/String;");
    assert_eq!(
        take().as_deref(),
        Some("(Ljava/lang/Object;)Ljava/lang/String;")
    );
    assert_eq!(take(), None, "a second reader must not see the first's");
    arm("(I)V");
    clear();
    assert_eq!(take(), None, "clear discards an armed descriptor");
}

/// Interpreter round i5 wave 107, lane L4: the caller travels with the
/// descriptor it was armed with and with no other one.
#[test]
fn the_armed_caller_belongs_to_the_take_it_was_armed_for() {
    assert_eq!(taken_caller(), 0, "a fresh thread has none");
    arm_with_caller("(LBox;)I", 42);
    assert_eq!(take().as_deref(), Some("(LBox;)I"));
    assert_eq!(taken_caller(), 42);
    assert_eq!(taken_caller(), 42, "a read does not consume it");
    arm("(LBox;)I");
    assert_eq!(take().as_deref(), Some("(LBox;)I"));
    assert_eq!(taken_caller(), 0, "a plain arm names no caller");
    arm_with_caller("(I)V", 7);
    clear();
    assert_eq!(take(), None);
    assert_eq!(taken_caller(), 0, "clear drops the caller too");
    arm_with_caller("(I)V", 7);
    arm_shared(&Arc::from("(J)V"));
    assert_eq!(take().as_deref(), Some("(J)V"));
    assert_eq!(taken_caller(), 0, "a later arm resets the caller");
}

/// Round 15 wave 4 (lane mh2; MH15-2): a re-armed descriptor is the SAME
/// shared text while it is among the last `RECENT_SITES` distinct ones, a
/// fresh one after it falls out, and whatever the ring holds, what is
/// taken is exactly what was last armed.
#[test]
fn a_rearmed_descriptor_is_shared_and_the_take_contract_holds() {
    let sites = ["(I)I", "(J)J", "(F)F", "(D)D", "(Z)Z"];
    for s in sites {
        arm("(I)I");
        arm(s);
        assert_eq!(take().as_deref(), Some(s), "the last armed wins");
        assert_eq!(take(), None);
    }
    let reuse = true;
    arm("(Ljava/lang/Object;)V");
    let first = take().expect("armed");
    arm("(Ljava/lang/Object;)V");
    let again = take().expect("armed");
    assert_eq!(&*first, &*again);
    assert_eq!(Arc::ptr_eq(&first, &again), reuse, "shared while recent");
    // `RECENT_SITES` descriptors the ring does not hold push it out.
    let fresh = ["(B)B", "(S)S", "(C)C", "(Ljava/lang/String;)V"];
    assert_eq!(fresh.len(), RECENT_SITES);
    for s in fresh {
        arm(s);
    }
    clear();
    arm("(Ljava/lang/Object;)V");
    let evicted = take().expect("armed");
    assert_eq!(&*evicted, "(Ljava/lang/Object;)V");
    assert!(!Arc::ptr_eq(&first, &evicted), "a fresh copy once evicted");
}

/// `arm_shared` publishes the same text as `arm`, under the same TAKE
/// contract, and hands the consumer the door's own allocation (a count,
/// not a copy).
#[test]
fn a_shared_descriptor_is_taken_once_without_a_copy() {
    let site: Arc<str> = Arc::from("(LH;)I");
    arm_shared(&site);
    assert_eq!(Arc::strong_count(&site), 2, "the cell holds one count");
    let taken = take().expect("armed");
    assert!(
        Arc::ptr_eq(&taken, &site),
        "the door's allocation, not a copy"
    );
    assert_eq!(&*taken, "(LH;)I");
    assert_eq!(take(), None, "taken, not peeked");
    drop(taken);
    arm_shared(&site);
    clear();
    assert_eq!(Arc::strong_count(&site), 1, "clear drops the cell's count");
    assert_eq!(take(), None);
}

/// The probe `R11W15RtVarHandleObjectSite`'s three call-site shapes, and
/// the two-value `compareAndSet` shape: the classes land in the right
/// byte, and a primitive is published as its own descriptor byte.
#[test]
fn vh_value_site_classes_reads_the_trailing_parameters() {
    let pack = |prev: u8, last: u8| ((prev as u16) << 8) | last as u16;
    assert_eq!(
        vh_value_site_classes("(LR11$H;Ljava/lang/Object;)V"),
        pack(vh_site::OTHER, vh_site::OBJECT)
    );
    assert_eq!(
        vh_value_site_classes("(LR11$H;Ljava/lang/String;)V"),
        pack(vh_site::OTHER, vh_site::OTHER)
    );
    assert_eq!(
        vh_value_site_classes("(LR11$H;I)V"),
        pack(vh_site::OTHER, b'I')
    );
    assert_eq!(
        vh_value_site_classes("(LR11$H;Ljava/lang/Long;Ljava/lang/Number;)Z"),
        pack(vh_site::W_LONG, vh_site::NUMBER)
    );
    // A static handle's `set`: one parameter, nothing before it.
    assert_eq!(vh_value_site_classes("(J)V"), pack(0, b'J'));
    // Array-element coordinates, and array-typed values, parse.
    assert_eq!(
        vh_value_site_classes("([IILjava/lang/Integer;)V"),
        pack(b'I', vh_site::W_INTEGER)
    );
    assert_eq!(
        vh_value_site_classes("(Ljava/lang/Object;[[Ljava/lang/String;)V"),
        pack(vh_site::OBJECT, vh_site::OTHER)
    );
    // The JIT's `R`-slot stand-ins (round 11 wave 18) must read as OTHER.
    assert_eq!(
        vh_value_site_classes("(Ljava/lang/Object;Ljava/lang/Void;Ljava/lang/Void;)Z"),
        pack(vh_site::OTHER, vh_site::OTHER)
    );
    // Nothing to classify, or nothing parseable: unknown.
    assert_eq!(vh_value_site_classes("()V"), 0);
    assert_eq!(vh_value_site_classes("(Ljava/lang/Object"), 0);
    assert_eq!(vh_value_site_classes("I)V"), 0);
}

/// Taken once, cleared by `clear_vh_value_site`, and independent of the
/// descriptor channel above.
#[test]
fn the_vh_value_site_is_taken_once_and_separate() {
    assert_eq!(take_vh_value_site(), 0, "a fresh thread has nothing armed");
    arm_vh_value_site(0x0f01);
    assert_eq!(take(), None, "the descriptor channel is a different cell");
    assert_eq!(take_vh_value_site(), 0x0f01);
    assert_eq!(take_vh_value_site(), 0, "a second reader must not see it");
    arm_vh_value_site(7);
    clear_vh_value_site();
    assert_eq!(take_vh_value_site(), 0);
    assert_eq!(vh_site::wrapper_prim(vh_site::W_CHARACTER), Some(b'C'));
    assert_eq!(vh_site::wrapper_prim(vh_site::OBJECT), None);
    assert!(vh_site::is_primitive(b'Z') && !vh_site::is_primitive(vh_site::OTHER));
}

/// Round 15 wave 3 (lane vh3; VH1-3): the widened word covers the
/// coordinates, keeps the old cell as its low half, and shares its cell.
#[test]
fn the_param_site_word_covers_the_coordinates() {
    // `f.get(arr, i)` with `(VarHandle, int[], char)` declared: the call
    // site is `([II)I`, coordinates only.
    let w = vh_param_site_classes("([II)I");
    assert_eq!(vh_param_site_class_at(w, 0), b'I');
    assert_eq!(vh_param_site_class_at(w, 1), vh_site::OTHER);
    assert_eq!(vh_param_site_class_at(w, 2), vh_site::UNKNOWN);
    assert_eq!(
        vh_param_site_class_at(w, VH_PARAM_SITE_SLOTS),
        vh_site::UNKNOWN
    );
    // The low half is the old cell for every shape the old test pins.
    for d in [
        "(LR11$H;Ljava/lang/Object;)V",
        "(LR11$H;I)V",
        "(LR11$H;Ljava/lang/Long;Ljava/lang/Number;)Z",
        "(J)V",
        "([IILjava/lang/Integer;)V",
        "()V",
        "(Ljava/lang/Object",
    ] {
        assert_eq!(
            vh_param_site_classes(d) & 0xffff,
            u64::from(vh_value_site_classes(d)),
            "{d}"
        );
    }
    // Nine parameters: the first two fall off the word (seven slots; the
    // top byte is the return's, wave 57).
    let nine = "(BSCIJFDZLjava/lang/Object;)V";
    let w = vh_param_site_classes(nine);
    assert_eq!(vh_param_site_class_at(w, 0), vh_site::OBJECT);
    assert_eq!(vh_param_site_class_at(w, 6), b'C');
    assert_eq!(vh_param_site_class_at(w, 7), vh_site::UNKNOWN);
    assert_eq!(
        w >> VH_SITE_RETURN_SHIFT,
        0,
        "the classes never reach the return byte"
    );
    // One cell: a word armed is taken whole, a u16 armed reads zero above.
    arm_vh_param_site(w);
    assert_eq!(take_vh_param_site(), w);
    assert_eq!(take_vh_param_site(), 0);
    arm_vh_value_site(0x0f01);
    assert_eq!(take_vh_param_site(), 0x0f01);
    arm_vh_param_site(w);
    assert_eq!(u64::from(take_vh_value_site()), w & 0xffff);
    assert_eq!(
        take_vh_param_site(),
        0,
        "the u16 take clears the whole word"
    );
}

/// Wave 57, lane L4: the word carries the return in its top byte -- a
/// primitive's descriptor byte, a reference's class code, nothing for
/// `Object` or `void` -- and keeps the classes.
#[test]
fn the_param_site_word_carries_the_return() {
    for (d, r) in [
        ("(LH;)I", b'I'),
        ("(LH;I)J", b'J'),
        ("()S", b'S'),
        ("([II)Z", b'Z'),
        ("(LH;)Ljava/lang/Object;", 0),
        ("(LH;I)V", 0),
        ("(LH;", 0),
        ("(LH;)[I", vh_site::OTHER),
        ("(LH;)Ljava/lang/String;", vh_site::OTHER),
        ("(LH;)Ljava/lang/Long;", vh_site::W_LONG),
        ("(LH;)Ljava/lang/Number;", vh_site::NUMBER),
    ] {
        let w = vh_param_site_word(d);
        assert_eq!(vh_param_site_return(w), r, "{d}");
        assert_eq!(
            w & VH_PARAM_SITE_PARAM_MASK,
            vh_param_site_classes(d),
            "{d}"
        );
    }
    assert_eq!(vh_param_site_return(vh_param_site_classes("(LH;)I")), 0);
    // Every class code sits below every descriptor letter.
    assert!(vh_site::OTHER < b'B');
}
