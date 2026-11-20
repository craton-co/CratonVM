use super::*;

#[test]
fn an_expected_throw_id_is_neither_the_wildcard_nor_the_guard_s_own_id() {
    for bci in 0..512u32 {
        for reason in EXPECTED_THROW_REASONS {
            let id = expected_throw_speculation_id(bci, reason);
            assert_ne!(id, SPECULATION_ID_ANY);
            assert_ne!(id, speculation_id(bci, reason));
            assert_eq!(id, expected_throw_speculation_id(bci, reason));
        }
    }
    // A callee's own compile asking about its own guard does not see it.
    let r = DespecRegistry::new();
    r.insert(
        "C.m:()I",
        7,
        expected_throw_speculation_id(7, DeoptReason::BoundsCheck),
    );
    assert!(!r.contains_speculation("C.m:()I", 7, speculation_id(7, DeoptReason::BoundsCheck)));
    assert_eq!(
        r.entries_for("C.m:()I"),
        vec![(
            7,
            expected_throw_speculation_id(7, DeoptReason::BoundsCheck)
        )]
    );
    assert!(r.entries_for("C.other:()I").is_empty());
    assert!(r.entries_for("").is_empty());
}

#[test]
fn only_a_throwing_opcode_is_an_expected_throw() {
    assert!(opcode_throws_for_reason(0x2e, DeoptReason::BoundsCheck)); // iaload
    assert!(opcode_throws_for_reason(0x4f, DeoptReason::BoundsCheck)); // iastore
    assert!(!opcode_throws_for_reason(0xa7, DeoptReason::BoundsCheck)); // goto
    assert!(opcode_throws_for_reason(0x6c, DeoptReason::DivByZero)); // idiv
    assert!(!opcode_throws_for_reason(0x6e, DeoptReason::DivByZero)); // fdiv
    assert!(opcode_throws_for_reason(0xb4, DeoptReason::NullCheck)); // getfield
    assert!(opcode_throws_for_reason(0xbe, DeoptReason::NullCheck)); // arraylength
    assert!(!opcode_throws_for_reason(0xb2, DeoptReason::NullCheck)); // getstatic
    assert!(!opcode_throws_for_reason(0xc0, DeoptReason::ClassCheck));
    assert!(!is_expected_throw_reason(DeoptReason::ClassCheck));
    assert!(is_expected_throw_reason(DeoptReason::NullCheck));
}

#[test]
fn the_mark_is_not_a_resume_kind() {
    let marked = ResumeSemantics::REEXECUTE.with_expected_throw(true);
    assert!(marked.expected_throw);
    assert_ne!(marked, ResumeSemantics::REEXECUTE);
    assert_eq!(marked.resume_kind(), ResumeSemantics::REEXECUTE);
    assert!(!ResumeSemantics::default().expected_throw);
    assert!(marked.to_string().contains("expected throw"));
}
