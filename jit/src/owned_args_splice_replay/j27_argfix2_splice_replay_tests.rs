use super::*;

/// `static int outer(Object o) { return mid(o); }`:
/// `0 aload_0  1 invokestatic #1  4 ireturn`.
const PURE: [u8; 5] = [0x2a, 0xb8, 0x00, 0x01, 0xac];
/// `{ S.f = 1; return mid(o); }`:
/// `0 iconst_1  1 putstatic #2  4 aload_0  5 invokestatic #1  8 ireturn`.
const COMMITS: [u8; 9] = [0x04, 0xb3, 0x00, 0x02, 0x2a, 0xb8, 0x00, 0x01, 0xac];
/// `{ synchronized (o) { ... } }` prefix: `0 aload_0  1 monitorenter
///  2 aload_0  3 invokestatic #1  6 ireturn`.
const LOCKS: [u8; 7] = [0x2a, 0xc2, 0x2a, 0xb8, 0x00, 0x01, 0xac];
const MID: &str = "P.mid:(Ljava/lang/Object;)I";
const LEAF: &str = "P.leaf:(Ljava/lang/Object;)I";

fn body(key: &str, code: &[u8], handlers: Vec<(usize, usize, usize)>) -> SplicedBodyCode {
    SplicedBodyCode {
        method_key: key.to_string(),
        code: code.to_vec(),
        num_params: 1,
        num_locals: 1,
        handlers,
    }
}

/// `mid` spliced at base 100, its call to `leaf` at body bci `bci`.
fn run(
    outer: &[u8],
    outer_handlers: &[(usize, usize, usize)],
    mid: Option<SplicedBodyCode>,
    bci: usize,
    osr: bool,
    locks: bool,
) -> Result<u8, ChainRefusal> {
    let len = mid.as_ref().map_or(PURE.len(), |b| b.code.len());
    let outer_bci = outer.iter().position(|&op| op == 0xb8).unwrap();
    let enclosing = [(100usize, len, outer_bci, MID)];
    let bodies: Vec<SplicedBodyCode> = mid.into_iter().collect();
    spliced_call_replay(100 + bci, &enclosing, outer, outer_handlers, &bodies, osr, locks)
}

#[test]
fn j27_argfix2_two_level_pure_chain_is_admitted() {
    let mid = body(MID, &PURE, Vec::new());
    assert_eq!(run(&PURE, &[], Some(mid), 1, false, false), Ok(0xb8));
}

#[test]
fn j27_argfix2_three_level_pure_chain_is_admitted_and_orders_outer_first() {
    // outer@1 -> mid at 100 (its call at 101) -> leaf at 200 (call at 201).
    let bodies = [body(MID, &PURE, Vec::new()), body(LEAF, &PURE, Vec::new())];
    let enclosing = [(200usize, 5usize, 101usize, LEAF), (100, 5, 1, MID)];
    let levels = replay_levels(201, &enclosing, &PURE, &[], &bodies).unwrap();
    assert_eq!(levels.iter().map(|l| l.parked_bci).collect::<Vec<_>>(), vec![1, 1, 1]);
    assert_eq!(chain_replay_refusal(&levels, false, false), None);
    assert_eq!(
        spliced_call_replay(201, &enclosing, &PURE, &[], &bodies, false, false),
        Ok(0xb8)
    );
}

#[test]
fn j27_argfix2_commit_at_outer_level_refuses() {
    let mid = body(MID, &PURE, Vec::new());
    assert_eq!(run(&COMMITS, &[], Some(mid), 1, false, false), Err(ChainRefusal::Commit(0)));
}

#[test]
fn j27_argfix2_commit_at_inner_level_refuses() {
    let mid = body(MID, &COMMITS, Vec::new());
    assert_eq!(run(&PURE, &[], Some(mid), 5, false, false), Err(ChainRefusal::Commit(1)));
}

#[test]
fn j27_argfix2_handler_at_either_level_refuses() {
    let mid = body(MID, &PURE, Vec::new());
    assert_eq!(
        run(&PURE, &[(0, 4, 4)], Some(mid), 1, false, false),
        Err(ChainRefusal::Handler(0))
    );
    let guarded = body(MID, &PURE, vec![(1, 4, 4)]);
    assert_eq!(run(&PURE, &[], Some(guarded), 1, false, false), Err(ChainRefusal::Handler(1)));
}

#[test]
fn j27_argfix2_lock_refuses() {
    let mid = body(MID, &PURE, Vec::new());
    assert_eq!(run(&PURE, &[], Some(mid), 1, false, true), Err(ChainRefusal::Lock));
    // An explicit monitorenter on the way is a commit at its level.
    let locking = body(MID, &LOCKS, Vec::new());
    assert_eq!(run(&PURE, &[], Some(locking), 3, false, false), Err(ChainRefusal::Commit(1)));
}

#[test]
fn j27_argfix2_osr_refuses() {
    let mid = body(MID, &PURE, Vec::new());
    assert_eq!(run(&PURE, &[], Some(mid), 1, true, false), Err(ChainRefusal::Osr));
}

#[test]
fn j27_argfix2_missing_unmasked_copy_refuses() {
    assert_eq!(run(&PURE, &[], None, 1, false, false), Err(ChainRefusal::NoUnmaskedCopy(1)));
    // A copy whose length disagrees with the spliced extent is no copy.
    let short = body(MID, &PURE[..4], Vec::new());
    let enclosing = [(100usize, 5usize, 1usize, MID)];
    assert_eq!(
        spliced_call_replay(101, &enclosing, &PURE, &[], &[short], false, false),
        Err(ChainRefusal::NoUnmaskedCopy(1))
    );
}

#[test]
fn j27_argfix2_malformed_chain_refuses() {
    let bodies = [body(MID, &PURE, Vec::new())];
    // pc outside the innermost body; outer caller pc past the outer code;
    // no splice at all.
    for (pc, enclosing) in [
        (99usize, vec![(100usize, 5usize, 1usize, MID)]),
        (101, vec![(100, 5, 40, MID)]),
        (101, Vec::new()),
    ] {
        assert_eq!(
            spliced_call_replay(pc, &enclosing, &PURE, &[], &bodies, false, false),
            Err(ChainRefusal::NoChain)
        );
    }
    // The call byte itself must be an invoke in the unmasked copy.
    let enclosing = [(100usize, 5usize, 1usize, MID)];
    assert_eq!(
        spliced_call_replay(100, &enclosing, &PURE, &[], &bodies, false, false),
        Err(ChainRefusal::NotParkedAtInvoke(1))
    );
}
