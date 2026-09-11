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
//! `loop-01-peeling-and-versioning.md` is the lane, and
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
/// compiled as itself.
///
/// This used to be the ONLY way to put a transformed artifact in front of
/// the emitter, because the wired path refused every compile while
/// `deopt_real` was on. It no longer is — see
/// `the_wired_compile_path_reaches_a_loop_under_the_default_configuration`
/// and `a_transformed_methods_published_deopt_bcis_are_interpreter_bcis`,
/// which drive the real wired path. Compiling planner output *as* a method
/// is still the sharper instrument for a pure-provenance question: it
/// isolates the emitter from the planner.
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

/// `walk(N o) { int a = 0; for (int i = 0; i < 5; i++) { a += o.v; o = o.next; }
/// return a; }` — one `int` parameter slot holding a reference, three locals,
/// and the shape the implicit-null test below needs: a receiver that is
/// REASSIGNED in the body, so no dataflow can prove it non-null and both
/// `getfield`s take the implicit check.
fn shape_pointer_walk_loop() -> Vec<u8> {
    vec![
        0x03, 0x3c, // 0: iconst_0; istore_1          a = 0
        0x03, 0x3d, // 2: iconst_0; istore_2          i = 0
        0x1c, 0x08, 0xa2, 0x00, 0x15, // 4: iload_2; iconst_5; if_icmpge 27
        0x1b, 0x2a, 0xb4, 0x00, 0x07, 0x60, 0x3c, // 9: a += o.v
        0x2a, 0xb4, 0x00, 0x0d, 0x4b, // 16: o = o.next
        0x84, 0x02, 0x01, // 21: iinc 2, 1
        0xa7, 0xff, 0xec, // 24: goto 4               <- back edge
        0x1b, 0xac, // 27: iload_1; ireturn
    ]
}

/// How many native offsets inside `cm` are registered implicit-null faulting
/// PCs?
///
/// Asked by RANGE rather than by a `implicit_null::counts()` delta on purpose:
/// the table is process-global and the test harness runs tests concurrently, so
/// a delta counts whatever a sibling test compiled at the same moment. A range
/// scan of this artifact's own code answers only about this artifact.
fn implicit_null_sites_in(cm: &CompiledMethod) -> usize {
    let base = cm.entry as usize;
    let len = cm.code_bytes().len();
    (0..len)
        .filter(|&off| crate::implicit_null::recover(base + off, 0).is_some())
        .count()
}

/// Every COPY of an implicitly-null-checked dereference is registered, not just
/// copy 0.
///
/// # The defect
///
/// A `getfield` whose receiver cannot be proved non-null emits **no test**: the
/// dereference is allowed to fault and `implicit_null::recover` translates the
/// SIGSEGV into a `NullPointerException` by looking the faulting PC up in a
/// table. The native unroller duplicates the body's MACHINE CODE, so each copy
/// contains that dereference at a different PC — and it shifted every other
/// patch vector (`forward_patches`, `bounds_check_stubs`,
/// `exception_check_stubs`, `null_check_store_stubs`, `self_call_patches`,
/// `deopt_stubs`, `jump_table_patches`, `oop_maps`, the IC slots) while this one
/// stayed behind, because it post-dates the Task #60 sweep that built the list.
///
/// The consequence is not a slow path or a missed optimisation. A null receiver
/// in copy 0 threw `NullPointerException`; the same receiver one iteration later
/// was an `EXCEPTION_ACCESS_VIOLATION` reading `0x0F`, i.e. the process died.
/// Measured on the fixture above over a THREE-element list: correct under
/// `CRATONVM_DISABLE_UNROLL=1`, fatal without it, with byte-identical loop
/// bodies in the two artifacts.
///
/// # Why the two arms
///
/// The unrolled count alone proves nothing — it would be satisfied by a build
/// that registered eight sites for two dereferences. The rolled arm pins what
/// one copy costs, and `4 *` is the unroll factor
/// `plan_native_unroll` admits for this 20-byte body (`extra_copies = 3`).
#[test]
fn every_unrolled_copy_of_an_implicit_null_check_is_registered() {
    if !crate::implicit_null::enabled() {
        // The kill switch is a legitimate way to run the suite; with it set
        // nothing registers and both arms are 0, which would pass vacuously.
        return;
    }
    let code = shape_pointer_walk_loop();
    // pc -> (field index, type tag): 11 is `v` (int), 17 is `next` (ref).
    let field_info = vec![(11usize, 0usize, b'I'), (17usize, 1usize, b'L')];
    // pc -> (packed byte offset, is_ref). The compact layout is what puts the
    // arm that takes the implicit check in play at all: its `GC_FLAGS` read at
    // `[RAX + 15]` is the dereference the fault lands on.
    let compact_field_info = vec![(11usize, 0u32, false), (17usize, 8u32, true)];
    // The guarded (default) inline-getfield mode needs a read-bounds helper
    // address to be present. Never called: this test compiles and reads
    // metadata, it does not execute the artifact.
    let mut helpers = JitRuntimeHelpers::default();
    helpers.read_bounds_addr = 0x1000;

    // Through `compile_with_param_slots`, not the legacy `compile()` wrapper:
    // that one passes an EMPTY `method_key`, and `receiver_is_trusted_oop` —
    // the condition the implicit-null arm sits behind — requires a non-empty
    // one. A fixture compiled the legacy way registers nothing at all, which
    // would make both arms of this test read zero.
    let compile_walk = |disable_unroll: bool| -> CompiledMethod {
        cratonvm_types::flags::with_thread_overrides(
            &[(
                "CRATONVM_DISABLE_UNROLL",
                if disable_unroll { Some("1") } else { None },
            )],
            || {
                compile_with_param_slots(
                    &crate::compile_gate::CompileAdmission::for_backend_test(),
                    &code,
                    29,
                    1,     // num_params: (N o)
                    3,     // max_locals: o, a, i
                    false, // needs_heap
                    Vec::new(),
                    field_info.clone(),
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
                    Vec::new(),
                    Vec::new(),
                    Default::default(), // ldc_fp_pcs
                    HashMap::new(),
                    HashMap::new(),
                    &helpers,
                    std::collections::HashSet::new(),
                    HashMap::new(),
                    HashMap::new(),
                    None,
                    &[0],
                    1,
                    1, // param_oop_mask: local 0 is a reference parameter
                    compact_field_info.clone(),
                    "UT3.walk:(LUT3$N;)I",
                    Vec::new(),
                    None,
                )
                .expect("the pointer-walk fixture compiles")
            },
        )
    };

    let rolled = compile_walk(true);
    let rolled_sites = implicit_null_sites_in(&rolled);
    // ONE, not two. The body has two `getfield`s on `o`, and the second is
    // correctly elided: the first dereference proves local 0 non-null on its
    // fall-through, and `o` is not reassigned until after it. The reassignment
    // (`astore_0`) then clears the fact, so the NEXT iteration's first
    // `getfield` is unproven again -- which is exactly why this fixture has an
    // implicit site inside a loop body at all.
    assert_eq!(
        rolled_sites, 1,
        "expected one implicit-null site in the un-unrolled body; found \
         {rolled_sites}. Either the receiver stopped taking the implicit check \
         or the dataflow changed which dereferences are proven -- and the \
         unrolled assertion below then means nothing.",
    );

    let unrolled = compile_walk(false);
    let unrolled_sites = implicit_null_sites_in(&unrolled);
    assert_eq!(
        unrolled_sites,
        4 * rolled_sites,
        "the body was duplicated into 4 copies but {unrolled_sites} faulting \
         PCs are registered instead of {}. An unregistered copy does not \
         throw NullPointerException on a null receiver -- it takes the \
         process down.",
        4 * rolled_sites,
    );
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
    assert!(
        !bytecode_loop_xform_rewrites_bytecode(),
        "guard must disarm"
    );
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
    let v = x
        .versioning
        .as_ref()
        .expect("the accum fixture is versioned");
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

/// The refusal tally must count the four conditions INDEPENDENTLY.
///
/// This is the whole point of `loop-02`'s first increment. The planner returns
/// on the first refusal that holds, so a tally keyed on "which one fired" would
/// record `deopt_real` — default-ON and process-wide — for 100% of compiles and
/// say nothing about the other three, which is exactly the measurement gap the
/// lane exists to close.
///
/// The edit that would trip this: moving any `tally(...)` call below the
/// `return Err(...)` it sits above, or counting the refusal instead of the
/// condition.
#[test]
fn the_tally_counts_all_four_conditions_not_just_the_one_that_refuses() {
    // Counted PER THREAD: the globals are bumped by every compile in this
    // crate's test binary. See `metrics::LoopXformCapture`.
    let tally = crate::metrics::LoopXformCapture::start();
    let code = shape_int_accum_loop();
    // Every condition true at once. Only the last of them refuses, and the
    // tally must still see all four — that independence is what measured the
    // other three out of the way in the first place.
    let all = LoopRewriteShape {
        deopt_real: true,
        precise_exception_frames: true,
        has_indy: true,
        has_inline_sites: true,
    };
    {
        // Armed, so a refusal here is one of the four rather than the arming
        // check, which sits above them.
        let _armed = Armed::new();
        assert_eq!(
            plan_bytecode_loop_xform(&code, 21, &[], &HashMap::new(), all).unwrap_err(),
            LoopRewriteRefusal::InlineSitesPresent,
            "inline sites are the only whole-compile refusal left"
        );
    }
    let counts = |name: &str| tally.count(name);
    assert_eq!(counts("loop_xform_compiles"), 1);
    for name in [
        "loop_xform_deopt_real",
        "loop_xform_precise_exception_frames",
        "loop_xform_invokedynamic",
        "loop_xform_inline_sites",
    ] {
        assert_eq!(counts(name), 1, "{name} was not counted");
    }
    assert_eq!(
        counts("loop_xform_eligible"),
        0,
        "`eligible` is `no whole-compile refusal held`, and inline sites held"
    );
    assert_eq!(
        counts("loop_xform_not_armed"),
        0,
        "a refused compile never reaches the arming check"
    );

    // …and a compile with none of the four set is `eligible`, whether or not
    // anything is armed. That is what makes the row a property of the METHOD.
    tally.reset();
    assert_eq!(
        plan_bytecode_loop_xform(&code, 21, &[], &HashMap::new(), accum_shape_ok()).unwrap_err(),
        LoopRewriteRefusal::NotArmed
    );
    assert_eq!(counts("loop_xform_compiles"), 1);
    assert_eq!(counts("loop_xform_eligible"), 1);
    assert_eq!(counts("loop_xform_not_armed"), 1);
    for name in [
        "loop_xform_deopt_real",
        "loop_xform_precise_exception_frames",
        "loop_xform_invokedynamic",
        "loop_xform_inline_sites",
    ] {
        assert_eq!(counts(name), 0, "{name}");
    }
}

/// The loop-level rows, which need the rewriter armed.
#[test]
fn the_tally_separates_no_candidate_loop_from_a_structural_refusal() {
    let tally = crate::metrics::LoopXformCapture::start();
    let counts = |name: &str| tally.count(name);
    let _armed = Armed::new();

    // A method with no loop the planner will take: counted as
    // `no_candidate_loop`, not as a planner refusal.
    let none = vec![0x03u8, 0xac]; // iconst_0; ireturn
    assert_eq!(
        plan_bytecode_loop_xform(&none, 2, &[], &HashMap::new(), accum_shape_ok()).unwrap_err(),
        LoopRewriteRefusal::NoCandidateLoop
    );
    assert_eq!(counts("loop_xform_eligible"), 1);
    assert_eq!(counts("loop_xform_no_candidate_loop"), 1);
    assert_eq!(counts("loop_xform_planner_refused"), 0);

    // …and a method the planner selects a loop in but the rewriter refuses:
    // the irreducible fixture, which is either skipped as a candidate or
    // refused structurally. Whichever it is, exactly one of the two rows moves.
    tally.reset();
    let irr = shape_irreducible();
    assert!(plan_bytecode_loop_xform(&irr, 21, &[], &HashMap::new(), accum_shape_ok()).is_err());
    assert_eq!(
        counts("loop_xform_no_candidate_loop") + counts("loop_xform_planner_refused"),
        1,
        "exactly one outcome row per refused compile"
    );
    assert_eq!(counts("loop_xform_applied"), 0);
}

/// Three of the four conditions no longer refuse, and the fourth still does.
///
/// `deopt_real`, precise exception frames and `invokedynamic` named ONE
/// problem — a path that hands the VM an emitter pc as a resume bci — and
/// `build_and_record_deopt_point`'s translation answers it for all three at
/// once. Inline sites are not that problem: an inlined callee's bcis are in
/// the CALLEE's space and there is nothing in this method to translate them
/// to, so that one is still a refusal.
///
/// The edit this catches is a re-tightening: putting any of the three back
/// would return `eligible` to zero on real code, which is the state the
/// measurement in `loop-02` exists to have gotten out of.
#[test]
fn planning_admits_the_three_translated_constructs_and_still_refuses_inlining() {
    let _armed = Armed::new();
    let code = shape_int_accum_loop();
    for (label, shape) in [
        (
            "deopt_real",
            LoopRewriteShape {
                deopt_real: true,
                ..accum_shape_ok()
            },
        ),
        (
            "precise exception frames",
            LoopRewriteShape {
                precise_exception_frames: true,
                ..accum_shape_ok()
            },
        ),
        (
            "invokedynamic",
            LoopRewriteShape {
                has_indy: true,
                ..accum_shape_ok()
            },
        ),
    ] {
        assert!(
            plan_bytecode_loop_xform(&code, 21, &[], &HashMap::new(), shape).is_ok(),
            "{label} is translated, not refused"
        );
    }
    assert_eq!(
        plan_bytecode_loop_xform(
            &code,
            21,
            &[],
            &HashMap::new(),
            LoopRewriteShape {
                has_inline_sites: true,
                ..accum_shape_ok()
            },
        )
        .unwrap_err(),
        LoopRewriteRefusal::InlineSitesPresent,
    );
    // Positive control: with none of them set, the same fixture IS admitted,
    // so the refusal above is not vacuous.
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
    assert_eq!(
        x.code_len,
        21 + 5 + 12 + 15,
        "guard + 2 copies + the fallback"
    );
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
            l5.iter()
                .filter(|e| e.4)
                .all(|e| (e.1, e.2, e.3) == (4, 5, 6)),
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
    let bypass = find_bypassable_loop_headers(&x.code, x.code_len, &out_loops, &x.exception_ranges);
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

/// Arming the rewriter is now sufficient, under whatever `deopt_real` this
/// process actually has.
///
/// This test is the inverse of the one it replaces. `deopt_real` was the
/// FIRST of four whole-compile refusals and is default-ON and process-wide,
/// so an armed compile could not reach a loop unless `CRATONVM_DEOPT_REAL`
/// was explicitly off — which no unit test can arrange (the flag snapshot is
/// latched process-wide and this one is additionally cached in a
/// `OnceLock`). Reading `crate::deopt_real_enabled()` rather than hard-coding
/// `false` is what makes this an assertion about the REAL configuration
/// instead of a hypothetical one, and it is why the assertion is
/// unconditional now: the answer must not depend on that flag any more.
#[test]
fn the_wired_compile_path_reaches_a_loop_under_the_default_configuration() {
    let _armed = Armed::new();
    let code = shape_int_accum_loop();
    let real_shape = LoopRewriteShape {
        deopt_real: crate::deopt_real_enabled(),
        precise_exception_frames: false,
        has_indy: false,
        has_inline_sites: false,
    };
    assert!(
        plan_bytecode_loop_xform(&code, 21, &[], &HashMap::new(), real_shape).is_ok(),
        "an armed compile must reach a loop whatever `deopt_real` is set to; \
         `deopt_real={}`",
        crate::deopt_real_enabled(),
    );
}

/// `for (i = 0; i < n; i++) s += i;` followed by an `invokedynamic` whose
/// bootstrap is not `StringConcatFactory`, so its lowering is the
/// unconditional uncommon trap.
///
/// The trap is the one snapshot path in this backend that is NOT gated on
/// `deopt_real_enabled()`, which is exactly why it is the fixture: it records
/// a `DeoptimizationPoint` in a plain unit-test compile, where the flag
/// cannot be turned on. Header 4, body 12 → the static heuristic's 4x arm,
/// and the indy sits in the SUFFIX, so its output pc is shifted by every
/// duplicated body (and by the versioning guard) and cannot coincide with its
/// bci by accident.
fn shape_accum_loop_then_indy() -> Vec<u8> {
    vec![
        0x03, // 0:  iconst_0
        0x3c, // 1:  istore_1
        0x03, // 2:  iconst_0
        0x3d, // 3:  istore_2
        0x1c, // 4:  iload_2             <- header
        0x1a, // 5:  iload_0
        0xa2, 0x00, 0x0d, // 6:  if_icmpge 19
        0x1b, // 9:  iload_1
        0x1c, // 10: iload_2
        0x60, // 11: iadd
        0x3c, // 12: istore_1
        0x84, 0x02, 0x01, // 13: iinc 2, 1
        0xa7, 0xff, 0xf4, // 16: goto 4              <- back edge
        0xba, 0x00, 0x01, 0x00, 0x00, // 19: invokedynamic ()I
        0xac, // 24: ireturn
    ]
}

/// LOOP-02's acceptance criterion: a transformed method's recorded bcis are
/// all in INTERPRETER space.
///
/// This is the whole of what retired the `DeoptRealEnabled`,
/// `PreciseExceptionFrames` and `InvokedynamicPresent` refusals. Each named a
/// path that records `DeoptimizationPoint::bci` from the emitter's own pc,
/// and the VM RESUMES at that field — so while it was an output pc the only
/// safe thing to do was refuse the whole compile.
///
/// Non-vacuous by construction, three ways: `loop_xform_applied` proves the
/// transform actually fired (the trap in measuring this is that arming also
/// turns the native unroller off, so "the code got longer" proves nothing);
/// the published bci is compared against the ORIGINAL indy pc, which the
/// rewrite shifts by 41 bytes; and the unarmed control compiles the same
/// bytes and must publish the same bci, which is what says the assertion is
/// about the translation rather than about this fixture's numbers.
#[test]
fn a_transformed_methods_published_deopt_bcis_are_interpreter_bcis() {
    // Counted per thread; see `metrics::LoopXformCapture`.
    let tally = crate::metrics::LoopXformCapture::start();
    let code = shape_accum_loop_then_indy();
    const INDY_BCI: u32 = 19;

    // Every instruction boundary of the fixture. Under `deopt_real` (the
    // default) a snapshot is recorded at every OSR-eligible pc as well as at
    // the indy trap, so the assertion below is about the whole published set,
    // not just the trap's.
    let boundaries: &[u32] = &[0, 1, 2, 3, 4, 5, 6, 9, 10, 11, 12, 13, 16, 19, 24];

    // Control: unarmed, nothing is rewritten and every bci is its own pc.
    let plain = compile_indy_fixture(&code).expect("the unarmed fixture compiles");
    assert!(
        plain.deopt_points.iter().any(|p| p.bci == INDY_BCI),
        "unarmed, the indy trap records its own pc as its bci",
    );
    for p in &plain.deopt_points {
        assert!(
            boundaries.contains(&p.bci),
            "unarmed: bci {} is not an instruction",
            p.bci
        );
    }

    tally.reset();
    let applied = {
        let _armed = Armed::new();
        let cm = compile_indy_fixture(&code).expect("the armed fixture compiles");
        let counts = |name: &str| tally.count(name);
        assert_eq!(
            counts("loop_xform_applied"),
            1,
            "the transform must actually have fired, or every assertion below \
             is about an untransformed method",
        );
        assert_eq!(
            counts("loop_xform_deopt_bci_unpublishable"),
            0,
            "the artifact must be published, not discarded by the backstop",
        );
        cm
    };

    assert!(
        applied.deopt_points.iter().any(|p| p.bci == INDY_BCI),
        "the indy trap records a snapshot whether or not `deopt_real` is on",
    );
    // THE assertion. The rewrite moves this method's instructions by up to 41
    // bytes; every one of these bcis would be an output pc without the
    // translation, and most of them are not even instruction boundaries of the
    // original method.
    for p in &applied.deopt_points {
        assert!(
            boundaries.contains(&p.bci),
            "a transformed method published bci {}, which is not an instruction \
             boundary of the ORIGINAL method — it is an output pc",
            p.bci,
        );
        assert_eq!(
            p.frame_state.bci, p.bci,
            "the frame state's bci is the one the resume sinks read; it must \
             agree with the point's",
        );
    }
    assert!(
        applied.osr_exit_points.contains(&(INDY_BCI as usize)),
        "the published OSR-exit bci set is in the same space",
    );
    for &bci in &applied.osr_exit_points {
        // Cast: bci fits u32
        assert!(
            boundaries.contains(&(bci as u32)),
            "osr exit bci {bci} is an output pc"
        );
    }
    {
        let mut seen = std::collections::HashSet::new();
        assert!(
            applied.osr_exit_points.iter().all(|b| seen.insert(*b)),
            "the copies collapse onto one bci each; publishing duplicates would \
             make the set `copies + 1` times too long",
        );
    }
}

/// [`compile_bytes`] with one resolved `invokedynamic` at pc 19: zero
/// argument slots, an `int` result and no `StringConcatFactory` bridge, so
/// the lowering takes the uncommon trap. The legacy `compile()` wrapper
/// takes no `indy_info`, so this goes to `compile_with_param_slots`.
fn compile_indy_fixture(code: &[u8]) -> Option<CompiledMethod> {
    compile_with_param_slots(
        &crate::compile_gate::CompileAdmission::for_backend_test(),
        code,
        code.len(),
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
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Default::default(), // ldc_fp_pcs
        HashMap::new(),
        HashMap::new(),
        &JitRuntimeHelpers::default(),
        std::collections::HashSet::new(),
        HashMap::new(),
        HashMap::new(),
        None,
        &[0],
        1,
        0,
        Vec::new(),
        "T.f:(I)I",
        // (pc, arg_slots, ret_type, arg_type_tags, concat_site)
        vec![(19, 0, b'I', Vec::new(), 0)],
        // elidable_init_pcs: fixture resolves no constant pool.
        None,
    )
}

/// The publishability check refuses what the translation cannot describe.
///
/// Three cases, each fatal to the METHOD rather than to the transform (which
/// is already emitted by the time it runs): a point on the versioning guard's
/// synthetic bytes, a point whose published bci is not what the provenance
/// map says, and two copies of one bci that describe different frames.
/// Refusing costs a compilation; publishing an output pc as a resume bci
/// resumes arbitrary bytecode.
#[test]
fn the_publishability_check_refuses_a_point_the_translation_cannot_describe() {
    let _armed = Armed::new();
    let code = shape_int_accum_loop();
    let x = plan_bytecode_loop_xform(&code, 21, &[], &HashMap::new(), accum_shape_ok())
        .expect("the fixture is admitted");
    let (guard_from, guard_to) = x.guard_span().expect("the fixture versions");

    // A well-formed point: inside the region, published bci = provenance.
    let body_pc = x.fast_base() + 1;
    let body_bci = x.bci_at(body_pc).expect("provenance is total");
    // Cast: original bci fits u32
    let point = |bci: usize| test_deopt_point(bci as u32);
    let ok = point(body_bci);
    assert!(
        rewritten_deopt_points_are_publishable(&x, &[ok], &[body_pc], 21).is_ok(),
        "a point whose published bci is its provenance is publishable",
    );

    // The guard's bytes carry the header's bci so provenance stays total, but
    // they are an image of no instruction: resuming there would re-enter the
    // interpreter at the header with the guard's own operands live.
    assert!(guard_to > guard_from);
    let on_guard = point(x.bci_at(guard_from).expect("total"));
    let err = rewritten_deopt_points_are_publishable(&x, &[on_guard], &[guard_from], 21)
        .expect_err("a point on the guard is not publishable");
    assert!(err.contains("versioning guard"), "{err}");

    // A raw output pc published as a bci — what a future emit path that skips
    // `orig_bci` would produce.
    // Cast: output pc fits u32 in this fixture
    let raw = test_deopt_point(body_pc as u32);
    let err = rewritten_deopt_points_are_publishable(&x, &[raw], &[body_pc], 21)
        .expect_err("an untranslated pc is not publishable");
    assert!(err.contains("provenance says"), "{err}");

    // Two copies of one bytecode that disagree about a field a bci-keyed
    // consumer takes ON TRUST. `reason` is the grouping key, so the field that
    // can actually differ under one key is one of the others.
    let other_pc = body_pc + x.body_len;
    assert_eq!(
        x.bci_at(other_pc),
        Some(body_bci),
        "same bytecode, next copy"
    );
    let mut conflicts = test_deopt_point(body_bci as u32);
    conflicts.speculation_id = 7;
    assert_eq!(conflicts.reason, point(body_bci).reason, "same group");
    let err = rewritten_deopt_points_are_publishable(
        &x,
        &[point(body_bci), conflicts],
        &[body_pc, other_pc],
        21,
    )
    .expect_err("conflicting copies are not publishable");
    assert!(err.contains("disagreeing"), "{err}");

    // A machine-LOCATION difference is not a disagreement at all. Two copies
    // differ in their operand spill offsets by construction — the walk hands
    // them out as it emits — and both are right for their own copy.
    let mut relocated = test_deopt_point(body_bci as u32);
    relocated.frame_state.stack = vec![crate::deopt::FrameValue::StackSlot(-64)];
    let mut elsewhere = test_deopt_point(body_bci as u32);
    elsewhere.frame_state.stack = vec![crate::deopt::FrameValue::StackSlot(-72)];
    assert!(
        rewritten_deopt_points_are_publishable(
            &x,
            &[relocated, elsewhere],
            &[body_pc, other_pc],
            21,
        )
        .is_ok(),
        "one `int` operand in two different spill slots is the SAME contract",
    );

    // A slot-KIND difference is reported, not refused: the only bci-keyed
    // reader of it is the OSR entry contract, which re-verifies every slot
    // against the live interpreter frame. `IndyDeoptProbe.concatLoop` is the
    // real case — its two unrolled copies disagree about local 3 at the
    // `invokedynamic`, because the forward oop dataflow reaches copy 1 through
    // copy 0's `astore_3`. Refusing it discarded the method for nothing.
    let mut retyped = test_deopt_point(body_bci as u32);
    retyped.frame_state.locals = vec![crate::deopt::FrameValue::RegisterRef(12)];
    let mut untyped = test_deopt_point(body_bci as u32);
    untyped.frame_state.locals = vec![crate::deopt::FrameValue::Register(12)];
    assert!(
        rewritten_deopt_points_are_publishable(&x, &[retyped, untyped], &[body_pc, other_pc], 21,)
            .is_ok(),
        "a slot-kind divergence is counted, not refused",
    );

    // …and identical copies are fine, which is the normal case.
    assert!(rewritten_deopt_points_are_publishable(
        &x,
        &[point(body_bci), point(body_bci)],
        &[body_pc, other_pc],
        21,
    )
    .is_ok());

    // A misaligned bookkeeping pair is fatal on its own.
    let err = rewritten_deopt_points_are_publishable(&x, &[point(body_bci)], &[], 21)
        .expect_err("a missing emitter pc is fatal");
    assert!(err.contains("emitter pcs"), "{err}");
}

/// A minimal `DeoptimizationPoint` at `bci`, with the frame state the checker
/// compares. `native_offset` is deliberately constant: it is the one field
/// two copies of a bytecode are SUPPOSED to differ in.
fn test_deopt_point(bci: u32) -> crate::deopt::DeoptimizationPoint {
    let reason = crate::deopt::DeoptReason::UnreachedCode;
    crate::deopt::DeoptimizationPoint {
        native_offset: 0,
        bci,
        reason,
        action: crate::deopt::DeoptAction::Reinterpret,
        semantics: crate::deopt::ResumeSemantics::for_reason(reason),
        speculation_id: 0,
        frame_state: crate::deopt::FrameState {
            method_key: "T.f:(I)I".to_string(),
            bci,
            locals: Vec::new(),
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        },
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
/// "Every entry" is not "every pc": since `abcfaec38` only a pc whose abstract
/// expression stack is EMPTY gets one, and this test pins both halves of that.
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
    assert_eq!(
        rebuilt.len(),
        22,
        "one slot per interpreter bci, plus the end"
    );
    // WHICH pcs may be entered at all, since `abcfaec38` (2026-09-04): an OSR
    // entry pc must have an EMPTY abstract expression stack. Entering
    // mid-expression lets the prologue materialise the pending operands once,
    // correctly, for the entering iteration - and then every later iteration
    // replays those frozen slots while the index advances, which is a
    // wrong-VALUE bug no termination test can see. HotSpot has the same rule.
    //
    // Spelled out for this fixture rather than derived, because the depth at
    // each pc IS the property:
    //
    //   4   iload_2    []        <- header, enterable
    //   5   iload_0    [i]
    //   6   if_icmpge  [i, n]
    //   9   iload_1    []        <- enterable
    //   10  iload_2    [s]
    //   11  iadd       [s, i]
    //   12  istore_1   [s+i]
    //   13  iinc       []        <- enterable
    //   16  goto       []        <- enterable, and the back edge
    const ENTERABLE: [usize; 4] = [4, 9, 13, 16];
    const MID_EXPRESSION: [usize; 5] = [5, 6, 10, 11, 12];

    // Versioning has no back-edge gap: the fallback is a full image of the
    // region, so every ENTERABLE pc in it keeps an entry - including the back
    // edge itself, where a plain 4x unroll answers `-1` because its steady
    // state is copy 0, which ends just before it.
    for bci in ENTERABLE {
        assert!(
            rebuilt[bci] >= 0,
            "bci {bci}: versioning must not refuse OSR at an empty-stack pc \
             inside the region"
        );
    }
    // BOTH DIRECTIONS, and this half is the one that earns its place. Asserting
    // only the loop above passes just as well against a build that dropped the
    // empty-stack rule and went back to publishing every pc - which is exactly
    // the wrong-value bug that rule fixed. Until 2026-09-05 this test asserted
    // the OPPOSITE of this loop, over every instruction in the region, and it
    // was the only thing red on dev when the rule landed.
    for bci in MID_EXPRESSION {
        assert_eq!(
            rebuilt[bci], -1,
            "bci {bci}: an OSR entry pc must have an empty expression stack"
        );
    }
    // The two lists have to PARTITION the region's real instruction
    // boundaries, or either could quietly name a pc that is not one - a
    // mid-instruction byte reads as `-1` and would satisfy the loop above for
    // the wrong reason.
    let mut bci = 4usize;
    let mut walked: Vec<usize> = Vec::new();
    while bci <= 16 {
        walked.push(bci);
        bci += bytecode_len_at(&code, bci);
    }
    assert_eq!(bci, 19, "the walk must land past the back edge");
    let mut listed: Vec<usize> = ENTERABLE
        .iter()
        .chain(MID_EXPRESSION.iter())
        .copied()
        .collect();
    listed.sort_unstable();
    assert_eq!(
        walked, listed,
        "the enterable and mid-expression lists must together be exactly the \
         region's instruction boundaries"
    );
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
    for bci in ENTERABLE {
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

/// `CRATONVM_JIT_NO_OSR_EMPTY_STACK_ENTRY=1` really does turn the rule off.
///
/// The switch was added 2026-09-05 to close the last open item on the
/// retired `osr-miscompiles-cachecoherence-20260904` write-up: the rule is
/// default-on CODEGEN that landed without a failure of its own, and re-opening
/// the question should not need a rebuild. A declared flag that no test arms
/// is a flag that might be inert, and every other test in this file would stay
/// green if it were — so this asserts the difference the flag makes, on the
/// same fixture and the same five pcs the test above pins as REFUSED.
///
/// `MID_EXPRESSION` is respelled rather than shared: the two tests must be
/// able to disagree. If the list above is edited to match a regression, this
/// one still names the pcs the rule was written for and fails.
#[test]
fn the_empty_stack_rule_has_an_off_switch_and_it_is_not_inert() {
    // Depth at each pc, from the fixture's own table in the test above:
    //   5 [i]  6 [i, n]  10 [s]  11 [s, i]  12 [s+i]
    const MID_EXPRESSION: [usize; 5] = [5, 6, 10, 11, 12];

    let rebuild = || {
        let _armed = Armed::new();
        let code = shape_int_accum_loop();
        let x = plan_bytecode_loop_xform(&code, 21, &[], &HashMap::new(), accum_shape_ok())
            .expect("versioned unroll");
        let art = compile_bytes(&x.code, x.code_len).expect("compiles");
        let out_osr = art
            .osr_pc_to_native
            .as_ref()
            .expect("the artifact publishes OSR entries");
        x.rebuild_pc_to_native(out_osr, 21)
    };

    // Default: refused, which is what `a_versioned_artifact_publishes_its_osr_
    // entries_inside_the_fallback_copy` also asserts. Repeated here so a
    // failure of this test says which half moved.
    let on = rebuild();
    for bci in MID_EXPRESSION {
        assert_eq!(on[bci], -1, "bci {bci}: refused with the rule ON");
    }

    // Off: every one of them gets an entry again. Thread-scoped, so this does
    // not disturb tests running in parallel in the same process — which is
    // also why `osr_empty_stack_entry_enabled` must not latch in a `OnceLock`.
    let off = cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_JIT_NO_OSR_EMPTY_STACK_ENTRY", Some("1"))],
        rebuild,
    );
    for bci in MID_EXPRESSION {
        assert!(
            off[bci] >= 0,
            "bci {bci}: the rule is off, so this pc must be enterable again — \
             the flag reached no read site"
        );
    }

    // And nothing else moved: the enterable pcs are unaffected either way, so
    // the difference above is the rule and not a wholesale change of artifact.
    for bci in [4usize, 9, 13, 16] {
        assert_eq!(
            on[bci], off[bci],
            "bci {bci}: an empty-stack pc is enterable under both settings"
        );
    }
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
    assert_eq!(
        rebuilt.len(),
        22,
        "one slot per interpreter bci, plus the end"
    );
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

    // Part 2 — the compile path now REACHES part 1's territory, and the
    // answer there is different because the artifact it produces is
    // VERSIONED.
    //
    // This half used to assert that the planner refused the compile outright
    // (four whole-compile refusals ahead of any loop selection, `deopt_real`
    // first), so `loop_xform` was `None` and the artifact was an ordinary
    // un-rewritten one. Three of those refusals are gone, and this fixture is
    // now rewritten by the wired path.
    //
    // The gap is still refused *for an unversioned unroll* — that is part 1,
    // and it is the invariant. It is not visible here because
    // `plan_versioned` proves `trip >= 4` for this loop and emits the
    // transform behind a pre-header guard, whose failing edge is an UNTOUCHED
    // image of the region, back edge included. `steady_state_base` is that
    // fallback copy, so every bci in the region — the back edge's included —
    // has a steady-state image and keeps its entry. That is the versioning
    // arm's whole point: an OSR-entered method runs the untouched loop.
    //
    // The trap this test has always been about still applies: arming ALSO
    // disables the native byte-copy unroller, so "the code length changed"
    // does not prove a bytecode transform happened. `loop_xform_applied` is
    // what proves it.
    let baseline =
        compile_accum_fixture().expect("the helper-free fixture must compile on the default path");
    let tally = crate::metrics::LoopXformCapture::start();
    let armed = {
        let _armed = Armed::new();
        compile_accum_fixture().expect("the armed fixture must still compile")
    };
    let applied = tally.count("loop_xform_applied");
    drop(tally);
    assert_eq!(
        applied, 1,
        "the wired path must now produce a rewritten artifact for this fixture",
    );
    let base_osr = baseline
        .osr_pc_to_native
        .as_ref()
        .expect("the fixture publishes OSR entries");
    let armed_osr = armed
        .osr_pc_to_native
        .as_ref()
        .expect("the armed artifact publishes OSR entries");
    assert_eq!(
        base_osr.len(),
        22,
        "baseline: one slot per bci, plus the end"
    );
    assert_eq!(
        armed_osr.len(),
        22,
        "armed: same length — the rewrite is 57 bytes longer, and the published \
         table is rebuilt in INTERPRETER-bci space, so its length must not move",
    );
    let x = {
        let _armed = Armed::new();
        plan_bytecode_loop_xform(&code, 21, &[], &HashMap::new(), accum_shape_ok())
            .expect("the fixture is admitted")
    };
    assert!(
        x.versioning.is_some(),
        "this fixture versions; the assertion below is about the fallback copy",
    );
    assert!(
        armed_osr[16] >= 0,
        "under versioning the steady state is the untouched fallback copy, so \
         the back-edge bci keeps an entry. An UNVERSIONED unroll refuses it — \
         that is part 1.",
    );
}
