use super::*;

/// A `switch` in the loop body contributes "everything from here to the end
/// of the body may be entered". That is a claim about a RANGE, and it used
/// to be materialised one pc at a time into a `Vec` that every `contains`
/// in the per-pc walk then scanned linearly — so a 4 KB body with one
/// switch produced a ~4000-element vector and made the walk O(body²), per
/// loop, times the number of loops.
///
/// The answers must be identical; only the representation changed.
#[test]
fn a_switch_covers_its_span_without_materialising_every_pc() {
    // 0: nop
    // 1: tableswitch — two padding bytes take the payload to offset 4,
    //    then default / low / high / one jump offset, four bytes each. The
    //    instruction spans [1, 20) and a trailing nop sits at 20.
    let mut code = vec![0x00, 0xaa, 0x00, 0x00];
    code.extend_from_slice(&0i32.to_be_bytes()); // default offset
    code.extend_from_slice(&0i32.to_be_bytes()); // low
    code.extend_from_slice(&0i32.to_be_bytes()); // high
    code.extend_from_slice(&0i32.to_be_bytes()); // one jump offset
    code.push(0x00); // trailing nop
    let end = code.len();

    let targets = branch_target_pcs(&code, 0, end);
    assert!(
        targets.covered.iter().any(|&(lo, hi)| lo == 1 && hi == end),
        "the switch must cover [1, {end}), got {:?}",
        targets.covered
    );
    assert!(
        targets.pcs.is_empty(),
        "and must not have materialised the span as individual targets"
    );
    // The observable contract, unchanged: everything from the switch on is
    // treated as enterable, and everything before it is not.
    assert!(!targets.contains(0));
    for pc in 1..end {
        assert!(targets.contains(pc), "pc {pc} is inside the covered span");
    }
}

/// The exact targets still land in the set, and nothing else does.
#[test]
fn an_ordinary_branch_records_exactly_its_target() {
    // 0: nop ; 1: goto +4 -> 5 ; 4: nop ; 5: nop
    let code = vec![0x00, 0xa7, 0x00, 0x04, 0x00, 0x00];
    let targets = branch_target_pcs(&code, 0, code.len());
    assert!(targets.contains(5));
    assert!(!targets.contains(0));
    assert!(!targets.contains(4));
    assert!(targets.covered.is_empty(), "no switch, no covered span");
}
