//! Round 9 wave 2 (`spdeopt` lane): the reason-11 coordinate fix and the
//! typed-stack upgrade on the int-only side of the `wide_fp` gate.

use crate::deopt::{DeoptReason, FrameValue};
use crate::x64::stack_kinds::{StackKind, StackKindMap};
use crate::x64::{Compiler, StackSlot};

/// A bare `Compiler` (no locals, no register assignments), shaped like
/// `flag_and_header_contracts::bounds_check_test_compiler` but with
/// `precise_exception_frames` and the protected ranges chosen by the test.
fn test_compiler(precise_exception_frames: bool, protected: Vec<(u32, u32)>) -> Compiler {
    let alloc_result = crate::regalloc::RegAllocResult {
        assignments: Vec::new(),
        xmm_assignments: Vec::new(),
        used_callee_saved: Vec::new(),
        used_xmm_regs: Vec::new(),
        block_live_in: Vec::new(),
    };
    let mut helpers = cratonvm_jit_api::JitRuntimeHelpers::default();
    // Any non-zero address: the stubs only have to be emittable.
    helpers.throw_aioobe = test_compiler as usize;
    Compiler::new(
        "r9w2-spdeopt-test".to_string(),
        crate::ExecutableBuffer::new(4096).expect("test executable buffer"),
        0,
        0,
        8,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        alloc_result,
        false,
        helpers,
        0,
        false,
        false,
        false,
        precise_exception_frames,
        false,
        protected,
    )
}

/// `precise-exception-stubs-double-map-bci-under-loop-rewrite`, reason 11.
///
/// Under a bytecode loop rewrite `orig_bci` is not the identity. The
/// provenance below maps emitter pc `EMIT` to interpreter bci `ORIG`, and
/// maps `ORIG` (read as an emitter pc) to a third value `DOUBLE`, so each
/// wrong coordinate is distinguishable: the point must be recorded at the
/// emitter pc, published at `ORIG`, and the `jit_throw_aioobe` pc argument
/// must be `ORIG` — never `EMIT` (untranslated) nor `DOUBLE` (translated
/// twice, the pre-fix shape).
#[test]
fn precise_aioobe_frame_is_keyed_by_the_emitter_pc_and_published_translated_once() {
    const EMIT: usize = 0x3A5C;
    const ORIG: u32 = 0x1B7D;
    const DOUBLE: u32 = 0x0E9F;
    let mut c = test_compiler(true, vec![(0, 0x4000)]);
    let mut provenance: Vec<u32> = (0..0x4000u32).collect();
    provenance[EMIT] = ORIG;
    provenance[ORIG as usize] = DOUBLE;
    c.bci_provenance = Some(provenance);
    assert_ne!(
        c.deopt_regs_base, 0,
        "precise_exception_frames must reserve the spill region"
    );

    c.emit_bounds_check(EMIT);

    assert_eq!(c.deopt_point_pcs, vec![EMIT], "recorded at the EMITTER pc");
    assert_eq!(c.deopt_points.len(), 1);
    assert!(matches!(
        c.deopt_points[0].reason,
        DeoptReason::PendingException
    ));
    assert_eq!(c.deopt_points[0].bci, ORIG, "published translated once");
    assert_eq!(c.deopt_points[0].frame_state.bci, ORIG);
    assert!(c.exc_frame_box_ptr_by_bci.contains_key(&EMIT));
    assert_eq!(c.deopt_stubs.len(), 1);
    assert_eq!((c.deopt_stubs[0].1, c.deopt_stubs[0].2), (EMIT, 11));

    let stub_start = c.buf.pos();
    c.emit_deopt_stubs();
    let stub: Vec<u8> = c.buf.as_slice()[stub_start..c.buf.pos()].to_vec();
    let has_imm32 = |v: u32| stub.windows(4).any(|w| w == &v.to_le_bytes()[..]);
    assert!(
        has_imm32(ORIG),
        "jit_throw_aioobe's bytecode-pc argument must be the interpreter bci"
    );
    assert!(
        !has_imm32(EMIT as u32),
        "an emitter pc leaked into the stub"
    );
    assert!(!has_imm32(DOUBLE), "the bci was translated twice");
}

/// r9 wave 6 `every-x64-deopt-point-is-stored-twice`: the frame-deopt
/// stub bakes the address of the published `deopt_points` element — the
/// single copy that moves into the artifact — not its identity box, and
/// the box no longer carries a second deep copy of the frame state.
#[test]
fn frame_deopt_stub_bakes_the_published_point_not_its_identity_box() {
    const EMIT: usize = 0x10;
    let mut c = test_compiler(true, vec![(0, 0x4000)]);
    c.emit_bounds_check(EMIT);
    assert_eq!(c.deopt_points.len(), 1);
    assert_eq!(c.deopt_boxes.len(), 1, "the lists stay in lockstep");
    assert!(
        c.deopt_boxes[0].frame_state.locals.is_empty()
            && c.deopt_boxes[0].frame_state.stack.is_empty()
            && c.deopt_boxes[0].frame_state.method_key.is_empty(),
        "the identity box must not duplicate the frame state"
    );
    // Cast: pointer to the imm64 value a stub would bake
    let box_addr = std::ptr::addr_of!(*c.deopt_boxes[0]) as usize as u64;

    let stub_start = c.buf.pos();
    c.emit_deopt_stubs();
    assert!(!c.buf.overflowed(), "the stub resolved its point");
    // `emit_deopt_stubs` may have shrunk the list, so read the element's
    // address only now — it is final from here on.
    // Cast: pointer to the imm64 value a stub would bake
    let point_addr = std::ptr::addr_of!(c.deopt_points[0]) as usize as u64;
    let stub: Vec<u8> = c.buf.as_slice()[stub_start..c.buf.pos()].to_vec();
    let has_imm64 = |v: u64| stub.windows(8).any(|w| w == &v.to_le_bytes()[..]);
    assert!(
        has_imm64(point_addr),
        "the stub must bake the published point"
    );
    assert!(!has_imm64(box_addr), "the stub baked the identity box");
    // Finalize's check: the buffer the stubs baked from is this one.
    let published = std::mem::take(&mut c.deopt_points);
    assert!(super::baked_point_table_is(c.baked_point_table, &published));
}

/// `deopt-stack-width-gate-misses-field-invoke-and-ldc-values` (loops R4).
///
/// A method with no opcode that NAMES a long (`uses_long_float_double ==
/// false`) can still hold a `getfield`'d long under a guarded receiver.
/// When the typed stack answers, the snapshot must publish it wide; when
/// it does not, the pre-existing cat-1 encoding is kept.
#[test]
fn the_int_only_side_of_the_width_gate_consults_the_typed_stack() {
    let mut c = test_compiler(false, Vec::new());
    c.uses_long_float_double = false;
    c.stack = vec![StackSlot::Frame(48), StackSlot::Frame(56)];
    c.stack_oop_marks = vec![false, true];
    c.stack_kinds = StackKindMap::from_entries_for_test([
        (7usize, vec![StackKind::Long, StackKind::Ref]),
        (8usize, vec![StackKind::Float, StackKind::Ref]),
        (9usize, vec![StackKind::Unknown, StackKind::Ref]),
    ]);
    let at = |c: &mut Compiler, bci: usize| {
        c.build_frame_state_at(bci, Some(DeoptReason::ReceiverTypeChanged))
            .stack
    };
    assert_eq!(
        at(&mut c, 7),
        vec![
            FrameValue::StackSlotLong(-48),
            FrameValue::StackSlotRef(-56)
        ]
    );
    assert_eq!(
        at(&mut c, 8),
        vec![
            FrameValue::StackSlotFloat(-48),
            FrameValue::StackSlotRef(-56)
        ]
    );
    // `Unknown`, and no answer at all: today's cat-1 int, unchanged.
    assert_eq!(
        at(&mut c, 9),
        vec![FrameValue::StackSlot(-48), FrameValue::StackSlotRef(-56)]
    );
    assert_eq!(
        at(&mut c, 10),
        vec![FrameValue::StackSlot(-48), FrameValue::StackSlotRef(-56)]
    );

    // Register-resident operand.
    c.stack = vec![StackSlot::CalleeSaved(12)];
    c.stack_oop_marks = vec![false];
    c.stack_kinds = StackKindMap::from_entries_for_test([
        (7usize, vec![StackKind::Long]),
        (8usize, vec![StackKind::Double]),
        (9usize, vec![StackKind::Int]),
    ]);
    assert_eq!(at(&mut c, 7), vec![FrameValue::RegisterLong(12)]);
    assert_eq!(
        at(&mut c, 8),
        vec![FrameValue::Unsupported],
        "a GPR-homed double has no encoding; `Register` would truncate it"
    );
    assert_eq!(at(&mut c, 9), vec![FrameValue::Register(12)]);
    assert_eq!(at(&mut c, 10), vec![FrameValue::Register(12)]);

    // The wide side is unchanged: no answer stays `Unsupported`.
    c.uses_long_float_double = true;
    assert_eq!(at(&mut c, 10), vec![FrameValue::Unsupported]);
    assert_eq!(at(&mut c, 7), vec![FrameValue::RegisterLong(12)]);
}

// -----------------------------------------------------------------------
// Round 10 wave 8 (`bybci`): the bare-bci key space has exactly one owner.
// -----------------------------------------------------------------------

/// The rule, in isolation. `splice_depth == 0` is the ONLY configuration in
/// which a bare bci names one method's bytecode; every depth is ambiguous and
/// none is less ambiguous than another.
///
/// Worth a test of its own precisely because the answer looks too obvious to
/// need one: `r10-splice-by-bci-deopt-maps-have-no-owner-FIXED-20260922.md` proposes
/// keying the four maps on this same depth, which would make depth 1 look like
/// an owner — and it is not one, because two splices of the same callee at two
/// different caller pcs are BOTH depth 1 and would share every entry. The
/// predicate deliberately says "one owner or none", not "which owner".
#[test]
fn only_depth_zero_owns_the_bare_bci_key_space() {
    assert!(super::by_bci_key_space_has_one_owner(0));
    for depth in 1..8usize {
        assert!(
            !super::by_bci_key_space_has_one_owner(depth),
            "depth {depth} shares the key space with the enclosing method"
        );
    }
}

/// The consequence, through a real emitter, on the population that no
/// workload can produce.
///
/// This is the half the known-issues page said could not be had ("there is
/// nothing to observe at runtime … the population is empty, so no counter,
/// trace or census can distinguish this from 'cannot happen'"). That is true
/// of a *workload*, and it is not true of a unit test: `inline_callee_scopes`
/// is a plain field, so seeding it reaches exactly the state four interlocks
/// keep a compile out of, and the guard can be exercised rather than reasoned
/// about. A widened `(owner, bci)` key would have no equivalent — there is no
/// second method's pc to collide with here, only the invariant to check.
///
/// `emit_bounds_check` is the vehicle because its refusal is observable in
/// three independent places: no `exc_frame_box_ptr_by_bci` entry (the key
/// space stayed clean), no reason-11 entry in `deopt_stubs` (the frame
/// trampoline was not armed), and one entry in `bounds_check_stubs` (the
/// shared, frame-less AIOOBE pad took it instead). A guard that silently
/// emitted nothing at all would pass the first two and fail the third.
#[test]
fn a_splice_cannot_file_in_the_bare_bci_key_space() {
    const PC: usize = 0x20;
    let mut c = test_compiler(true, vec![(0, 0x100)]);

    // Baseline: outside a splice this pc DOES get the precise arm. Without
    // this half the test below would pass on a compiler that had stopped
    // emitting precise bounds checks altogether.
    c.emit_bounds_check(PC);
    assert!(
        c.exc_frame_box_ptr_by_bci.contains_key(&PC),
        "the precise arm must still fire outside a splice"
    );
    assert_eq!(c.deopt_stubs.iter().filter(|s| s.2 == 11).count(), 1);
    assert!(c.bounds_check_stubs.is_empty());

    // Now the same emitter at a DIFFERENT pc with a splice open. `key` and
    // `max_locals` are the shape `push_inline_scope` builds from an
    // `InlineSite`; `caller_stack_floor` is 0 because this bare compiler has
    // no abstract operand stack.
    const SPLICED_PC: usize = 0x40;
    c.inline_callee_scopes.push(crate::x64::InlineCalleeScope {
        class_id: 0,
        key: "Callee.leaf:(I)I".to_string(),
        max_locals: 2,
        caller_stack_floor: 0,
    });
    let points_before = c.deopt_points.len();
    c.emit_bounds_check(SPLICED_PC);

    assert!(
        !c.exc_frame_box_ptr_by_bci.contains_key(&SPLICED_PC),
        "a splice filed in the bare-bci key space: the enclosing method's own \
         guard at this bci would find this entry, skip its own publication, \
         and bake a pointer to a frame describing the spliced callee"
    );
    assert_eq!(
        c.deopt_points.len(),
        points_before,
        "the refused arm must not record a point either — it never reaches \
         `build_and_record_deopt_point`"
    );
    assert_eq!(
        c.deopt_stubs.iter().filter(|s| s.2 == 11).count(),
        1,
        "no second reason-11 stub: the refused bounds check must not arm the \
         frame trampoline it has no frame for"
    );
    assert_eq!(
        c.bounds_check_stubs.len(),
        1,
        "the refusal must FALL BACK to the shared AIOOBE pad, not emit an \
         unguarded access"
    );
    assert_eq!(c.bounds_check_stubs[0].1, SPLICED_PC);

    // And the original entry is untouched, which is the substitution the
    // known-issues page calls the worse direction: had the splice filed at
    // `PC` instead, this pointer would now name the callee's frame.
    assert!(c.exc_frame_box_ptr_by_bci.contains_key(&PC));
    assert_eq!(c.exc_frame_box_ptr_by_bci.len(), 1);
}

/// Round 10 wave 8 follow-up: one bci carries a reason-2 AND a reason-6
/// snapshot, and the two are told apart by the key rather than by which
/// producer ran first.
///
/// The shape this reproduces is the one the filing page could not establish
/// by reading: a loop header whose guard block files `BoundsCheck` at a pc,
/// followed by the invoke AT THAT SAME pc asking for a `ReceiverTypeChanged`
/// snapshot. Emission order puts the header's guard block first, so before
/// the key carried a reason it was the receiver guard that was dropped —
/// `snapshot_pre_intrinsic_call`'s `contains_key(&bci)` returned early, the
/// point was never built, and `emit_deopt_stubs`' `6 =>` arm read the
/// `BoundsCheck` box.
///
/// The assertion that matters is the last one. The frame CONTENTS were never
/// wrong (both producers describe the same bci from the same provenance), so
/// a test on the box alone would have passed before the fix too. What was
/// wrong is what a bci-keyed reader concludes: `deopt_reason_at_bci`
/// answered `Unique(BoundsCheck)` for a pc that genuinely carries both, and
/// `recommend_action` then denied the receiver guard its de-spec sparing
/// path. `Ambiguous` is the honest answer and every caller in the tree
/// handles it conservatively.
#[test]
fn one_bci_carries_a_reason_2_and_a_reason_6_snapshot_independently() {
    use crate::deopt::DeoptReason::{BoundsCheck, ReceiverTypeChanged};
    const PC: usize = 0x30;
    let mut c = test_compiler(true, Vec::new());

    // The loop-header guard block, emitted before the instruction at PC.
    c.emit_deopt_snapshot_at_guard(PC);
    assert!(c.deopt_box_ptr_by_bci.contains_key(&(PC, BoundsCheck)));
    let after_first = c.deopt_points.len();
    assert_eq!(after_first, 1);

    // The invoke at the same pc. Before the reason component this call was a
    // silent no-op.
    c.snapshot_pre_intrinsic_call(PC, ReceiverTypeChanged);
    assert!(
        c.deopt_box_ptr_by_bci
            .contains_key(&(PC, ReceiverTypeChanged)),
        "the receiver guard's snapshot was suppressed by the bounds check's \
         entry at the same bci — the collision this key component exists for"
    );
    assert_eq!(
        c.deopt_points.len(),
        after_first + 1,
        "the second point must be BUILT, not skipped"
    );

    // Two distinct boxes, each stamped with its own reason. `emit_deopt_stubs`
    // bakes these pointers as imm64 into the tag-2 and tag-6 stubs
    // respectively, so this is the property those two lookups rely on.
    let bounds = c.deopt_box_ptr_by_bci[&(PC, BoundsCheck)];
    let receiver = c.deopt_box_ptr_by_bci[&(PC, ReceiverTypeChanged)];
    assert_ne!(bounds, receiver);
    // SAFETY: both pointers name `deopt_boxes` elements owned by `c`, which
    // outlives this block and is not mutated between the reads.
    unsafe {
        assert_eq!((*bounds).reason, BoundsCheck);
        assert_eq!((*receiver).reason, ReceiverTypeChanged);
    }

    // Idempotence survives, PER PAIR: a second call for the same
    // `(bci, reason)` is still a no-op.
    c.snapshot_pre_intrinsic_call(PC, ReceiverTypeChanged);
    assert_eq!(c.deopt_points.len(), after_first + 1);

    // The consumer's answer, which is the whole cost of the old key.
    assert!(
        matches!(
            crate::osr_exit::deopt_reason_at_bci(&c.deopt_points, PC as u32),
            crate::osr_exit::ReasonAtBci::Ambiguous
        ),
        "with both points present the bci-keyed reader must refuse to guess; \
         a `Unique(_)` here is the confident wrong answer the suppression \
         used to manufacture"
    );
}

// -----------------------------------------------------------------------
// Round 11 wave 8 (`x64`): the javac `synchronized` block with a static
// store, and the precise `/ by zero`.
// -----------------------------------------------------------------------

/// `r11w7-orch-putstatic-in-a-synchronized-block-keeps-the-method-interpreted`.
///
/// RBC.6 now admits a protected `putstatic` (lib.rs
/// `precise_frame_publishing_opcode`) on the claim that its one arm always
/// publishes a reason-9 frame at its own bci. This is that claim, through
/// the real `0xb3` arm: protected, the sentinel edge files a frame and a
/// tag-9 stub keyed by the putstatic's pc (not its successor, which javac
/// routinely puts outside the range); unprotected, it keeps the cheap
/// shared exit and records no frame.
#[test]
fn a_protected_putstatic_publishes_a_reason_9_frame_at_its_own_bci() {
    use crate::x64::bytecode_walk::WalkStep;
    // `R11LockInlineHitRate.plain`: body [6, 44), putstatic at pc 20.
    const PC: usize = 20;
    let run = |protected: Vec<(u32, u32)>| {
        let mut c = test_compiler(true, protected);
        c.heap_local_offset = 16; // gch h3/fd (d1a): the putstatic helper loads the VM context
        c.static_field_info = vec![(PC, 7, 0, b'I', false)];
        c.static_field_info_idx.insert(PC, 0);
        c.stack = vec![StackSlot::Frame(48)];
        c.stack_oop_marks = vec![false];
        c.dbg_last_pc = PC;
        let mut dead = false;
        let step = c.walk_field(&[], 0, 0xb3, PC, &mut dead, &[], &[]);
        assert!(
            matches!(step, WalkStep::Next(n) if n == PC + 3),
            "the putstatic arm must lower, not abandon the compile"
        );
        c
    };

    let mut c = run(vec![(6, 44)]);
    assert!(
        c.exc_frame_box_ptr_by_bci.contains_key(&PC),
        "a protected putstatic must record its exceptional frame"
    );
    assert_eq!(c.deopt_points.len(), 1);
    assert!(matches!(
        c.deopt_points[0].reason,
        DeoptReason::PendingException
    ));
    assert_eq!(
        c.deopt_points[0].bci, PC as u32,
        "keyed on the putstatic itself"
    );
    assert_eq!(
        c.deopt_stubs
            .iter()
            .filter(|s| s.1 == PC && s.2 == 9)
            .count(),
        1,
        "the sentinel edge must go to the reason-9 stub"
    );
    assert!(c.exception_check_stubs.is_empty(), "no frame-less exit");
    c.emit_deopt_stubs();
    assert!(!c.buf.overflowed(), "the reason-9 stub resolved its point");

    let c = run(Vec::new());
    assert!(c.exc_frame_box_ptr_by_bci.is_empty());
    assert!(c.deopt_points.is_empty());
    assert_eq!(
        c.exception_check_stubs.len(),
        1,
        "unprotected: the shared exit"
    );
}

/// The zero-divisor guard inside a protected range takes reason 12 — the
/// `ArithmeticException` plus this bci's frame — and outside every range
/// keeps reason 3, the frame-less direct throw. Neither may fall through
/// to `jit_uncommon_trap`, which would re-run the method from entry.
#[test]
fn a_protected_zero_divisor_guard_publishes_a_precise_frame() {
    const PC: usize = 0x18;
    const OUT: usize = 0x30;
    let mut c = test_compiler(true, vec![(0x10, 0x20)]);
    c.helpers.throw_arithmetic = test_compiler as usize;
    c.helpers.set_throw_bci = test_compiler as usize;

    c.emit_safe_idiv(PC, false, false);
    assert!(c.exc_frame_box_ptr_by_bci.contains_key(&PC));
    assert_eq!(c.deopt_points.len(), 1);
    assert!(matches!(
        c.deopt_points[0].reason,
        DeoptReason::PendingException
    ));
    assert_eq!(c.deopt_points[0].bci, PC as u32);
    assert_eq!(
        c.deopt_stubs
            .iter()
            .filter(|s| s.1 == PC && s.2 == 12)
            .count(),
        1
    );

    c.emit_safe_idiv(OUT, true, true);
    assert!(!c.exc_frame_box_ptr_by_bci.contains_key(&OUT));
    assert_eq!(
        c.deopt_points.len(),
        1,
        "no frame outside a protected range"
    );
    assert_eq!(
        c.deopt_stubs
            .iter()
            .filter(|s| s.1 == OUT && s.2 == 3)
            .count(),
        1
    );

    c.emit_deopt_stubs();
    assert!(!c.buf.overflowed(), "the reason-12 stub resolved its point");
    assert!(
        !c.has_frameless_trap_stub,
        "a divide-by-zero stub reached `jit_uncommon_trap`"
    );
}

/// Without precise frames nothing changes: every zero guard is reason 3.
#[test]
fn without_precise_frames_the_zero_divisor_guard_is_unchanged() {
    let mut c = test_compiler(false, vec![(0, 0x100)]);
    c.emit_safe_idiv(0x18, false, true);
    assert!(c.deopt_points.is_empty());
    assert_eq!(c.deopt_stubs.len(), 1);
    assert_eq!((c.deopt_stubs[0].1, c.deopt_stubs[0].2), (0x18, 3));
}

/// H2-1 (round 12 wave 3): an entry the typed stack calls `Ref` but the
/// emitter left unmarked makes the marks inexact (and refuses the compile
/// under strict); agreement and `Unknown` change nothing.
#[test]
fn an_unmarked_reference_makes_the_marks_inexact_and_strict_refuses() {
    for strict in [false, true] {
        let mut c = test_compiler(false, Vec::new());
        c.stack_kinds = StackKindMap::from_entries_for_test([(
            7usize,
            vec![StackKind::Int, StackKind::Ref],
        )]);
        c.stack_push(StackSlot::Frame(16), false);
        c.stack_push(StackSlot::Frame(24), false); // the Ref, left unmarked
        assert!(c.stack_oop_marks_exact);
        c.check_oop_marks_against_stack_kinds(7, strict, false);
        assert!(!c.stack_oop_marks_exact);
        assert_eq!(c.failed, strict);
    }
    let mut c = test_compiler(false, Vec::new());
    c.stack_kinds = StackKindMap::from_entries_for_test([(
        7usize,
        vec![StackKind::Unknown, StackKind::Ref],
    )]);
    c.stack_push(StackSlot::Frame(16), false);
    c.stack_push(StackSlot::Frame(24), true);
    c.check_oop_marks_against_stack_kinds(7, true, false);
    assert!(c.stack_oop_marks_exact && !c.failed);
}
