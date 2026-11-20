use super::*;

/// The VM half of the stamp is the VM's never-reused identity, not its
/// address, so a VM built where a dropped one lived cannot redeem the
/// dropped VM's entries (the same class ids name other classes there).
#[test]
fn a_memo_is_scoped_to_the_vm_identity() {
    let first = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    assert_eq!(stamp(&first).vm, first.vm_identity);
    fill(&first, ClassId::new(5), "run", "()V", ClassId::new(6));
    let second = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    assert_ne!(stamp(&first).vm, stamp(&second).vm);
    assert_eq!(lookup(&second, ClassId::new(5), "run", "()V"), None);
}

/// Interpreter round i1 wave 21 (lane L5): the definition half of the
/// stamp is the VM's own class store's, so a class defined in ANOTHER VM
/// keeps this VM's entry, and one defined in this VM retires it.
#[test]
fn only_this_vms_class_definitions_retire_its_entries() {
    let mine = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let other = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    if cratonvm_classloading::store_epoch_slot(mine.jit.class_layout_domain)
        == cratonvm_classloading::store_epoch_slot(other.jit.class_layout_domain)
    {
        // The two stores share an epoch slot: the documented
        // conservative case, nothing to assert.
        return;
    }
    // A name bound in the VM's namespace: the class-definition event the
    // stamp's `class_definition` half tracks, and nothing else. (A
    // fabricated synthetic class would also rewrite a provenance in place,
    // which moves the process-wide `class_origin_epoch` and retires every
    // VM's entries on purpose.)
    let define = |vm: &crate::vm::SharedVm, name: &str| {
        vm.classes.class_manager.write().register_class_name(
            crate::classloading::ClassLoaderId::Application,
            name,
            ClassId::new(1),
        );
    };
    let (receiver, name, desc) = (21_005u32, "w21L5", "()V");
    let mut checked = false;
    for round in 0..50 {
        // Taken BEFORE the fill, so an unchanged stamp afterwards proves
        // the fill carried it too.
        let before = stamp(&mine);
        fill(&mine, ClassId::new(receiver), name, desc, ClassId::new(7));
        define(&other, format!("cratonvm/test/W21L5Other{round}").as_str());
        if stamp(&mine) != before {
            // A process stamp moved, or a concurrent test's store shares
            // this VM's slot.
            continue;
        }
        assert_eq!(
            lookup(&mine, ClassId::new(receiver), name, desc),
            Some(ClassId::new(7)),
            "another VM's class definition must not retire this VM's entry"
        );
        checked = true;
        break;
    }
    assert!(checked, "no quiet window for this VM's stamp");
    let before = stamp(&mine);
    define(&mine, "cratonvm/test/W21L5Mine");
    assert_ne!(
        stamp(&mine).class_definition,
        before.class_definition,
        "a definition in this VM moves its store's epoch"
    );
    assert_eq!(lookup(&mine, ClassId::new(receiver), name, desc), None);
}

#[test]
fn a_slot_index_is_stable_and_spread() {
    // The same triple always lands in the same slot...
    assert_eq!(
        slot(7, "postComplete", "()V"),
        slot(7, "postComplete", "()V")
    );
    // ...and the receiver class is part of it, which is what keeps two
    // classes' answers apart.
    let a = slot(7, "postComplete", "()V");
    let b = slot(8, "postComplete", "()V");
    assert_ne!(a, b, "the receiver id must move the slot");
    // Empty strings must not panic — `slot` samples first/last bytes.
    let _ = slot(0, "", "");
}

#[test]
fn the_witness_is_only_claimed_by_its_own_triple() {
    let name = "postComplete";
    let desc = "()V";
    let save = arm(ClassId::new(42), name, desc, 3);
    // A nested arrival with a DIFFERENT triple (a `<clinit>`, say) must
    // not claim it.
    note_plain_bytecode_tail(ClassId::new(99), None, "<clinit>", "()V", true, 3);
    // ...nor may an arrival with the right strings but a non-plain callee.
    note_plain_bytecode_tail(
        ClassId::new(99),
        Some(ClassId::new(42)),
        name,
        desc,
        false,
        3,
    );
    note_plain_bytecode_tail(
        ClassId::new(77),
        Some(ClassId::new(42)),
        name,
        desc,
        true,
        3,
    );
    assert_eq!(disarm(save), Some(ClassId::new(77)));
    // And the witness is back to idle, so a later tail claims nothing.
    let save = arm(ClassId::new(1), name, desc, 3);
    assert_eq!(disarm(save), None);
}

#[test]
fn a_nested_arm_restores_the_outer_candidate() {
    let name = "postComplete";
    let desc = "()V";
    let outer = arm(ClassId::new(42), name, desc, 3);
    {
        let inner = arm(ClassId::new(43), "inner", "()I", 3);
        note_plain_bytecode_tail(
            ClassId::new(43),
            Some(ClassId::new(43)),
            "inner",
            "()I",
            true,
            3,
        );
        assert_eq!(disarm(inner), Some(ClassId::new(43)));
    }
    // The outer candidate survived the nested dispatch.
    note_plain_bytecode_tail(
        ClassId::new(42),
        Some(ClassId::new(42)),
        name,
        desc,
        true,
        3,
    );
    assert_eq!(disarm(outer), Some(ClassId::new(42)));
}

#[test]
fn a_tail_one_native_deeper_does_not_claim_the_outer_witness() {
    // The outer call is answered by a special arm's native, whose body
    // calls back on the SAME receiver with the SAME literals by a route
    // that arms nothing. That tail is one native deeper than the arm.
    let name = "size";
    let desc = "()I";
    let save = arm(ClassId::new(10), name, desc, 3);
    {
        let _native = enter_native();
        note_plain_bytecode_tail(
            ClassId::new(30),
            Some(ClassId::new(10)),
            name,
            desc,
            true,
            3,
        );
    }
    assert_eq!(disarm(save), None);
    // At the arming depth the same tail is the candidate's own.
    let save = arm(ClassId::new(10), name, desc, 3);
    note_plain_bytecode_tail(
        ClassId::new(30),
        Some(ClassId::new(10)),
        name,
        desc,
        true,
        3,
    );
    assert_eq!(disarm(save), Some(ClassId::new(30)));
    // And arming inside a native compares against THAT depth.
    let _outer_native = enter_native();
    let save = arm(ClassId::new(10), name, desc, 3);
    note_plain_bytecode_tail(
        ClassId::new(31),
        Some(ClassId::new(10)),
        name,
        desc,
        true,
        3,
    );
    assert_eq!(disarm(save), Some(ClassId::new(31)));
}

#[test]
fn a_tail_on_another_receiver_with_the_same_strings_does_not_claim() {
    // The `size` -> `backing.size()` shape: same literals, same addresses,
    // but the tail's receiver is the BACKING MAP, not the armed set.
    let name = "size";
    let desc = "()I";
    let save = arm(ClassId::new(10), name, desc, 3);
    note_plain_bytecode_tail(
        ClassId::new(20),
        Some(ClassId::new(20)),
        name,
        desc,
        true,
        3,
    );
    assert_eq!(disarm(save), None);
}

#[test]
fn a_tail_from_deeper_java_code_does_not_claim_the_outer_witness() {
    // No native separates the nested arrival from the arm (same native
    // depth), and it names the same receiver class and the same string
    // addresses — but it comes from inside Java code the dispatch ran, so
    // it is at least one frame deeper than the arm.
    let name = "size";
    let desc = "()I";
    let save = arm(ClassId::new(10), name, desc, 3);
    note_plain_bytecode_tail(
        ClassId::new(30),
        Some(ClassId::new(10)),
        name,
        desc,
        true,
        4,
    );
    assert_eq!(disarm(save), None);
    // The candidate's own tail, at the arm's frame depth, still claims it,
    // and a deeper nested arrival first does not spoil that.
    let save = arm(ClassId::new(10), name, desc, 3);
    note_plain_bytecode_tail(
        ClassId::new(31),
        Some(ClassId::new(10)),
        name,
        desc,
        true,
        5,
    );
    note_plain_bytecode_tail(
        ClassId::new(30),
        Some(ClassId::new(10)),
        name,
        desc,
        true,
        3,
    );
    assert_eq!(disarm(save), Some(ClassId::new(30)));
}

/// Interpreter round i1 wave 18, lane L4: an entry filled before a class
/// redefinition is not redeemed after it, and the memo keeps serving what
/// is filled afterwards (it used to be switched off for the process).
/// The process count is not moved here (other tests fill memos in this
/// process); an entry is planted with an earlier count instead.
#[test]
fn an_entry_filled_before_a_redefinition_is_not_redeemed() {
    let vm = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let (receiver, name, desc) = (18_431u32, "i18L4", "()V");
    let now = stamp(&vm);
    assert_eq!(
        now.redefinitions,
        crate::classloading::class_redefinition_count()
    );
    TABLE.with(|t| {
        let mut table = t.borrow_mut();
        if table.is_empty() {
            table.resize_with(SLOTS, || None);
        }
        table[slot(receiver, name, desc)] = Some(Entry {
            receiver_class: receiver,
            static_key: None,
            invoke_class: None,
            method_name: name.into(),
            descriptor: desc.into(),
            stamp: Stamp {
                redefinitions: now.redefinitions.wrapping_sub(1),
                ..now
            },
            declaring_class: 7,
        });
    });
    assert_eq!(lookup(&vm, ClassId::new(receiver), name, desc), None);
    fill(&vm, ClassId::new(receiver), name, desc, ClassId::new(7));
    if stamp(&vm) == now {
        assert_eq!(
            lookup(&vm, ClassId::new(receiver), name, desc),
            Some(ClassId::new(7))
        );
    }
}

/// Round 13 wave 11 (lane callcost5): a static witness is claimed only by
/// the static tail of its own owner and first-argument key, never by an
/// instance tail, and an instance witness never by a static tail.
#[test]
fn a_static_witness_is_only_claimed_by_its_own_static_tail() {
    let (name, desc) = ("twice", "(I)I");
    let (owner, decl, other) = (ClassId::new(40), ClassId::new(41), ClassId::new(99));
    let not_obj = STATIC_FIRST_NOT_AN_OBJECT;
    let key = Some(not_obj);
    let save = arm_static(owner, not_obj, name, desc, 3);
    // An instance tail, another first-argument key, another owner, another
    // frame depth, a non-plain callee, a declined key: none claims it.
    note_plain_bytecode_tail(decl, Some(owner), name, desc, true, 3);
    note_plain_static_tail(decl, owner, Some(7), name, desc, true, 3);
    note_plain_static_tail(decl, other, key, name, desc, true, 3);
    note_plain_static_tail(decl, owner, key, name, desc, true, 4);
    note_plain_static_tail(decl, owner, key, name, desc, false, 3);
    note_plain_static_tail(decl, owner, None, name, desc, true, 3);
    assert_eq!(disarm(save), None);
    let save = arm_static(owner, not_obj, name, desc, 3);
    note_plain_static_tail(decl, owner, key, name, desc, true, 3);
    assert_eq!(disarm(save), Some(decl));
    // An instance witness is never claimed by a static tail.
    let save = arm(owner, name, desc, 3);
    note_plain_static_tail(decl, owner, key, name, desc, true, 3);
    assert_eq!(disarm(save), None);
}

#[test]
fn static_tail_keys_are_lazy_and_only_matching_witnesses_can_claim() {
    let (owner, decl) = (ClassId::new(40), ClassId::new(41));
    let (name, desc) = ("lazy", "(I)I");
    let asks = Cell::new(0);
    let key = || {
        asks.set(asks.get() + 1);
        Some(9)
    };
    let save = arm_static(owner, 9, name, desc, 3);
    note_plain_static_tail_lazy(decl, ClassId::new(99), key, name, desc, true, 3);
    note_plain_static_tail_lazy(decl, owner, key, name, desc, true, 4);
    note_plain_static_tail_lazy(decl, owner, key, name, desc, false, 3);
    note_plain_static_tail_lazy(decl, owner, key, "other", desc, true, 3);
    assert_eq!(asks.get(), 0);
    note_plain_static_tail_lazy(decl, owner, key, name, desc, true, 3);
    assert_eq!(asks.get(), 1);
    assert_eq!(disarm(save), Some(decl));
    let save = arm(owner, name, desc, 3);
    note_plain_static_tail_lazy(decl, owner, key, name, desc, true, 3);
    assert_eq!(asks.get(), 1);
    assert_eq!(disarm(save), None);
}

/// A static entry and an instance entry of the same ids and strings are
/// different entries.
#[test]
fn static_and_instance_entries_do_not_alias() {
    let vm = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let now = stamp(&vm);
    fill_static(
        &vm,
        ClassId::new(12_010),
        5,
        "w11cc5",
        "()V",
        ClassId::new(9),
    );
    if stamp(&vm) == now {
        assert_eq!(lookup(&vm, ClassId::new(12_010), "w11cc5", "()V"), None);
        assert_eq!(
            lookup_static(&vm, ClassId::new(12_010), 6, "w11cc5", "()V"),
            None
        );
        assert_eq!(
            lookup_static(&vm, ClassId::new(12_010), 5, "w11cc5", "()V"),
            Some(ClassId::new(9))
        );
    }
}

/// The stack-walker residual-costs page, item 1: a `NativeContext::invoke`
/// witness is claimed by the instance tail whose receiver is the first
/// argument's class, or by the static tail of its own owner and key, and
/// by nothing else; a plain instance or static witness is not claimed
/// through it.
#[test]
fn an_invoke_witness_is_claimed_by_its_receiver_or_its_own_static_tail() {
    let (name, desc) = ("doStackWalk", "(JIIII)Ljava/lang/Object;");
    let (owner, recv, decl, other) = (
        ClassId::new(50),
        ClassId::new(51),
        ClassId::new(52),
        ClassId::new(99),
    );
    let key = recv.as_u32();
    // Another receiver, a deeper frame, a static tail of another owner or
    // key, a non-plain callee: none claims it.
    let save = arm_invoke(owner, key, name, desc, 3);
    note_plain_bytecode_tail(decl, Some(other), name, desc, true, 3);
    note_plain_bytecode_tail(decl, Some(recv), name, desc, true, 4);
    note_plain_bytecode_tail(decl, Some(recv), name, desc, false, 3);
    note_plain_static_tail(decl, other, Some(key), name, desc, true, 3);
    note_plain_static_tail(decl, owner, Some(key + 1), name, desc, true, 3);
    assert_eq!(disarm(save), None);
    // The instance tail on the first argument's class claims it.
    let save = arm_invoke(owner, key, name, desc, 3);
    note_plain_bytecode_tail(decl, Some(recv), name, desc, true, 3);
    assert_eq!(disarm(save), Some(decl));
    // So does the static tail of its owner and key.
    let save = arm_invoke(owner, key, name, desc, 3);
    note_plain_static_tail(decl, owner, Some(key), name, desc, true, 3);
    assert_eq!(disarm(save), Some(decl));
    // A first argument that is no object is never an instance receiver.
    let not_obj = STATIC_FIRST_NOT_AN_OBJECT;
    let save = arm_invoke(owner, not_obj, name, desc, 3);
    note_plain_bytecode_tail(decl, None, name, desc, true, 3);
    assert_eq!(disarm(save), None);
}

/// `NativeContext::invoke` entries are redeemed by `lookup_invoke` only,
/// keyed by the class name, and redeem no instance or static entry.
#[test]
fn invoke_entries_alias_no_other_entry() {
    let vm = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let now = stamp(&vm);
    let (name, desc) = ("wrInvoke1", "()V");
    fill_invoke(&vm, "cratonvm/test/WrOwner", 61_001, name, desc, ClassId::new(9));
    fill(&vm, ClassId::new(61_002), name, desc, ClassId::new(10));
    fill_static(&vm, ClassId::new(61_003), 61_001, name, desc, ClassId::new(11));
    if stamp(&vm) != now {
        return;
    }
    assert_eq!(
        lookup_invoke(&vm, "cratonvm/test/WrOwner", 61_001, name, desc),
        Some(ClassId::new(9))
    );
    assert_eq!(lookup_invoke(&vm, "cratonvm/test/WrOther", 61_001, name, desc), None);
    assert_eq!(lookup_invoke(&vm, "cratonvm/test/WrOwner", 61_002, name, desc), None);
    assert_eq!(lookup(&vm, ClassId::new(61_001), name, desc), None);
    assert_eq!(lookup_static(&vm, ClassId::new(0), 61_001, name, desc), None);
    assert_eq!(lookup(&vm, ClassId::new(61_002), name, desc), Some(ClassId::new(10)));
    assert_eq!(
        lookup_static(&vm, ClassId::new(61_003), 61_001, name, desc),
        Some(ClassId::new(11))
    );
}

/// Wave 26: the yielding native alone does not pin; a native under it
/// (one that re-entered Java) or a `pin_continuation` door does, and each
/// guard releases what it took.
#[test]
fn a_native_under_the_yielding_one_pins_the_continuation() {
    assert!(!continuation_pinned_by_native());
    let outer = enter_native();
    assert!(
        !continuation_pinned_by_native(),
        "the yielding native alone"
    );
    {
        let _inner = enter_native();
        assert!(continuation_pinned_by_native(), "a native re-entered Java");
    }
    assert!(!continuation_pinned_by_native());
    {
        let _door = pin_continuation();
        assert!(continuation_pinned_by_native(), "a pinning door");
    }
    {
        // A method-handle door (`outer`) that declared its nested call
        // transparent, and the native that yields beneath it.
        let _transparent = cratonvm_native_api::continuation_pin::transparent();
        let _yielder = enter_native();
        assert!(!continuation_pinned_by_native(), "a transparent door");
        let _opaque = enter_native();
        let _yielder_deeper = enter_native();
        assert!(
            continuation_pinned_by_native(),
            "an opaque native beneath it"
        );
    }
    drop(outer);
    assert!(!continuation_pinned_by_native());
}

/// Interpreter round i6 wave 130, lane L6: a slice run on its carrier's
/// OS thread starts from a runner's zero native depth, so the yielding
/// native alone (`wait0`, `park`) does not pin it and a contended
/// `monitorenter` (no native) finds no native frame; the carrier's
/// `enterSpecial` depth comes back afterwards.
#[test]
fn i6w130_a_carrier_slice_starts_from_a_zero_native_depth() {
    let enter_special = enter_native();
    assert!(continuation_has_native_frame(), "the carrier's enterSpecial");
    let base = SliceNativeBase::enter();
    assert!(!continuation_has_native_frame(), "the slice's monitorenter");
    {
        let _park = enter_native();
        assert!(!continuation_pinned_by_native(), "the slice's park native");
    }
    base.restore();
    assert!(continuation_has_native_frame(), "enterSpecial counted again");
    drop(enter_special);
    assert!(!continuation_has_native_frame());
}
