use super::*;
use crate::ir::{SafepointSnapshot, SpliceScopeChain, SpliceScopeFrame};

/// The caller, `static int run(Object o, int n) { return f(o, n); }`:
/// `0 aload_0  1 iload_1  2 invokestatic #1  5 ireturn` -- nothing reads a
/// local after the invoke.
const RUN: [u8; 6] = [0x2a, 0x1b, 0xb8, 0x00, 0x01, 0xac];
/// The callee, `static int f(Object a, int n) { int k = 0; while (k < n)
/// k++; return k; }`: `a` (slot 0) is never read.
/// `0 iconst_0  1 istore_2  2 iload_2  3 iload_1  4 if_icmpge 13
///  7 iinc 2, 1  10 goto 2  13 iload_2  14 ireturn`
const F: [u8; 15] = [
    0x03, 0x3d, 0x1c, 0x1b, 0xa2, 0x00, 0x09, 0x84, 0x02, 0x01, 0xa7, 0xff, 0xf8, 0x1c, 0xac,
];
const F_KEY: &str = "T.f:(Ljava/lang/Object;I)I";

/// A chain snapshot at the callee's loop header (its bci 2), parked in
/// the caller's invoke at bci 2: callee locals `[a, n, k]`, the caller's
/// `[o, n]` riding in `stack`.
fn snapshot(callee_max_locals: u32) -> SafepointSnapshot {
    let mut sp = SafepointSnapshot::new(40, vec![10, 11, 12], vec![20, 21], Vec::new());
    sp.splice_scope = Some(std::sync::Arc::new(SpliceScopeChain {
        scopes: vec![
            SpliceScopeFrame {
                class_id: 0,
                method_key: F_KEY.to_string(),
                bci: 2,
                code_len: F.len() as u32,
                max_locals: callee_max_locals,
                locals_len: 0,
                stack_len: 0,
            },
            SpliceScopeFrame {
                class_id: 0,
                method_key: String::new(),
                bci: 2,
                code_len: 0,
                max_locals: 0,
                locals_len: 2,
                stack_len: 0,
            },
        ],
    }));
    sp
}

fn live(code: &[u8], params: usize, locals: usize) -> (Vec<u64>, Vec<bool>, usize) {
    crate::regalloc::live_locals_per_pc_all(code, code.len(), params, &[], &[], locals)
}

#[test]
fn t1_argfix_each_scope_drops_what_its_own_liveness_calls_dead() {
    let f_live = live(&F, 2, 3);
    let run_live = live(&RUN, 2, 2);
    let mut bodies = HashMap::new();
    bodies.insert(F_KEY, (&f_live, 3usize));
    let (locals, stack) =
        splice_snapshot_dead_slots(&snapshot(3), &bodies, Some((&run_live, 2)))
            .expect("a chain with answerable scopes");
    assert_eq!(locals, vec![true, false, false], "callee: `a` dead, `n` and `k` live");
    assert_eq!(stack, vec![true, true], "caller: nothing is read after the invoke");
    // Without the caller's liveness only the callee scope narrows.
    let (locals, stack) =
        splice_snapshot_dead_slots(&snapshot(3), &bodies, None).expect("the callee answers");
    assert_eq!(locals, vec![true, false, false]);
    assert_eq!(stack, vec![false, false]);
}

#[test]
fn t1_argfix_an_unanswerable_scope_or_a_flat_frame_narrows_nothing() {
    let f_live = live(&F, 2, 3);
    let mut bodies = HashMap::new();
    bodies.insert(F_KEY, (&f_live, 3usize));
    // `max_locals` disagrees with the body: that scope is not answered.
    assert!(splice_snapshot_dead_slots(&snapshot(4), &bodies, None).is_none());
    // No body for the key.
    assert!(splice_snapshot_dead_slots(&snapshot(3), &HashMap::new(), None).is_none());
    // A flat (own) frame is not this rule's.
    let flat = SafepointSnapshot::new(2, vec![10, 11], Vec::new(), Vec::new());
    assert!(splice_snapshot_dead_slots(&flat, &bodies, None).is_none());
    // A malformed chain (lengths do not add up) is refused, not guessed.
    let mut bad = snapshot(3);
    bad.stack.push(22);
    assert!(splice_snapshot_dead_slots(&bad, &bodies, None).is_none());
}

