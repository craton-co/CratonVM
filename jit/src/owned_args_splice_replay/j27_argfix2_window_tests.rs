use super::*;

fn verified(code: &[u8]) -> std::sync::Arc<cratonvm_reader::VerifiedCode> {
    cratonvm_reader::verified_code(code).expect("test bytecode verifies")
}

/// `mid`: `0 aload_0  1 iload_1  2 invokestatic #1  5 iconst_1  6 iadd
/// 7 ireturn` -- after the call only `5..8` runs.
#[test]
fn j27_argfix2_reach_after_invoke_is_the_tail_of_the_level() {
    let code = [0x2a, 0x1b, 0xb8, 0x00, 0x01, 0x04, 0x60, 0xac];
    let v = verified(&code);
    let reach = reach_after_invoke(2, |p| v.successors(p), &[]).unwrap();
    let mut got: Vec<usize> = reach.into_iter().collect();
    got.sort_unstable();
    assert_eq!(got, vec![5, 6, 7]);
}

/// `0 aload_0  1 invokestatic #1  4 goto 0` -- the call is on a cycle.
#[test]
fn j27_argfix2_reach_after_invoke_refuses_a_cycle_and_follows_handlers() {
    let looping = [0x2a, 0xb8, 0x00, 0x01, 0xa7, 0xff, 0xfc];
    let v = verified(&looping);
    assert!(reach_after_invoke(1, |p| v.successors(p), &[]).is_none());
    let (tail, call_cycle) =
        reach_after_invoke_with_cycle(1, |p| v.successors(p), &[]).unwrap();
    assert!(call_cycle);
    assert_eq!(tail, std::collections::HashSet::from([0, 4]));
    // A handler row covering the tail and pointing back at the invoke
    // closes the same cycle over an exception edge.
    let edges = |p: usize| if p == 1 { vec![4] } else { Vec::new() };
    assert!(reach_after_invoke(1, edges, &[(4, 5, 1)]).is_none());
    let (tail, call_cycle) =
        reach_after_invoke_with_cycle(1, edges, &[(4, 5, 1)]).unwrap();
    assert!(call_cycle);
    assert_eq!(tail, std::collections::HashSet::from([4]));
    assert_eq!(reach_after_invoke(1, edges, &[(4, 5, 6)]).map(|r| r.len()), Some(2));
}

#[test]
fn j27_argfix2_seen_blocks_acyclic() {
    let succ: Vec<Vec<usize>> = vec![vec![1, 2], vec![3], vec![3], vec![]];
    let at = |b: usize| succ[b].as_slice();
    assert!(seen_blocks_acyclic(&[false, true, true, true], at));
    let looped: Vec<Vec<usize>> = vec![vec![1], vec![2], vec![1, 3], vec![]];
    let at = |b: usize| looped[b].as_slice();
    assert!(!seen_blocks_acyclic(&[false, true, true, true], at));
    // A cycle through an unseen block is not in the subgraph.
    assert!(seen_blocks_acyclic(&[false, false, true, true], at));
}

#[test]
fn j27_argfix2_frame_names_value_narrows_only_dead_slots() {
    let none = |_: usize| false;
    // On the stack of the call's own state: named.
    assert!(frame_names_value(&[], &[7], &[], 7, (&[], &[]), none));
    // A caller scope's local riding in the stack, dead there: not named.
    assert!(!frame_names_value(&[], &[7], &[], 7, (&[], &[true]), none));
    // A scope-0 local dead by the body's liveness: not named.
    assert!(!frame_names_value(&[7, 8], &[], &[], 7, (&[true, false], &[]), none));
    assert!(frame_names_value(&[7, 8], &[], &[], 8, (&[true, false], &[]), none));
    // The own-instruction liveness, as the single-level window had it.
    assert!(!frame_names_value(&[7], &[], &[], 7, (&[], &[]), |k| k == 0));
    // A monitor is always read.
    assert!(frame_names_value(&[], &[], &[7], 7, (&[], &[true]), none));
    assert!(!frame_names_value(&[1], &[2], &[3], 7, (&[], &[]), none));
}

#[test]
fn j27_argfix2_chain_state_reached_through_an_ancestor() {
    let reach: std::collections::HashSet<usize> = [105, 106, 107].into_iter().collect();
    assert!(chain_state_reached(106, &[2], &reach), "the body's own tail");
    assert!(chain_state_reached(203, &[106, 2], &reach), "a body spliced after the call");
    assert!(!chain_state_reached(102, &[2], &reach), "the call's own state");
    assert!(!chain_state_reached(100, &[2], &reach), "a state before the call");
}

#[test]
fn j27_argfix2_levels_carry_their_combined_base() {
    let mid = SplicedBodyCode {
        method_key: "P.mid:(Ljava/lang/Object;)I".to_string(),
        code: vec![0x2a, 0xb8, 0x00, 0x01, 0xac],
        num_params: 1,
        num_locals: 1,
        handlers: Vec::new(),
    };
    let outer = [0x2a, 0xb8, 0x00, 0x01, 0xac];
    let enclosing = [(100usize, 5usize, 1usize, "P.mid:(Ljava/lang/Object;)I")];
    let levels = replay_levels(101, &enclosing, &outer, &[], std::slice::from_ref(&mid)).unwrap();
    assert_eq!(levels.iter().map(|l| l.base).collect::<Vec<_>>(), vec![0, 100]);
}
