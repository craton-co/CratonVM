use super::*;

/// Run the guard-dominated pass on a handler-free method.
fn run(code: &[u8]) -> Vec<usize> {
    let mut v: Vec<usize> = range_safe_pcs(code, code.len(), &[]).into_iter().collect();
    v.sort_unstable();
    v
}

/// `int f(int[] a, int i) { if (i >= 0 && i < a.length) return a[i]; return -1; }`
///
/// The canonical guarded access, and the one the counted-loop reason
/// cannot touch at all: there is no loop.
///
/// ```text
///  0: iload_1            (i)
///  1: iflt   +14 -> 15
///  4: iload_1
///  5: aload_0
///  6: arraylength
///  7: if_icmpge +8 -> 15
/// 10: aload_0
/// 11: iload_1
/// 12: iaload             <-- the access
/// 13: ireturn
/// 15: iconst_m1
/// 16: ireturn
/// ```
fn guarded_load() -> Vec<u8> {
    vec![
        0x1b, 0x9b, 0x00, 0x0e, 0x1b, 0x2a, 0xbe, 0xa2, 0x00, 0x08, 0x2a, 0x1b, 0x2e, 0xac,
        0x02, 0xac,
    ]
}

#[test]
fn both_halves_of_the_guard_eliminate_the_check() {
    assert_eq!(run(&guarded_load()), vec![12]);
}

/// MUST REFUSE: drop the `i >= 0` half. `i < a.length` alone admits a
/// negative index, which is an access *below* the array base — strictly
/// worse than the overrun the check normally catches.
#[test]
fn upper_bound_alone_is_not_enough() {
    // Same as `guarded_load` with the `iflt` replaced by two `nop`s plus a
    // `pop` of the loaded `i`, so the length guard still dominates.
    //  0: iload_1 ; 1: pop ; 2: nop ; 3: nop
    //  4: iload_1 ; 5: aload_0 ; 6: arraylength ; 7: if_icmpge +8 -> 15
    // 10: aload_0 ; 11: iload_1 ; 12: iaload ; 13: ireturn
    // 15: iconst_m1 ; 16: ireturn
    let code: Vec<u8> = vec![
        0x1b, 0x57, 0x00, 0x00, 0x1b, 0x2a, 0xbe, 0xa2, 0x00, 0x08, 0x2a, 0x1b, 0x2e, 0xac,
        0x02, 0xac,
    ];
    assert!(
        run(&code).is_empty(),
        "an unbounded-below index must keep its check"
    );
}

/// MUST REFUSE: drop the length half, keep `i >= 0`.
#[test]
fn lower_bound_alone_is_not_enough() {
    //  0: iload_1 ; 1: iflt +12 -> 13
    //  4: aload_0 ; 5: iload_1 ; 6: iaload ; 7: ireturn
    //  ... padding so the branch target is a real instruction start
    let code: Vec<u8> = vec![
        0x1b, 0x9b, 0x00, 0x0c, 0x2a, 0x1b, 0x2e, 0xac, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02,
        0xac,
    ];
    assert!(run(&code).is_empty(), "no upper bound, no elision");
}

/// MUST REFUSE: the guard names a DIFFERENT array from the access. This is
/// the multi-array out-of-bounds store
/// (docs/feature-designs/jit-ir-optimizations.md) in its
/// guard-dominated spelling — `a.length` says nothing about `b.length`.
#[test]
fn a_guard_on_another_array_proves_nothing() {
    // locals: 0=a 1=b 2=i
    //  0: iload_2 ; 1: iflt +15 -> 16
    //  4: iload_2 ; 5: aload_0 ; 6: arraylength ; 7: if_icmpge +9 -> 16
    // 10: aload_1 ; 11: iload_2 ; 12: iaload ; 13: pop ; 14: nop ; 15: nop
    // 16: return
    let code: Vec<u8> = vec![
        0x1c, 0x9b, 0x00, 0x0f, 0x1c, 0x2a, 0xbe, 0xa2, 0x00, 0x09, 0x2b, 0x1c, 0x2e, 0x57,
        0x00, 0x00, 0xb1,
    ];
    assert!(
        run(&code).is_empty(),
        "b[i] under a guard on a.length must keep its check"
    );
    // …and the SAME method with the access on `a` does eliminate, so the
    // refusal above is about the array identity and not about the shape.
    let mut on_a = code.clone();
    on_a[10] = 0x2a; // aload_1 -> aload_0
    assert_eq!(run(&on_a), vec![12]);
}

/// MUST REFUSE: the guard is *after* the access.
#[test]
fn a_guard_that_does_not_dominate_proves_nothing() {
    //  0: aload_0 ; 1: iload_1 ; 2: iaload ; 3: pop      (unguarded access)
    //  4: iload_1 ; 5: aload_0 ; 6: arraylength ; 7: if_icmpge +4 -> 11
    // 10: nop ; 11: return
    let code: Vec<u8> = vec![
        0x2a, 0x1b, 0x2e, 0x57, 0x1b, 0x2a, 0xbe, 0xa2, 0x00, 0x04, 0x00, 0xb1,
    ];
    assert!(run(&code).is_empty());
}

/// MUST REFUSE: the index is reassigned between the guard and the access.
#[test]
fn a_write_to_the_index_kills_the_fact() {
    //  0: iload_1 ; 1: iflt +16 -> 17
    //  4: iload_1 ; 5: aload_0 ; 6: arraylength ; 7: if_icmpge +10 -> 17
    // 10: sipush 9999 ; 13: istore_1          <-- i is now anything
    // 14: aload_0 ; 15: iload_1 ; 16: iaload
    // 17: return
    let code: Vec<u8> = vec![
        0x1b, 0x9b, 0x00, 0x10, 0x1b, 0x2a, 0xbe, 0xa2, 0x00, 0x0a, 0x11, 0x27, 0x0f, 0x3c,
        0x2a, 0x1b, 0x2e, 0xb1,
    ];
    assert!(
        run(&code).is_empty(),
        "the guarded value is gone; so is the guard"
    );
}

/// MUST REFUSE: the ARRAY local is reassigned between the guard and the
/// access. The fact was about the object `a` referred to then.
#[test]
fn a_write_to_the_array_kills_the_fact() {
    // locals: 0=a 1=i 2=other
    //  0: iload_1 ; 1: iflt +14 -> 15
    //  4: iload_1 ; 5: aload_0 ; 6: arraylength ; 7: if_icmpge +8 -> 15
    // 10: aload_2 ; 11: astore_0                <-- a := other
    // 12: aload_0 ; 13: iload_1 ; 14: iaload
    // 15: return
    let code: Vec<u8> = vec![
        0x1b, 0x9b, 0x00, 0x0f, 0x1b, 0x2a, 0xbe, 0xa2, 0x00, 0x09, 0x2c, 0x4b, 0x2a, 0x1b,
        0x2e, 0x00, 0xb1,
    ];
    assert!(run(&code).is_empty());
}

/// `iinc` is where the machine-integer discipline earns its keep.
///
/// `i++` after the guard invalidates `i < a.length`; `i--` does not (the
/// value only moved down, and `add_no_wrap` proved the step representable),
/// but it *can* break `i >= 0`, so the numeric half has to catch it.
/// Either way the access must keep its check — the point of the test is
/// that both halves are consulted, and neither is assumed.
#[test]
fn an_increment_after_the_guard_invalidates_it() {
    //  0: iload_1 ; 1: iflt +15 -> 16
    //  4: iload_1 ; 5: aload_0 ; 6: arraylength ; 7: if_icmpge +9 -> 16
    // 10: iinc 1,<d>
    // 13: aload_0 ; 14: iload_1 ; 15: iaload
    // 16: return
    let mk = |d: u8| -> Vec<u8> {
        vec![
            0x1b, 0x9b, 0x00, 0x0f, 0x1b, 0x2a, 0xbe, 0xa2, 0x00, 0x09, 0x84, 0x01, d, 0x2a,
            0x1b, 0x2e, 0xb1,
        ]
    };
    assert!(
        run(&mk(1)).is_empty(),
        "i++ can walk off the end: the upper-bound fact is dead"
    );
    // Cast: 0xff is `-1` as the signed iinc delta.
    assert!(
        run(&mk(0xff)).is_empty(),
        "i-- can walk below zero: the lower-bound fact is dead"
    );
}

/// A constant index needs no `< length` fact of its own — but it does not
/// GET one either, so it keeps its check. Pinned so a future "constants are
/// obviously fine" shortcut cannot be added without a length proof.
#[test]
fn a_constant_index_without_a_length_proof_is_refused() {
    // 0: aload_0 ; 1: iconst_0 ; 2: iaload ; 3: pop ; 4: return
    let code: Vec<u8> = vec![0x2a, 0x03, 0x2e, 0x57, 0xb1];
    assert!(run(&code).is_empty());
}

/// An array STORE is eliminated on the same evidence — with the extra
/// requirement that the stored value comes from a single pushing
/// instruction, since anything longer would desynchronise the positional
/// operand decode.
#[test]
fn a_guarded_store_eliminates_but_a_computed_value_does_not() {
    // locals: 0=a 1=i 2=v
    //  0: iload_1 ; 1: iflt +14 -> 15
    //  4: iload_1 ; 5: aload_0 ; 6: arraylength ; 7: if_icmpge +8 -> 15
    // 10: aload_0 ; 11: iload_1 ; 12: iload_2 ; 13: iastore
    // 15: return   (14 is a nop)
    let simple: Vec<u8> = vec![
        0x1b, 0x9b, 0x00, 0x0e, 0x1b, 0x2a, 0xbe, 0xa2, 0x00, 0x08, 0x2a, 0x1b, 0x1c, 0x4f,
        0x00, 0xb1,
    ];
    assert_eq!(run(&simple), vec![13]);

    // The same store with a two-instruction value expression (`v + v`):
    // the instruction before `iastore` is now `iadd`, so the positional
    // decode refuses rather than guessing.
    //  0: iload_1 ; 1: iflt +16 -> 17
    //  4: iload_1 ; 5: aload_0 ; 6: arraylength ; 7: if_icmpge +10 -> 17
    // 10: aload_0 ; 11: iload_1 ; 12: iload_2 ; 13: iload_2 ; 14: iadd
    // 15: iastore ; 16: nop ; 17: return
    let computed: Vec<u8> = vec![
        0x1b, 0x9b, 0x00, 0x10, 0x1b, 0x2a, 0xbe, 0xa2, 0x00, 0x0a, 0x2a, 0x1b, 0x1c, 0x1c,
        0x60, 0x4f, 0x00, 0xb1,
    ];
    assert!(run(&computed).is_empty());
}

/// An exception-handler entry resets the analysis. The handler here starts
/// *inside* the guarded region, so control can arrive at the access having
/// skipped the guard entirely — the fact must not survive.
#[test]
fn a_handler_entry_inside_the_guarded_region_kills_the_facts() {
    let code = guarded_load();
    // No handlers: eliminated (the control test).
    assert_eq!(run(&code), vec![12]);
    // A handler landing on pc 10 (the `aload_0` of the access) means
    // control can reach pc 12 without ever running either guard.
    let with_handler = range_safe_pcs(&code, code.len(), &[(0, 10, 10)]);
    assert!(
        with_handler.is_empty(),
        "a handler entry ahead of the access invalidates the guard, got {with_handler:?}"
    );
    // A handler landing after the access does not.
    let after = range_safe_pcs(&code, code.len(), &[(0, 16, 15)]);
    assert_eq!(after.into_iter().collect::<Vec<_>>(), vec![12]);
}

/// Unmodelled control flow refuses the whole method rather than the one
/// construct: a `tableswitch`'s successors are not walked, so a fact could
/// survive an edge this pass never saw.
#[test]
fn unmodelled_control_flow_refuses_the_method() {
    let mut code = guarded_load();
    // Overwrite the trailing `iconst_m1; ireturn` with a `lookupswitch`
    // opcode. The decode never reaches it, but the whole-method scan does.
    let n = code.len();
    code[n - 2] = 0xab;
    assert!(run(&code).is_empty());
}

/// The pass refuses an over-long method outright, so the analysis can
/// never become the compile-time outlier.
#[test]
fn an_over_long_method_is_refused() {
    let mut code = guarded_load();
    code.resize(RANGE_MAX_CODE_LEN + 1, 0x00);
    assert!(range_safe_pcs(&code, code.len(), &[]).is_empty());
}

/// The gate is a real switch in both directions, and it is the ONLY thing
/// that decides whether the range reason contributes.
#[test]
fn the_gate_switches_the_range_reason_only() {
    let code = guarded_load();
    let loops = super::super::detect_loops(&code, code.len());

    __set_range_bce_override(Some(true));
    let (on, _) =
        analyze_bounds_elimination_with_handlers(&code, code.len(), &loops, Some(&[]));
    __set_range_bce_override(Some(false));
    let (off, _) =
        analyze_bounds_elimination_with_handlers(&code, code.len(), &loops, Some(&[]));
    // The handler-blind entry point never runs the range reason at all.
    __set_range_bce_override(Some(true));
    let (blind, _) = analyze_bounds_elimination(&code, code.len(), &loops);
    __set_range_bce_override(None);

    assert!(on.contains(&12));
    assert!(!off.contains(&12));
    assert!(
        !blind.contains(&12),
        "an unknown exception table must refuse, not assume there is none"
    );
}

/// The counted-loop reason is untouched by any of this: the same fixture
/// the loop tests use must still produce the same verdict through the new
/// handler-aware entry point.
#[test]
fn the_counted_loop_reason_is_unchanged() {
    // `for (i = 0; i < a.length; i++) a[i];` — see
    // `range_bce_tests::inline_arraylength_limit_eliminates_statically`.
    let code: Vec<u8> = vec![
        0x03, 0x3c, 0x1b, 0x2a, 0xbe, 0xa2, 0x00, 0x0d, 0x2a, 0x1b, 0x2e, 0x57, 0x84, 0x01,
        0x01, 0xa7, 0xff, 0xf3, 0xb1,
    ];
    let loops = super::super::detect_loops(&code, code.len());
    __set_range_bce_override(Some(false));
    let (safe, guards) =
        analyze_bounds_elimination_with_handlers(&code, code.len(), &loops, Some(&[]));
    __set_range_bce_override(None);
    let mut safe: Vec<usize> = safe.into_iter().collect();
    safe.sort_unstable();
    assert_eq!(safe, vec![10]);
    assert!(guards.is_empty());
}

/// A loop an exception can enter from OUTSIDE is not a counted loop: the
/// handler entry bypasses the header's test and the IV's init store, so
/// neither the span nor the constant start holds on it
/// (`bytecode-loop-scans-do-not-model-exception-handler-entries-20260918.md`).
#[test]
fn a_handler_entering_the_loop_from_outside_refuses_the_elision() {
    // Same fixture as above; the handler lands on the `iinc` at pc 12 and
    // protects `[0, 2)`, the init store's range, outside the loop.
    let code: Vec<u8> = vec![
        0x03, 0x3c, 0x1b, 0x2a, 0xbe, 0xa2, 0x00, 0x0d, 0x2a, 0x1b, 0x2e, 0x57, 0x84, 0x01,
        0x01, 0xa7, 0xff, 0xf3, 0xb1,
    ];
    let loops = super::super::detect_loops(&code, code.len());
    __set_range_bce_override(Some(false));
    let (entered, entered_guards) = analyze_bounds_elimination_with_handlers(
        &code,
        code.len(),
        &loops,
        Some(&[(0, 2, 12)]),
    );
    // MUST ACCEPT twin: a handler whose range lies inside the loop.
    let (inside, _) = analyze_bounds_elimination_with_handlers(
        &code,
        code.len(),
        &loops,
        Some(&[(8, 11, 12)]),
    );
    __set_range_bce_override(None);
    assert!(
        entered.is_empty() && entered_guards.is_empty(),
        "a side entry by exception must refuse the loop, got {entered:?}"
    );
    assert!(
        inside.contains(&10),
        "twin: a handler protecting only in-loop code keeps the elision, got {inside:?}"
    );
}

// ---- the state lattice itself ----------------------------------------

#[test]
fn merge_intersects_facts_and_hulls_ranges() {
    let mut a = RangeState::top(4);
    a.locals[1] = Range::exact(IntWidth::W32, 0, 10);
    a.add_lt_len(1, 0);
    a.add_lt_len(2, 0);
    let mut b = RangeState::top(4);
    b.locals[1] = Range::exact(IntWidth::W32, 20, 30);
    b.add_lt_len(1, 0);

    assert!(a.merge(&b, false));
    assert_eq!(a.locals[1], Range::exact(IntWidth::W32, 0, 30));
    assert!(a.proves_lt_len(1, 0), "held on both paths");
    assert!(!a.proves_lt_len(2, 0), "held on only one path");
    // Idempotent: merging the same state again changes nothing.
    let snapshot = a.clone();
    assert!(!a.merge(&snapshot, false));
}

#[test]
fn merge_widens_once_asked_to() {
    let mut a = RangeState::top(2);
    a.locals[0] = Range::exact(IntWidth::W32, 0, 10);
    let mut b = RangeState::top(2);
    b.locals[0] = Range::exact(IntWidth::W32, 0, 11);
    a.merge(&b, true);
    assert_eq!(
        a.locals[0],
        // Widening: the i32 endpoints into the i64 interval domain.
        Range::exact(IntWidth::W32, 0, i32::MAX as i64),
        "an outward-moving endpoint goes straight to the extreme"
    );
}

#[test]
fn kill_removes_the_range_and_every_fact_naming_the_slot() {
    let mut s = RangeState::top(4);
    s.locals[1] = Range::exact(IntWidth::W32, 0, 5);
    s.add_lt_len(1, 0); // i=1 indexes a=0
    s.add_lt_len(2, 1); // j=2 indexes a=1
    s.kill(1);
    assert!(s.locals[1].is_top());
    assert!(!s.proves_lt_len(1, 0), "killed as the index");
    assert!(!s.proves_lt_len(2, 1), "killed as the array");
}

#[test]
fn narrow_by_handles_the_endpoints_without_wrapping() {
    let top = Range::top(IntWidth::W32);
    // Widening: the i32 endpoints into the i64 interval domain.
    let (min, max) = (i32::MIN as i64, i32::MAX as i64);
    assert_eq!(
        narrow_by(top, Rel::Ge, 0),
        Range::exact(IntWidth::W32, 0, max)
    );
    assert_eq!(
        narrow_by(top, Rel::Lt, 10),
        Range::exact(IntWidth::W32, min, 9)
    );
    // `x < Integer.MIN_VALUE` is impossible — bottom, not a wrapped range.
    assert!(narrow_by(top, Rel::Lt, min).is_empty());
    // `x > Integer.MAX_VALUE` likewise.
    assert!(narrow_by(top, Rel::Gt, max).is_empty());
    // `!=` cannot be expressed by an interval, so it must narrow nothing.
    assert_eq!(narrow_by(top, Rel::Ne, 5), top);
}

#[test]
fn relation_algebra() {
    for r in [Rel::Eq, Rel::Ne, Rel::Lt, Rel::Le, Rel::Gt, Rel::Ge] {
        assert_eq!(negate_rel(negate_rel(r)), r);
        assert_eq!(swap_rel(swap_rel(r)), r);
    }
    assert_eq!(negate_rel(Rel::Lt), Rel::Ge);
    assert_eq!(swap_rel(Rel::Lt), Rel::Gt);
    // `if_icmpge` not taken means `<`, which is where the fall-through
    // edge of javac's guard gets its fact.
    assert_eq!(branch_relation(0xa2), Some((2, Rel::Ge)));
    assert_eq!(negate_rel(Rel::Ge), Rel::Lt);
    // `if_acmpeq` / `ifnull` carry no integer relation.
    assert_eq!(branch_relation(0xa5), None);
    assert_eq!(branch_relation(0xc6), None);
}
