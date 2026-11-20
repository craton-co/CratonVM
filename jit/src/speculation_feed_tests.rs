use super::speculatable_forward_branch;

/// `ifeq` at pc 0 with a +8 offset: a plain forward `if`, the shape the
/// whole speculation feature exists for.
#[test]
fn a_forward_conditional_branch_is_offered_to_the_builder() {
    // 0x99 = ifeq, offset +8 → target 8.
    let code = [0x99, 0x00, 0x08, 0, 0, 0, 0, 0, 0xb1, 0, 0];
    assert!(speculatable_forward_branch(&code, 9, 0));
}

/// The defect this filter exists for: a bottom-tested loop's back edge is
/// `not_taken == 0` in any profile taken while the loop is still running,
/// so it passes the counter test and must be rejected on its shape.
#[test]
fn a_backward_branch_is_never_offered_however_hot_its_taken_arm_is() {
    // `if_icmplt` at pc 6 with offset -6 → target 0, a back edge.
    let code = [0, 0, 0, 0, 0, 0, 0xa1, 0xff, 0xfa, 0xb1, 0, 0];
    assert!(!speculatable_forward_branch(&code, 10, 6));
}

/// A branch to itself is not forward either. `prune_always_taken_branch`
/// declines `target <= pc`, and the feed says the same thing.
#[test]
fn a_self_branch_is_not_a_forward_branch() {
    let code = [0x99, 0x00, 0x00, 0xb1, 0, 0];
    assert!(!speculatable_forward_branch(&code, 4, 0));
}

/// A target past the end of the compiling method cannot be walked. A
/// stale profile keyed to a since-redefined method is how that arrives.
#[test]
fn a_target_outside_the_method_is_not_offered() {
    // offset +64, but `code_len` is 4.
    let code = [0x99, 0x00, 0x40, 0xb1, 0, 0];
    assert!(!speculatable_forward_branch(&code, 4, 0));
}

/// A profile pc that does not name an offset branch at all — the other
/// way a stale profile lies — is filtered rather than fed through.
#[test]
fn a_pc_that_is_not_a_branch_is_not_offered() {
    // 0x60 = iadd.
    let code = [0x60, 0x60, 0x60, 0xb1, 0, 0];
    assert!(!speculatable_forward_branch(&code, 4, 0));
}

/// `goto_w` / `ifnull` are branches too, and the 32-bit form's offset is
/// decoded by the same helper the CFG builder uses.
#[test]
fn the_wide_and_reference_branch_forms_are_decoded_the_same_way() {
    // 0xc6 = ifnull, +5.
    let code = [0xc6, 0x00, 0x05, 0, 0, 0xb1, 0, 0];
    assert!(speculatable_forward_branch(&code, 6, 0));
    // 0xc8 = goto_w, -4 → backward.
    let wide = [0, 0, 0, 0, 0xc8, 0xff, 0xff, 0xff, 0xfc, 0xb1, 0, 0];
    assert!(!speculatable_forward_branch(&wide, 10, 4));
}

/// `static int f(int n) { int i = 0; while (i < n) { if (i == 0) i++;
/// i++; } return i; }`: header 2, exit test at 4 (-> 20), in-loop
/// `ifne` at 8 (-> 14), back edge `goto` at 17.
const NIO21W17_LOOP: [u8; 24] = [
    0x03, 0x3c, // 0: iconst_0; 1: istore_1
    0x1b, 0x1a, // 2: iload_1 (header); 3: iload_0
    0xa2, 0x00, 0x10, // 4: if_icmpge +16 -> 20 (the loop EXIT)
    0x1b, // 7: iload_1
    0x9a, 0x00, 0x06, // 8: ifne +6 -> 14 (inside the loop)
    0x84, 0x01, 0x01, // 11: iinc 1 1
    0x84, 0x01, 0x01, // 14: iinc 1 1
    0xa7, 0xff, 0xf1, // 17: goto -15 -> 2
    0x1b, 0xac, // 20: iload_1; 21: ireturn
    0, 0,
];

/// nio21 wave 17 (lane L4): a loop EXIT is never a never-taken candidate
/// (a profile taken mid-loop has not seen it), an in-loop forward branch
/// is, and an unconditional `goto` or a non-branch never is.
#[test]
fn nio21w17_never_taken_candidates_exclude_loop_exits_and_gotos() {
    use super::never_taken_prune_candidate;
    assert!(
        !never_taken_prune_candidate(&NIO21W17_LOOP, 22, 4),
        "the loop exit"
    );
    assert!(
        never_taken_prune_candidate(&NIO21W17_LOOP, 22, 8),
        "in-loop branch"
    );
    assert!(
        !never_taken_prune_candidate(&NIO21W17_LOOP, 22, 17),
        "a goto"
    );
    assert!(
        !never_taken_prune_candidate(&NIO21W17_LOOP, 22, 0),
        "not a branch"
    );
}

/// nio21 wave 17 (lane L4): the feed offers a pc whose taken edge was
/// never seen over the sample, and not one taken once, one under the
/// sample floor, or one this VM's de-spec registry withdrew.
#[test]
fn nio21w17_the_never_taken_feed_reads_the_profile_and_the_despec_registry() {
    use super::{ir, ir_never_taken_branches, profile};
    let mut prof = profile::MethodProfile::default();
    let floor = ir::MIN_OBSERVATIONS_TO_PRUNE;
    prof.branches.insert(
        8,
        profile::BranchCounts {
            taken: 0,
            not_taken: floor,
        },
    );
    let offered = ir_never_taken_branches(Some(&prof), &NIO21W17_LOOP, 22, None, "K.f:(I)I");
    assert!(offered.contains(&8));
    // The loop exit is never offered, whatever the counts.
    prof.branches.insert(
        4,
        profile::BranchCounts {
            taken: 0,
            not_taken: floor * 4,
        },
    );
    let offered = ir_never_taken_branches(Some(&prof), &NIO21W17_LOOP, 22, None, "K.f:(I)I");
    assert!(!offered.contains(&4));
    // De-specialised at pc 8: withdrawn.
    let reg = super::deopt::DespecRegistry::new();
    reg.insert("K.f:(I)I", 8, super::deopt::SPECULATION_ID_ANY);
    let offered =
        ir_never_taken_branches(Some(&prof), &NIO21W17_LOOP, 22, Some(&reg), "K.f:(I)I");
    assert!(!offered.contains(&8));
    // Taken once, or under the floor: not offered.
    prof.branches.insert(
        8,
        profile::BranchCounts {
            taken: 1,
            not_taken: floor * 4,
        },
    );
    assert!(
        ir_never_taken_branches(Some(&prof), &NIO21W17_LOOP, 22, None, "K.f:(I)I").is_empty()
    );
    prof.branches.insert(
        8,
        profile::BranchCounts {
            taken: 0,
            not_taken: floor - 1,
        },
    );
    assert!(
        ir_never_taken_branches(Some(&prof), &NIO21W17_LOOP, 22, None, "K.f:(I)I").is_empty()
    );
    assert!(ir_never_taken_branches(None, &NIO21W17_LOOP, 22, None, "K.f:(I)I").is_empty());
}

/// nio21 wave 17 (lane L4): `int f(int k) { int r; if (k <= 100) r = k;
/// else { r = 0; while (r < k) r++; } return r; }` -- the never-taken
/// `if_icmpgt` at 3 is the only way into the loop at 13, so it is not
/// offered (the OSR door may enter a body at any header), though it is a
/// candidate by shape.
#[test]
fn nio21w17_a_branch_that_is_the_only_way_into_a_loop_is_not_offered() {
    use super::{ir, ir_never_taken_branches, never_taken_prune_candidate, profile};
    const COLD_LOOP: [u8; 28] = [
        0x1a, 0x10, 0x64, // 0: iload_0; 1: bipush 100
        0xa3, 0x00, 0x08, // 3: if_icmpgt +8 -> 11 (never taken)
        0x1a, 0x3c, // 6: iload_0; 7: istore_1
        0xa7, 0x00, 0x10, // 8: goto +16 -> 24
        0x03, 0x3c, // 11: iconst_0; 12: istore_1
        0x1b, 0x1a, // 13: iload_1 (header); 14: iload_0
        0xa2, 0x00, 0x09, // 15: if_icmpge +9 -> 24
        0x84, 0x01, 0x01, // 18: iinc 1 1
        0xa7, 0xff, 0xf8, // 21: goto -8 -> 13
        0x1b, 0xac, // 24: iload_1; 25: ireturn
        0, 0,
    ];
    assert!(never_taken_prune_candidate(&COLD_LOOP, 26, 3));
    let mut prof = profile::MethodProfile::default();
    prof.branches.insert(
        3,
        profile::BranchCounts {
            taken: 0,
            not_taken: ir::MIN_OBSERVATIONS_TO_PRUNE,
        },
    );
    assert!(ir_never_taken_branches(Some(&prof), &COLD_LOOP, 26, None, "K.g:(I)I").is_empty());
}
