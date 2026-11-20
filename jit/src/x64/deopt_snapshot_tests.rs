use super::frame_value_for_slot;
use crate::deopt::FrameValue;

#[test]
fn frame_value_for_slot_maps_provenance() {
    // Register-resident wins (provenance is the GPR), oop-ness aside.
    assert_eq!(
        frame_value_for_slot(Some(3), None, 16, false),
        FrameValue::Register(3)
    );
    assert_eq!(
        frame_value_for_slot(Some(3), None, 16, true),
        FrameValue::Register(3)
    );
    // XMM-resident: not resolvable in Phase A.
    assert_eq!(
        frame_value_for_slot(None, Some(9), 16, false),
        FrameValue::Unsupported
    );
    // Spilled primitive vs spilled oop -> StackSlot vs StackSlotRef at [rbp-off].
    assert_eq!(
        frame_value_for_slot(None, None, 16, false),
        FrameValue::StackSlot(-16)
    );
    assert_eq!(
        frame_value_for_slot(None, None, 24, true),
        FrameValue::StackSlotRef(-24)
    );
}

use super::{
    classify_local_kinds, code_uses_long_float_double, opcode_touches_long_float_double,
    pure_high_halves, refine_ambiguous_local_kinds, typed_local_frame_value,
    wide_local_high_halves, LocalKind,
};

#[test]
fn fu2_wide_fp_gate_detects_long_float_double() {
    // Pure int/ref method: iload_0; iconst_1; iadd; ireturn → no wide/FP.
    let int_only = [0x1a, 0x04, 0x60, 0xac];
    assert!(!code_uses_long_float_double(&int_only, int_only.len()));
    // A long: lload_0; lconst_1; ladd; lstore_0; return.
    let with_long = [0x1e, 0x0a, 0x61, 0x3f, 0xb1];
    assert!(code_uses_long_float_double(&with_long, with_long.len()));
    // A float: fload_0; freturn.
    let with_float = [0x22, 0xae];
    assert!(code_uses_long_float_double(&with_float, with_float.len()));
    // A double constant: ldc2_w #idx; dreturn.
    let with_double = [0x14, 0x00, 0x05, 0xaf];
    assert!(code_uses_long_float_double(&with_double, with_double.len()));
    // wide-prefixed dload: wide; dload idx16; dreturn.
    let wide_dload = [0xc4, 0x18, 0x00, 0x03, 0xaf];
    assert!(code_uses_long_float_double(&wide_dload, wide_dload.len()));
    // Per-opcode spot checks: i-arith excluded, l/f/d-arith included.
    assert!(!opcode_touches_long_float_double(0x60)); // iadd
    assert!(opcode_touches_long_float_double(0x61)); // ladd
    assert!(opcode_touches_long_float_double(0x62)); // fadd
    assert!(opcode_touches_long_float_double(0x63)); // dadd
    assert!(!opcode_touches_long_float_double(0x6c)); // idiv
    assert!(opcode_touches_long_float_double(0x6d)); // ldiv
    assert!(!opcode_touches_long_float_double(0x74)); // ineg
    assert!(opcode_touches_long_float_double(0x75)); // lneg
    assert!(!opcode_touches_long_float_double(0x36)); // istore
    assert!(opcode_touches_long_float_double(0x37)); // lstore
}

#[test]
fn classify_local_kinds_per_opcode() {
    // Indexed load/store forms exercising each kind + cat-2 high-halves.
    // local7 is never touched -> Unknown.
    let code = [
        0x15, 0x00, // iload 0   -> Int@0
        0x37, 0x01, // lstore 1  -> Long@1, HighHalf@2
        0x38, 0x03, // fstore 3  -> Float@3
        0x39, 0x04, // dstore 4  -> Double@4, HighHalf@5
        0x3a, 0x06, // astore 6  -> Ref@6
        0xb1, // return
    ];
    let kinds = classify_local_kinds(&code, code.len(), 8);
    assert_eq!(kinds[0], LocalKind::Int);
    assert_eq!(kinds[1], LocalKind::Long);
    assert_eq!(kinds[2], LocalKind::HighHalf);
    assert_eq!(kinds[3], LocalKind::Float);
    assert_eq!(kinds[4], LocalKind::Double);
    assert_eq!(kinds[5], LocalKind::HighHalf);
    assert_eq!(kinds[6], LocalKind::Ref);
    assert_eq!(kinds[7], LocalKind::Unknown);
}

/// A method with MORE THAN 64 LOCALS gets NO oop mask at all — and
/// `classify_local_kinds` is what covers for it.
///
/// `compute_local_oop_masks` returns empty vectors for `max_locals > 64`, so
/// `build_and_record_deopt_point`'s `is_oop` reads false for EVERY local in
/// such a method (slot 0 included, not just the ones above 63). The only
/// thing that then publishes a reference local as a reference rather than as
/// a truncating `StackSlot` is the `LocalKind::Ref` arm — and that is sound
/// exactly because this classifier is a per-slot `Vec`, with no bitset and
/// no cap.
///
/// Both halves are asserted together on purpose. Either one alone reads as a
/// property of a helper; together they are the invariant a reader of that
/// call site needs, and the pairing is what fails if someone "optimises"
/// `classify_local_kinds` into a `u64` to match its neighbours.
///
/// Witnessed end-to-end by `probes/HighLocalOopProbe.java` (84 locals,
/// references at slots 74/75/76, OSR-entered, 100 forced collections):
/// `oop_reached=false oop_mask=0x0` while those three slots published
/// `StackSlotRef`. Its `probes/LowLocalOopProbe.java` twin, identical but
/// under 64 locals, reports `oop_reached=true oop_mask=0x700000e`.
#[test]
fn classify_local_kinds_types_a_reference_above_slot_63() {
    // wide astore 74; wide aload 74; wide istore 70; return
    let code = [
        0xc4, 0x3a, 0x00, 0x4a, // wide astore 74 -> Ref@74
        0xc4, 0x19, 0x00, 0x4a, // wide aload  74 -> Ref@74
        0xc4, 0x36, 0x00, 0x46, // wide istore 70 -> Int@70
        0xb1, // return
    ];
    let kinds = classify_local_kinds(&code, code.len(), 84);
    assert_eq!(
        kinds[74],
        LocalKind::Ref,
        "a reference local above slot 63 must still be classified Ref — it is the \
         ONLY thing that publishes it as a reference in a >64-local method, because \
         `compute_local_oop_masks` gives that method no mask at all"
    );
    assert_eq!(kinds[70], LocalKind::Int);
    assert_eq!(kinds[83], LocalKind::Unknown);

    // THE PREMISE, REWRITTEN 2026-09-03 AS THIS TEST ASKED.
    //
    // It used to assert that nothing answers above 64 locals, and warned
    // that if anything ever started answering, "the deopt snapshot's
    // `is_oop` gains a second source and this test's premise needs
    // rewriting rather than deleting". `CRATONVM_JIT_WIDE_LOCAL_OOP_MAPS`
    // is that change, so here is the rewrite.
    //
    // Two entry points now, and the split is the invariant:
    //
    //   * the SPLICE path (`compute_local_oop_masks`) still answers nothing
    //     above 64, because `InlineOopScope::masks` is a single `u64` by
    //     construction and a spliced callee past the cliff must keep failing
    //     its safepoints closed through `mask_at_cur() == None`;
    //   * the METHOD path (`compute_local_oop_masks_windowed`) answers, one
    //     word per 64 slots.
    let (masks, reached) = crate::x64::licm::compute_local_oop_masks(&code, code.len(), 84, 0);
    assert!(
        masks.is_empty() && reached.is_empty(),
        "the SPLICE entry point must still answer nothing above 64 locals -- \
         `InlineOopScope` cannot hold a second word"
    );
    let (wmasks, wreached, windows) =
        crate::x64::licm::compute_local_oop_masks_windowed(&code, code.len(), 84, 0, &[]);
    assert_eq!(windows, 2, "84 locals is two 64-slot windows");
    assert_eq!(wmasks.len(), windows * code.len());
    // `wide astore 74` at pc 0 makes local 74 an oop from pc 4 onward.
    // Local 74 is window 1 (base 64), bit 10.
    let w1_at_pc4 = wmasks[code.len() + 4];
    assert_ne!(
        w1_at_pc4 & (1u64 << 10),
        0,
        "local 74 must be a proven oop after its `wide astore` -- this is the \
         coverage a >64-local method used to have none of"
    );
    // `wide istore 70` at pc 8 clears local 70 (window 1, bit 6), which was
    // never set; assert it stays clear so the window rebasing is not just
    // setting everything.
    assert_eq!(
        wmasks[code.len() + 12] & (1u64 << 6),
        0,
        "local 70 is an int"
    );
    // And slot 0 in window 0 is untouched by any of it.
    assert_eq!(wmasks[4] & 1, 0, "local 0 is never stored here");
    assert!(wreached[4], "pc 4 is reachable");

    // The DEOPT COUPLING this test exists to hold onto. `is_oop` there reads
    // bits `< 64` only, so for a local above slot 63 `classify_local_kinds`
    // remains the sole authority -- exactly as asserted at the top. What
    // changed is that locals BELOW 64 in such a method now have a second,
    // strictly more precise source, which is the same arrangement every
    // method under the cliff already had.
    assert_eq!(
        kinds[74],
        LocalKind::Ref,
        "the classifier is still the only thing that types a local above slot 63 \
         for the deopt snapshot"
    );

    // And the same method one local smaller DOES get a mask, so the cliff is
    // the local count and not something about `wide` encodings.
    let (masks64, reached64) =
        crate::x64::licm::compute_local_oop_masks(&code, code.len(), 64, 0);
    assert!(!masks64.is_empty() && !reached64.is_empty());
}

/// The `RowDataType.read` shape: slot 1 is an `int` in one arm of a branch
/// and a `ref` in the other, so the whole-method classifier must call it
/// `Ambiguous` — but at a pc only the `istore` arm reaches, the refinement
/// must answer `Int`.
#[test]
fn refine_resolves_branch_disjoint_slot_reuse() {
    // 0: iconst_0        (0x03)
    // 1: ifne 9          (0x9a 0x00 0x08)  -> targets 9
    // 4: iconst_1        (0x04)
    // 5: istore_1        (0x3c)            <- Int definition
    // 6: goto 12         (0xa7 0x00 0x06)  -> targets 12
    // 9: aconst_null     (0x01)
    // 10: astore_1       (0x4c)            <- Ref definition
    // 11: nop            (0x00)
    // 12: return         (0xb1)
    let code = [
        0x03, 0x9a, 0x00, 0x08, 0x04, 0x3c, 0xa7, 0x00, 0x06, 0x01, 0x4c, 0x00, 0xb1,
    ];
    let kinds = classify_local_kinds(&code, code.len(), 2);
    assert_eq!(
        kinds[1],
        LocalKind::Ambiguous,
        "the whole-method vote must still be Ambiguous"
    );
    let refined = refine_ambiguous_local_kinds(&code, code.len(), &kinds, &[]);
    // pc 6 is the `goto` right after `istore_1` — only the Int arm reaches
    // it, so the slot is provably an int there.
    assert_eq!(refined.kind_at(6, 1), Some(LocalKind::Int));
    // pc 11 is the `nop` after `astore_1` — the Ref arm. `kind_at` never
    // answers `Ref`: the oop mask owns ref-typed slots.
    assert_eq!(refined.kind_at(11, 1), None);
    // pc 12 is the join of both arms — genuinely ambiguous.
    assert_eq!(refined.kind_at(12, 1), None);
    // A slot that was never ambiguous is not tracked at all.
    assert_eq!(refined.kind_at(6, 0), None);
}

/// A method with no ambiguous local pays nothing and answers nothing.
#[test]
fn refine_is_empty_without_ambiguity() {
    // iconst_0; istore_1; return
    let code = [0x03, 0x3c, 0xb1];
    let kinds = classify_local_kinds(&code, code.len(), 2);
    assert_eq!(kinds[1], LocalKind::Int);
    let refined = refine_ambiguous_local_kinds(&code, code.len(), &kinds, &[]);
    assert_eq!(refined.kind_at(1, 1), None);
}

/// An exception handler is reachable from anywhere inside its protected
/// range, so nothing may be assumed on entry to one — or anywhere the
/// handler flows to.
#[test]
fn refine_seeds_exception_handlers_top() {
    // Same shape as the branch test, but pc 6 is declared a handler entry.
    let code = [
        0x03, 0x9a, 0x00, 0x08, 0x04, 0x3c, 0xa7, 0x00, 0x06, 0x01, 0x4c, 0x00, 0xb1,
    ];
    let kinds = classify_local_kinds(&code, code.len(), 2);
    assert_eq!(kinds[1], LocalKind::Ambiguous);
    let refined = refine_ambiguous_local_kinds(&code, code.len(), &kinds, &[(0, 13, 6)]);
    assert_eq!(
        refined.kind_at(6, 1),
        None,
        "a handler entry must not inherit the kind of its protected range"
    );
}

/// A cat-2 store must clear a stale kind from the slot above it, or a
/// reused high-half keeps answering with the value it used to hold.
#[test]
fn refine_clears_cat2_high_half() {
    // 0: aconst_null (0x01)
    // 1: astore_2    (0x4d)          <- Ref definition of slot 2
    // 2: lconst_0    (0x09)
    // 3: lstore_1    (0x40)          <- Long at slot 1, high half at slot 2
    // 4: return      (0xb1)
    let code = [0x01, 0x4d, 0x09, 0x40, 0xb1];
    let kinds = classify_local_kinds(&code, code.len(), 3);
    assert_eq!(kinds[2], LocalKind::Ambiguous);
    let refined = refine_ambiguous_local_kinds(&code, code.len(), &kinds, &[]);
    assert_eq!(
        refined.kind_at(4, 2),
        None,
        "the high half of a live cat-2 is not an independently typed slot"
    );
}

fn classify_local_kinds_reuse_is_ambiguous() {
    // local0 stored as int then as float -> Ambiguous (slot reuse).
    let code = [
        0x36, 0x00, // istore 0 -> Int
        0x38, 0x00, // fstore 0 -> conflict -> Ambiguous
        0xb1,
    ];
    let kinds = classify_local_kinds(&code, code.len(), 1);
    assert_eq!(kinds[0], LocalKind::Ambiguous);
}

#[test]
fn classify_local_kinds_highhalf_reuse_is_ambiguous() {
    // local0 long (hi half @1), but local1 also used as int -> Ambiguous@1.
    let code = [
        0x37, 0x00, // lstore 0 -> Long@0, would-be HighHalf@1
        0x15, 0x01, // iload 1  -> Int@1 (independently used)
        0xb1,
    ];
    let kinds = classify_local_kinds(&code, code.len(), 2);
    assert_eq!(kinds[0], LocalKind::Long);
    assert_eq!(kinds[1], LocalKind::Ambiguous);
}

/// REGRESSION (`arrays-sort-long-osr-miscompile`): the OSR publication
/// strips register homes from cat-2 high-half slots so the trampoline
/// cannot seed a dead half over a live local sharing its register. That
/// strip must NOT touch a slot which is also a real local in a disjoint
/// live range.
///
/// `java.util.DualPivotQuicksort.mixedInsertionSort` is the shape: slot 7
/// is `long ai`'s high half in the method's first region and the `int i`
/// loop counter in the other two. Stripping it made the trampoline seed
/// only its frame slot while the compiled body read its register, so an OSR
/// entry ran with a garbage `i` and `Arrays.sort(long[])` threw an
/// `ArrayIndexOutOfBoundsException` with an index in the hundreds of
/// millions.
///
/// The classifier already draws the distinction; this pins the predicate
/// the OSR publication filters on, which is where the bug was.
#[test]
fn only_pure_high_halves_may_lose_their_osr_register_home() {
    // slot 0: long (so slot 1 would be its high half, untouched otherwise)
    // slot 2: long (so slot 3 would be its high half) — but slot 3 is also
    //         loaded as an int, exactly the reuse `mixedInsertionSort` has.
    let code = [
        0x37, 0x00, // lstore 0  -> Long@0, HighHalf@1
        0x37, 0x02, // lstore 2  -> Long@2, would-be HighHalf@3
        0x15, 0x03, // iload  3  -> Int@3, so @3 is Ambiguous, not HighHalf
        0xb1, // return
    ];
    let kinds = classify_local_kinds(&code, code.len(), 4);
    assert_eq!(kinds[1], LocalKind::HighHalf, "untouched high half");
    assert_eq!(kinds[3], LocalKind::Ambiguous, "high half reused as an int");

    // `wide_local_high_halves` names BOTH — it is a whole-method scan with
    // no notion of reuse, which is why the publication must filter it.
    let halves = wide_local_high_halves(&code, code.len());
    assert!(halves.contains(&1) && halves.contains(&3));

    // The filter the OSR publication applies — the same function it calls,
    // not a copy of its predicate: a copy passes while the call site drifts.
    assert_eq!(
        pure_high_halves(&kinds, &halves),
        vec![1],
        "a reused high half must keep its home"
    );
}

/// The same rule for a slot reused as a **reference**, which is the shape
/// the Tomcat annotation-scan SIGSEGV was: every stage of
/// `probes/AnnotationScanSplitProbe` (and of the minimised
/// `probes/OsrRefSlotReuseProbe`) has
///
///     slot 4 : Iterator, then `lstore 4` for the trailing `long ns`
///     slot 5 : byte[] b inside the loop, high half after it
///
/// so slot 5 is a LIVE `byte[]` at the OSR entry PC and a dead high half
/// forty bytecodes later. Stripping its register home there left the
/// trampoline seeding only the frame slot while the compiled body read the
/// register: `b.length` off a garbage base (SIGSEGV on Windows, a silently
/// wrong checksum on Linux, where slot 5 lands elsewhere in `LOCAL_REGS`).
///
/// The sibling case in `only_pure_high_halves_may_lose_their_osr_register_home`
/// reuses the slot as an `int`. Both are `Ambiguous`, but they arrive
/// through different opcode families (`iload`/`istore` vs `aload`/`astore`)
/// and `local_access_at` classifies them in different arms, so one passing
/// is not evidence for the other.
#[test]
fn a_high_half_reused_as_a_reference_keeps_its_osr_register_home() {
    // slot 4: astore/aload (Ref) AND lstore (Long)  -> Ambiguous
    // slot 5: astore/aload (Ref), and `lstore 4`'s would-be high half
    let code = [
        0x3a, 0x04, // astore 4  -> Ref@4
        0x19, 0x04, // aload  4
        0x3a, 0x05, // astore 5  -> Ref@5
        0x19, 0x05, // aload  5
        0x37, 0x04, // lstore 4  -> @4 Ambiguous, would-be HighHalf@5
        0xb1, // return
    ];
    let kinds = classify_local_kinds(&code, code.len(), 6);
    assert_eq!(kinds[4], LocalKind::Ambiguous, "Ref then Long in slot 4");
    assert_eq!(
        kinds[5],
        LocalKind::Ref,
        "slot 5 is only ever a reference: slot 4 never settles on Long, so \
         the cat-2 high-half pass must not even reach it"
    );

    // The whole-method scan still names slot 5 — `lstore 4` is there.
    let halves = wide_local_high_halves(&code, code.len());
    assert!(
        halves.contains(&5),
        "the scan names it; the filter is the gate"
    );

    assert!(
        pure_high_halves(&kinds, &halves).is_empty(),
        "a live reference slot must keep its OSR register home"
    );
}

#[test]
fn typed_local_frame_value_picks_typed_variant() {
    // long in GPR / spilled.
    assert_eq!(
        typed_local_frame_value(Some(7), None, 8, LocalKind::Long),
        FrameValue::RegisterLong(7)
    );
    assert_eq!(
        typed_local_frame_value(None, None, 16, LocalKind::Long),
        FrameValue::StackSlotLong(-16)
    );
    // float/double in XMM / spilled.
    assert_eq!(
        typed_local_frame_value(None, Some(2), 8, LocalKind::Float),
        FrameValue::XmmFloat(2)
    );
    assert_eq!(
        typed_local_frame_value(None, None, 8, LocalKind::Float),
        FrameValue::StackSlotFloat(-8)
    );
    assert_eq!(
        typed_local_frame_value(None, Some(3), 8, LocalKind::Double),
        FrameValue::XmmDouble(3)
    );
    assert_eq!(
        typed_local_frame_value(None, None, 24, LocalKind::Double),
        FrameValue::StackSlotDouble(-24)
    );
    // int unchanged.
    assert_eq!(
        typed_local_frame_value(Some(1), None, 8, LocalKind::Int),
        FrameValue::Register(1)
    );
    // HighHalf and never-accessed Unknown -> harmless Undefined.
    assert_eq!(
        typed_local_frame_value(None, None, 8, LocalKind::HighHalf),
        FrameValue::Undefined
    );
    assert_eq!(
        typed_local_frame_value(None, None, 8, LocalKind::Unknown),
        FrameValue::Undefined
    );
    // Ambiguous (slot reuse) -> re-run.
    assert_eq!(
        typed_local_frame_value(None, None, 8, LocalKind::Ambiguous),
        FrameValue::Unsupported
    );
    // Provenance/kind contradiction (float in a GPR) -> re-run.
    assert_eq!(
        typed_local_frame_value(Some(4), None, 8, LocalKind::Float),
        FrameValue::Unsupported
    );
}
