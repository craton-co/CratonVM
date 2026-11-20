use super::super::detect_loops;
use super::*;
use crate::scev::{BoundSource, BoundTerm, PreheaderGuard, SymBound};

/// A guard flattened to `(header, array, bound, iv, covered)`.
type GuardTuple = (usize, usize, usize, usize, Vec<usize>);

/// `(safe_pcs sorted, guards sorted)`.
fn run(code: &[u8], code_len: usize) -> (Vec<usize>, Vec<GuardTuple>) {
    let loops = detect_loops(code, code_len);
    let (safe, guards) = analyze_bounds_elimination(code, code_len, &loops);
    let mut safe: Vec<usize> = safe.into_iter().collect();
    safe.sort_unstable();
    let mut g: Vec<GuardTuple> = guards
        .iter()
        .map(|g| {
            (
                g.loop_header,
                g.array_local,
                g.bound_local,
                g.iv_local,
                g.covered_pcs.clone(),
            )
        })
        .collect();
    g.sort();
    (safe, g)
}

/// `for (int i = 0; i < a.length; i++) a[i];` with the length read INLINE
/// in the exit test — the single most common Java loop, and one the old
/// `analyze_loop_bound` refused outright because it demanded the limit be a
/// bare `iload`. The limit IS the accessed array's length, so
/// `prove_index_in_bounds_of` discharges it against its own tautology and
/// the elision is STATIC: no guard, no deopt.
fn inline_length_loop() -> (Vec<u8>, usize) {
    // 0: iconst_0 ; 1: istore_1                       (i = 0)
    // 2: iload_1 ; 3: aload_0 ; 4: arraylength        (header)
    // 5: if_icmpge +13 -> 18
    // 8: aload_0 ; 9: iload_1 ; 10: iaload ; 11: pop  (a[i])
    // 12: iinc 1,1 ; 15: goto -13 -> 2 ; 18: return
    let code = vec![
        0x03, 0x3c, 0x1b, 0x2a, 0xbe, 0xa2, 0x00, 0x0d, 0x2a, 0x1b, 0x2e, 0x57, 0x84, 0x01,
        0x01, 0xa7, 0xff, 0xf3, 0xb1,
    ];
    let len = code.len();
    (code, len)
}

#[test]
fn inline_arraylength_limit_eliminates_statically() {
    let (code, code_len) = inline_length_loop();
    assert_eq!(detect_loops(&code, code_len)[0], (2, 15));
    let (safe, guards) = run(&code, code_len);
    assert_eq!(
        safe,
        vec![10],
        "a[i] under `i < a.length` is statically safe"
    );
    assert!(
        guards.is_empty(),
        "the limit IS this array's length — nothing left to guard, got {guards:?}"
    );
}


/// A provably zero-trip loop (`for (i = 5; i < 3; i++)`) has an EMPTY index
/// span: the body never runs, so there is no access to check and the
/// verdict is `Static`. This is the lattice's bottom doing real work — the
/// old pattern set had no way to express it.
#[test]
fn zero_trip_loop_is_vacuously_safe() {
    // 0: iconst_5 ; 1: istore_1                       (i = 5)
    // 2: iload_1 ; 3: iconst_3 ; 4: if_icmpge +13 -> 17
    // 7: aload_0 ; 8: iload_1 ; 9: iaload ; 10: pop
    // 11: iinc 1,1 ; 14: goto -12 -> 2 ; 17: return
    let code: Vec<u8> = vec![
        0x08, 0x3c, 0x1b, 0x06, 0xa2, 0x00, 0x0d, 0x2a, 0x1b, 0x2e, 0x57, 0x84, 0x01, 0x01,
        0xa7, 0xff, 0xf4, 0xb1,
    ];
    let code_len = code.len();
    assert_eq!(detect_loops(&code, code_len)[0], (2, 14));
    let (safe, guards) = run(&code, code_len);
    assert_eq!(safe, vec![9], "a provably zero-trip body needs no check");
    assert!(guards.is_empty(), "and no guard, got {guards:?}");
}

/// `for (i = 0; i < n; i++) c[i] = a[i] + b[i];` with `n` a parameter.
/// Three arrays, three SEPARATE guards. `LengthAtLeast` names no array by
/// contract, so all three read identically — merging them would discharge
/// the shortest array's obligation with the longest array's length, which
/// is exactly the multi-array out-of-bounds store
/// (docs/feature-designs/jit-ir-optimizations.md).
#[test]
fn multi_array_loop_keeps_one_guard_per_array() {
    // locals: 0=a 1=b 2=c 3=n 4=i
    // 0: iconst_0 ; 1: istore 4
    // 3: iload 4 ; 5: iload_3 ; 6: if_icmpge +22 -> 28   (header)
    // 9: aload_2 ; 10: iload 4
    // 12: aload_0 ; 13: iload 4 ; 15: iaload
    // 16: aload_1 ; 17: iload 4 ; 19: iaload
    // 20: iadd ; 21: iastore
    // 22: iinc 4,1 ; 25: goto -22 -> 3 ; 28: return
    let code: Vec<u8> = vec![
        0x03, 0x36, 0x04, 0x15, 0x04, 0x1d, 0xa2, 0x00, 0x16, 0x2c, 0x15, 0x04, 0x2a, 0x15,
        0x04, 0x2e, 0x2b, 0x15, 0x04, 0x2e, 0x60, 0x4f, 0x84, 0x04, 0x01, 0xa7, 0xff, 0xea,
        0xb1,
    ];
    let code_len = code.len();
    assert_eq!(detect_loops(&code, code_len)[0], (3, 25));
    let (safe, guards) = run(&code, code_len);
    assert_eq!(safe, vec![15, 19, 21]);
    assert_eq!(
        guards,
        vec![
            (3, 0, 3, 4, vec![15]),
            (3, 1, 3, 4, vec![19]),
            (3, 2, 3, 4, vec![21]),
        ],
        "one guard per array, each covering only its own access"
    );
}

/// MUST ELIMINATE / MUST REFUSE pair on the stride.
///
/// `iinc i, 1` eliminates behind the ordinary guard. `iinc i, 2` is a stride
/// the *proof* now accepts (`Stride::Const(2)`) but whose no-wrap obligation
/// comes back as `PreheaderGuard::AtMost { bound, i32::MAX - 1 }` — and the
/// pre-header emitter only emits its `bound != Integer.MAX_VALUE` test for
/// the INCLUSIVE comparator. One uncoverable guard refuses the whole proof.
#[test]
fn unit_stride_eliminates_non_unit_stride_refuses() {
    // 0: iload_0 ; 1: iload_2 ; 2: if_icmpge +13 -> 15
    // 5: aload_1 ; 6: iload_0 ; 7: iaload ; 8: pop
    // 9: iinc 0,<step> ; 12: goto -12 -> 0 ; 15: return
    let mk = |step: u8| -> Vec<u8> {
        vec![
            0x1a, 0x1c, 0xa2, 0x00, 0x0d, 0x2b, 0x1a, 0x2e, 0x57, 0x84, 0x00, step, 0xa7, 0xff,
            0xf4, 0xb1,
        ]
    };
    let unit = mk(1);
    let (safe, guards) = run(&unit, unit.len());
    assert_eq!(safe, vec![7], "unit stride still eliminates");
    assert_eq!(guards, vec![(0, 1, 2, 0, vec![7])]);

    let wide = mk(2);
    let (safe2, guards2) = run(&wide, wide.len());
    assert!(
        safe2.is_empty(),
        "a stride whose wrap guard has no emitter must refuse, got {safe2:?}"
    );
    assert!(guards2.is_empty());
}

/// MUST ELIMINATE / MUST REFUSE pair on the limit's shape.
///
/// A limit held in a LOCAL eliminates behind `a.length >= n`. The same loop
/// with a `getfield` limit is proved just as well by the range analysis
/// (`BoundSource::Field`, invariant because the body is heap-stable) but has
/// no local home for the pre-header to load, so the guard cannot be emitted
/// and the check stays.
#[test]
fn local_limit_eliminates_field_limit_refuses() {
    // local limit: 0: iload_0 ; 1: iload_2 ; 2: if_icmpge +13 -> 15 ; ...
    let local_bound: Vec<u8> = vec![
        0x1a, 0x1c, 0xa2, 0x00, 0x0d, 0x2b, 0x1a, 0x2e, 0x57, 0x84, 0x00, 0x01, 0xa7, 0xff,
        0xf4, 0xb1,
    ];
    let (safe, guards) = run(&local_bound, local_bound.len());
    assert_eq!(safe, vec![7]);
    assert_eq!(guards, vec![(0, 1, 2, 0, vec![7])]);

    // field limit: 0: iload_0 ; 1: aload_3 ; 2: getfield #7 ;
    //              5: if_icmpge +13 -> 18 ; 8: aload_1 ; 9: iload_0 ;
    //              10: iaload ; 11: pop ; 12: iinc 0,1 ; 15: goto -15 -> 0 ;
    //              18: return
    let field_bound: Vec<u8> = vec![
        0x1a, 0x2d, 0xb4, 0x00, 0x07, 0xa2, 0x00, 0x0d, 0x2b, 0x1a, 0x2e, 0x57, 0x84, 0x00,
        0x01, 0xa7, 0xff, 0xf1, 0xb1,
    ];
    let (safe2, guards2) = run(&field_bound, field_bound.len());
    assert!(
        safe2.is_empty(),
        "a field limit has no local for the pre-header to load, got {safe2:?}"
    );
    assert!(guards2.is_empty());
}

/// MUST REFUSE: a guarded elision in a loop whose body holds an OSR entry.
///
/// `for (; i < n; i++) { a[i]; for (j = 0; j < m; j++) {} }` -- the outer
/// access is exactly `local_limit_eliminates_field_limit_refuses`' guarded
/// one, but the inner loop's header (pc 11) is a backward-branch target,
/// i.e. a pc the interpreter can OSR into. That entry skips the outer
/// pre-header, and so does every later outer iteration (the back edge
/// targets its far side), so the `a.length >= n` guard would never run.
/// Measured before the fix (`review8a/src/OsrInnerGuard.java`, w7b
/// binary): a store past the end of the array instead of AIOOBE.
#[test]
fn a_guarded_elision_is_refused_when_an_inner_header_can_take_an_osr_entry() {
    // locals: 0=i 1=a 2=n 3=j 4=m
    //  0: iload_0 ; 1: iload_2 ; 2: if_icmpge +27 -> 29      (outer header)
    //  5: aload_1 ; 6: iload_0 ; 7: iaload ; 8: pop           (a[i])
    //  9: iconst_0 ; 10: istore_3                             (j = 0)
    // 11: iload_3 ; 12: iload 4 ; 14: if_icmpge +9 -> 23      (inner header)
    // 17: iinc 3,1 ; 20: goto -9 -> 11
    // 23: iinc 0,1 ; 26: goto -26 -> 0 ; 29: return
    let nested: Vec<u8> = vec![
        0x1a, 0x1c, 0xa2, 0x00, 0x1b, 0x2b, 0x1a, 0x2e, 0x57, 0x03, 0x3e, 0x1d, 0x15, 0x04,
        0xa2, 0x00, 0x09, 0x84, 0x03, 0x01, 0xa7, 0xff, 0xf7, 0x84, 0x00, 0x01, 0xa7, 0xff,
        0xe6, 0xb1,
    ];
    let loops = detect_loops(&nested, nested.len());
    assert!(
        loops.contains(&(0, 26)) && loops.contains(&(11, 20)),
        "{loops:?}"
    );
    let (safe, guards) = run(&nested, nested.len());
    assert!(
        safe.is_empty(),
        "the outer guard cannot be relied on past an inner OSR entry, got {safe:?}"
    );
    assert!(
        guards.is_empty(),
        "and no guard is emitted for it, got {guards:?}"
    );

    // Control: the same outer loop with the inner loop's back edge removed
    // (`goto -9` replaced by three `nop`s, so pc 11 is no longer a
    // backward-branch target) keeps its guarded elision.
    let mut flat = nested.clone();
    flat[20] = 0x00;
    flat[21] = 0x00;
    flat[22] = 0x00;
    let (safe2, guards2) = run(&flat, flat.len());
    assert_eq!(safe2, vec![7]);
    assert_eq!(guards2, vec![(0, 1, 2, 0, vec![7])]);
}

/// The inclusive comparator, both sides of its flag.
///
/// The proof handles `i <= n` as `bound_addend() == 0`: a `length >= n + 1`
/// guard (emitted as JBE) plus `PreheaderGuard::AtMost { n, MAX - 1 }` (the
/// `n != Integer.MAX_VALUE` entry test). It is nevertheless gated OFF by
/// default — the elision measured a ~2x NET LOSS on the Sieve OSR artifact,
/// a code-layout effect that generalising the proof does not change.
#[test]
fn inclusive_loop_is_provable_but_stays_flag_gated() {
    // 0: iload_0 ; 1: iload_2 ; 2: if_icmpgt +13 -> 15   (i <= n)
    // 5: aload_1 ; 6: iload_0 ; 7: iaload ; 8: pop
    // 9: iinc 0,1 ; 12: goto -12 -> 0 ; 15: return
    let code: Vec<u8> = vec![
        0x1a, 0x1c, 0xa3, 0x00, 0x0d, 0x2b, 0x1a, 0x2e, 0x57, 0x84, 0x00, 0x01, 0xa7, 0xff,
        0xf4, 0xb1,
    ];
    let code_len = code.len();

    __set_inclusive_spec_bce_override(Some(false));
    let (off_safe, off_guards) = run(&code, code_len);
    assert!(off_safe.is_empty(), "default-off keeps the check");
    assert!(off_guards.is_empty());

    __set_inclusive_spec_bce_override(Some(true));
    let loops = detect_loops(&code, code_len);
    let (safe, guards) = analyze_bounds_elimination(&code, code_len, &loops);
    __set_inclusive_spec_bce_override(None);
    assert!(safe.contains(&7), "opt-in elides the inclusive access");
    assert_eq!(guards.len(), 1);
    assert!(
        guards[0].inclusive,
        "the guard must record the inclusive form (JBE + `n != MAX_VALUE`)"
    );
    assert_eq!(guards[0].step_local, None);
}

/// The exclusive/inclusive distinction is the `bound_addend`, and it is the
/// ONLY thing that changes the emitted length guard. Pinned here because
/// `GuardShape::covers` reads the addend directly.
#[test]
fn addend_is_the_whole_inclusive_question() {
    let excl = GuardShape {
        iv_local: 0,
        bound_local: Some(2),
        bound_term: BoundTerm::Bound(BoundSource::Local(2)),
        addend: -1,
        step_local: None,
        inline_length: false,
        down_entry: false,
    };
    let incl = GuardShape {
        addend: 0,
        ..excl_clone(&excl)
    };
    let need = |k: i32| {
        PreheaderGuard::LengthAtLeast(SymBound {
            base: BoundTerm::Bound(BoundSource::Local(2)),
            addend: k,
        })
    };
    // Exclusive emits JB: proves `length >= n`, not `length >= n + 1`.
    assert!(excl.covers(&need(0)));
    assert!(!excl.covers(&need(1)));
    // Inclusive emits JBE: proves `length >= n + 1`.
    assert!(incl.covers(&need(0)));
    assert!(incl.covers(&need(1)));
    assert!(!incl.covers(&need(2)));
}

fn excl_clone(g: &GuardShape) -> GuardShape {
    GuardShape {
        iv_local: g.iv_local,
        bound_local: g.bound_local,
        bound_term: g.bound_term.clone(),
        addend: g.addend,
        step_local: g.step_local,
        inline_length: g.inline_length,
        down_entry: g.down_entry,
    }
}

/// The all-or-nothing contract, stated directly.
///
/// Every guard shape the pre-header cannot emit — a non-negativity
/// obligation on anything but the IV's entry value, a length obligation on
/// a DIFFERENT limit than the one the emitter loads, the decreasing loop's
/// `AtLeast`, a stride guard naming a local that is not the recorded step,
/// and the exclusive loop's `AtMost` — must answer `false`, because one
/// uncovered guard refuses the whole proof.
#[test]
fn uncoverable_guards_are_refused() {
    let shape = GuardShape {
        iv_local: 0,
        bound_local: Some(2),
        bound_term: BoundTerm::Bound(BoundSource::Local(2)),
        addend: -1,
        step_local: Some(3),
        inline_length: false,
        down_entry: false,
    };
    let sym = |base: BoundTerm, addend: i32| SymBound { base, addend };

    // Covered: the `iv >= 0` header test.
    assert!(shape.covers(&PreheaderGuard::NonNegative(sym(BoundTerm::IvEntry(0), 0))));
    // Not covered: non-negativity of anything else.
    assert!(!shape.covers(&PreheaderGuard::NonNegative(sym(BoundTerm::IvEntry(1), 0))));
    assert!(!shape.covers(&PreheaderGuard::NonNegative(sym(
        BoundTerm::Bound(BoundSource::Local(2)),
        1
    ))));
    // Not covered: a length obligation against a limit this pre-header does
    // not load. This is the cross-array / foreign-limit case.
    assert!(!shape.covers(&PreheaderGuard::LengthAtLeast(sym(
        BoundTerm::Bound(BoundSource::Local(5)),
        0
    ))));
    assert!(!shape.covers(&PreheaderGuard::LengthAtLeast(sym(
        BoundTerm::Bound(BoundSource::ArrayLength(1)),
        0
    ))));
    // Not covered: the exclusive comparator emits no `n != MAX_VALUE` test,
    // so an IV that can wrap has nothing discharging it.
    assert!(!shape.covers(&PreheaderGuard::AtMost {
        term: sym(BoundTerm::Bound(BoundSource::Local(2)), 0),
        limit: i32::MAX - 1,
    }));
    // Not covered at all: the decreasing loop's `i32::MIN` obligation.
    assert!(!shape.covers(&PreheaderGuard::AtLeast {
        term: sym(BoundTerm::Bound(BoundSource::Local(2)), 0),
        limit: i32::MIN + 1,
    }));
    // Covered: the recorded step, headroom against this loop's own limit.
    assert!(shape.covers(&PreheaderGuard::StrideInRange {
        local: 3,
        headroom: sym(BoundTerm::Bound(BoundSource::Local(2)), -1),
    }));
    // Not covered: a stride guard on a different local.
    assert!(!shape.covers(&PreheaderGuard::StrideInRange {
        local: 4,
        headroom: sym(BoundTerm::Bound(BoundSource::Local(2)), -1),
    }));
    // Not covered: a limit with no local home cannot be loaded at all.
    let homeless = GuardShape {
        bound_local: None,
        ..excl_clone(&shape)
    };
    assert!(!homeless.covers(&PreheaderGuard::LengthAtLeast(sym(
        BoundTerm::Bound(BoundSource::Local(2)),
        0
    ))));
}

/// Pattern B (`if_icmplt <body>` as the back edge) is pre-tested ONLY
/// because the loop is entered through a `goto` that lands at or after the
/// comparison. `analyze_loop_bound` assumed that and never checked it.
/// Here both shapes are built from the same body: the rotated one keeps its
/// elision, the `do { } while` one is reported post-tested and refuses
/// (its untested first iteration puts an unbounded entry value in the span,
/// which needs a `length >= 1` guard the pre-header cannot emit).
#[test]
fn pattern_b_rotation_is_verified_not_assumed() {
    // Rotated (`goto cond`), locals 0=?, 1=arr, 2=n, 3=i:
    // 0: iconst_0 ; 1: istore_3 ; 2: goto +10 -> 12
    // 5: aload_1 ; 6: iload_3 ; 7: iaload ; 8: pop      (body, header=5)
    // 9: iinc 3,1
    // 12: iload_3 ; 13: iload_2 ; 14: if_icmplt -9 -> 5 (back edge)
    // 17: return
    let rotated: Vec<u8> = vec![
        0x03, 0x3e, 0xa7, 0x00, 0x0a, 0x2b, 0x1d, 0x2e, 0x57, 0x84, 0x03, 0x01, 0x1d, 0x1c,
        0xa1, 0xff, 0xf7, 0xb1,
    ];
    let code_len = rotated.len();
    assert_eq!(detect_loops(&rotated, code_len)[0], (5, 14));
    let (safe, guards) = run(&rotated, code_len);
    assert_eq!(safe, vec![7], "the rotated entry lands on the test");
    assert_eq!(guards, vec![(5, 1, 2, 3, vec![7])]);

    // `do { } while`: identical bytes except the entry `goto` is replaced
    // by three `nop`s, so control falls through into the header and the
    // body runs once BEFORE the first test.
    let mut do_while = rotated.clone();
    do_while[2] = 0x00; // nop
    do_while[3] = 0x00; // nop
    do_while[4] = 0x00; // nop
    let (safe2, guards2) = run(&do_while, code_len);
    assert!(
        safe2.is_empty(),
        "a post-tested body executes a[i] before any test, got {safe2:?}"
    );
    assert!(guards2.is_empty());
}
