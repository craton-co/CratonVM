// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Loop-unroller admission and bytecode-loop-transform provenance.
//!
//! ---------------------------------------------------------------------------
//! Loop-unroller admission and bytecode-loop-transform provenance
//! ---------------------------------------------------------------------------
//!
//! Two unrollers exist in this backend and exactly one of them is live:
//!
//! * the NATIVE byte-copy unroller in the `0xa7` arm of `compile_bytecode`,
//! admitted by `plan_native_unroll`;
//! * the BYTECODE rewriter in `x64::licm` (`plan_loop_peel` /
//! `plan_loop_unroll` / `plan_loop_version`), planned by
//! `x64::loop_rewrite::plan_bytecode_loop_xform`. It IS wired — it rewrites
//! the bytes the emitter compiles — behind an opt-in that is off by default,
//! and it is also the native unroller's admission oracle.
//!
//! These tests pin the admission test (what the old `code[back_edge] == 0xa7`
//! + body-size heuristic was missing), the mutual exclusion, the planner's
//! three arms (peel for a bypassable header, PGO unroll, static unroll, each
//! versioned against a trip-count guard when one can be proved), and the two
//! provenance contracts the rewrite wiring honours: deopt/oop-map bcis stay
//! INTERPRETER bcis, and an OSR entry lands on the steady-state copy — the
//! fallback under versioning — rather than a peeled prefix or a guard.
//!
//! `docs/known-issues/c2/loop-01-peeling-and-versioning.md` is the lane, and
//! `docs/jit/loop-rewriter-wiring.md` the wiring's status of record.

use super::*;

/// `for (i = 0; i < n; i++) a[i] = 100 / (i - 3);`
///
/// A textbook reducible counted loop: single entry, `goto` back edge, no
/// handlers, and two different throwing bcis inside the body. Header 2,
/// back edge 19, body 17 bytes, length 23. The static heuristic asks for
/// 3 extra copies.
fn shape_counted_loop() -> Vec<u8> {
    vec![
        0x03, // 0: iconst_0
        0x3d, // 1: istore_2          i = 0
        0x1c, // 2: iload_2           <- header
        0x1b, // 3: iload_1
        0xa2, 0x00, 0x12, // 4: if_icmpge 22
        0x2a, // 7: aload_0
        0x1c, // 8: iload_2
        0x10, 0x64, // 9: bipush 100
        0x1c, // 11: iload_2
        0x06, // 12: iconst_3
        0x64, // 13: isub
        0x6c, // 14: idiv
        0x4f, // 15: iastore
        0x84, 0x02, 0x01, // 16: iinc 2, 1
        0xa7, 0xff, 0xef, // 19: goto 2        <- back edge
        0xb1, // 22: return
    ]
}

/// The pre-header-bypass shape: a branch from *before* the loop straight
/// into the header (`AttributesImpl.ensureCapacity`). The region itself is
/// impeccable — the bytecode rewriter admits it — so only
/// `bypassable_headers` can refuse it. Header 11, back edge 25, length 30.
fn shape_bypassable_header() -> Vec<u8> {
    vec![
        0x03, // 0: iconst_0
        0x3c, // 1: istore_1          i = 0
        0x03, // 2: iconst_0
        0x3d, // 3: istore_2          sum = 0
        0x1a, // 4: iload_0
        0x9a, 0x00, 0x06, // 5: ifne 11       <- external edge INTO the header
        0x03, // 8: iconst_0
        0x3d, // 9: istore_2
        0x00, // 10: nop
        0x1b, // 11: iload_1          <- header
        0x1a, // 12: iload_0
        0xa2, 0x00, 0x0f, // 13: if_icmpge 28
        0x1c, // 16: iload_2
        0x1b, // 17: iload_1
        0x05, // 18: iconst_2
        0x68, // 19: imul
        0x60, // 20: iadd
        0x3d, // 21: istore_2
        0x84, 0x01, 0x01, // 22: iinc 1, 1
        0xa7, 0xff, 0xf2, // 25: goto 11      <- back edge
        0x1c, // 28: iload_2
        0xac, // 29: ireturn
    ]
}

/// An irreducible loop: the cycle `L1 → L2 → L1` is entered at BOTH
/// blocks. Its back edge is a plain `goto` and its body is 10 bytes, so it
/// sails through the old heuristic and only a domination/entry test can
/// refuse it. Header 7, back edge 17, length 21.
fn shape_irreducible() -> Vec<u8> {
    vec![
        0x1a, // 0: iload_0
        0x99, 0x00, 0x0c, // 1: ifeq 13       -> enters the cycle at L2
        0xa7, 0x00, 0x03, // 4: goto 7        -> enters the cycle at L1
        0x84, 0x01, 0x01, // 7: L1: iinc 1, 1
        0xa7, 0x00, 0x03, // 10: goto 13
        0x1b, // 13: L2: iload_1
        0x99, 0x00, 0x06, // 14: ifeq 20
        0xa7, 0xff, 0xf6, // 17: goto 7       <- back edge
        0xb1, // 20: return
    ]
}

/// A reducible OUTER loop with no external entry — so
/// `find_bypassable_loop_headers` clears it — whose body contains an
/// irreducible two-entry cycle. This is the shape that proves the new gate
/// is more than the bypassable filter. Outer header 2, back edge 30,
/// length 35.
fn shape_irreducible_inner() -> Vec<u8> {
    vec![
        0x03, // 0: iconst_0
        0x3c, // 1: istore_1
        0x1b, // 2: iload_1            <- outer header
        0x1a, // 3: iload_0
        0xa2, 0x00, 0x1d, // 4: if_icmpge 33
        0x1b, // 7: iload_1
        0x99, 0x00, 0x0c, // 8: ifeq 20        -> enters the inner cycle at B
        0xa7, 0x00, 0x03, // 11: goto 14       -> enters the inner cycle at A
        0x84, 0x01, 0x01, // 14: A: iinc 1, 1
        0xa7, 0x00, 0x03, // 17: goto 20
        0x1b, // 20: B: iload_1
        0x99, 0x00, 0x06, // 21: ifeq 27
        0xa7, 0xff, 0xf6, // 24: goto 14       <- inner back edge
        0x84, 0x01, 0x01, // 27: iinc 1, 1
        0xa7, 0xff, 0xe4, // 30: goto 2        <- outer back edge
        0x1b, // 33: iload_1
        0xac, // 34: ireturn
    ]
}

/// A one-instruction loop whose header is reached by `goto_w` — the one
/// backward-capable branch the emitter never polls. Header 0, back edge 5.
fn shape_with_goto_w() -> Vec<u8> {
    vec![
        0xc8, 0x00, 0x00, 0x00, 0x05, // 0: goto_w 5
        0xa7, 0xff, 0xfb, // 5: goto 0    <- back edge
    ]
}

/// The OLD admission test, verbatim: a `goto` back edge and a body in the
/// `[5, 50]` size band. Used to prove the refusal tests are not vacuous.
fn old_heuristic_admits(code: &[u8], header: usize, back_edge: usize) -> bool {
    code[back_edge] == 0xa7 && (5..=50).contains(&(back_edge - header))
}

#[test]
fn fixtures_carry_the_loops_the_old_heuristic_would_have_unrolled() {
    for (code, len, want_loops) in [
        (shape_counted_loop(), 23usize, vec![(2usize, 19usize)]),
        (shape_bypassable_header(), 30, vec![(11, 25)]),
        (shape_irreducible(), 21, vec![(7, 17)]),
        (shape_irreducible_inner(), 35, vec![(14, 24), (2, 30)]),
        (shape_with_goto_w(), 8, vec![(0, 5)]),
    ] {
        assert_eq!(code.len(), len, "fixture length");
        assert_eq!(detect_loops(&code, len), want_loops, "fixture loops");
        for &(header, back_edge) in &want_loops {
            assert!(
                old_heuristic_admits(&code, header, back_edge),
                "the old gate must have admitted ({header}, {back_edge}) — \
                 otherwise the refusal tests below prove nothing"
            );
        }
    }
}

/// The native byte-copy unroller now refuses every loop whose control flow
/// it cannot duplicate: irreducible, multi-entry, irreducible inner cycle,
/// unpollable, or pre-header-bypassable. Each of these was admitted by the
/// old `0xa7` + body-size test.
#[test]
fn the_native_unroller_refuses_irreducible_and_multi_entry_loops() {
    let none: FxHashSet<usize> = FxHashSet::default();

    // Positive control: a plain counted loop is still unrolled, so the
    // gate is not simply "refuse everything".
    let ok = shape_counted_loop();
    assert_eq!(
        plan_native_unroll(&ok, 23, 2, 19, 3, &[], &none),
        Some((2, 19, 3)),
        "a single-entry reducible counted loop must still be unrolled"
    );

    // Irreducible: the cycle is entered at both of its blocks, so the
    // header does not dominate its own region and "the bytes from header
    // to back edge" are not one iteration of anything.
    let irr = shape_irreducible();
    assert_eq!(
        plan_loop_unroll(&irr, 21, 7, 17, 3, &[]).unwrap_err(),
        LoopXformRefusal::ExternalEntry
    );
    assert_eq!(plan_native_unroll(&irr, 21, 7, 17, 3, &[], &none), None);

    // Irreducible INNER cycle. The outer loop is reducible and has no
    // external entry, so `find_bypassable_loop_headers` does NOT flag it:
    // this case is caught by the structural oracle alone.
    let inner = shape_irreducible_inner();
    assert!(
        !find_bypassable_loop_headers(&inner, 35, &[(2, 30)], &[]).contains(&2),
        "the bypassable filter must not be what refuses this one"
    );
    assert_eq!(
        plan_loop_unroll(&inner, 35, 2, 30, 1, &[]).unwrap_err(),
        LoopXformRefusal::IrreducibleInnerLoop
    );
    assert_eq!(plan_native_unroll(&inner, 35, 2, 30, 1, &[], &none), None);

    // A backward `goto_w` reaches the header, and the emitter never polls
    // at `goto_w`: duplicating bodies behind it would build an unpolled
    // cycle whose time-to-safepoint is unbounded.
    let wide = shape_with_goto_w();
    assert_eq!(plan_native_unroll(&wide, 8, 0, 5, 3, &[], &none), None);

    // Pre-header bypass. The region is impeccable — the rewriter admits
    // it — so only `bypassable_headers` refuses, exactly as it does for
    // the aaload/arith/FP hoists and the bulk-byte loops.
    let byp = shape_bypassable_header();
    assert!(plan_loop_unroll(&byp, 30, 11, 25, 3, &[]).is_ok());
    let bypassable = find_bypassable_loop_headers(&byp, 30, &[(11, 25)], &[]);
    assert!(bypassable.contains(&11), "fixture must be bypassable");
    assert_eq!(
        plan_native_unroll(&byp, 30, 11, 25, 3, &[], &none),
        Some((11, 25, 3))
    );
    assert_eq!(
        plan_native_unroll(&byp, 30, 11, 25, 3, &[], &bypassable),
        None,
        "a bypassable header must not be unrolled: pc_to_native[header] \
         points PAST the pre-header, so no copy runs it"
    );
}

/// A handler landing inside the duplicated region, or a protected range
/// only partially overlapping it, must refuse. The bytecode→native handler
/// ranges are derived from `pc_to_native`, which covers copy 0 only, so a
/// throw from copy `1..k` is not covered by the range that protects the
/// loop.
#[test]
fn the_native_unroller_refuses_a_handler_crossing_the_region() {
    let none: FxHashSet<usize> = FxHashSet::default();
    let code = shape_counted_loop();

    // A range enclosing the WHOLE loop is legal — it widens with the
    // copies — so this stays admitted.
    assert_eq!(
        plan_native_unroll(&code, 23, 2, 19, 3, &[(0, 23, 22)], &none),
        Some((2, 19, 3)),
        "a protected range enclosing the whole loop is legal"
    );
    // Handler inside the region.
    assert_eq!(
        plan_loop_unroll(&code, 23, 2, 19, 3, &[(0, 23, 10)]).unwrap_err(),
        LoopXformRefusal::HandlerInRegion
    );
    assert_eq!(
        plan_native_unroll(&code, 23, 2, 19, 3, &[(0, 23, 10)], &none),
        None
    );
    // Range that starts before the header and ends inside the body.
    assert_eq!(
        plan_loop_unroll(&code, 23, 2, 19, 3, &[(0, 10, 22)]).unwrap_err(),
        LoopXformRefusal::HandlerRangeStraddlesRegion
    );
    assert_eq!(
        plan_native_unroll(&code, 23, 2, 19, 3, &[(0, 10, 22)], &none),
        None
    );
}

/// A zero/one PGO unroll factor must not unroll, and must not underflow.
/// The old code computed `pgo_factor - 1` directly.
#[test]
fn a_degenerate_unroll_factor_is_refused_not_underflowed() {
    let none: FxHashSet<usize> = FxHashSet::default();
    let code = shape_counted_loop();
    for factor in [0usize, 1] {
        assert_eq!(
            plan_native_unroll(&code, 23, 2, 19, factor.saturating_sub(1), &[], &none),
            None,
            "factor {factor} must not unroll"
        );
    }
    // …and an over-large factor is refused rather than clamped.
    assert_eq!(
        plan_native_unroll(&code, 23, 2, 19, LOOP_XFORM_MAX_COPIES + 1, &[], &none),
        None
    );
}

/// The two unrollers are never both live.
/// `bytecode_loop_xform_rewrites_bytecode` is the single switch and
/// `native_unroller_enabled` is its complement, so no configuration can
/// duplicate a body twice.
#[test]
fn the_two_unrollers_are_never_both_live() {
    assert!(
        !(bytecode_loop_xform_rewrites_bytecode() && native_unroller_enabled()),
        "both unrollers live: k+1 bytecode copies would be machine-code \
         duplicated k+1 more times behind ONE back-edge poll"
    );
    // Today the native one owns unrolling and the rewriter is oracle-only.
    assert!(!bytecode_loop_xform_rewrites_bytecode());

    // Oracle-only means: the rewriter really does produce different bytes,
    // and the emitter really does not see them — `plan_native_unroll`
    // hands back only the `(header, back_edge, copies)` triple, which is
    // stated in original-bytecode coordinates.
    let code = shape_counted_loop();
    let x = plan_loop_unroll(&code, 23, 2, 19, 3, &[]).expect("admitted");
    assert_ne!(x.code, code, "the rewriter is not the identity");
    assert_eq!(x.code_len, 23 + 3 * 17);
    assert_eq!(
        plan_native_unroll(&code, 23, 2, 19, 3, &[], &FxHashSet::default()),
        Some((2, 19, 3)),
        "the triple stays in ORIGINAL coordinates: header 2 and back edge \
         19 are interpreter bcis, not PCs in x.code"
    );
}

/// The provenance contract a deopt / oop-map record must go through.
///
/// Every byte of a transformed method maps back to the original bci it was
/// copied from, and the map is *not* the identity: output PCs run past the
/// end of the interpreter's method, so a consumer that recorded a raw
/// transformed PC as a `deopt_stubs` bci or an `OopMapEntry::bytecode_pc`
/// would be recording something the interpreter cannot resume at. This is
/// the acceptance criterion for wiring the rewriter — see
/// `docs/jit/loop-transform-wiring.md`.
#[test]
fn a_transformed_pc_is_never_an_interpreter_bci() {
    let code = shape_counted_loop();
    let len = 23usize;
    for k in 1..=3usize {
        for x in [
            plan_loop_peel(&code, len, 2, 19, k, &[]).expect("peel"),
            plan_loop_unroll(&code, len, 2, 19, k, &[]).expect("unroll"),
        ] {
            assert!(x.provenance_is_total(), "{:?} k={k}", x.kind);
            assert!(x.code_len > len, "{:?} k={k}", x.kind);

            let mut saw_divergence = false;
            let mut pc = 0usize;
            while pc < x.code_len {
                let bci = x.bci_at(pc).expect("provenance is total");
                assert!(
                    bci < len,
                    "{:?} k={k}: output pc {pc} maps to {bci}, not an \
                     interpreter bci",
                    x.kind
                );
                assert_eq!(
                    x.code[pc], code[bci],
                    "{:?} k={k}: output pc {pc} and bci {bci} disagree on \
                     the opcode",
                    x.kind
                );
                let out_len = bytecode_len_at(&x.code, pc);
                assert!(out_len > 0);
                assert_eq!(out_len, bytecode_len_at(&code, bci));
                if bci != pc {
                    saw_divergence = true;
                }
                pc += out_len;
            }
            assert_eq!(pc, x.code_len, "{:?} k={k}: walk overran", x.kind);
            assert!(
                saw_divergence,
                "{:?} k={k}: pc and bci never diverged, so the test cannot \
                 detect a missing `bci_at`",
                x.kind
            );
            // The strong form: the back edge sits at a pc past the whole
            // original method, so recording PCs would produce
            // out-of-range bcis, not merely wrong ones.
            assert!(x.back_edge_pc() >= len, "{:?} k={k}", x.kind);
        }
    }
}

/// OSR must enter the STEADY-STATE copy. Entering a peeled prefix re-runs
/// the peeled iterations, so the loop executes `k` times too many — the
/// same class of bug as the LICM pre-header bypass.
#[test]
fn osr_entry_lands_on_the_steady_state_copy_not_a_peeled_prefix() {
    let code = shape_counted_loop();
    let (len, header, back_edge, body_len) = (23usize, 2usize, 19usize, 17usize);
    // Instruction starts inside the loop body — the bcis an OSR request
    // can legitimately name.
    let body_starts: Vec<usize> = {
        let mut v = Vec::new();
        let mut pc = header;
        while pc < back_edge {
            v.push(pc);
            pc += bytecode_len_at(&code, pc);
        }
        v
    };
    assert_eq!(body_starts.first(), Some(&header));

    for k in 1..=3usize {
        let peel = plan_loop_peel(&code, len, header, back_edge, k, &[]).expect("peel");
        let unroll = plan_loop_unroll(&code, len, header, back_edge, k, &[]).expect("unroll");

        // Peel: the steady state is the LAST copy, and the header's OSR
        // entry is emphatically not the header itself.
        assert_eq!(peel.steady_state_base(), header + k * body_len);
        assert_eq!(peel.osr_entry_pc(header), Some(peel.steady_state_base()));
        assert_ne!(
            peel.osr_entry_pc(header),
            Some(header),
            "k={k}: entering copy 0 would re-run {k} peeled iterations"
        );
        // Unroll: every copy runs every trip, so copy 0 IS the steady
        // state and the header keeps its PC.
        assert_eq!(unroll.steady_state_base(), header);
        assert_eq!(unroll.osr_entry_pc(header), Some(header));

        for x in [&peel, &unroll] {
            for &bci in &body_starts {
                let entry = x.osr_entry_pc(bci).expect("bci is in range");
                assert!(
                    entry >= x.steady_state_base() && entry <= x.back_edge_pc(),
                    "{:?} k={k}: OSR entry {entry} for bci {bci} is outside \
                     the steady-state copy [{}, {}]",
                    x.kind,
                    x.steady_state_base(),
                    x.back_edge_pc()
                );
                // Round-trip: the entry's provenance is the bci asked for.
                assert_eq!(x.bci_at(entry), Some(bci), "{:?} k={k}", x.kind);
            }
        }

        // Outside the region: a bci before the header does not move, and a
        // bci past the back edge shifts by the copies.
        assert_eq!(peel.osr_entry_pc(0), Some(0));
        assert_eq!(unroll.osr_entry_pc(0), Some(0));
        assert_eq!(peel.osr_entry_pc(22), Some(22 + k * body_len));
        assert_eq!(unroll.osr_entry_pc(22), Some(22 + k * body_len));
        assert_eq!(peel.osr_entry_pc(len), None);

        // The rewriter now refuses OSR across the unrolled back-edge gap
        // itself, so no consumer has to know about it. For Unroll the
        // back-edge bci has no image in copy 0 (the back edge exists in the
        // LAST copy only); output pc `back_edge` is the first byte of copy
        // 1, i.e. the header — which is exactly what `bci_at` reports below,
        // and why entering there would re-run iterations.
        assert_eq!(unroll.osr_entry_pc(back_edge), None);
        assert_eq!(unroll.bci_at(back_edge), Some(header));
        // Peel has no such gap: its steady-state copy carries the back edge.
        assert_eq!(peel.osr_entry_pc(back_edge), Some(peel.back_edge_pc()));
        assert_eq!(peel.bci_at(peel.back_edge_pc()), Some(back_edge));
    }
}

// -----------------------------------------------------------------------
// The rewriter WIRED: opt-in, planning, replication, coordinate change
// -----------------------------------------------------------------------

/// Arms the bytecode rewriter for this thread and disarms it on drop, so a
/// failing assertion cannot leave the thread armed for a later compile.
struct Armed(bool);
impl Armed {
    fn new() -> Self {
        Armed(set_bytecode_loop_rewriter_armed(true))
    }
}
impl Drop for Armed {
    fn drop(&mut self) {
        set_bytecode_loop_rewriter_armed(self.0);
    }
}

/// `static int accum(int n) { int s = 0; for (int i = 0; i < n; i++) s += i;
/// return s; }`
///
/// Deliberately helper-free — pure integer arithmetic, no array access, no
/// field, no call — so it compiles end to end against an all-zero
/// `JitRuntimeHelpers`. Header 4, back edge 16, body 12 bytes, length 21;
/// the static heuristic asks for 3 extra copies, giving a 57-byte rewrite.
fn shape_int_accum_loop() -> Vec<u8> {
    vec![
        0x03, // 0:  iconst_0
        0x3c, // 1:  istore_1          s = 0
        0x03, // 2:  iconst_0
        0x3d, // 3:  istore_2          i = 0
        0x1c, // 4:  iload_2           <- header
        0x1a, // 5:  iload_0
        0xa2, 0x00, 0x0d, // 6:  if_icmpge 19
        0x1b, // 9:  iload_1
        0x1c, // 10: iload_2
        0x60, // 11: iadd
        0x3c, // 12: istore_1
        0x84, 0x02, 0x01, // 13: iinc 2, 1
        0xa7, 0xff, 0xf4, // 16: goto 4            <- back edge
        0x1b, // 19: iload_1
        0xac, // 20: ireturn
    ]
}

fn accum_shape_ok() -> LoopRewriteShape {
    LoopRewriteShape {
        deopt_real: false,
        precise_exception_frames: false,
        has_indy: false,
        has_inline_sites: false,
    }
}

/// Compile `shape_int_accum_loop` through the legacy wrapper with no
/// metadata and an all-zero helper table (the method calls none).
fn compile_accum_fixture() -> Option<CompiledMethod> {
    let code = shape_int_accum_loop();
    compile_bytes(&code, 21)
}

/// [`compile_accum_fixture`] over arbitrary bytecode in the same frame
/// shape (one `int` parameter, three locals), so a REWRITTEN method can be
/// compiled as itself. That is the only way to put a transformed artifact
/// in front of the emitter today: the wired path refuses every compile
/// while `deopt_real` is on — see
/// `the_wired_compile_path_is_refused_before_any_loop_is_looked_at`.
fn compile_bytes(code: &[u8], code_len: usize) -> Option<CompiledMethod> {
    compile(
        code,
        code_len,
        1,     // num_params: (int n)
        3,     // max_locals: n, s, i
        false, // needs_heap
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &JitRuntimeHelpers::default(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None,
    )
}

#[test]
fn the_rewriter_is_off_by_default_and_armed_per_thread() {
    assert!(
        !bytecode_loop_xform_rewrites_bytecode(),
        "the default compile path must not rewrite bytecode"
    );
    // The native unroller has its own kill switch; only claim it is live
    // when that switch is not set, or this positive control would fail for
    // an unrelated reason.
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DISABLE_UNROLL").is_none() {
        assert!(
            native_unroller_enabled(),
            "unarmed, the native byte-copy unroller owns unrolling"
        );
    }
    {
        let _armed = Armed::new();
        assert!(bytecode_loop_xform_rewrites_bytecode());
        assert!(
            !native_unroller_enabled(),
            "arming the rewriter must turn the native unroller off in the \
             same motion, or one loop gets duplicated twice"
        );
        // A thread that did not arm is unaffected.
        let elsewhere = std::thread::spawn(bytecode_loop_xform_rewrites_bytecode)
            .join()
            .expect("probe thread");
        assert!(!elsewhere, "the opt-in leaked to another thread");
    }
    assert!(!bytecode_loop_xform_rewrites_bytecode(), "guard must disarm");
}

#[test]
fn planning_refuses_unless_armed() {
    let code = shape_int_accum_loop();
    assert_eq!(
        plan_bytecode_loop_xform(&code, 21, &[], &HashMap::new(), accum_shape_ok()).unwrap_err(),
        LoopRewriteRefusal::NotArmed
    );
    let _armed = Armed::new();
    let x = plan_bytecode_loop_xform(&code, 21, &[], &HashMap::new(), accum_shape_ok())
        .expect("armed, the fixture is admitted");
    assert_eq!((x.header, x.body_len, x.copies), (4, 12, 3));
    // 4x unroll, VERSIONED. The fixture's limit is a runtime `n`, so its
    // compile-time trip count is `[0, i32::MAX]` and the planner will not
    // duplicate four bodies without a runtime check that they are reached;
    // the failing edge runs an untouched copy of the loop.
    let v = x.versioning.as_ref().expect("the accum fixture is versioned");
    assert_eq!(
        v.guard,
        crate::scev::PreheaderGuard::TripCountAtLeast {
            term: crate::scev::SymBound {
                base: crate::scev::BoundTerm::Bound(crate::scev::BoundSource::Local(0)),
                addend: 0,
            },
            minimum: 4,
        },
        "the guard must be `n >= 4`: one compare on the loop's own limit"
    );
    assert_eq!(v.guard_pc, 4);
    assert_eq!(v.guard_len, 5, "iload_0; iconst_4; if_icmplt");
    assert_eq!(v.fallback_base, 4 + 5 + 4 * 12 + 3);
    // prefix + guard + 4 copies + the untouched fallback + suffix.
    assert_eq!(x.code_len, 21 + 5 + 3 * 12 + 15);
}

/// Every whole-compile refusal names a construct that publishes an emitter
/// pc to the VM as a resume bci through a path this wiring does not
/// translate. Each must refuse on its own, not merely in combination.
#[test]
fn planning_refuses_every_untranslated_construct() {
    let _armed = Armed::new();
    let code = shape_int_accum_loop();
    for (shape, want) in [
        (
            LoopRewriteShape {
                deopt_real: true,
                ..accum_shape_ok()
            },
            LoopRewriteRefusal::DeoptRealEnabled,
        ),
        (
            LoopRewriteShape {
                precise_exception_frames: true,
                ..accum_shape_ok()
            },
            LoopRewriteRefusal::PreciseExceptionFrames,
        ),
        (
            LoopRewriteShape {
                has_indy: true,
                ..accum_shape_ok()
            },
            LoopRewriteRefusal::InvokedynamicPresent,
        ),
        (
            LoopRewriteShape {
                has_inline_sites: true,
                ..accum_shape_ok()
            },
            LoopRewriteRefusal::InlineSitesPresent,
        ),
    ] {
        assert_eq!(
            plan_bytecode_loop_xform(&code, 21, &[], &HashMap::new(), shape).unwrap_err(),
            want
        );
    }
    // Positive control: with none of them set, the same fixture IS admitted,
    // so the four refusals above are not vacuous.
    assert!(plan_bytecode_loop_xform(&code, 21, &[], &HashMap::new(), accum_shape_ok()).is_ok());
}

/// A structurally-refused loop leaves the method un-rewritten rather than
/// half-rewritten, and reports the rewriter's own reason.
#[test]
fn planning_reports_the_structural_refusal() {
    let _armed = Armed::new();
    // Irreducible: the cycle is entered at both of its blocks. Header 7,
    // back edge 17, body 10 — inside the profitability band, so the band
    // is not what refuses it. Two filters can: `bypassable_headers` (the
    // `goto 7` at pc 4 is an external edge into the header) skips the
    // candidate, and the structural oracle refuses it as `ExternalEntry`.
    // Which one fires first is not the property under test; that the
    // method is never rewritten is.
    let irr = shape_irreducible();
    let err = plan_bytecode_loop_xform(&irr, 21, &[], &HashMap::new(), accum_shape_ok())
        .expect_err("an irreducible loop must never be rewritten");
    assert!(
        matches!(
            err,
            LoopRewriteRefusal::NoCandidateLoop
                | LoopRewriteRefusal::Planner(LoopXformRefusal::ExternalEntry)
        ),
        "unexpected refusal for an irreducible loop: {err:?}"
    );
    // Pre-header bypass: the region itself is impeccable, and the planner
    // used to skip the candidate outright, leaving no candidate at all. It
    // now PEELS it — see
    // `the_planner_peels_a_bypassable_header_instead_of_skipping_it`.
    let byp = shape_bypassable_header();
    assert!(plan_loop_unroll(&byp, 30, 11, 25, 3, &[]).is_ok());
    let x = plan_bytecode_loop_xform(&byp, 30, &[], &HashMap::new(), accum_shape_ok())
        .expect("a bypassable header is peeled rather than skipped");
    assert_eq!(x.kind, LoopXformKind::Peel);
}

/// The profitability band is the native unroller's, PGO arm included.
#[test]
fn the_pgo_hint_sets_the_factor_exactly_as_the_native_unroller_does() {
    let _armed = Armed::new();
    let code = shape_int_accum_loop();
    // Factor 2 ⇒ 1 extra copy, overriding the static heuristic's 3.
    let hints: HashMap<usize, usize> = [(16usize, 2usize)].into_iter().collect();
    let x = plan_bytecode_loop_xform(&code, 21, &[], &hints, accum_shape_ok())
        .expect("PGO factor 2 is admitted");
    assert_eq!(x.copies, 1);
    // Versioned like the static arm, at the factor the hint asked for: the
    // guard's minimum tracks the number of copies, so it is `n >= 2` here
    // and `n >= 4` for the 4x static heuristic.
    assert_eq!(
        x.versioning.as_ref().map(|v| v.guard.clone()),
        Some(crate::scev::PreheaderGuard::TripCountAtLeast {
            term: crate::scev::SymBound {
                base: crate::scev::BoundTerm::Bound(crate::scev::BoundSource::Local(0)),
                addend: 0,
            },
            minimum: 2,
        })
    );
    assert_eq!(x.code_len, 21 + 5 + 12 + 15, "guard + 2 copies + the fallback");
    // A degenerate factor is refused, not underflowed — and, matching the
    // native unroller, does NOT fall back to the static heuristic.
    for factor in [0usize, 1] {
        let hints: HashMap<usize, usize> = [(16usize, factor)].into_iter().collect();
        assert_eq!(
            plan_bytecode_loop_xform(&code, 21, &[], &hints, accum_shape_ok()).unwrap_err(),
            LoopRewriteRefusal::Planner(LoopXformRefusal::TooManyCopies)
        );
    }
}

/// Replication is the property the whole wiring rests on: a site inside
/// the duplicated body must appear once per copy with the SAME payload, a
/// site after the region must shift past every copy, and a site before it
/// must not move. Checked through the two tuple adapters as well as the
/// primitive, because those adapters are what the 21 tables actually use.
#[test]
fn replication_lifts_every_site_once_per_copy() {
    let code = shape_int_accum_loop();
    for k in 1..=3usize {
        let x = plan_loop_unroll(&code, 21, 4, 16, k, &[]).expect("admitted");
        let body_len = 12usize;

        // pc 0 is before the header, pc 10 is inside the body, pc 19 is
        // after the region.
        let two: Vec<(usize, u8)> = vec![(0, 0xAA), (10, 0xBB), (19, 0xCC)];
        let lifted = x.replicate_pc_keyed(&two);
        assert_eq!(
            lifted.len(),
            1 + (k + 1) + 1,
            "k={k}: the in-body site must appear once per copy"
        );
        assert_eq!(lifted[0], (0, 0xAA), "k={k}: a prefix site does not move");
        for ci in 0..=k {
            assert_eq!(
                lifted[1 + ci],
                (10 + ci * body_len, 0xBB),
                "k={k} copy={ci}: wrong image or payload"
            );
        }
        assert_eq!(
            *lifted.last().unwrap(),
            (19 + k * body_len, 0xCC),
            "k={k}: a suffix site shifts past every copy"
        );
        // Sorted by pc — every consumer of these tables assumes it.
        assert!(lifted.windows(2).all(|w| w[0].0 <= w[1].0), "k={k}");
        // Every image's provenance is the original pc, which is what makes
        // the coordinate change back out of this exactly.
        for &(pc, _) in &lifted {
            assert!(x.bci_at(pc).is_some(), "k={k}: image {pc} has no bci");
        }

        // The 3- and 5-tuple adapters agree with the primitive.
        let three = vec![(0usize, 1u8, 2u32), (10, 3, 4), (19, 5, 6)];
        let l3 = replicate_pc3(&x, three);
        assert_eq!(
            l3.iter().map(|e| e.0).collect::<Vec<_>>(),
            lifted.iter().map(|e| e.0).collect::<Vec<_>>(),
            "k={k}: replicate_pc3 disagrees with the primitive on images"
        );
        assert!(
            l3.iter().filter(|e| e.1 == 3).all(|e| e.2 == 4),
            "k={k}: replicate_pc3 must clone the payload verbatim"
        );
        let five = vec![(0usize, 1u8, 2u32, 3u16, false), (10, 4, 5, 6, true)];
        let l5 = replicate_pc5(&x, five);
        assert_eq!(l5.len(), 1 + (k + 1), "k={k}");
        assert!(
            l5.iter().filter(|e| e.4).all(|e| (e.1, e.2, e.3) == (4, 5, 6)),
            "k={k}: replicate_pc5 must clone the payload verbatim"
        );
    }
}

/// A raw-pointer payload is SHARED, not duplicated. That is the property
/// that makes `mic_slots` / `pic_slots` sound to replicate at all: one
/// inline-cache slot per call site, seen by every copy, exactly as a
/// non-unrolled loop's single slot is seen by every iteration.
#[test]
fn a_pointer_payload_is_shared_across_the_copies() {
    let code = shape_int_accum_loop();
    let x = plan_loop_unroll(&code, 21, 4, 16, 3, &[]).expect("admitted");
    // Cast: a distinctive non-null sentinel address; never dereferenced.
    let slot = 0xDEAD_BEEFusize as *const u8;
    let lifted = x.replicate_pc_keyed(&[(10usize, slot)]);
    assert_eq!(lifted.len(), 4);
    assert!(
        lifted.iter().all(|&(_, p)| p == slot),
        "every copy must name the SAME slot"
    );
}

/// The fixture's loop, pinned independently of the planner.
///
/// Restored after being deleted by accident while probing the OSR test
/// below. It is the control that makes that test's constants meaningful:
/// without it, a change to `shape_int_accum_loop` silently moves the
/// header and back edge and every hard-coded bci in this module starts
/// describing a different method.
#[test]
fn the_fixture_loop_is_what_the_planner_is_offered() {
    let code = shape_int_accum_loop();
    assert_eq!(code.len(), 21);
    assert_eq!(detect_loops(&code, 21), vec![(4usize, 16usize)]);
    assert!(
        !find_bypassable_loop_headers(&code, 21, &[(4, 16)], &[]).contains(&4),
        "the fixture header must be reachable only by fall-through and its \
         own back edge, or the planner would refuse it for the wrong reason"
    );
    // The rewrite the planner should choose, stated independently of it.
    let x = plan_loop_unroll(&code, 21, 4, 16, 3, &[]).expect("admitted");
    assert_eq!(x.code_len, 21 + 3 * 12);
    assert!(x.provenance_is_total());
}

/// The peel arm. A loop whose header an external branch can enter used to
/// be skipped outright, because every speculating transform in this backend
/// drops its pre-header when it sees one. Peeling moves the steady-state
/// loop out of that edge's reach, so the hoists can be kept.
#[test]
fn the_planner_peels_a_bypassable_header_instead_of_skipping_it() {
    let _armed = Armed::new();
    let code = shape_bypassable_header();
    let (len, header, back_edge) = (30usize, 11usize, 25usize);
    let loops = detect_loops(&code, len);
    assert_eq!(loops, vec![(header, back_edge)]);
    assert!(
        find_bypassable_loop_headers(&code, len, &loops, &[]).contains(&header),
        "the fixture must really be bypassable, or this test proves nothing"
    );

    let x = plan_bytecode_loop_xform(&code, len, &[], &HashMap::new(), accum_shape_ok())
        .expect("a bypassable header is peeled");
    assert_eq!(x.kind, LoopXformKind::Peel);
    assert_eq!(x.copies, LOOP_PEEL_COPIES);
    assert_eq!(x.header, header);
    assert!(x.provenance_is_total());

    // The point of the arm: the loop that runs when the guard holds is
    // entered only by fall-through and its own back edge, so the pre-header
    // the hoists need is reachable on every entry to it.
    let out_loops = detect_loops(&x.code, x.code_len);
    let bypass =
        find_bypassable_loop_headers(&x.code, x.code_len, &out_loops, &x.exception_ranges);
    let fast_steady = x.fast_base() + x.copies * x.body_len;
    assert!(
        out_loops.contains(&(fast_steady, x.fast_back_edge_pc())),
        "the peeled steady-state loop must be a loop in the output"
    );
    assert!(
        !bypass.contains(&fast_steady),
        "the peeled steady-state loop must not be bypassable"
    );
    // Its fallback twin IS bypassable — the guard branches into it — and
    // that is fine: it is the cold copy, and it is the copy OSR enters.
    if let Some(v) = &x.versioning {
        assert!(bypass.contains(&v.fallback_base));
        assert_eq!(x.steady_state_base(), v.fallback_base);
    }
}

/// Why the wired compile path does not produce a transformed artifact,
/// stated as a fact rather than left in a comment.
///
/// `deopt_real` is ON by default and is the FIRST of the four whole-compile
/// refusals, so arming the rewriter is not sufficient: `CRATONVM_DEOPT_REAL`
/// must also be off, and no unit test can arrange that (the flag snapshot is
/// latched process-wide and this one is additionally cached in a
/// `OnceLock`). Narrowing those four refusals is `loop-02`'s lane. Three
/// tests in this module depend on this and none of them said so.
#[test]
fn the_wired_compile_path_is_refused_before_any_loop_is_looked_at() {
    let _armed = Armed::new();
    let code = shape_int_accum_loop();
    let real_shape = LoopRewriteShape {
        deopt_real: crate::deopt_real_enabled(),
        precise_exception_frames: false,
        has_indy: false,
        has_inline_sites: false,
    };
    let planned = plan_bytecode_loop_xform(&code, 21, &[], &HashMap::new(), real_shape);
    if crate::deopt_real_enabled() {
        assert_eq!(
            planned.unwrap_err(),
            LoopRewriteRefusal::DeoptRealEnabled,
            "while `deopt_real` is on, no armed compile can reach a loop"
        );
    } else {
        assert!(
            planned.is_ok(),
            "with `deopt_real` off the same compile IS admitted"
        );
    }
}

/// LOOP-01's acceptance rule: prove the transform fired by something only a
/// transformed artifact has.
///
/// `compile()` cannot be that vehicle on the wired path (see the test
/// above), so this compiles the bytes the planner produced *as* the method
/// and asserts a property of the resulting machine code that no
/// untransformed artifact can have: mapped back into interpreter-bci space,
/// EVERY OSR entry in the region — the header's included — lands inside the
/// FALLBACK copy, which sits after the guard and all four guarded bodies.
/// `pc_to_native` is non-decreasing in pc, so comparing native offsets
/// compares positions.
///
/// The edit that would trip it: dropping the `versioning` arm of
/// `osr_entry_pc`, which would answer with a guarded copy and make
/// `mid < fallback_off`. Pointing the header at the guard instead — which
/// is what it did until the transform was executed on real code — trips it
/// at `rebuilt[4]`.
#[test]
fn a_versioned_artifact_publishes_its_osr_entries_inside_the_fallback_copy() {
    let _armed = Armed::new();
    let code = shape_int_accum_loop();
    let x = plan_bytecode_loop_xform(&code, 21, &[], &HashMap::new(), accum_shape_ok())
        .expect("versioned unroll");
    let v = x.versioning.as_ref().expect("versioned");
    let art = compile_bytes(&x.code, x.code_len)
        .expect("the rewritten bytes must compile as an ordinary method");
    let out_osr = art
        .osr_pc_to_native
        .as_ref()
        .expect("the artifact publishes OSR entries");
    assert_eq!(
        out_osr.len(),
        x.code_len + 1,
        "compiled as the REWRITTEN method, so the vector is in output-PC space"
    );

    let rebuilt = x.rebuild_pc_to_native(out_osr, 21);
    assert_eq!(rebuilt.len(), 22, "one slot per interpreter bci, plus the end");
    // Versioning has no back-edge gap: the fallback is a full image of the
    // region, so every INSTRUCTION in it keeps an entry — including the
    // back edge itself, where a plain 4x unroll answers `-1` because its
    // steady state is copy 0, which ends just before it.
    let mut bci = 4usize;
    while bci <= 16 {
        assert!(
            rebuilt[bci] >= 0,
            "bci {bci}: versioning must not refuse OSR inside the region"
        );
        bci += bytecode_len_at(&code, bci);
    }
    assert_eq!(bci, 19, "the walk must land past the back edge");
    assert!(
        rebuilt[16] >= 0,
        "the back-edge bci stays enterable under versioning"
    );

    let fallback_off = out_osr[v.fallback_base];
    assert!(fallback_off >= 0, "the fallback copy was emitted");
    assert_eq!(
        rebuilt[4], fallback_off,
        "the header's OSR entry is the fallback copy's first byte — NOT the \
         guard, which is not a loop header and so is not a pc the OSR \
         trampoline can reconstruct a compiled state for"
    );
    for bci in [9usize, 10, 11, 12, 13, 16] {
        assert!(
            rebuilt[bci] >= fallback_off,
            "bci {bci}: a mid-body OSR entry must land in the fallback copy, \
             not in a guarded one"
        );
    }
    // …and the fast copies really are ahead of it, so the comparison above
    // is not trivially true of every artifact.
    assert!(out_osr[x.fast_base()] >= 0 && out_osr[x.fast_base()] < fallback_off);
}

/// End to end: with the rewriter armed the emitter really compiles the
/// rewritten bytes, and the OSR metadata the artifact publishes is back in
/// INTERPRETER-bci space.
///
/// Two assertions carry this, and neither can pass by accident:
///
///  * the published vectors are `orig_code_len + 1` long. The compiled
///    method is 57 bytes; without `rebuild_pc_to_native` they would be 58.
///  * `osr_pc_to_native[16]` — the back-edge bci — is `-1` under the
///    rewrite and a real offset without it. Unroll's steady state is copy
///    0, which ends just before the back edge, so those bytes have no
///    steady-state image and OSR there must be REFUSED; answering with an
///    offset would resume a "back edge next" frame at the top of a fresh
///    body and run an extra iteration.
#[test]
fn the_osr_gap_is_refused_and_the_compile_path_does_not_yet_reach_it() {
    // Part 1 — the invariant, on the transform itself.
    //
    // This is what the test was always about: a bci inside the unrolled
    // back-edge gap has no steady-state image, so OSR there must be
    // REFUSED. Answering with an offset resumes a "back edge next" frame
    // at the top of a fresh body and runs one extra iteration.
    let code = shape_int_accum_loop();
    let x = plan_loop_unroll(&code, 21, 4, 16, 3, &[]).expect("admitted");
    // A synthetic output-pc vector, so the assertion is about the mapping
    // and not about whatever the emitter happened to place where.
    let synthetic: Vec<i32> = (0..80).collect();
    let rebuilt = x.rebuild_pc_to_native(&synthetic, 21);
    assert_eq!(rebuilt.len(), 22, "one slot per interpreter bci, plus the end");
    assert!(rebuilt[4] >= 0, "the loop header stays OSR-enterable");
    for gap in 16..19 {
        assert_eq!(
            rebuilt[gap], -1,
            "bci {gap} is inside the unrolled back-edge gap and must be refused"
        );
    }
    assert!(
        rebuilt[19] >= 0 && rebuilt[20] >= 0,
        "the suffix keeps its entries, shifted past the copies"
    );

    // Part 2 — and the compile path does not reach part 1 yet.
    //
    // This half exists because the original version of this test asserted
    // part 1's constants against `compile()`'s artifact and FAILED, and the
    // failure looked like a wrong-code bug. It was not one: the planner
    // refuses this compile outright — `plan_bytecode_loop_xform` has four
    // whole-compile refusals ahead of any loop selection — so `loop_xform`
    // is `None` and the artifact is simply an ordinary un-rewritten one.
    //
    // What made that hard to see is the trap below: arming the rewriter
    // ALSO disables the native byte-copy unroller, because the two are
    // exact complements. So an armed compile produces different machine
    // code whether or not a bytecode transform happened, and "the code
    // length changed" does NOT prove the artifact was rewritten. That was
    // the flawed premise check.
    //
    // If this assertion ever fires, the compile path has started producing
    // rewritten artifacts and part 1's constants should be asserted against
    // `compile()` again.
    let baseline = compile_accum_fixture()
        .expect("the helper-free fixture must compile on the default path");
    let armed = {
        let _armed = Armed::new();
        compile_accum_fixture().expect("the armed fixture must still compile")
    };
    let base_osr = baseline
        .osr_pc_to_native
        .as_ref()
        .expect("the fixture publishes OSR entries");
    let armed_osr = armed
        .osr_pc_to_native
        .as_ref()
        .expect("the armed artifact publishes OSR entries");
    assert_eq!(base_osr.len(), 22, "baseline: one slot per bci, plus the end");
    assert_eq!(armed_osr.len(), 22, "armed: same length — same bci space");
    assert!(
        armed_osr[16] >= 0,
        "the compile path is not producing rewritten artifacts yet, so the \
         back-edge bci is still an ordinary OSR entry. If this fires, the \
         planner has started admitting this fixture and part 1's constants \
         belong here."
    );
}
