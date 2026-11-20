use super::*;

/// `bytecode_analysis::step` never answers 0, which is what makes every
/// `if l == 0` guard in this file unreachable.
///
/// Pinned rather than asserted in prose: the guards read as live defences,
/// and their comments now say they are not. If `step` ever grows a zero
/// answer this fails, and the two comments — plus the desync page they point
/// at — have to be revisited together.
///
/// Every opcode byte is covered, at a pc with and without room for operands,
/// because the zero would come from an operand read rather than from the
/// opcode table.
#[test]
fn the_linear_decode_step_never_answers_zero() {
    for op in 0u16..=255 {
        let op = op as u8;
        // Plenty of operand room, then none at all.
        let roomy = [op, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let bare = [op];
        assert_ne!(
            bytecode_analysis::step(&roomy, 0),
            0,
            "step answered 0 for opcode {op:#04x} with operand room"
        );
        assert_ne!(
            bytecode_analysis::step(&bare, 0),
            0,
            "step answered 0 for opcode {op:#04x} with no operand room"
        );
    }
}

#[test]
fn getfield_proves_receiver_nonnull() {
    // aload_0; getfield #1; ... ; aload_0; getfield #2
    // After the first getfield, local 0 is proven non-null.
    let code = vec![
        0x2A, // 0: aload_0
        0xB4, 0x00, 0x01, // 1: getfield #1
        0x57, // 4: pop (discard field value)
        0x2A, // 5: aload_0
        0xB4, 0x00, 0x02, // 6: getfield #2
        0xB1, // 9: return
    ];
    let info = analyze(&code, code.len());
    // At PC 0, local 0 is NOT proven non-null (no prior evidence).
    assert!(!info.is_nonnull(0, 0));
    // After the aload_0;getfield#1 pair completes, local 0 IS
    // non-null at the next aload_0 (PC 5) and the following
    // getfield (PC 6).
    assert!(info.is_nonnull(5, 0));
    assert!(info.is_nonnull(6, 0));
}

#[test]
fn ifnull_proves_nonnull_on_fallthrough() {
    // aload_1; ifnull +5; aload_1; ...
    // The fall-through after `ifnull` proves local 1 non-null.
    let code = vec![
        0x2B, // 0: aload_1
        0xC6, 0x00, 0x05, // 1: ifnull → PC 6
        0x2B, // 4: aload_1 (fall-through: local 1 non-null)
        0xB1, // 5: return
        0xB1, // 6: return (taken path)
    ];
    let info = analyze(&code, code.len());
    assert!(info.is_nonnull(4, 1));
    // At PC 6 (taken), local 1 must be NULL — definitely NOT non-null.
    assert!(!info.is_nonnull(6, 1));
}

#[test]
fn astore_after_new_sets_nonnull() {
    // new #X; astore_1 → local 1 is non-null
    let code = vec![
        0xBB, 0x00, 0x01, // 0: new #1
        0x4C, // 3: astore_1
        0x2B, // 4: aload_1
        0xB1, // 5: return
    ];
    let info = analyze(&code, code.len());
    assert!(info.is_nonnull(4, 1));
}

#[test]
fn total_facts_counts_correctly() {
    let code = vec![
        0xBB, 0x00, 0x01, // 0: new
        0x4C, // 3: astore_1
        0xB1, // 4: return
    ];
    let info = analyze(&code, code.len());
    assert!(info.total_facts() >= 1);
}

#[test]
fn merge_intersects_predecessors() {
    // Two paths merge at the return (PC 8). On the fall-through
    // path local 1 is proven non-null by the getfield; on the
    // ifeq-taken path it is not. The dataflow meet (bitwise AND)
    // over the two predecessors must yield "not proven non-null"
    // at PC 8.
    //
    //   0: iconst_0                    (0x03)
    //   1: ifeq → PC 8  (offset +7)    (0x99 00 07)
    //   4: aload_1                     (0x2B)
    //   5: getfield #1 (proves L1)     (0xB4 00 01)
    //   8: return  ← merge point       (0xB1)
    //
    // The ifeq branch offset is relative to the ifeq opcode (PC 1),
    // so reaching the return at PC 8 needs offset 7 — NOT 6, which
    // would land mid-getfield (PC 7 is not an instruction start),
    // get dropped by the CFG walk, and leave PC 8 with only the
    // getfield predecessor.
    let code = vec![
        0x03, // 0: iconst_0
        0x99, 0x00, 0x07, // 1: ifeq → PC 8
        0x2B, // 4: aload_1
        0xB4, 0x00, 0x01, // 5: getfield (proves L1 nonnull on this path)
        0xB1, // 8: return (merge)
    ];
    // The "merge" PC here is 8 (the return). Local 1 should NOT
    // be proven non-null at PC 8 because the ifeq-taken path
    // bypassed the getfield.
    let info = analyze(&code, code.len());
    assert!(!info.is_nonnull(8, 1));
}

#[test]
fn fixpoint_terminates_on_loop() {
    // Tight loop should converge without iterating forever.
    //   0: aload_0; getfield (proves L0)
    //   4: pop
    //   5: goto -5 → PC 0 (loop back)
    let code = vec![
        0x2A, // 0: aload_0
        0xB4, 0x00, 0x01, // 1: getfield
        0x57, // 4: pop
        0xA7, 0xFF, 0xFB, // 5: goto -5 → PC 0
    ];
    let info = analyze(&code, code.len());
    // At PC 0 on the first iteration L0 is unproven; after fixpoint
    // it's STILL unproven (the meet over [entry: 0, back-edge:
    // {L0}] = 0). This is the correctness check.
    assert!(!info.is_nonnull(0, 0));
}

/// A `CONSTANT_Dynamic` resolves by running a bootstrap method, and a
/// bootstrap may return `null` (`ConstantBootstraps.nullConstant` is in the
/// JDK for exactly that). This module has no constant pool, so it cannot
/// tell a condy from a string literal and must assume the one that can be
/// null.
#[test]
fn an_ldc_constant_is_not_assumed_non_null() {
    // ldc #1; astore_1; aload_1; return
    let code = vec![
        0x12, 0x01, // 0: ldc #1        (may be a null condy)
        0x4C, // 2: astore_1
        0x2B, // 3: aload_1
        0xB1, // 4: return
    ];
    let info = analyze(&code, code.len());
    assert!(
        !info.is_nonnull(3, 1),
        "a local loaded from `ldc` may hold the null a condy bootstrap \
         returned; proving it non-null deletes the program's own null test"
    );

    // The twin: `new` really does produce a non-null reference, so the
    // refusal above is about `ldc` and not about the `astore` rule.
    let allocated = vec![
        0xBB, 0x00, 0x01, // 0: new #1
        0x4C, // 3: astore_1
        0x2B, // 4: aload_1
        0xB1, // 5: return
    ];
    let info = analyze(&allocated, allocated.len());
    assert!(info.is_nonnull(4, 1));
}

/// `successors` models `jsr` as falling through, so every fact established
/// before the `jsr` survives the subroutine — including facts the
/// subroutine destroyed. The whole method is refused instead.
#[test]
fn a_method_with_a_subroutine_produces_no_facts_at_all() {
    // new #1; astore_1; jsr → 8; aload_1; return
    // 8: astore_2 (the return address); aconst_null; astore_1; ret 2
    //
    // Local 1 is non-null when the `jsr` is reached and null when the
    // subroutine returns. The fall-through model keeps the stale fact.
    let code = vec![
        0xBB, 0x00, 0x01, // 0: new #1
        0x4C, // 3: astore_1
        0xA8, 0x00, 0x05, // 4: jsr +5 → 9
        0x2B, // 7: aload_1
        0xB1, // 8: return
        0x4D, // 9: astore_2   (subroutine entry: the return address)
        0x01, // 10: aconst_null
        0x4C, // 11: astore_1  ← the fact the fall-through model loses
        0xA9, 0x02, // 12: ret 2
    ];
    let info = analyze(&code, code.len());
    assert_eq!(
        info.total_facts(),
        0,
        "a method using jsr/ret must produce no facts: the analysis cannot \
         carry the subroutine's effects to the return site, and a stale \
         fact there elides a null check that was doing real work"
    );
    assert!(!info.is_nonnull(7, 1));

    // The twin: the same prefix with the `jsr`/`ret` pair removed keeps its
    // facts, so the refusal is about the subroutine and not about the shape
    // of the fixture.
    let without = vec![
        0xBB, 0x00, 0x01, // 0: new #1
        0x4C, // 3: astore_1
        0x00, 0x00, 0x00, // 4-6: nop, nop, nop  (where the jsr was)
        0x2B, // 7: aload_1
        0xB1, // 8: return
    ];
    let info = analyze(&without, without.len());
    assert!(info.is_nonnull(7, 1));
}
