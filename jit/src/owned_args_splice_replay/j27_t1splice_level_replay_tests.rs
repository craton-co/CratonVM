use super::*;

/// `0 aload_0  1 invokestatic #1  4 ireturn` (call at 1).
const PURE: [u8; 5] = [0x2a, 0xb8, 0x00, 0x01, 0xac];
/// `0 iconst_1  1 putstatic #2  4 aload_0  5 invokestatic #1  8 ireturn`.
const COMMITS: [u8; 9] = [0x04, 0xb3, 0x00, 0x02, 0x2a, 0xb8, 0x00, 0x01, 0xac];
/// `0 aload_0  1 astore_0  2 aload_0  3 invokestatic #1  6 ireturn`.
const WRITES_PARAM: [u8; 7] = [0x2a, 0x4b, 0x2a, 0xb8, 0x00, 0x01, 0xac];
/// `0 aload_0  1 wide astore 0  5 aload_0  6 invokestatic #1  9 ireturn`.
const WIDE_WRITE: [u8; 10] = [0x2a, 0xc4, 0x3a, 0x00, 0x00, 0x2a, 0xb8, 0x00, 0x01, 0xac];
/// `0 iinc 0 1  3 iload_0  4 invokestatic #1  7 ireturn`.
const IINC: [u8; 8] = [0x84, 0x00, 0x01, 0x1a, 0xb8, 0x00, 0x01, 0xac];
/// `0 aload_0  1 astore_1  2 aload_1  3 invokestatic #1  6 ireturn`.
const WRITES_LOCAL: [u8; 7] = [0x2a, 0x4c, 0x2b, 0xb8, 0x00, 0x01, 0xac];
/// `0 aload_0  1 invokestatic #1  4 pop  5 aload_0  6 astore_0  7 goto 0`.
const LOOP: [u8; 10] = [0x2a, 0xb8, 0x00, 0x01, 0x57, 0x2a, 0x4b, 0xa7, 0xff, 0xf9];
const MID: &str = "P.mid:(Ljava/lang/Object;)I";
const LEAF: &str = "P.leaf:(Ljava/lang/Object;)I";

fn body(key: &str, code: &[u8], handlers: Vec<(usize, usize, usize)>) -> SplicedBodyCode {
    SplicedBodyCode {
        method_key: key.to_string(),
        code: code.to_vec(),
        num_params: 1,
        num_locals: 2,
        handlers,
    }
}

fn call_bci(code: &[u8]) -> usize {
    code.iter().position(|&op| op == 0xb8).unwrap()
}

/// `outer -> mid` (base 100), the call in `mid`.
fn two(outer: &[u8], mid: SplicedBodyCode, osr: bool) -> Result<usize, ChainRefusal> {
    let enclosing = [(100usize, mid.code.len(), call_bci(outer), MID)];
    let pc = 100 + call_bci(&mid.code);
    spliced_call_level_replay(pc, &enclosing, outer, &[], &[mid], osr, false)
}

/// `outer -> mid` (base 100) `-> leaf` (base 200), the call in `leaf`.
fn three(outer: &[u8], mid: &[u8], leaf: &[u8], osr: bool) -> Result<usize, ChainRefusal> {
    let bodies = [body(MID, mid, Vec::new()), body(LEAF, leaf, Vec::new())];
    let enclosing = [
        (200usize, leaf.len(), 100 + call_bci(mid), LEAF),
        (100, mid.len(), call_bci(outer), MID),
    ];
    let pc = 200 + call_bci(leaf);
    spliced_call_level_replay(pc, &enclosing, outer, &[], &bodies, osr, false)
}

#[test]
fn j27_t1splice_osr_pure_chain_resumes_at_the_spliced_level() {
    assert_eq!(two(&PURE, body(MID, &PURE, Vec::new()), true), Ok(1));
    // The physical re-run refuses the same site.
    let mid = body(MID, &PURE, Vec::new());
    let enclosing = [(100usize, 5usize, 1usize, MID)];
    assert_eq!(
        spliced_call_replay(101, &enclosing, &PURE, &[], &[mid], true, false),
        Err(ChainRefusal::Osr)
    );
}

#[test]
fn j27_t1splice_an_exact_chain_is_the_physical_rerun() {
    assert_eq!(two(&PURE, body(MID, &PURE, Vec::new()), false), Ok(0));
    // A written parameter does not matter to a re-run of the whole frame.
    assert_eq!(two(&PURE, body(MID, &WRITES_PARAM, Vec::new()), false), Ok(0));
}

#[test]
fn j27_t1splice_an_outer_commit_moves_the_plan_to_level_1() {
    assert_eq!(two(&COMMITS, body(MID, &PURE, Vec::new()), false), Ok(1));
}

#[test]
fn j27_t1splice_a_written_parameter_slot_refuses_its_level() {
    for code in [&WRITES_PARAM[..], &WIDE_WRITE[..], &IINC[..]] {
        assert_eq!(
            two(&PURE, body(MID, code, Vec::new()), true),
            Err(ChainRefusal::ParamWritten(1)),
            "{code:02x?}"
        );
    }
    // A store into a non-parameter local keeps the entry state.
    assert_eq!(two(&PURE, body(MID, &WRITES_LOCAL, Vec::new()), true), Ok(1));
}

#[test]
fn j27_t1splice_three_levels_pick_the_outermost_recoverable_level() {
    assert_eq!(three(&COMMITS, &WRITES_PARAM, &PURE, true), Ok(2));
    assert_eq!(three(&COMMITS, &PURE, &PURE, true), Ok(1));
    // A committing middle level is resumed, not re-run.
    assert_eq!(three(&PURE, &COMMITS, &PURE, true), Ok(2));
    // The innermost level is re-run in every plan.
    assert_eq!(three(&PURE, &PURE, &COMMITS, true), Err(ChainRefusal::Commit(2)));
}

#[test]
fn j27_t1splice_handler_lock_and_unspliced_sites_refuse() {
    assert_eq!(
        two(&PURE, body(MID, &PURE, vec![(1, 4, 4)]), true),
        Err(ChainRefusal::Handler(1))
    );
    let mid = body(MID, &PURE, Vec::new());
    let enclosing = [(100usize, 5usize, 1usize, MID)];
    assert_eq!(
        spliced_call_level_replay(101, &enclosing, &PURE, &[], &[mid], true, true),
        Err(ChainRefusal::Lock)
    );
    assert_eq!(spliced_call_level_replay(1, &[], &PURE, &[], &[], true, false), Err(ChainRefusal::Osr));
    assert_eq!(
        spliced_call_level_replay(1, &[], &PURE, &[], &[], false, false),
        Err(ChainRefusal::NoChain)
    );
}

#[test]
fn j27_t1splice_param_slots_are_checked_over_the_loop_closed_extent() {
    // Textually the prefix `0..1` writes nothing; the loop brings the
    // `astore_0` at 6 back before the call.
    assert!(!prefix_keeps_param_slots(&LOOP, 1, 1, &[]));
    assert!(prefix_keeps_param_slots(&WRITES_PARAM, 3, 0, &[]));
    assert!(!prefix_keeps_param_slots(&WRITES_PARAM, 3, 1, &[]));
    assert!(prefix_keeps_param_slots(&WRITES_LOCAL, 3, 1, &[]));
    // A parked pc past the code is no answer.
    assert!(!prefix_keeps_param_slots(&PURE, 9, 1, &[]));
}
