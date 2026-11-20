use super::*;
use crate::jit::conservative_roots::InlinedLevel;

fn frame(label: &str, bci: i32, chain: Vec<InlinedLevel>) -> ActiveCompiledFrame {
    ActiveCompiledFrame {
        inline_chain_shared: None,
        interp_depth: 0,
        chain_index: 0,
        label: label.into(),
        owner_class_id: 0,
        cm_ptr: 0,
        bci,
        inline_chain: chain,
        chain_exact: true,
        trap_site_exact: false,
        chain_poisoned: false,
        compile_cp_stamp: None,
        fast_throw_c2_tier: false,
        artifact_id: 0,
        frame_rbp: None,
    }
}

/// The same frame with a chain recovered from the COARSE safepoint-bci key.
fn frame_coarse_chain(label: &str, bci: i32, chain: Vec<InlinedLevel>) -> ActiveCompiledFrame {
    ActiveCompiledFrame {
        chain_exact: false,
        ..frame(label, bci, chain)
    }
}

fn level(label: &str, bci: u32) -> InlinedLevel {
    InlinedLevel {
        label: label.into(),
        bci,
        class_id: 0,
        hidden: None,
    }
}

/// The INNERMOST frame is the last one, and an inlined callee inside it
/// beats the artifact's own bci — that callee is where the null was
/// dereferenced.
#[test]
fn the_innermost_site_is_the_last_frame_and_its_first_inline_level() {
    let site = innermost_trap_site(&[
        frame("P.outerMost:()V", 3, vec![]),
        frame("P.holder:()I", 7, vec![level("P.leaf:()I", 1)]),
    ])
    .expect("a site");
    assert_eq!(site.class_name, "P");
    assert_eq!(site.method_name, "leaf");
    assert_eq!(site.method_descriptor, "()I");
    assert_eq!(site.bci, 1);
}

/// Round 15 wave 7 (lane throw2, TH1-5): the allocation-free deferral
/// admission answers exactly `innermost_trap_site`'s `exact`, shape by
/// shape -- a deferred message is asked of the same site the eager one is.
#[test]
fn the_deferral_admission_is_innermost_trap_site_exactness() {
    let trap = |label: &str, bci: i32| ActiveCompiledFrame {
        trap_site_exact: true,
        ..frame(label, bci, vec![])
    };
    let shapes: Vec<Vec<ActiveCompiledFrame>> = vec![
        vec![],
        vec![frame("P.m:()V", 4, vec![])],
        vec![trap("P.m:()V", 4)],
        vec![frame("P.outer:()V", 2, vec![]), trap("P.m:()V", 4)],
        vec![frame("P.holder:()I", 7, vec![level("P.leaf:()I", 1)])],
        vec![frame_coarse_chain(
            "P.holder:()I",
            7,
            vec![level("P.leaf:()I", 1)],
        )],
        vec![frame("P.holder:()I", 7, vec![level("P.leaf:()I", 70_000)])],
        vec![trap("P.m:()V", -1)],
        vec![trap("garbage", 3)],
        vec![trap(".m:()V", 3)],
    ];
    for (i, shape) in shapes.iter().enumerate() {
        let want = innermost_trap_site(shape).is_some_and(|site| site.exact);
        assert_eq!(
            snapshot_names_an_exact_trap_site(Some(shape)),
            want,
            "shape {i}"
        );
    }
    assert!(!snapshot_names_an_exact_trap_site(None));
}

/// With no inline chain the artifact's own bci is the answer.
#[test]
fn an_artifact_with_no_inlined_callee_answers_with_its_own_bci() {
    let site = innermost_trap_site(&[frame("a/b/C.m:(I)J", 12, vec![])]).expect("a site");
    assert_eq!(site.class_name, "a/b/C");
    assert_eq!(site.method_name, "m");
    assert_eq!(site.method_descriptor, "(I)J");
    assert_eq!(site.bci, 12);
}

/// `-1` is the "no usable safepoint id" refusal, not a bci. Accepting it
/// would index `code[usize::MAX]` on one reading and, on the reading that
/// clamps, produce a message about a completely different opcode.
#[test]
fn a_frame_with_no_recovered_bci_is_declined_rather_than_guessed() {
    assert_eq!(
        innermost_trap_site(&[frame("a/b/C.m:()V", -1, vec![])]),
        None
    );
    assert_eq!(innermost_trap_site(&[]), None);
}

/// A bci past JVMS 4.9.1's `code_length` bound is refused for the same
/// reason `stackwalker::inlined_frame_entry` refuses it: the lookups
/// downstream answer confidently for an out-of-range index.
#[test]
fn a_bci_outside_the_jvms_bound_is_refused() {
    assert_eq!(
        innermost_trap_site(&[frame("a/b/C.m:()V", MAX_CODE_LENGTH as i32, vec![])]),
        None
    );
}

/// A POISONED safepoint bci (the entry of a spliced region with several
/// calls) is declined: the opcode there is the invoke the splice replaced,
/// not the dereference that trapped. An exact trap site at the same bci,
/// or a frame that did recover a chain, still answers.
#[test]
fn a_poisoned_splice_entry_bci_is_declined() {
    let poisoned = ActiveCompiledFrame {
        chain_poisoned: true,
        ..frame(
            "java/lang/reflect/Field.get:(Ljava/lang/Object;)Ljava/lang/Object;",
            14,
            vec![],
        )
    };
    assert_eq!(innermost_trap_site(std::slice::from_ref(&poisoned)), None);
    let exact = ActiveCompiledFrame {
        trap_site_exact: true,
        ..poisoned.clone()
    };
    assert_eq!(innermost_trap_site(&[exact]).map(|s| s.bci), Some(14));
    let chained = ActiveCompiledFrame {
        inline_chain: vec![level("P.leaf:()I", 2)],
        ..poisoned
    };
    assert_eq!(innermost_trap_site(&[chained]).map(|s| s.bci), Some(2));
}

/// An unparsable label yields no site at all rather than a mangled name.
#[test]
fn an_unparsable_label_yields_no_site() {
    assert_eq!(
        innermost_trap_site(&[frame("no-colon-here", 0, vec![])]),
        None
    );
    assert_eq!(innermost_trap_site(&[frame("nodot:()V", 0, vec![])]), None);
    assert_eq!(innermost_trap_site(&[frame(".m:()V", 0, vec![])]), None);
    assert_eq!(innermost_trap_site(&[frame("C.:()V", 0, vec![])]), None);
}

/// A chain recovered from the coarse key names the same site an EXACT one
/// does -- it is what the trace already prints for this frame -- but it is
/// not a proof, so it does not make the site exact and the caller still has
/// to corroborate it.
#[test]
fn a_coarse_chain_names_the_site_but_does_not_make_it_exact() {
    let coarse = innermost_trap_site(&[frame_coarse_chain(
        "P.caller:()V",
        11,
        vec![level("P.callee:()I", 2)],
    )])
    .expect("a coarse chain still names a site");
    assert_eq!(
        (coarse.method_name.as_str(), coarse.bci, coarse.exact),
        ("callee", 2, false)
    );
    let site =
        innermost_trap_site(&[frame("P.caller:()V", 11, vec![level("P.callee:()I", 2)])])
            .expect("an exact chain names a site");
    assert_eq!(
        (site.method_name.as_str(), site.bci, site.exact),
        ("callee", 2, true)
    );
}

/// The site carries the class id the snapshot recorded for the trapping
/// method -- the artifact owner's when unspliced, the inlined level's when
/// spliced -- so the bytecode lookup need not guess by name.
#[test]
fn the_site_carries_the_recorded_class_id_of_the_trapping_method() {
    let mut owner = frame("P.m:()V", 3, vec![]);
    owner.owner_class_id = 41;
    assert_eq!(innermost_trap_site(&[owner.clone()]).unwrap().class_id, 41);
    let mut inlined = level("Q.leaf:()I", 1);
    inlined.class_id = 57;
    owner.inline_chain = vec![inlined];
    assert_eq!(innermost_trap_site(&[owner.clone()]).unwrap().class_id, 57);
    // A level that recorded no id stays "unknown"; it does NOT inherit the
    // artifact owner's, which names a different class.
    owner.inline_chain = vec![level("Q.leaf:()I", 1)];
    assert_eq!(innermost_trap_site(&[owner]).unwrap().class_id, 0);
}

/// An artifact with NO splice at the trapping point answers from its own
/// safepoint-id bci -- the shape the `invoke` warrant is built for -- and
/// says plainly that the site is not an emitter-described one.
#[test]
fn an_unspliced_artifact_answers_from_its_own_bci_and_is_not_exact() {
    let site = innermost_trap_site(&[frame("P.m:()V", 3, vec![])]).expect("a site");
    assert_eq!((site.bci, site.exact), (3, false));
}

/// The two warrants are independent: an emitter-described site needs no
/// action code, and a corroborating action code needs no described site.
#[test]
fn a_site_is_trusted_on_either_warrant_and_on_neither_is_refused() {
    use cratonvm_jit_api::npe_action as a;
    let inexact = TrapSite {
        class_name: "C".into(),
        method_name: "m".into(),
        method_descriptor: "()V".into(),
        bci: 4,
        exact: false,
        class_id: 0,
    };
    let exact = TrapSite {
        exact: true,
        ..inexact.clone()
    };
    // getfield, no action recorded: only an emitter-described site will do.
    assert!(!site_is_trustworthy(&inexact, 0xb4, a::NONE));
    assert!(site_is_trustworthy(&exact, 0xb4, a::NONE));
    // arraylength corroborated by the recorded action.
    assert!(site_is_trustworthy(&inexact, 0xbe, a::ARRAY_LENGTH));
    // ... and the SAME action against a different opcode is not
    // corroboration, it is a contradiction.
    assert!(!site_is_trustworthy(&inexact, 0x2e, a::ARRAY_LENGTH));
    // `INVOKE_RECEIVER` corroborates all three invoke opcodes and nothing
    // else -- `invokestatic` (0xb8) included, which has no receiver and
    // can never raise this NPE.
    for op in [0xb6u8, 0xb7, 0xb9] {
        assert!(site_is_trustworthy(&inexact, op, a::INVOKE_RECEIVER));
    }
    assert!(!site_is_trustworthy(&inexact, 0xb8, a::INVOKE_RECEIVER));
    assert!(!site_is_trustworthy(&inexact, 0xb4, a::INVOKE_RECEIVER));
}

/// The action-to-opcode table is the corroboration, so a wrong row in it
/// would silently license exactly the message this guard exists to refuse.
#[test]
fn every_array_action_names_its_own_opcode() {
    use cratonvm_jit_api::npe_action as a;
    for (code, opcode) in [
        (a::ARRAY_LENGTH, 0xbeu8),
        (a::ALOAD_INT, 0x2e),
        (a::ALOAD_LONG, 0x2f),
        (a::ALOAD_FLOAT, 0x30),
        (a::ALOAD_DOUBLE, 0x31),
        (a::ALOAD_OBJECT, 0x32),
        (a::ALOAD_BYTE, 0x33),
        (a::ALOAD_CHAR, 0x34),
        (a::ALOAD_SHORT, 0x35),
        (a::ASTORE_INT, 0x4f),
        (a::ASTORE_LONG, 0x50),
        (a::ASTORE_FLOAT, 0x51),
        (a::ASTORE_DOUBLE, 0x52),
        (a::ASTORE_OBJECT, 0x53),
        (a::ASTORE_BYTE, 0x54),
        (a::ASTORE_CHAR, 0x55),
        (a::ASTORE_SHORT, 0x56),
    ] {
        assert!(
            action_admits_opcode(code, opcode),
            "action {code} must admit opcode {opcode:#x}"
        );
        // And nothing else: every action is admitted by ONE opcode.
        assert!(!action_admits_opcode(code, opcode.wrapping_add(1)));
    }
    // `NONE` describes nothing and must corroborate nothing.
    for opcode in [0xb4u8, 0xb5, 0xb6, 0xbe, 0x2e, 0x4f] {
        assert!(!action_admits_opcode(a::NONE, opcode));
    }
    // An array action must not be admitted by an invoke, and vice versa.
    assert!(!action_admits_opcode(a::ARRAY_LENGTH, 0xb6));
    assert!(!action_admits_opcode(a::INVOKE_RECEIVER, 0xbe));
}

/// Both array families are laid out `i l f d a b c s`, and this is the
/// table that depends on it.
#[test]
fn the_array_element_table_follows_the_jvms_opcode_order() {
    assert_eq!(array_elem_kind(0), ArrayElemKind::Int);
    assert_eq!(array_elem_kind(1), ArrayElemKind::Long);
    assert_eq!(array_elem_kind(2), ArrayElemKind::Float);
    assert_eq!(array_elem_kind(3), ArrayElemKind::Double);
    assert_eq!(array_elem_kind(4), ArrayElemKind::Object);
    assert_eq!(array_elem_kind(5), ArrayElemKind::Byte);
    assert_eq!(array_elem_kind(6), ArrayElemKind::Char);
    assert_eq!(array_elem_kind(7), ArrayElemKind::Short);
}
