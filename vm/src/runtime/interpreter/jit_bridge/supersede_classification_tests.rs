use super::{classify_supersede, note_supersede_outcome, supersede_census, SupersedeOutcome};

/// The three outcomes a C2 publish can have, and which of them the epoch
/// bump is for.
#[test]
fn classify_supersede_separates_the_three_publish_outcomes() {
    // No predecessor: `promote_scalar_selfrec_to_ir` reaches C2 without a
    // C1 body ever existing, and the old diagnostic reported that as
    // `c1=?` — indistinguishable from a failed lookup.
    assert_eq!(
        classify_supersede(None, Some(&[0x90, 0xc3])),
        SupersedeOutcome::FirstPublish,
    );
    assert_eq!(
        classify_supersede(Some(&[0x90, 0xc3]), Some(&[0x90, 0xc3])),
        SupersedeOutcome::Unchanged,
    );
    assert_eq!(
        classify_supersede(Some(&[0x90, 0xc3]), Some(&[0x31, 0xc0, 0xc3])),
        SupersedeOutcome::Changed,
    );
}

/// A missing replacement must NOT read as one of the two cheap outcomes.
/// This decides whether to skip an invalidation, so "I could not tell" has
/// to fail towards the historical unconditional bump.
#[test]
fn classify_supersede_fails_towards_bumping_when_the_replacement_is_unknown() {
    assert_eq!(
        classify_supersede(Some(&[0x90]), None),
        SupersedeOutcome::Changed,
    );
}

/// Same length is NOT the same body, and this is the case that refuted the
/// hypothesis this classifier was built for: a C2 task that falls back to
/// the single-pass backend recompiles the same bytecode, but each compile
/// embeds fresh `JitInvokeInfo` pointers as absolute immediates
/// (`emit_mov_imm64(ARG_REGS[1], info as *const _ as i64)`), so the bodies
/// differ in 0.1-1% of their bytes. Measured on CratonBench: 6 of 5193
/// bytes for `sieve`, 379 of 35686 for `Pattern.clazz`.
#[test]
fn classify_supersede_does_not_treat_equal_length_as_equal_code() {
    let before = [0x48, 0xb8, 0x00, 0x10, 0x20, 0x30];
    let after = [0x48, 0xb8, 0x00, 0x10, 0x99, 0x30];
    assert_eq!(before.len(), after.len());
    assert_eq!(
        classify_supersede(Some(&before), Some(&after)),
        SupersedeOutcome::Changed,
        "a relocated absolute immediate is a different body as far as byte \
         equality is concerned; treating equal length as equal code would \
         skip an invalidation that IS needed when the code really changed",
    );
}

/// The census must move, or a "no regression" reading cannot be told apart
/// from "the classifier never ran".
#[test]
fn supersede_census_counts_each_outcome() {
    let (f0, u0, c0) = supersede_census();
    note_supersede_outcome(SupersedeOutcome::FirstPublish);
    note_supersede_outcome(SupersedeOutcome::Unchanged);
    note_supersede_outcome(SupersedeOutcome::Changed);
    note_supersede_outcome(SupersedeOutcome::Changed);
    let (f1, u1, c1) = supersede_census();
    assert_eq!((f1 - f0, u1 - u0, c1 - c0), (1, 1, 2));
}
