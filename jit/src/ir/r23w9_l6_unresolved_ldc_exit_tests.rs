use super::*;

/// `if (len == 0) return ""; return new String(Arrays.copyOfRange(val,
/// index, index + len), LATIN1);`
const NEW_STRING: [u8; 24] = [
    0x1c, // 0: iload_2
    0x9a, 0x00, 0x06, // 1: ifne -> 7
    0x12, 0x94, // 4: ldc #148 ("")
    0xb0, // 6: areturn
    0xbb, 0x00, 0x01, // 7: new #1 (String)
    0x59, // 10: dup
    0x2a, // 11: aload_0
    0x1b, // 12: iload_1
    0x1b, // 13: iload_1
    0x1c, // 14: iload_2
    0x60, // 15: iadd
    0xb8, 0x00, 0xe8, // 16: invokestatic Arrays.copyOfRange
    0x03, // 19: iconst_0
    0xb7, 0x00, 0x6d, // 20: invokespecial String.<init>([BB)V
    0xb0, // 23: areturn
];

fn profile(taken: u32, not_taken: u32) -> crate::profile::MethodProfile {
    let mut p = crate::profile::MethodProfile::default();
    p.branches
        .insert(1, crate::profile::BranchCounts { taken, not_taken });
    p
}

#[test]
fn the_empty_string_ldc_is_cold_when_no_empty_part_was_ever_made() {
    assert!(splice_typecheck_arm_unexecuted(&NEW_STRING, 24, 24, 4, &profile(64, 0)));
}

#[test]
fn a_split_that_made_an_empty_part_keeps_the_ldc_warm() {
    assert!(!splice_typecheck_arm_unexecuted(&NEW_STRING, 24, 24, 4, &profile(64, 1)));
    assert!(
        !splice_typecheck_arm_unexecuted(&NEW_STRING, 24, 24, 4, &profile(3, 0)),
        "too few samples prove nothing"
    );
}

