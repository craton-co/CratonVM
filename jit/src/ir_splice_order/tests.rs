use super::*;

// pc 0: invokestatic #1; pc 3: areturn; pc 4: invokevirtual #2;
// pc 7: istore_1; pc 8: invokeinterface #3 n 0; pc 13: ireturn;
// pc 14: invokestatic #4; pc 17: pop; pc 18: return
const CODE: [u8; 19] = [
    0xb8, 0, 1, 0xb0, 0xb6, 0, 2, 0x3c, 0xb9, 0, 3, 1, 0, 0xac, 0xb8, 0, 4, 0x57, 0xb1,
];

#[test]
fn tail_exit_is_an_invoke_followed_by_a_return() {
    assert!(is_tail_exit_invoke(&CODE, CODE.len(), 0));
    assert!(!is_tail_exit_invoke(&CODE, CODE.len(), 4));
    assert!(
        is_tail_exit_invoke(&CODE, CODE.len(), 8),
        "5-byte invokeinterface"
    );
    assert!(
        !is_tail_exit_invoke(&CODE, CODE.len(), 14),
        "a pop sits between"
    );
    // Not an invoke, and out of range.
    assert!(!is_tail_exit_invoke(&CODE, CODE.len(), 3));
    assert!(!is_tail_exit_invoke(&CODE, CODE.len(), 40));
    // A spliced pc past the caller's own code never counts.
    assert!(!is_tail_exit_invoke(&CODE, 3, 0));
}

#[test]
fn order_is_a_stable_partition() {
    let ops = [
        (0usize, 1u16, 0xb8u8),
        (4, 2, 0xb6),
        (8, 3, 0xb9),
        (14, 4, 0xb8),
    ];
    let (order, moved) = tail_exits_last(&CODE, CODE.len(), &ops);
    assert_eq!(moved, 2);
    assert_eq!(
        order,
        vec![(4, 2, 0xb6), (14, 4, 0xb8), (0, 1, 0xb8), (8, 3, 0xb9)]
    );
}

// A decodeArrayLoop-shaped method:
//  0: invokestatic #1          pre-loop site A
//  3: iload_1                  loop header
//  4: ifge -> 25               loop exit
//  7: iload_2
//  8: ifne -> 15               profile: always taken
// 11: invokestatic #2          B: in-loop malformed exit (cold, tail exit)
// 14: areturn
// 15: invokestatic #3          C: the loop's hot helper
// 18: pop
// 19: iinc 1 -1
// 22: goto -> 3
// 25: invokestatic #4          D: post-loop site
// 28: invokestatic #5          E: tail exit
// 31: areturn
const LOOP: [u8; 32] = [
    0xb8, 0, 1, 0x1b, 0x9c, 0, 21, 0x1c, 0x9a, 0, 7, 0xb8, 0, 2, 0xb0, 0xb8, 0, 3, 0x57, 0x84,
    1, 0xff, 0xa7, 0xff, 0xed, 0xb8, 0, 4, 0xb8, 0, 5, 0xb0,
];
const LOOP_OPS: [(usize, u16, u8); 5] = [
    (0, 1, 0xb8),
    (11, 2, 0xb8),
    (15, 3, 0xb8),
    (25, 4, 0xb8),
    (28, 5, 0xb8),
];

fn pcs(order: &ProfilePlanOrder) -> Vec<usize> {
    order.sites.iter().map(|s| s.op.0).collect()
}

fn loop_profile(backedges: u64) -> MethodProfile {
    let mut p = MethodProfile::default();
    let b = p.branches.entry(4).or_default();
    b.taken = 1;
    b.not_taken = 500;
    let b = p.branches.entry(8).or_default();
    b.taken = 500;
    b.not_taken = 0;
    p.loops.entry(22).or_default().backedge_count = backedges;
    p
}

#[test]
fn profile_order_ranks_loop_then_straight_then_tail_then_cold() {
    let p = loop_profile(500);
    let order = profile_plan_order(&LOOP, LOOP.len(), &LOOP_OPS, Some(&p));
    assert!(order.profiled);
    assert_eq!(pcs(&order), vec![15, 0, 25, 28, 11]);
    let classes: Vec<SiteClass> = order.sites.iter().map(|s| s.class).collect();
    assert_eq!(
        classes,
        vec![
            SiteClass::InLoop,
            SiteClass::Straight,
            SiteClass::Straight,
            SiteClass::TailExit,
            SiteClass::Cold
        ]
    );
    assert_eq!(order.sites[0].depth, 1);
    assert!(order.hot_loop);
    assert!(order.takes_hot_loop_slot(15));
    assert!(!order.takes_hot_loop_slot(0));
    assert!(
        !order.takes_hot_loop_slot(11),
        "a cold in-loop site gets no extra slot"
    );
    assert_eq!(order.count_of(SiteClass::Cold), 1);
    assert_eq!(order.class_of(15), Some(SiteClass::InLoop));
    assert_eq!(order.class_of(11), Some(SiteClass::Cold));
    assert_eq!(order.class_of(28), Some(SiteClass::TailExit));
    assert_eq!(order.class_of(99), None);
    assert_eq!(order.describe(), "15:L 0:S 25:S 28:T 11:C");
    assert_eq!(order.ops()[0], (15, 3, 0xb8));
}

#[test]
fn without_a_profile_only_structure_ranks() {
    let order = profile_plan_order(&LOOP, LOOP.len(), &LOOP_OPS, None);
    assert!(!order.profiled);
    // B is reached now; as a tail exit it ranks with E, deeper first.
    assert_eq!(pcs(&order), vec![15, 0, 25, 11, 28]);
    // No evidence either way: the loop is presumed hot.
    assert!(order.hot_loop);
}

#[test]
fn thin_branch_evidence_keeps_both_directions() {
    let mut p = loop_profile(500);
    let b = p.branches.get_mut(&8).unwrap();
    b.taken = MIN_BRANCH_EVIDENCE - 1;
    let order = profile_plan_order(&LOOP, LOOP.len(), &LOOP_OPS, Some(&p));
    assert_eq!(order.sites.last().unwrap().op.0, 28);
    assert_eq!(order.count_of(SiteClass::Cold), 0);
}

#[test]
fn an_observed_call_count_overrides_the_branch_walk_and_breaks_ties() {
    let mut p = loop_profile(500);
    p.call_sites.insert(11, 3);
    p.call_sites.insert(25, 9);
    let order = profile_plan_order(&LOOP, LOOP.len(), &LOOP_OPS, Some(&p));
    // B is reached (it ran), D outranks A on its count.
    assert_eq!(pcs(&order), vec![15, 25, 0, 11, 28]);
    assert_eq!(order.sites[1].count, 9);
}

// Two nested loops (JIT round 20 wave 11, lane pSplice, L3W2-4):
//  0: invokestatic #1          A: outer loop (depth 1)
//  3: iload_1                  inner header
//  4: invokestatic #2          B: inner loop (depth 2)
//  7: pop
//  8: iinc 1 -1
// 11: iload_1
// 12: ifgt -> 3
// 15: goto -> 0
// 18: return
const NESTED: [u8; 19] = [
    0xb8, 0, 1, 0x1b, 0xb8, 0, 2, 0x57, 0x84, 1, 0xff, 0x1b, 0x9d, 0xff, 0xf7, 0xa7, 0xff,
    0xf1, 0xb1,
];
const NESTED_OPS: [(usize, u16, u8); 2] = [(0, 1, 0xb8), (4, 2, 0xb8)];


#[test]
fn a_cool_loop_keeps_the_flat_cap() {
    let mut p = loop_profile(10);
    // Branch totals inside the loop below the threshold too.
    p.branches.get_mut(&4).unwrap().not_taken = 20;
    p.branches.get_mut(&8).unwrap().taken = 20;
    let order = profile_plan_order(&LOOP, LOOP.len(), &LOOP_OPS, Some(&p));
    assert!(!order.hot_loop);
    assert!(!order.takes_hot_loop_slot(15));
}

#[test]
fn a_profile_of_another_body_is_ignored() {
    let mut p = loop_profile(500);
    p.note_code_len(LOOP.len() + 7);
    let order = profile_plan_order(&LOOP, LOOP.len(), &LOOP_OPS, Some(&p));
    assert!(!order.profiled);
    assert_eq!(pcs(&order), vec![15, 0, 25, 11, 28]);
    // The frame length (two sentinel bytes) is the same body.
    let mut p = loop_profile(500);
    p.note_code_len(LOOP.len() + 2);
    assert!(profile_plan_order(&LOOP, LOOP.len(), &LOOP_OPS, Some(&p)).profiled);
}

// A for-each loop:
//  0: invokevirtual #1        c.iterator()
//  3: astore_2
//  4: aload_2                 header
//  5: invokeinterface #2      it.hasNext()
// 10: ifeq -> 23
// 13: aload_2
// 14: invokeinterface #3      it.next()
// 19: pop
// 20: goto -> 4
// 23: return
const FOR_EACH: [u8; 24] = [
    0xb6, 0, 1, 0x4d, 0x2c, 0xb9, 0, 2, 1, 0, 0x99, 0, 13, 0x2c, 0xb9, 0, 3, 1, 0, 0x57, 0xa7,
    0xff, 0xf0, 0xb1,
];

#[test]
fn a_factory_producer_is_hoisted_in_front_of_its_loop_consumer() {
    let ops = [(0usize, 1u16, 0xb6u8), (5, 2, 0xb9), (14, 3, 0xb9)];
    let order = profile_plan_order(&FOR_EACH, FOR_EACH.len(), &ops, None);
    assert_eq!(pcs(&order), vec![0, 5, 14]);
    assert_eq!(order.sites[0].class, SiteClass::Straight);
    assert_eq!(order.sites[1].class, SiteClass::InLoop);
    // A different receiver local: no producer, plain rank order.
    let mut code = FOR_EACH;
    code[3] = 0x4e; // astore_3
    let order = profile_plan_order(&code, code.len(), &ops, None);
    assert_eq!(pcs(&order), vec![5, 14, 0]);
}



//  0: invokevirtual #1        producer, result -> local 2
//  3: astore_2
//  4: aload_1                 loop header; the REAL receiver is local 1
//  5: bipush 44               operand byte 0x2c reads as `aload_2`
//  7: invokeinterface #2 n 2
// 12: ifeq -> 18
// 15: goto -> 4
// 18: return
const BIPUSH_ARG: [u8; 19] = [
    0xb6, 0, 1, 0x4d, 0x2b, 0x10, 0x2c, 0xb9, 0, 2, 2, 0, 0x99, 0, 6, 0xa7, 0xff, 0xf5, 0xb1,
];