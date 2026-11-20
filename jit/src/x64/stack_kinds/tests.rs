use super::*;

impl StackKindMap {
    /// A map with exactly these answers, for consumer-side unit tests (the
    /// deopt snapshot's width selection) that must not depend on the
    /// analysis modelling a whole bytecode sequence.
    ///
    /// Defined here rather than beside `get`: a test gate above production
    /// code ends the panic-free ratchet's scan of the file (round 9 wave 7).
    pub(crate) fn from_entries_for_test(
        entries: impl IntoIterator<Item = (usize, Vec<StackKind>)>,
    ) -> Self {
        Self {
            at: entries.into_iter().collect(),
        }
    }
}

fn no_meta() -> (
    FxHashSet<usize>,
    FxHashMap<usize, u8>,
    FxHashMap<usize, (usize, u8)>,
) {
    (
        FxHashSet::default(),
        FxHashMap::default(),
        FxHashMap::default(),
    )
}

fn run(code: &[u8], calls: FxHashMap<usize, (usize, u8)>) -> StackKindMap {
    let (ldc_refs, types, _) = no_meta();
    // No resolver ran, so no `ldc` pc is resolved and every one of them
    // stays `Unknown` — the pre-fix behaviour these cases were written
    // against. `run_with_ldc` is the arm that supplies the tags.
    let inputs = StackKindInputs {
        field_types: types.clone(),
        static_types: types,
        calls,
        ldc_refs: &ldc_refs,
        ldc_fp: &FxHashSet::default(),
        ldc_resolved: &FxHashSet::default(),
        handler_pcs: &[],
    };
    analyze(code, code.len(), &inputs)
}

/// [`run`] with the `ldc`-family constant-pool tags the compiler resolves:
/// `resolved` is every `ldc`/`ldc2_w` pc reduced to an immediate, `fp` the
/// subset whose constant is a `CONSTANT_Float`/`CONSTANT_Double`.
fn run_with_ldc(
    code: &[u8],
    calls: FxHashMap<usize, (usize, u8)>,
    resolved: &[usize],
    fp: &[usize],
) -> StackKindMap {
    let (ldc_refs, types, _) = no_meta();
    let resolved: FxHashSet<usize> = resolved.iter().copied().collect();
    let fp: FxHashSet<usize> = fp.iter().copied().collect();
    let inputs = StackKindInputs {
        field_types: types.clone(),
        static_types: types,
        calls,
        ldc_refs: &ldc_refs,
        ldc_fp: &fp,
        ldc_resolved: &resolved,
        handler_pcs: &[],
    };
    analyze(code, code.len(), &inputs)
}

/// [`run`] with exception-handler entry points seeded.
fn run_with_handlers(
    code: &[u8],
    calls: FxHashMap<usize, (usize, u8)>,
    handler_pcs: &[usize],
) -> StackKindMap {
    let (ldc_refs, types, _) = no_meta();
    let inputs = StackKindInputs {
        field_types: types.clone(),
        static_types: types,
        calls,
        ldc_refs: &ldc_refs,
        ldc_fp: &FxHashSet::default(),
        ldc_resolved: &FxHashSet::default(),
        handler_pcs,
    };
    analyze(code, code.len(), &inputs)
}

/// A handler body is reached by an edge no branch instruction names, so
/// without a seed the analysis has no state there and every deopt point in
/// a `catch` block falls back to `Unsupported` — which vetoes the whole
/// artifact's OSR entry, because `osr_exit_policy` is artifact-wide.
///
/// The seed is not a guess: JVMS §2.10 fixes the handler-entry stack at
/// exactly one reference.
#[test]
fn a_handler_entry_is_seeded_with_the_throwable() {
    // 0: return
    // 1: astore_0     <- handler_pc; entry stack is [Ref]
    // 2: iconst_m1
    // 3: return
    let code = [0xb1u8, 0x4b, 0x02, 0xb1];
    let without = run_with_handlers(&code, FxHashMap::default(), &[]);
    assert_eq!(
        without.get(1),
        None,
        "unseeded, a handler body is unreached and has no answer at all"
    );

    let with = run_with_handlers(&code, FxHashMap::default(), &[1]);
    assert_eq!(with.get(1), Some(&[StackKind::Ref][..]));
    assert_eq!(
        with.get(2),
        Some(&[][..]),
        "the `astore_0` consumed it, so the stack is empty at pc 2"
    );
    assert_eq!(
        with.get(3),
        Some(&[StackKind::Int][..]),
        "and the typing carries on through the handler body"
    );
}

/// A handler pc that ordinary control flow also reaches at a different
/// depth still poisons — the seed goes through the same merge as any other
/// incoming edge, so it cannot launder a disagreement into an answer.
#[test]
fn a_handler_entry_that_disagrees_with_a_branch_still_poisons() {
    // 0: iconst_0   1: goto +3 (-> 4)   4: pop   5: return
    // pc 4 is reached by the goto with depth 1 AND seeded with depth 1 but
    // a different KIND, which merges to Unknown rather than poisoning; pc 5
    // is where the depths would differ if the seed were wrong.
    let code = [0x03u8, 0xa7, 0x00, 0x03, 0x57, 0xb1];
    let merged = run_with_handlers(&code, FxHashMap::default(), &[4]);
    assert_eq!(
        merged.get(4),
        Some(&[StackKind::Unknown][..]),
        "one path says Int and the other says Ref: the merge must forget, not pick"
    );

    // 0: iconst_0   1: iconst_0   2: goto +3 (-> 5)   5: return
    // Here the branch arrives with depth 2 and the seed says depth 1.
    let code = [0x03u8, 0x03, 0xa7, 0x00, 0x03, 0xb1];
    let poisoned = run_with_handlers(&code, FxHashMap::default(), &[5]);
    assert_eq!(
        poisoned.get(5),
        None,
        "a depth disagreement is not representable and must not be answered"
    );
}

// -----------------------------------------------------------------------
// The category-dependent dup family.
//
// Until 2026-08-18 all four of `dup_x2`/`dup2`/`dup2_x1`/`dup2_x2` poisoned
// here — "not modelling them costs precision, guessing them costs
// correctness". Modelling them costs neither, because every form is decided
// by categories this analysis already tracks, and `Unknown` still poisons.
//
// The payoff is not only precision downstream of a dup: this is the
// SECOND-ENTRY WIDTH ORACLE the single-pass `dup2_x2` arm needs, and whose
// absence kept that opcode unlowered on x64. A form picked from a wrong
// answer here duplicates an unrelated slot, so each case below pins the
// exact resulting vector, not just that an answer exists.
// -----------------------------------------------------------------------

/// `dup2` FORM 2 — a single category-2 entry, duplicated like `dup`.
#[test]
fn dup2_over_a_category_2_top_duplicates_one_entry() {
    // 0: lconst_0  1: dup2  2: ladd  3: lreturn
    let code: Vec<u8> = vec![0x09, 0x5c, 0x61, 0xad];
    let m = run(&code, FxHashMap::default());
    assert_eq!(m.get(1), Some(&[StackKind::Long][..]));
    assert_eq!(m.get(2), Some(&[StackKind::Long, StackKind::Long][..]));
}

/// `dup2` FORM 1 — two category-1 entries.
#[test]
fn dup2_over_two_category_1_entries_duplicates_both() {
    // 0: iconst_0  1: fconst_0  2: dup2  3: return
    let code: Vec<u8> = vec![0x03, 0x0b, 0x5c, 0xb1];
    let m = run(&code, FxHashMap::default());
    assert_eq!(
        m.get(3),
        Some(
            &[
                StackKind::Int,
                StackKind::Float,
                StackKind::Int,
                StackKind::Float
            ][..]
        )
    );
}

/// `dup_x2` FORM 2 — the value below the category-1 top is a category-2,
/// so the copy is inserted TWO entries down, not three.
#[test]
fn dup_x2_inserts_below_one_entry_when_that_entry_is_category_2() {
    // 0: dconst_0  1: iconst_0  2: dup_x2  3: return
    let code: Vec<u8> = vec![0x0e, 0x03, 0x5b, 0xb1];
    let m = run(&code, FxHashMap::default());
    assert_eq!(
        m.get(3),
        Some(&[StackKind::Int, StackKind::Double, StackKind::Int][..])
    );
}

/// `dup2_x1` FORM 2 — a category-2 top over one category-1.
#[test]
fn dup2_x1_form2_slides_a_category_2_under_one_entry() {
    // 0: iconst_0  1: dconst_0  2: dup2_x1  3: return
    let code: Vec<u8> = vec![0x03, 0x0e, 0x5d, 0xb1];
    let m = run(&code, FxHashMap::default());
    assert_eq!(
        m.get(3),
        Some(&[StackKind::Double, StackKind::Int, StackKind::Double][..])
    );
}

/// `dup2_x2` FORM 4 — both operands category-2, two entries.
#[test]
fn dup2_x2_form4_is_two_entries() {
    // 0: lconst_0  1: dconst_0  2: dup2_x2  3: return
    let code: Vec<u8> = vec![0x09, 0x0e, 0x5e, 0xb1];
    let m = run(&code, FxHashMap::default());
    assert_eq!(
        m.get(3),
        Some(&[StackKind::Double, StackKind::Long, StackKind::Double][..])
    );
}

/// `dup2_x2` FORM 2 — a category-2 top over two category-1 entries. This
/// is the form javac emits (`longArr[i] = otherArr[j] = v`), and the one
/// whose depth an unconditional four-pop gets wrong.
#[test]
fn dup2_x2_form2_slides_a_category_2_under_two_entries() {
    // 0: iconst_0  1: fconst_0  2: dconst_0  3: dup2_x2  4: return
    let code: Vec<u8> = vec![0x03, 0x0b, 0x0e, 0x5e, 0xb1];
    let m = run(&code, FxHashMap::default());
    assert_eq!(
        m.get(4),
        Some(
            &[
                StackKind::Double,
                StackKind::Int,
                StackKind::Float,
                StackKind::Double
            ][..]
        )
    );
}

/// `dup2_x2` FORM 3 — two category-1 entries duplicated over ONE
/// category-2. Three entries deep, not four: the difference between this
/// and FORM 1 is exactly what the top-entry oracle cannot see.
#[test]
fn dup2_x2_form3_duplicates_two_entries_over_a_category_2() {
    // 0: lconst_0  1: iconst_0  2: fconst_0  3: dup2_x2  4: return
    let code: Vec<u8> = vec![0x09, 0x03, 0x0b, 0x5e, 0xb1];
    let m = run(&code, FxHashMap::default());
    assert_eq!(
        m.get(4),
        Some(
            &[
                StackKind::Int,
                StackKind::Float,
                StackKind::Long,
                StackKind::Int,
                StackKind::Float
            ][..]
        )
    );
}

/// `dup2_x2` FORM 1 — four category-1 entries, six after.
#[test]
fn dup2_x2_form1_duplicates_two_entries_over_two() {
    // 0: iconst_0 1: iconst_1 2: fconst_0 3: iconst_2 4: dup2_x2 5: return
    let code: Vec<u8> = vec![0x03, 0x04, 0x0b, 0x05, 0x5e, 0xb1];
    let m = run(&code, FxHashMap::default());
    assert_eq!(
        m.get(5),
        Some(
            &[
                StackKind::Float,
                StackKind::Int,
                StackKind::Int,
                StackKind::Int,
                StackKind::Float,
                StackKind::Int
            ][..]
        )
    );
}

/// Rule 2 of the module's safety argument still holds for the new arms: an
/// `Unknown` in a position whose category decides the form poisons instead
/// of guessing. An unresolved `ldc2_w` is `Unknown` but its CATEGORY is not
/// in doubt, so use an unresolved `ldc` (int-or-float, both category-1 —
/// still `Unknown` as a KIND) under a category-1 top, where the third
/// entry's category is what picks FORM 1 from FORM 3.
#[test]
fn dup2_x2_over_an_unknown_third_entry_poisons() {
    // 0: ldc #0 (unresolved -> Unknown)  2: iconst_0  3: iconst_1
    // 4: dup2_x2  5: return
    let code: Vec<u8> = vec![0x12, 0x00, 0x03, 0x04, 0x5e, 0xb1];
    let m = run(&code, FxHashMap::default());
    // The dup's own bci still has its IN state (published pre-transfer)...
    assert_eq!(
        m.get(4),
        Some(&[StackKind::Unknown, StackKind::Int, StackKind::Int][..])
    );
    // ...but nothing downstream of it gets an answer.
    assert_eq!(m.get(5), None, "an unprovable form must poison, not guess");
}

/// The shape the whole thing exists for: a counted loop with a `long`
/// accumulator. Mid-expression bcis have a typed stack, which is what the
/// per-method `wide_fp` gate could never give.
#[test]
fn counted_long_loop_is_typed_at_every_bci() {
    // 0: lconst_0   1: lstore_1   2: iconst_0   3: istore_3
    // 4: iload_3     5: iload_0    6: if_icmpge 25
    // 9: lload_1    10: iload_3   11: i2l      12: ladd
    // 13: lstore_1  14: iinc 3,1  17: goto 4
    // 20..: return
    let code: Vec<u8> = vec![
        0x09, 0x40, 0x03, 0x3e, 0x1d, 0x1a, 0xa2, 0x00, 0x13, 0x1f, 0x1d, 0x85, 0x61, 0x40,
        0x84, 0x03, 0x01, 0xa7, 0xff, 0xf3, 0xb1,
    ];
    let m = run(&code, FxHashMap::default());

    // bci 1 (`lstore_1`): the long pushed by `lconst_0` is still live.
    assert_eq!(m.get(1), Some(&[StackKind::Long][..]));
    // bci 4, the loop header: empty stack.
    assert_eq!(m.get(4), Some(&[][..]));
    // bci 6 (`if_icmpge`): two ints.
    assert_eq!(m.get(6), Some(&[StackKind::Int, StackKind::Int][..]));
    // bci 12 (`ladd`): the accumulator and the widened counter.
    assert_eq!(m.get(12), Some(&[StackKind::Long, StackKind::Long][..]));
}

/// A call's arguments are typed from where they were PUSHED, which is why
/// the analysis needs only arity and return type at the call itself.
#[test]
fn a_call_pops_its_arguments_and_pushes_its_return_kind() {
    // 0: dload_1   1: invokestatic (D)J   4: lstore_3   5: return
    let code: Vec<u8> = vec![0x28, 0xb8, 0x00, 0x01, 0x40, 0xb1];
    let mut calls = FxHashMap::default();
    calls.insert(1usize, (1usize, b'J'));
    let m = run(&code, calls);

    assert_eq!(m.get(1), Some(&[StackKind::Double][..]));
    assert_eq!(m.get(4), Some(&[StackKind::Long][..]));
}

/// An instance call also pops its receiver — the opcode supplies that, not
/// the arity, which is why `calls` stores the descriptor's count alone.
#[test]
fn an_instance_call_also_pops_the_receiver() {
    // 0: aload_0  1: iload_1  2: invokevirtual (I)I  5: istore_2  6: return
    let code: Vec<u8> = vec![0x2a, 0x1b, 0xb6, 0x00, 0x01, 0x3d, 0xb1];
    let mut calls = FxHashMap::default();
    calls.insert(2usize, (1usize, b'I'));
    let m = run(&code, calls);

    assert_eq!(m.get(2), Some(&[StackKind::Ref, StackKind::Int][..]));
    assert_eq!(m.get(5), Some(&[StackKind::Int][..]));
}

/// An unmodelled opcode poisons its successors rather than guessing a
/// depth. `jsr` (0xa8) is the canonical one — it pushes a return address
/// this backend has no representation for.
#[test]
fn an_unmodelled_opcode_poisons_everything_after_it() {
    // 0: iconst_0  1: jsr +3  4: pop  5: return
    let code: Vec<u8> = vec![0x03, 0xa8, 0x00, 0x03, 0x57, 0xb1];
    let m = run(&code, FxHashMap::default());

    assert_eq!(m.get(0), Some(&[][..]));
    assert_eq!(m.get(1), Some(&[StackKind::Int][..]));
    assert_eq!(m.get(4), None, "the jsr's successor must have no answer");
}

/// The defect behind `osr-refused-for-a-loop-inline-in-main-FIXED-20260818`: a
/// numeric `ldc` answered `Unknown`, the deopt snapshot recorded it
/// `Unsupported`, and `osr_exit_policy`'s artifact-wide veto then refused
/// OSR entry at every pc of the method — 180 ns/iter against 1 compiled.
/// The constant-pool tag is known, so the entry must be typed.
#[test]
fn a_resolved_numeric_ldc_is_typed_from_its_constant_pool_tag() {
    // 0: ldc #1   2: ldc #2   4: return
    let code: Vec<u8> = vec![0x12, 0x01, 0x12, 0x02, 0xb1];

    // pc 0 is a CONSTANT_Integer, pc 2 a CONSTANT_Float.
    let m = run_with_ldc(&code, FxHashMap::default(), &[0, 2], &[2]);
    assert_eq!(m.get(2), Some(&[StackKind::Int][..]));
    assert_eq!(m.get(4), Some(&[StackKind::Int, StackKind::Float][..]));
}

/// `ldc2_w` is one compact entry either way, so only the KIND was ever in
/// doubt — and the same tag answers it.
#[test]
fn a_resolved_ldc2w_is_typed_from_its_constant_pool_tag() {
    // 0: ldc2_w #1   3: ldc2_w #2   6: return
    let code: Vec<u8> = vec![0x14, 0x00, 0x01, 0x14, 0x00, 0x02, 0xb1];

    let m = run_with_ldc(&code, FxHashMap::default(), &[0, 3], &[3]);
    assert_eq!(m.get(3), Some(&[StackKind::Long][..]));
    assert_eq!(m.get(6), Some(&[StackKind::Long, StackKind::Double][..]));
}

/// Rule 2 of the module's safety argument: `Unknown` is not a guess. A pc
/// the resolver never answered must NOT default to the non-floating-point
/// member of its pair just because it is absent from the `fp` set.
#[test]
fn an_unresolved_ldc_stays_unknown_rather_than_defaulting_to_int() {
    // 0: ldc #1   2: ldc2_w #2   5: return
    let code: Vec<u8> = vec![0x12, 0x01, 0x14, 0x00, 0x02, 0xb1];

    let m = run_with_ldc(&code, FxHashMap::default(), &[], &[]);
    assert_eq!(m.get(2), Some(&[StackKind::Unknown][..]));
    assert_eq!(
        m.get(5),
        Some(&[StackKind::Unknown, StackKind::Unknown][..]),
        "no resolver answer means no kind, for both ldc families"
    );
}

/// The exact operand stack the refusal named: a `long` static under an
/// `ldc` int argument, at the `invokestatic` that carries the deopt point.
/// Both entries must be describable, or OSR is refused for the whole
/// artifact.
#[test]
fn a_long_static_under_an_ldc_int_argument_is_fully_typed() {
    // 0: getstatic #1 (J)   3: ldc #2 (int)   5: invokestatic #3 (I)J
    let code: Vec<u8> = vec![0xb2, 0x00, 0x01, 0x12, 0x02, 0xb8, 0x00, 0x03, 0xb1];
    let mut statics: FxHashMap<usize, u8> = FxHashMap::default();
    statics.insert(0, b'J');
    let mut calls: FxHashMap<usize, (usize, u8)> = FxHashMap::default();
    calls.insert(5, (1, b'J'));

    let ldc_resolved: FxHashSet<usize> = [3usize].into_iter().collect();
    let inputs = StackKindInputs {
        field_types: FxHashMap::default(),
        static_types: statics,
        calls,
        ldc_refs: &FxHashSet::default(),
        ldc_fp: &FxHashSet::default(),
        ldc_resolved: &ldc_resolved,
        handler_pcs: &[],
    };
    let m = analyze(&code, code.len(), &inputs);

    assert_eq!(
        m.get(5),
        Some(&[StackKind::Long, StackKind::Int][..]),
        "the deopt point's operand stack must have no Unknown entry"
    );
}

/// Two paths that disagree about DEPTH cannot be merged, and the answer
/// must be withdrawn rather than taken from whichever path ran first.
#[test]
fn a_depth_disagreement_withdraws_the_answer() {
    // Two paths reach bci 9 with different depths. Note the extra
    // `iconst_0`: a conditional branch CONSUMES its operand, so without a
    // spare value on the stack both arms would arrive at equal depth and
    // the test would prove nothing.
    //
    // 0: iconst_0      -> [I]
    // 1: iconst_0      -> [I, I]
    // 2: ifeq 9        -> pops the condition; target 9 sees [I]
    // 5: iconst_1      -> [I, I]
    // 6: goto 9        -> target 9 sees [I, I]
    // 9: pop   10: return
    let code: Vec<u8> = vec![
        0x03, // 0: iconst_0
        0x03, // 1: iconst_0
        0x99, 0x00, 0x07, // 2: ifeq -> 9
        0x04, // 5: iconst_1
        0xa7, 0x00, 0x03, // 6: goto -> 9
        0x57, // 9: pop
        0xb1, // 10: return
    ];
    let m = run(&code, FxHashMap::default());
    assert_eq!(m.get(9), None, "conflicting depths must not be answered");
    assert_eq!(m.get(10), None, "and the conflict propagates forward");
}

/// `pop2` is category-dependent: over a known cat-1 top it pops two
/// entries, over a cat-2 top one. Over an `Unknown` top the depth is
/// genuinely unknown, so it poisons instead of picking.
#[test]
fn pop2_over_an_unknown_top_poisons() {
    // 0: iconst_0  1: ldc2_w #1 (long-or-double, Unknown)  4: pop2  5: return
    let code: Vec<u8> = vec![0x03, 0x14, 0x00, 0x01, 0x58, 0xb1];
    let m = run(&code, FxHashMap::default());
    assert_eq!(m.get(4), Some(&[StackKind::Int, StackKind::Unknown][..]));
    assert_eq!(m.get(5), None);
}

/// `pop2` over a known cat-2 top pops one entry; over cat-1, two.
#[test]
fn pop2_respects_a_known_category() {
    // 0: lconst_0  1: pop2  2: return   -> one entry popped, stack empty
    let m = run(&[0x09, 0x58, 0xb1], FxHashMap::default());
    assert_eq!(m.get(2), Some(&[][..]));

    // 0: iconst_0  1: iconst_1  2: pop2  3: return -> two popped, empty
    let m = run(&[0x03, 0x04, 0x58, 0xb1], FxHashMap::default());
    assert_eq!(m.get(3), Some(&[][..]));
}

/// A conditional whose two arms push the same DEPTH but different KINDS
/// merges to `Unknown` — describable depth, undescribable type, which the
/// consult site then treats exactly as it did before this analysis existed.
#[test]
fn conflicting_kinds_at_the_same_depth_merge_to_unknown() {
    let code: Vec<u8> = vec![
        0x03, // 0: iconst_0
        0x99, 0x00, 0x06, // 1: ifeq -> 7
        0x0b, // 4: fconst_0
        0xa7, 0x00, 0x03, // 5: goto -> 8
        0x03, // 7: iconst_0
        0x57, // 8: pop
        0xb1, // 9: return
    ];
    let m = run(&code, FxHashMap::default());
    assert_eq!(m.get(8), Some(&[StackKind::Unknown][..]));
}

/// `checkcast` proves its operand is a reference; `instanceof` replaces it
/// with an int. Both are everywhere in generic-heavy loop bodies.
#[test]
fn checkcast_keeps_a_ref_and_instanceof_yields_an_int() {
    // 0: aload_0  1: checkcast #1  4: astore_1  5: aload_0
    // 6: instanceof #1  9: istore_2  10: return
    let code: Vec<u8> = vec![
        0x2a, 0xc0, 0x00, 0x01, 0x4c, 0x2a, 0xc1, 0x00, 0x01, 0x3d, 0xb1,
    ];
    let m = run(&code, FxHashMap::default());
    assert_eq!(m.get(4), Some(&[StackKind::Ref][..]));
    assert_eq!(m.get(9), Some(&[StackKind::Int][..]));
}

/// `dup`, `dup_x1` and `swap` have one JVMS form each, so an `Unknown`
/// operand leaves their compact stack effect — and therefore the depth —
/// fully known. They used to poison on it, which withdrew the answer at
/// every pc downstream of the shuffle.
#[test]
fn single_form_shuffles_over_an_unknown_operand_keep_the_depth() {
    // 0: ldc #1 (unresolved -> Unknown)  2: dup  3: pop  4: pop  5: return
    let dup = vec![0x12, 0x01, 0x59, 0x57, 0x57, 0xb1];
    let m = run(&dup, FxHashMap::default());
    assert_eq!(
        m.get(3),
        Some(&[StackKind::Unknown, StackKind::Unknown][..])
    );
    assert_eq!(m.get(5).map(<[StackKind]>::len), Some(0));

    // 0: iconst_0  1: ldc #1  3: swap  4: pop  5: pop  6: return
    let swap = vec![0x03, 0x12, 0x01, 0x5f, 0x57, 0x57, 0xb1];
    let m = run(&swap, FxHashMap::default());
    assert_eq!(m.get(4), Some(&[StackKind::Unknown, StackKind::Int][..]));
    assert_eq!(m.get(6).map(<[StackKind]>::len), Some(0));

    // 0: iconst_0  1: ldc #1  3: dup_x1  4: pop  5: pop  6: pop  7: return
    let dup_x1 = vec![0x03, 0x12, 0x01, 0x5a, 0x57, 0x57, 0x57, 0xb1];
    let m = run(&dup_x1, FxHashMap::default());
    assert_eq!(
        m.get(4),
        Some(&[StackKind::Unknown, StackKind::Int, StackKind::Unknown][..])
    );

    // A KNOWN category-2 operand is malformed for all three and still
    // poisons: 0: lconst_0  1: dup  2: pop  3: return
    let bad = vec![0x09, 0x59, 0x57, 0xb1];
    let m = run(&bad, FxHashMap::default());
    assert_eq!(m.get(1), Some(&[StackKind::Long][..]));
    assert_eq!(m.get(2), None);
    assert_eq!(m.get(3), None);
}
