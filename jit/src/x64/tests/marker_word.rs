#[allow(unused_imports)]
use super::*;


thread_local! {
    /// The words this file's `extern "C"` marker helpers record into (a
    /// helper the compiled code calls cannot capture a test's local). Per
    /// thread: a compiled body runs on the test's own thread and calls its
    /// helpers there, so parallel tests never share a word. Every test resets
    /// the words it reads before reading them. JIT r25 wave 9 (W9-6): one
    /// static for the eight per-test recorder statics it replaced.
    pub(in crate::x64::tests) static MARKER_WORDS: [std::cell::Cell<usize>; 2] =
        const { [std::cell::Cell::new(0), std::cell::Cell::new(0)] };
}

/// One of this thread's [`MARKER_WORDS`], with the `AtomicUsize` calls the
/// marker tests make (the ordering is moot on a thread-local word).
#[derive(Clone, Copy)]
pub(in crate::x64::tests) struct MarkerWord(pub(in crate::x64::tests) usize);

impl MarkerWord {
    pub(in crate::x64::tests) fn load(self, _: std::sync::atomic::Ordering) -> usize {
        MARKER_WORDS.with(|w| w[self.0].get())
    }

    pub(in crate::x64::tests) fn store(self, value: usize, _: std::sync::atomic::Ordering) {
        MARKER_WORDS.with(|w| w[self.0].set(value));
    }

    pub(in crate::x64::tests) fn fetch_add(self, value: usize, _: std::sync::atomic::Ordering) -> usize {
        MARKER_WORDS.with(|w| {
            let old = w[self.0].get();
            w[self.0].set(old.wrapping_add(value));
            old
        })
    }
}
use cratonvm_types::{ObjectRef, Value, ARRAY_DATA_OFFSET};

#[test]
pub(in crate::x64::tests) fn gc_inert_selfrec_accepts_forward_field_walk_and_rejects_gc_edges() {
    // aload_0; getfield; ifnonnull L; iconst_1; ireturn;
    // L: iconst_1; aload_0; getfield; invokestatic self; iadd; ireturn
    let pure = [
        0x2a, 0xb4, 0x00, 0x01, 0xc7, 0x00, 0x05, 0x04, 0xac, 0x04, 0x2a, 0xb4, 0x00, 0x01, 0xb8,
        0x00, 0x02, 0x60, 0xac,
    ];
    let fields = [(1usize, 0usize, b'L'), (11, 0, b'L')];
    assert!(gc_inert_selfrec_candidate(
        &pure,
        pure.len(),
        &fields,
        &[], // new_info
        &[], // new_deferred_info
        &[], // anewarray_info
        &[], // anewarray_deferred_info
        &[],
        &[],
        &[],
        &[],
        &[],
    ));

    let mut allocates = pure.to_vec();
    allocates[9] = 0xbb; // new
    assert!(!gc_inert_selfrec_candidate(
        &allocates,
        allocates.len(),
        &fields,
        &[(9, 1, 0, false, false)], // new_info
        &[],                        // new_deferred_info
        &[],                        // anewarray_info
        &[],                        // anewarray_deferred_info
        &[],
        &[],
        &[],
        &[],
        &[],
    ));

    let mut loops = pure;
    loops[4..7].copy_from_slice(&[0xa7, 0xff, 0xfc]); // goto pc 0
    assert!(!gc_inert_selfrec_candidate(
        &loops,
        loops.len(),
        &fields,
        &[], // new_info
        &[], // new_deferred_info
        &[], // anewarray_info
        &[], // anewarray_deferred_info
        &[],
        &[],
        &[],
        &[],
        &[],
    ));
}

#[test]
pub(in crate::x64::tests) fn stack_bang_frame_probes_cover_crossed_pages() {
    assert_eq!(stack_bang_frame_probe_disps(0), Some(Vec::new()));
    assert_eq!(stack_bang_frame_probe_disps(128), Some(vec![-128]));
    assert_eq!(
        stack_bang_frame_probe_disps(STACK_BANG_PAGE_SIZE),
        Some(vec![-STACK_BANG_PAGE_SIZE])
    );
    assert_eq!(
        stack_bang_frame_probe_disps(STACK_BANG_PAGE_SIZE + 64),
        Some(vec![-STACK_BANG_PAGE_SIZE, -(STACK_BANG_PAGE_SIZE + 64)])
    );
    assert_eq!(
        stack_bang_frame_probe_disps(STACK_BANG_PAGE_SIZE * 2),
        Some(vec![-STACK_BANG_PAGE_SIZE, -(STACK_BANG_PAGE_SIZE * 2)])
    );
}

#[test]
pub(in crate::x64::tests) fn estimate_max_stack_counts_ldc_family_and_dup_pushes() {
    let ldc_code = [
        0x12, 0x01, // ldc #1
        0x13, 0x00, 0x02, // ldc_w #2
        0x14, 0x00, 0x03, // ldc2_w #3
        0xac, // ireturn
        0x00, 0x00,
    ];
    assert!(
        estimate_max_stack(&ldc_code, 9) >= 3,
        "ldc/ldc_w/ldc2_w must contribute stack pushes"
    );

    let dup_code = [
        0x03, // iconst_0
        0x04, // iconst_1
        0x5a, // dup_x1: depth 2 -> 3
        0x5c, // dup2: conservative depth 3 -> 5
        0xac, // ireturn
        0x00, 0x00,
    ];
    assert!(
        estimate_max_stack(&dup_code, 5) >= 5,
        "dup_x*/dup2* must contribute stack pushes"
    );
}

/// Page `r11-x64loop-rotated-preheader-despec-looks-up-the-wrong-bci`: a
/// rotated loop's pre-header deopts at its entry `goto`, so the de-spec filter
/// must consult that bci as well as the header's.
#[test]
pub(in crate::x64::tests) fn rotated_preheader_despec_consults_the_entry_goto() {
    let mut rotation: FxHashMap<usize, usize> = FxHashMap::default();
    rotation.insert(10, 3); // header 10, entered by the `goto` at 3
    let rotated: Vec<usize> = preheader_deopt_pcs(10, &rotation).collect();
    assert_eq!(rotated, vec![10, 3]);
    let plain: Vec<usize> = preheader_deopt_pcs(20, &rotation).collect();
    assert_eq!(plain, vec![20]);
}

/// Page `r11-x64gen-max-stack-estimate-inflates-frames`: a resolved call site
/// pops its arguments, so `aload_0; invokevirtual foo()V` repeated does not
/// grow the estimate, while an unresolved site keeps the conservative `+1`.
#[test]
pub(in crate::x64::tests) fn estimate_max_stack_pops_resolved_invoke_arguments() {
    let mut code = Vec::new();
    for _ in 0..50 {
        code.extend_from_slice(&[0x2a, 0xb6, 0x00, 0x01]); // aload_0; invokevirtual #1
    }
    code.push(0xb1); // return
    let len = code.len();
    let unresolved = estimate_max_stack(&code, len);
    assert!(
        unresolved >= 50,
        "unresolved invokes stay conservative: {unresolved}"
    );
    let void_one_arg = estimate_max_stack_with_invokes(&code, len, |_| Some((1, false)));
    assert_eq!(
        void_one_arg,
        1 + 4,
        "receiver pushed, popped by the void call"
    );

    let mut discarded = Vec::new();
    for _ in 0..50 {
        // aload_0; invokevirtual #1 (returns int); pop
        discarded.extend_from_slice(&[0x2a, 0xb6, 0x00, 0x01, 0x57]);
    }
    discarded.push(0xb1);
    let dlen = discarded.len();
    let int_result = estimate_max_stack_with_invokes(&discarded, dlen, |_| Some((1, true)));
    assert_eq!(
        int_result,
        1 + 4,
        "the receiver's slot is reused by the result"
    );
}

/// A spill slot a BURIED operand-stack entry still owns must not be handed out
/// again by the next push.
///
/// `invalidate_callee_saved` reserves ONE fresh slot at the top of the spill
/// reserve and repoints EVERY entry that reads the register at it — including
/// entries buried under the top of the stack, and including two entries at the
/// same shared slot. `pop_stack` then reclaimed on `off == next_spill_offset -
/// 8` alone, which assumes the top entry owns the topmost slot. Popping the
/// shallower of two aliases satisfied that test while the deeper alias still
/// read the slot, so the next `push_stack` was handed it and the pushed value
/// overwrote the buried operand.
///
/// Measured consequence: `kotlin.reflect...KotlinTypeFactory
/// .simpleTypeWithNonTrivialMemberScope` — five `astore`/`istore` pops into
/// locals 7..11 (an invalidation each) with four earlier operands still on the
/// stack — passed a null where its caller had pushed
/// `Collections.emptyList()`, i.e.
/// `InvocableHandlerMethodKotlinTests.genericParameter()` in the Spring
/// Framework suite.
#[test]
pub(in crate::x64::tests) fn pop_does_not_reclaim_a_slot_a_buried_entry_still_owns() {
    let alloc_result = crate::regalloc::RegAllocResult {
        assignments: Vec::new(),
        xmm_assignments: Vec::new(),
        used_callee_saved: Vec::new(),
        used_xmm_regs: Vec::new(),
        block_live_in: Vec::new(),
    };
    let mut compiler = Compiler::new(
        "buried-spill-alias-test".to_string(),
        ExecutableBuffer::new(4096).expect("test executable buffer"),
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
        test_helpers(),
        0,
        false,
        false,
        false,
        false,
        false,
        Vec::new(),
    );

    // Two entries reading the same callee-saved register, one buried under the
    // other — the shape `aload N; ...; aload N` leaves behind.
    compiler.stack_push(StackSlot::CalleeSaved(R12), true);
    compiler.stack_push(StackSlot::CalleeSaved(R12), true);

    // The store that overwrites R12 materialises both to ONE shared slot.
    compiler.invalidate_callee_saved(R12);
    let shared = match compiler.stack[0] {
        StackSlot::Frame(off) => off,
        other => panic!("buried entry not materialised: {other:?}"),
    };
    assert!(
        matches!(compiler.stack[1], StackSlot::Frame(off) if off == shared),
        "both aliases should share one spill slot, got {:?}",
        compiler.stack[1]
    );

    // Pop the shallower alias. The buried one still reads `shared`.
    let popped = compiler.pop_stack();
    assert!(
        matches!(popped, StackSlot::Frame(off) if off == shared),
        "expected the shared slot on top, got {popped:?}"
    );

    let pushed = compiler.push_stack().expect("push after pop");
    assert!(
        !matches!(pushed, StackSlot::Frame(off) if off == shared),
        "push reused spill slot {shared}, which the buried entry {:?} still reads",
        compiler.stack[0]
    );
}

/// A splice must not rewind the caller's spill cursor UNDER an operand the
/// caller still owns.
///
/// `try_emit_inline_body` restores the cursor from the LOWEST frame offset
/// among the arguments it popped. That is the caller's new top only while
/// frame offsets are handed out in stack ORDER, so that the arguments are the
/// topmost slots. `invalidate_callee_saved` breaks exactly that — it reserves
/// ONE fresh slot at the top of the reserve and repoints every entry reading
/// the register at it, however deep — and it runs on every write to a
/// register-homed local, `iinc` on a loop counter included. The rewind then
/// hands the buried operand's address out a second time, to this splice's
/// return value or to the next splice's callee locals.
///
/// The sibling test above is the same defect one table over:
/// `pop_stack` grew a live-slot scan for it, and this is that scan on the path
/// that bypasses `pop_stack`'s reclaim arm entirely (`reserve_spill_slots` has
/// already moved the cursor past the argument slots by the time the arguments
/// are popped, which is why the splice derives the cursor by hand).
///
/// Measured consequence: bc-java `LEATest` inside `SimpleTestTest`, where
///
/// ```java
/// pWork[j] = rol32(pWork[j] + rol32(myDelta, j++), ROT3);
/// ```
///
/// lost the array-store index `j` — pushed early, repointed by the `iinc`,
/// and still live across two `rol32` splices — and reached `iastore` holding
/// `0xC3EFE9DB`: LEA's `DELTA[0]`, i.e. `myDelta` on the first iteration.
/// `ArrayIndexOutOfBoundsException: Index -1007687205`.
///
/// The assertion is on the CURSOR rather than on a computed answer, because
/// the operand-stack simulation is where the defect lives; a bytecode-level
/// test cannot reach it through the `compile` test wrapper, which never
/// requests register homes for locals and so can never make the offsets
/// non-monotonic in the first place.
#[test]
pub(in crate::x64::tests) fn a_splice_does_not_rewind_the_cursor_under_a_buried_operand() {
    let alloc_result = crate::regalloc::RegAllocResult {
        assignments: Vec::new(),
        xmm_assignments: Vec::new(),
        used_callee_saved: Vec::new(),
        used_xmm_regs: Vec::new(),
        block_live_in: Vec::new(),
    };
    let mut compiler = Compiler::new(
        "inline-buried-operand-test".to_string(),
        ExecutableBuffer::new(65536).expect("test executable buffer"),
        0,
        0,
        64,
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
        test_helpers(),
        0,
        false,
        false,
        false,
        false,
        false,
        Vec::new(),
    );

    // Depth 0 — the operand that stays live UNDER the call. In LEA this is the
    // array-store index, read from a register-homed local.
    compiler.stack_push(StackSlot::CalleeSaved(R12), false);
    // Depth 1 — a COMPUTED value, so it owns a frame slot, and it owns one LOW
    // in the reserve because it was pushed first.
    let arg_lo = match compiler.push_stack().expect("first argument slot") {
        StackSlot::Frame(off) => off,
        other => panic!("push_stack must hand out a frame slot, got {other:?}"),
    };
    // The `iinc`. The buried entry is materialised at a FRESH slot, which is
    // ABOVE the argument sitting below it: offsets are no longer monotonic
    // with stack depth.
    compiler.invalidate_callee_saved(R12);
    let buried = match compiler.stack[0] {
        StackSlot::Frame(off) => off,
        other => panic!("the buried entry was not materialised: {other:?}"),
    };
    assert!(
        buried > arg_lo,
        "the hazard requires the buried operand ABOVE an argument \
         (buried={buried}, arg={arg_lo}); with these two in the other order \
         the `min` would be right and this test would prove nothing"
    );
    // Depth 2 — the second argument, at the top as usual.
    compiler.push_stack().expect("second argument slot");

    // `static int leaf(int a, int b) { return a; }`
    let site = make_inline_site(&[0x1a, 0xac], 2, 2, true, b'I');
    assert!(
        compiler.try_emit_inline_site(0, &site),
        "a two-argument static leaf must splice; a bail would make every \
         assertion below vacuous"
    );

    assert!(
        compiler.next_spill_offset > buried,
        "the splice left the cursor at {} with the buried operand still \
         owning slot {buried}: the next reservation gets that address a \
         second time",
        compiler.next_spill_offset
    );
    let next = compiler.push_stack().expect("push after the splice");
    assert!(
        !matches!(next, StackSlot::Frame(off) if off == buried),
        "the push after the splice was handed slot {buried}, which the buried \
         entry {:?} still reads",
        compiler.stack[0]
    );
}

/// A nested splice must close the callee-local oop scope it opened.
///
/// `try_emit_nested_inline` used to leave its `InlineOopScope` on
/// `inline_oop_scopes` after the nested body was emitted. The enclosing
/// splice's `xreturn` then saw its OWN locals as an enclosing scope's (only the
/// last entry is exempt as "the one returning"), and the reservation floor
/// moved its result one word above `caller_post_pop_spill` -- the word the
/// splice then restores the cursor to. So the next push was handed the
/// result's own slot.
///
/// The argument is register-resident (`CalleeSaved`), so no popped argument
/// owns a frame word and `caller_post_pop_spill == callee_local_base`: the
/// result is meant to land on the returning callee's local 0, which is the
/// shape the leaked scope turned into a collision. Found as netty
/// `HttpPostRequestDecoderTest` (`NoSuchMethodError: MixedAttribute.newEncoder()`).
#[test]
pub(in crate::x64::tests) fn a_nested_splice_does_not_leave_its_oop_scope_open() {
    let alloc_result = crate::regalloc::RegAllocResult {
        assignments: Vec::new(),
        xmm_assignments: Vec::new(),
        used_callee_saved: Vec::new(),
        used_xmm_regs: Vec::new(),
        block_live_in: Vec::new(),
    };
    let mut compiler = Compiler::new(
        "inline-nested-oop-scope-test".to_string(),
        ExecutableBuffer::new(65536).expect("test executable buffer"),
        0,
        0,
        64,
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
        test_helpers(),
        0,
        false,
        false,
        false,
        false,
        false,
        Vec::new(),
    );

    // The single argument, held in a register-homed local.
    compiler.stack_push(StackSlot::CalleeSaved(R12), false);
    let cursor_before = compiler.next_spill_offset;

    // inner:  `static int inner(int a) { return a; }`
    let inner = make_inline_site(&[0x1a, 0xac], 1, 1, true, b'I');
    // outer:  `static int outer(int a) { return inner(a); }`
    //   0: iload_0; 1: invokestatic #3 -> nested splice; 4: ireturn
    let mut outer = make_inline_site(&[0x1a, 0xb8, 0x00, 0x03, 0xac], 1, 1, true, b'I');
    outer.resolved_invoke_infos =
        vec![resolved_invoke(1, "pkg/Inner", "inner", "(I)I", 1, b'I', 3)];
    outer.nested_sites = vec![crate::NestedInlineSite {
        callee_pc: 1,
        guard_class_id: 0,
        site: inner,
        bimorphic_second: None,
    }].into();

    assert!(
        compiler.try_emit_inline_site(0, &outer),
        "a static body splicing a static leaf must splice; a bail makes every \
         assertion below vacuous"
    );
    assert!(
        compiler.inline_oop_scopes.is_empty(),
        "every scope the splice tree opened must be closed again: {} left",
        compiler.inline_oop_scopes.len()
    );
    let result = match compiler.stack.last() {
        Some(StackSlot::Frame(off)) => *off,
        other => panic!("the spliced call's result must be in a frame slot, got {other:?}"),
    };
    assert_eq!(
        result, cursor_before,
        "with a register-resident argument the result belongs on the returning \
         callee's local 0, i.e. at the pre-splice cursor"
    );
    let next = compiler.push_stack().expect("push after the splice");
    assert!(
        !matches!(next, StackSlot::Frame(off) if off == result),
        "the push after the splice was handed slot {result}, which still holds \
         the spliced call's result"
    );
}


/// A reservation must not hand out a frame word that an OPEN inline scope's
/// LOCALS still own.
///
/// # What this is about
///
/// A spliced callee's locals are reserved once, at its `callee_local_base`, and
/// stay live until the wrapper pops the scope. They are NOT on the operand
/// stack, so every "is this slot still owned?" scan in the compiler — the one
/// in `pop_stack`'s reclaim arm, the one in `reset_spills`, the live-slot clamp
/// in `try_emit_inline_body` — is blind to them. Any of the ~70 places that
/// assign `next_spill_offset` can therefore leave the cursor pointing into an
/// enclosing splice's locals, and the next reservation hands that word out to a
/// second owner. The enclosing body then reads back whatever the new owner
/// stored.
///
/// Measured on `CriteriaWindowFunctionTest` with `CRATONVM_DBG=jit-slot-overlap`
/// before the floor: **151 of 304** overlap reports were against an ENCLOSING
/// scope, i.e. one whose body continues after the inner call returns. Nearly
/// all were `#1/3` — a middle scope in a three-deep splice — and in every
/// sampled report the reservation range was EXACTLY the scope's locals range.
/// With the floor: **0**, same binary, one switch, 11/11 both arms.
///
/// # Why the assertion is on the RESERVATION and not on a computed answer
///
/// The same reason its sibling above asserts on the cursor: the defect lives in
/// the spill-slot simulation, and the descent that reaches an enclosing scope's
/// locals needs a three-deep splice in an 11 KB bytebuddy body to occur
/// naturally. What can be pinned here is the invariant itself — that a
/// reservation never starts inside an open scope's locals, however the cursor
/// got there — which is exactly what `reserve_spill_slots` now enforces.
///
/// The `A` arm is `CRATONVM_JIT_NO_INLINE_LOCALS_FLOOR=1`; it is not exercised
/// here because the switch is a process-wide environment read and these tests
/// run in parallel in one process.
#[test]
pub(in crate::x64::tests) fn a_reservation_never_starts_inside_an_open_inline_scopes_locals() {
    let alloc_result = crate::regalloc::RegAllocResult {
        assignments: Vec::new(),
        xmm_assignments: Vec::new(),
        used_callee_saved: Vec::new(),
        used_xmm_regs: Vec::new(),
        block_live_in: Vec::new(),
    };
    let mut compiler = Compiler::new(
        "inline-enclosing-locals-test".to_string(),
        ExecutableBuffer::new(65536).expect("test executable buffer"),
        0,
        0,
        64,
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
        test_helpers(),
        0,
        false,
        false,
        false,
        false,
        false,
        Vec::new(),
    );

    // THREE open scopes, the `#1/3` shape the census reports, because the
    // floor is the maximum over the ENCLOSING ones only. Two scopes would leave
    // a single enclosing scope and would pass just as well against a
    // `last()`-style floor that ignores the outer one; three separates "max
    // over the enclosing scopes" from "the scope just below the top".
    let outer_base = compiler.next_spill_offset;
    let outer = InlineOopScope {
        local_base: outer_base,
        num_locals: 3,
        masks: Vec::new(),
        reached: Vec::new(),
        cur_pc: 0,
    };
    let middle_base = outer_base + 3 * 8;
    let middle = InlineOopScope {
        local_base: middle_base,
        num_locals: 2,
        masks: Vec::new(),
        reached: Vec::new(),
        cur_pc: 0,
    };
    // The top of the highest ENCLOSING scope: the floor.
    let locals_top = middle_base + 2 * 8;
    // The INNERMOST scope is the splice that is RETURNING. Its locals are dead
    // at the `xreturn` that reclaims them, and the floor must NOT include them:
    // a result pushed above them lands at the wrong operand depth, which is the
    // ECJ `OperandStack.pop(OperandCategory)` miscompile that
    // `a_spliced_callees_result_lands_at_the_callers_operand_depth` guards. The
    // first cut of this floor included it and that test caught it.
    let innermost = InlineOopScope {
        local_base: locals_top,
        num_locals: 4,
        masks: Vec::new(),
        reached: Vec::new(),
        cur_pc: 0,
    };
    compiler.inline_oop_scopes.push(outer);
    compiler.inline_oop_scopes.push(middle);
    compiler.inline_oop_scopes.push(innermost);

    assert_eq!(
        compiler.open_inline_locals_floor(),
        locals_top,
        "the floor is the top of the highest ENCLOSING scope's locals - not the \
         innermost's, whose locals are dead at the return that reclaims them"
    );

    // Any of the cursor-lowering paths, simulated directly: what matters is not
    // which assignment did it (the first attempt at this fix guarded two of
    // them and took 151 reports only to 125) but that the cursor can end up
    // here at all.
    compiler.next_spill_offset = outer_base;
    assert!(
        compiler.next_spill_offset < locals_top,
        "the hazard requires the cursor BELOW the enclosing locals; with it \
         above, this test would prove nothing"
    );

    let start = compiler
        .reserve_spill_slots(1, SpillReason::Push)
        .expect("the reservation must succeed");
    assert!(
        start >= locals_top,
        "reservation was handed slot {start}, inside the locals of an open \
         enclosing inline scope ({outer_base}..{locals_top}): that callee's \
         body continues after the inner call returns and still reads the word"
    );
    assert!(
        compiler.next_spill_offset > locals_top,
        "the cursor must also come back ABOVE the enclosing locals, or the NEXT \
         reservation walks straight back into them"
    );

    // A LONE open scope IS the innermost, so the floor is inert for it: a
    // single splice must keep landing its result at the caller's operand depth.
    compiler.inline_oop_scopes.truncate(1);
    assert_eq!(
        compiler.open_inline_locals_floor(),
        i32::MIN,
        "one open splice has no ENCLOSING scope, so nothing may be floored"
    );

    // And with nothing spliced at all the floor is inert - this must not cost a
    // frame word in the methods that inline nothing, which is most of them.
    compiler.inline_oop_scopes.clear();
    assert_eq!(
        compiler.open_inline_locals_floor(),
        i32::MIN,
        "no open scope must mean no floor"
    );
    let before = compiler.next_spill_offset;
    let plain = compiler
        .reserve_spill_slots(1, SpillReason::Push)
        .expect("the reservation must succeed");
    assert_eq!(
        plain, before,
        "with no splice open a reservation must start exactly at the cursor"
    );
}

/// A reservation that fits ENTIRELY UNDER an open enclosing scope's locals is
/// left where it is.
///
/// This is the other half of
/// `a_reservation_never_starts_inside_an_open_inline_scopes_locals`, and the
/// two together are the whole contract: the floor moves a reservation that
/// would TOUCH an enclosing splice's locals, and only that one.
///
/// The first cut of the floor could not tell them apart. It compared the
/// cursor against the TOP of the highest enclosing scope's locals -- a
/// one-sided test -- so it also displaced reservations sitting below those
/// locals, which own nothing and overlap nothing.
///
/// That displacement was a miscompile. Measured 2026-09-10 on
/// `scala.runtime.Statics.anyHash(Long)`: ONE floor bump in the whole run,
/// overlapping NO scope, and the compiled body then answered a per-run
/// constant for every argument -- 299,498 wrong hashes in 300,000 calls, 9 runs
/// in 10, against 0 in 10 with `CRATONVM_JIT_NO_INLINE_LOCALS_FLOOR=1`. Its
/// page:
/// `known-issues/springboot/kafka-scala-statics-anyhash-jit-miscompile-20260910.md`.
///
/// So this test asserts a NON-movement, which is the thing a guard's own test
/// usually cannot see. Without it, restoring the one-sided rule -- which is a
/// strictly more conservative-LOOKING change, and the obvious "fix" for any
/// future overlap report -- passes every other test in this file.
#[test]
pub(in crate::x64::tests) fn a_reservation_that_fits_under_an_open_scopes_locals_is_not_moved() {
    let alloc_result = crate::regalloc::RegAllocResult {
        assignments: Vec::new(),
        xmm_assignments: Vec::new(),
        used_callee_saved: Vec::new(),
        used_xmm_regs: Vec::new(),
        block_live_in: Vec::new(),
    };
    let mut compiler = Compiler::new(
        "inline-locals-range-test".to_string(),
        ExecutableBuffer::new(65536).expect("test executable buffer"),
        0,
        0,
        64,
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
        test_helpers(),
        0,
        false,
        false,
        false,
        false,
        false,
        Vec::new(),
    );

    // Two scopes, so exactly one is ENCLOSING and the floor is its top. Its
    // locals start four words above the cursor, leaving a gap that a small
    // reservation fits inside without touching them.
    let cursor = compiler.next_spill_offset;
    let enclosing_base = cursor + 4 * 8;
    let enclosing_top = enclosing_base + 2 * 8;
    compiler.inline_oop_scopes.push(InlineOopScope {
        local_base: enclosing_base,
        num_locals: 2,
        masks: Vec::new(),
        reached: Vec::new(),
        cur_pc: 0,
    });
    compiler.inline_oop_scopes.push(InlineOopScope {
        local_base: enclosing_top,
        num_locals: 3,
        masks: Vec::new(),
        reached: Vec::new(),
        cur_pc: 0,
    });

    // The premise: the one-sided rule WOULD move this reservation. Without
    // this the test would pass against either rule and prove nothing.
    assert_eq!(
        compiler.open_inline_locals_floor(),
        enclosing_top,
        "the floor is the top of the enclosing scope's locals"
    );
    assert!(
        cursor < enclosing_top,
        "the cursor must be below the one-sided floor, or this test is vacuous"
    );

    // One word at the cursor: [cursor, cursor + 8) against locals
    // [enclosing_base, enclosing_top) -- four words apart, no intersection.
    assert_eq!(
        compiler.inline_locals_clear_of_range(cursor, 1),
        cursor,
        "a reservation that touches no enclosing scope's locals must not move"
    );
    let start = compiler
        .reserve_spill_slots(1, SpillReason::Push)
        .expect("the reservation must succeed");
    assert_eq!(
        start, cursor,
        "the reservation was displaced past locals it does not overlap; that \
         is the one-sided rule, and it miscompiled Statics.anyHash"
    );

    // And the range rule still moves one that DOES reach them: four words from
    // the cursor is exactly `enclosing_base`, so a five-word reservation from
    // the cursor covers the enclosing scope's first local.
    assert_eq!(
        compiler.inline_locals_clear_of_range(cursor, 5),
        enclosing_top,
        "a reservation whose RANGE reaches the enclosing locals must clear \
         them entirely, not merely start above their base"
    );

    // A zero-word reservation occupies nothing, so it intersects nothing.
    assert_eq!(
        compiler.inline_locals_clear_of_range(cursor, 0),
        cursor,
        "a zero-word reservation owns no word and must not be moved"
    );

    // A lone open splice IS the innermost, so nothing is enclosing and the
    // range rule is inert for it -- the same exemption the one-sided floor has.
    compiler.inline_oop_scopes.truncate(1);
    assert_eq!(
        compiler.inline_locals_clear_of_range(cursor, 5),
        cursor,
        "one open splice has no ENCLOSING scope, so nothing may be moved"
    );
}

#[test]
pub(in crate::x64::tests) fn push_stack_refuses_to_cross_spill_limit() {
    let alloc_result = crate::regalloc::RegAllocResult {
        assignments: Vec::new(),
        xmm_assignments: Vec::new(),
        used_callee_saved: Vec::new(),
        used_xmm_regs: Vec::new(),
        block_live_in: Vec::new(),
    };
    let mut compiler = Compiler::new(
        "spill-limit-test".to_string(),
        ExecutableBuffer::new(4096).expect("test executable buffer"),
        0,
        0,
        1,
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
        test_helpers(),
        0,
        false,
        false,
        false,
        false,
        // No `invokedynamic` in this fixture, so no register-spilling
        // frame-deopt stub and nothing to reserve a spill region for.
        false,
        Vec::new(),
    );

    // Fill the spill region rather than assuming a single push exhausts it.
    // `spill_size` reserves `max_stack` slots PLUS the direct-call
    // argument-service headroom, so the limit no longer sits at
    // `max_stack`. What this test pins is the refusal AT the limit — and
    // that the refusal leaves the cursor untouched — not where the limit
    // happens to fall.
    let mut pushes = 0;
    while compiler.next_spill_offset < compiler.spill_limit_offset {
        assert!(
            matches!(compiler.push_stack(), Some(StackSlot::Frame(_))),
            "push {pushes} is still inside the spill region and must succeed",
        );
        pushes += 1;
        assert!(!compiler.failed, "push {pushes} must not have failed");
    }
    assert!(pushes > 0, "the spill region must hold at least one slot");
    assert_eq!(compiler.next_spill_offset, compiler.spill_limit_offset);

    let cursor = compiler.next_spill_offset;
    assert!(compiler.push_stack().is_none());
    assert!(compiler.failed);
    assert_eq!(compiler.next_spill_offset, cursor);
}

// ---- Test stub helpers for getfield/putfield ----
// These mirror the real helpers in vm/src/jit/helpers.rs but live in the
// jit crate so unit tests can exercise compiled code without pulling in
// the full VM.

// Read `num_slots` from the object header at the given pointer.
// `num_slots` is at `NUM_SLOTS_OFFSET` (8), a LONG header's shape word --
// valid for the legacy objects these stubs are handed; a compact instance
// has none.
//
// # Safety
// `obj_ptr` must point to a valid, properly aligned `ObjectHeader` that has not been freed.
// SAFETY: obj_ptr points to a live, properly aligned ObjectHeader (repr(C)); reading the
// u32 num_slots field at its fixed offset is in-bounds and the object is not freed.
pub(in crate::x64::tests) unsafe fn read_num_slots(obj_ptr: *const u8) -> u32 {
    std::ptr::read(obj_ptr.add(cratonvm_types::NUM_SLOTS_OFFSET) as *const u32)
}

/// Records what the last `stub_invoke_dispatch` call received.
///
/// A spliced call is emitted, not executed, by most of these tests — and an
/// assertion that machine code "contains a CALL" proves very little about
/// whether the ARGUMENTS reached it in the right buffer, in the right order.
/// This lets a test actually run the code and read back what the helper saw.
///
/// Thread-local because the test harness calls compiled code on the test's own
/// thread and several inline tests run concurrently under `cargo test`.
thread_local! {
    pub(in crate::x64::tests) static LAST_DISPATCH: std::cell::RefCell<Option<(String, String, String, Vec<i64>)>> =
        const { std::cell::RefCell::new(None) };
}

/// Test stand-in for `jit_invoke_dispatch`.
///
/// The real helper's contract, and all this reproduces: read `num_jit_args`
/// i64s from `args` (arg[0] at the LOWEST address), and return the callee's
/// result in RAX. It returns the SUM of the arguments, which is a value a test
/// can predict exactly and which changes if the buffer is built in the wrong
/// order with any argument set that is not symmetric.
///
/// SAFETY: called from JIT-compiled code that passes the `JitInvokeInfo`
/// pointer this compile interned and an `args` buffer of exactly `num_args`
/// i64 slots, per `emit_inline_invoke`.
pub(in crate::x64::tests) unsafe extern "C" fn stub_invoke_dispatch(
    _vm_ptr: i64,
    info: *const crate::JitInvokeInfo,
    args: *const i64,
    num_args: i32,
) -> i64 {
    let mut seen = Vec::new();
    let mut sum: i64 = 0;
    for i in 0..num_args.max(0) {
        let v = std::ptr::read(args.add(i as usize)); // Cast: ABI buffer index
        seen.push(v);
        sum = sum.wrapping_add(v);
    }
    let (c, m, d) = if info.is_null() {
        (String::new(), String::new(), String::new())
    } else {
        let r = &*info;
        (
            r.class_name.to_string(),
            r.method_name.to_string(),
            r.descriptor.to_string(),
        )
    };
    LAST_DISPATCH.with(|slot| *slot.borrow_mut() = Some((c, m, d, seen)));
    sum
}

// SAFETY: Called from JIT-compiled code which passes a valid heap-allocated object pointer
// and a field index that is bounds-checked within the function body before any dereference.
//
// This is a test double for `jit_getfield`, so it has to decode the ARGUMENT the
// same way: the third parameter is a slot index PLUS the flag bits
// `GETFIELD_FLAG_BITS` (`GETFIELD_RECEIVER_PROVEN_OOP`, and since 2026-08-23
// `GETFIELD_EXPECT_REFERENCE`). Stripping them is not optional here. The
// original stub checked `field_index as u32` against `num_slots` and then
// indexed with `field_index as usize`: with a flag set, the u32 truncation
// passed the bounds check and the usize index walked off the object, which
// aborted the whole test binary the first time a reference load carried a flag.
// Check and index now use one value.
pub(in crate::x64::tests) unsafe extern "C" fn stub_getfield(_vm_ptr: i64, obj_ptr: i64, field_index: i64) -> i64 {
    if obj_ptr == 0 {
        return 0;
    }
    let field_index = cratonvm_jit_api::getfield_index_of(field_index);
    let base = obj_ptr as *const u8; // Cast: address arithmetic
    let num_slots = read_num_slots(base);
    if field_index < 0 || field_index as u64 >= num_slots as u64 {
        return 0;
    }
    let ptr = base.add(HEADER_SIZE + field_index as usize * SLOT_SIZE); // Cast: address arithmetic
    let val: Value = std::ptr::read(ptr as *const Value); // Cast: address arithmetic
    match val {
        Value::Int(i) => i as i64, // Cast: JIT ABI convention
        Value::Long(l) => l,
        Value::Float(f) => f.to_bits() as i64, // Cast: JIT ABI convention
        Value::Double(d) => d.to_bits() as i64, // Cast: JIT ABI convention
        Value::Object(Some(r)) => r.as_ptr() as i64, // Cast: JIT ABI convention
        Value::Object(None) => 0,
        _ => 0,
    }
}

// SAFETY: Called from JIT-compiled code which passes a valid heap-allocated object pointer
// and a field index that is bounds-checked within the function body before any write.
pub(in crate::x64::tests) unsafe extern "C" fn stub_putfield_int(obj_ptr: i64, field_index: i64, val: i64) {
    if obj_ptr == 0 {
        return;
    }
    let base = obj_ptr as *const u8; // Cast: address arithmetic
    let num_slots = read_num_slots(base);
    if field_index < 0 || field_index as u32 >= num_slots {
        return;
    } // Cast: x86-64 immediate encoding
    let ptr = (obj_ptr as *mut u8).add(HEADER_SIZE + field_index as usize * SLOT_SIZE); // Cast: address arithmetic
    std::ptr::write(ptr as *mut Value, Value::Int(val as i32)); // Cast: address arithmetic
}

// SAFETY: Called from JIT-compiled code which passes a valid heap-allocated object pointer
// and a field index that is bounds-checked within the function body before any write.
pub(in crate::x64::tests) unsafe extern "C" fn stub_putfield_long(obj_ptr: i64, field_index: i64, val: i64) {
    if obj_ptr == 0 {
        return;
    }
    let base = obj_ptr as *const u8; // Cast: address arithmetic
    let num_slots = read_num_slots(base);
    if field_index < 0 || field_index as u32 >= num_slots {
        return;
    } // Cast: x86-64 immediate encoding
    let ptr = (obj_ptr as *mut u8).add(HEADER_SIZE + field_index as usize * SLOT_SIZE); // Cast: address arithmetic
    std::ptr::write(ptr as *mut Value, Value::Long(val)); // Cast: address arithmetic
}

// SAFETY: Called from JIT-compiled code which passes a valid heap-allocated object pointer
// and a field index that is bounds-checked within the function body before any write.
pub(in crate::x64::tests) unsafe extern "C" fn stub_putfield_float(obj_ptr: i64, field_index: i64, val: i64) {
    if obj_ptr == 0 {
        return;
    }
    let base = obj_ptr as *const u8; // Cast: address arithmetic
    let num_slots = read_num_slots(base);
    if field_index < 0 || field_index as u32 >= num_slots {
        return;
    } // Cast: x86-64 immediate encoding
    let ptr = (obj_ptr as *mut u8).add(HEADER_SIZE + field_index as usize * SLOT_SIZE); // Cast: address arithmetic
    std::ptr::write(ptr as *mut Value, Value::Float(f32::from_bits(val as u32)));
    // Cast: address arithmetic
}

// SAFETY: Called from JIT-compiled code which passes a valid heap-allocated object pointer
// and a field index that is bounds-checked within the function body before any write.
pub(in crate::x64::tests) unsafe extern "C" fn stub_putfield_double(obj_ptr: i64, field_index: i64, val: i64) {
    if obj_ptr == 0 {
        return;
    }
    let base = obj_ptr as *const u8; // Cast: address arithmetic
    let num_slots = read_num_slots(base);
    if field_index < 0 || field_index as u32 >= num_slots {
        return;
    } // Cast: x86-64 immediate encoding
    let ptr = (obj_ptr as *mut u8).add(HEADER_SIZE + field_index as usize * SLOT_SIZE); // Cast: address arithmetic
    std::ptr::write(ptr as *mut Value, Value::Double(f64::from_bits(val as u64)));
    // Cast: address arithmetic
}

// SAFETY: Called from JIT-compiled code which passes a valid heap-allocated object pointer
// and a field index that is bounds-checked within the function body before any write.
// val is either 0 (null) or a valid ObjectRef pointer from the managed heap.
pub(in crate::x64::tests) unsafe extern "C" fn stub_putfield_object(_vm_ptr: i64, obj_ptr: i64, field_index: i64, val: i64) {
    if obj_ptr == 0 {
        return;
    }
    let base = obj_ptr as *const u8; // Cast: address arithmetic
    let num_slots = read_num_slots(base);
    if field_index < 0 || field_index as u32 >= num_slots {
        return;
    } // Cast: x86-64 immediate encoding
    let ptr = (obj_ptr as *mut u8).add(HEADER_SIZE + field_index as usize * SLOT_SIZE); // Cast: address arithmetic
    if val == 0 {
        std::ptr::write(ptr as *mut Value, Value::Object(None)); // Cast: address arithmetic
    } else {
        let obj_ref = ObjectRef::from_raw(val as usize as *mut u8); // Cast: address arithmetic
        std::ptr::write(ptr as *mut Value, Value::Object(Some(obj_ref))); // Cast: address arithmetic
    }
}

/// Helpers for tests -- provides real getfield/putfield stubs so that
/// compiled code can read/write heap objects.  Other helpers point to a
/// stub that panics with a clear message if called unexpectedly.
pub(in crate::x64::tests) fn test_helpers() -> JitRuntimeHelpers {
    // Stub that panics -- used for helpers not wired up in these tests.
    // SAFETY: This stub is registered as a function pointer in JitRuntimeHelpers but
    // should never be called during these tests; it panics to flag unexpected invocations.
    unsafe extern "C" fn unimplemented_stub() {
        panic!("JIT test helper called an unimplemented runtime stub");
    }
    let sentinel = unimplemented_stub as *const () as usize; // Cast: address arithmetic
                                                             // `set_throw_bci` only records the throwing bci in a thread-local and is
                                                             // called on the throw path of every method carrying an exception check,
                                                             // so it needs a real no-op rather than the panicking stub.
    unsafe extern "C" fn record_throw_bci(_bci: i64) {}
    let throw_bci = record_throw_bci as *const () as usize; // Cast: address arithmetic
    JitRuntimeHelpers {
        newarray: sentinel,
        new_object: sentinel,
        anewarray_object: sentinel,
        baload: sentinel,
        bastore: sentinel,
        iaload: sentinel,
        iastore: sentinel,
        aaload: sentinel,
        aastore: sentinel,
        multianewarray_2d: sentinel,
        multianewarray_n: sentinel,
        arraylength: sentinel,
        getfield: stub_getfield as *const () as usize, // Cast: address arithmetic
        putfield_int: stub_putfield_int as *const () as usize, // Cast: address arithmetic
        putfield_long: stub_putfield_long as *const () as usize, // Cast: address arithmetic
        putfield_float: stub_putfield_float as *const () as usize, // Cast: address arithmetic
        putfield_double: stub_putfield_double as *const () as usize, // Cast: address arithmetic
        putfield_object: stub_putfield_object as *const () as usize, // Cast: address arithmetic
        getstatic: sentinel,
        putstatic_int: sentinel,
        putstatic_long: sentinel,
        putstatic_float: sentinel,
        putstatic_double: sentinel,
        putstatic_object: sentinel,
        checkcast: sentinel,
        instanceof_check: sentinel,
        throw_aioobe: sentinel,
        throw_arithmetic: sentinel,
        // Was `sentinel`. Nothing executed it then — a test that did would
        // have jumped to a bogus address — so wiring a real stub changes no
        // existing test's behaviour and lets the spliced-call tests below
        // run the code instead of merely inspecting it.
        invoke_dispatch: stub_invoke_dispatch as *const () as usize, // Cast: address arithmetic
        invoke_virtual_mic: sentinel,
        lambda_int_to_double: sentinel,
        write_barrier: sentinel,
        satb_pre_write_barrier: sentinel,
        uncommon_trap: sentinel,
        math_fma_double: sentinel,
        math_fma_float: sentinel,
        // Inline TLAB bump wiring is exercised only in the real VM
        // helper-table path. Tests use the helper-call fallback so
        // leave the cursor/end offsets at 0 (layout-safe for `Tlab`
        // with `cursor` at offset 0) and the optional thread-pointer
        // helper unset — `get_current_thread == 0` tells the JIT to
        // emit the pre-existing `new_object` call.
        tlab_cursor_offset_in_thread: 0,
        tlab_end_offset_in_thread: 8,
        class_id_offset_in_obj: 0,
        get_current_thread: 0,
        tlab_post_init: 0,
        frame_record: 0,
        shadow_stack_offset_in_thread: 0,
        throw_exception: sentinel,
        set_throw_bci: throw_bci,
        service_callee_deopt: sentinel,
        jit_npe_with_action: sentinel,
        dispatch_threw: sentinel,
        jit_frem: sentinel,
        jit_drem: sentinel,
        // Unwired (0) — the self-call arm emits the legacy direct CALL
        // with no stack guard, keeping these tests byte-identical.
        self_call_stack_guard: 0,
        region_bounds_addr: 0,
        read_bounds_addr: 0,
        // Compiled local handlers are never armed in a backend unit test: the
        // feature is flag-gated and the tables here are hand-built.
        local_handler_lookup: 0,
        native_stack_floor_fn: 0,
        ldc_string: sentinel,
        // Unwired (0) — CRATONVM_JIT_SAFEPOINT_POLLS is off by default,
        // and `emit_safepoint_poll` also requires this to be non-zero,
        // so leaving it 0 keeps these tests byte-identical either way.
        safepoint_flag_addr: 0,
        safepoint_slow_path: 0,
        jit_card_table_addr: 0,
        jit_card_old_base: 0,
        jit_card_old_end: 0,
        // Unwired (0) — these tests never build a deferred (not-yet-loaded)
        // `new`/`anewarray` site, and 0 makes the backend refuse one rather
        // than emit a null CALL, so the tests stay byte-identical.
        new_object_cp: 0,
        anewarray_object_cp: 0,
        // 0 is meaningful here: `ir_lower` refuses a graph containing
        // monitor ops when the helper is absent, rather than emitting
        // nothing for them.
        monitor_enter: 0,
        monitor_exit: 0,
        // Unwired (0) — these tests build no class-`ldc` site, and 0 makes
        // the backend refuse one rather than emit a null CALL.
        ldc_class_cp: 0,
        // Wired to the panicking sentinel: the `0x53` arm is unconditionally
        // inline and always emits a CALL to this slot, and the slot is
        // `required` in `jit-api`, so 0 would fail `validate()` rather than
        // select a different lowering.
        aastore_type_check: sentinel,
        // Unwired (0), for the same reason `ldc_class_cp` is: these tests
        // build no string-`ldc` site, and 0 makes the backend refuse one
        // rather than emit a null CALL. `ldc_string` above stays sentinel-wired
        // because its slot is `required` in `jit-api`; nothing calls it.
        ldc_string_cp: 0,
        // 0 = not wired: this hand-built table emits no FFM fast path, so every
        // accessor site in these tests keeps its ordinary native dispatch.
        ffm_segment_get: 0,
        ffm_segment_set: 0,
        // 0 = no collector published a reference-store barrier plan, so every
        // ref-store site in these tests keeps the full-helper path. That is
        // what makes the gated sequence additive: a hand-built table gets the
        // pre-existing emission, byte for byte.
        ref_store_pre_gate: 0,
        ref_store_post_gate: 0,
        ref_store_post_young_floor: 0,
        // F-08: 0 = not wired. `g1_inline_barrier_available` requires BOTH a
        // published `JIT_G1_BARRIER` table and this helper, so these tests emit
        // no inline G1 barrier and every reference store keeps the
        // `putfield_object` call it has always emitted here. The dedicated F-08
        // tests below build their own table and wire their own helper.
        g1_barrier_addr: 0,
        g1_post_write_barrier: 0,
        ref_store_post_skip_mask: 0,
        zgc_note_tlab_object: 0,
        // 0 = not wired: no inline thin-lock, every monitor site keeps its
        // helper call (which `live_monitor_ops_execute_direct_runtime_stubs`
        // below counts).
        monitor_block_offset_in_thread: 0,
        // 0 = not wired: inline-cache stack checks keep their frame slot.
        jit_stack_floor_offset_in_thread: 0,
    }
}

#[cfg(target_arch = "x86_64")]
#[test]
pub(in crate::x64::tests) fn live_monitor_ops_execute_direct_runtime_stubs() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static ENTER_CALLS: AtomicUsize = AtomicUsize::new(0);
    static EXIT_CALLS: AtomicUsize = AtomicUsize::new(0);
    static LAST_CONTEXT: AtomicUsize = AtomicUsize::new(0);
    static LAST_OBJECT: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn monitor_enter_stub(context: i64, object: i64) -> i64 {
        LAST_CONTEXT.store(context as usize, Ordering::SeqCst);
        LAST_OBJECT.store(object as usize, Ordering::SeqCst);
        ENTER_CALLS.fetch_add(1, Ordering::SeqCst);
        object
    }

    unsafe extern "C" fn monitor_exit_stub(context: i64, object: i64) -> i64 {
        LAST_CONTEXT.store(context as usize, Ordering::SeqCst);
        LAST_OBJECT.store(object as usize, Ordering::SeqCst);
        EXIT_CALLS.fetch_add(1, Ordering::SeqCst);
        object
    }

    ENTER_CALLS.store(0, Ordering::SeqCst);
    EXIT_CALLS.store(0, Ordering::SeqCst);
    let direct_helpers = crate::DirectHelperTable {
        monitor_enter: monitor_enter_stub as *const () as usize,
        monitor_exit: monitor_exit_stub as *const () as usize,
        ..crate::DirectHelperTable::EMPTY
    };

    // static int locked(Object o) {
    //     monitorenter(o); monitorexit(o); return 7;
    // }
    // Keeping the receiver live and passing no scalar-replacement facts
    // proves both bytecodes reach the runtime lowering rather than the
    // exact per-site elision path.
    let code = [
        0x2a, // aload_0
        0xc2, // monitorenter
        0x2a, // aload_0
        0xc3, // monitorexit
        0x10, 0x07, // bipush 7
        0xac, // ireturn
        0x00, 0x00,
    ];
    let compiled = compile_with_request(
        BackendRequest {
            direct_helpers,
            ..BackendRequest::default()
        },
        Vec::new(), // compact_field_info
        &code,
        7,
        1,
        1,
        true,
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
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None,
    )
    .expect("live monitor operations should compile through direct stubs");
    assert!(
        compiled.has_dispatch,
        "live monitor helpers require dispatch-aware entry to publish JIT_THREAD"
    );

    let context = 0x1111_i64;
    let object = 0x2222_i64;
    // SAFETY: the generated method has the context ABI and one i64 object
    // parameter; the test stubs do not dereference either synthetic value.
    let result = unsafe {
        compiled
            .try_call_with_context(context, &[object])
            .expect("compiled monitor method should execute")
    };

    assert_eq!(result, 7);
    assert_eq!(ENTER_CALLS.load(Ordering::SeqCst), 1);
    assert_eq!(EXIT_CALLS.load(Ordering::SeqCst), 1);
    assert_eq!(LAST_CONTEXT.load(Ordering::SeqCst), context as usize);
    assert_eq!(LAST_OBJECT.load(Ordering::SeqCst), object as usize);
}

// -----------------------------------------------------------------------
// Regression: moving-young-gen-drops-jit-held-oops-FIXED.md
// -----------------------------------------------------------------------
//
// `BinTreesClassic.bottomUpTree` holds the result of its FIRST recursive
// call on the operand stack across its SECOND — an entire subtree. The
// direct self-recursive `invokestatic` arm pushed that result without
// tagging it as a reference, so `collect_live_oop_homes` never published
// it, `emit_oop_map_for_safepoint` never recorded its slot, and
// `moving_young_safepoint_coverage_complete` certified the frame anyway
// (it only checks that MARKED entries have frame/register homes). Under
// `CRATONVM_MOVING_YOUNG` the conservative frame scan is suppressed on a
// certified frame, so the subtree was neither marked nor rewritten and
// bt18 returned a wrong, run-varying checksum.

/// Compile `static <ret> f(int)` whose body is two direct self-recursive
/// calls with the first result live across the second, and return the oop
/// map recorded at the second call (bytecode pc 5).
pub(in crate::x64::tests) fn self_recursive_second_call_map(method_key: &str) -> Option<crate::OopMapEntry> {
    //  0: iload_0
    //  1: invokestatic f      -> r1 pushed
    //  4: iload_0
    //  5: invokestatic f      -> SAFEPOINT, r1 live on the operand stack
    //  8: pop
    //  9: areturn
    let code = [0x1a, 0xb8, 0x00, 0x00, 0x1a, 0xb8, 0x00, 0x00, 0x57, 0xb0];
    let helpers = test_helpers();
    let compiled = compile_with_param_slots(
        &crate::compile_gate::CompileAdmission::for_backend_test(),
        &code,
        code.len(),
        1,
        1,
        false,
        Vec::new(),         // multianewarray_info
        Vec::new(),         // field_info
        Vec::new(),         // typecheck_info
        Vec::new(),         // static_field_info
        Vec::new(),         // new_info
        Vec::new(),         // new_deferred_info
        Vec::new(),         // anewarray_info
        Vec::new(),         // anewarray_deferred_info
        Vec::new(),         // invoke_info
        Vec::new(),         // direct_calls
        Vec::new(),         // mic_slots
        Vec::new(),         // pic_slots
        Vec::new(),         // ldc_info
        Vec::new(),         // ldc_string_info
        Vec::new(),         // ldc_class_info
        Vec::new(),         // ldc2w_info
        Default::default(), // ldc_fp_pcs
        HashMap::new(),
        HashMap::new(),
        &helpers,
        std::collections::HashSet::new(),
        HashMap::new(),
        HashMap::new(), // inline_guard_variants (PGO-02)
        None,
        &[0],
        1,
        0,
        Vec::new(),
        method_key,
        None, // despec: no VM
        Vec::new(),
        None, // elidable_init_pcs: no constant pool, so nothing is proven empty,
        crate::x64::BackendRequest::default(),
    )?;
    compiled
        .oop_maps
        .iter()
        .find(|m| m.bytecode_pc == 5)
        .cloned()
}

/// `static T f(int d) { return new T(f(d), f(d)); }` — `bottomUpTree`'s shape —
/// compiled with allocation sinking forced to `on`.
pub(in crate::x64::tests) fn compile_sunk_ctor_method(on: bool) -> CompiledMethod {
    compile_ctor_shape(on, BOTTOM_UP_TREE_SHAPE, 12)
}

//  0: new T ; 3: dup ; 4: iload_0 ; 5: invokestatic f (self) ; 8: iload_0 ;
//  9: invokestatic f (self) ; 12: invokespecial T.<init>(TT)V ; 15: areturn
pub(in crate::x64::tests) const BOTTOM_UP_TREE_SHAPE: &[u8] = &[
    0xbb, 0x00, 0x02, 0x59, 0x1a, 0xb8, 0x00, 0x00, 0x1a, 0xb8, 0x00, 0x00, 0xb7, 0x00, 0x04, 0xb0,
];

/// `code` is a `static T f(int)` whose `new T` is at pc 0 and whose
/// `invokespecial T.<init>(TT)V` (spliced) is at `init_pc`.
pub(in crate::x64::tests) fn compile_ctor_shape(on: bool, code: &[u8], init_pc: usize) -> CompiledMethod {
    // T(T l, T r) { left = l; right = r; }
    let mut site = make_inline_site(
        &[
            0x2a, 0x2b, 0xb5, 0x00, 0x01, 0x2a, 0x2c, 0xb5, 0x00, 0x02, 0xb1,
        ],
        3,
        3,
        false,
        b'V',
    );
    site.method_name = "<init>".to_string();
    site.descriptor = "(LT;LT;)V".to_string();
    site.field_info = vec![(2usize, 0usize, b'L'), (7usize, 1usize, b'L')];
    site.compact_field_info = vec![(2usize, 8u32, true), (7usize, 16u32, true)];
    site.needs_heap = true;
    let mut sites = HashMap::new();
    sites.insert(init_pc, site);
    let helpers = test_helpers();
    crate::x64::licm::ALLOC_SINK_FORCE.with(|c| c.set(Some(on)));
    let compiled = compile_with_param_slots(
        &crate::compile_gate::CompileAdmission::for_backend_test(),
        code,
        code.len(),
        1,
        1,
        true,
        Vec::new(),                                 // multianewarray_info
        Vec::new(),                                 // field_info
        Vec::new(),                                 // typecheck_info
        Vec::new(),                                 // static_field_info
        vec![(0usize, 7u32, 2usize, false, false)], // new_info
        Vec::new(),                                 // new_deferred_info
        Vec::new(),                                 // anewarray_info
        Vec::new(),                                 // anewarray_deferred_info
        Vec::new(),                                 // invoke_info
        Vec::new(),                                 // direct_calls
        Vec::new(),                                 // mic_slots
        Vec::new(),                                 // pic_slots
        Vec::new(),                                 // ldc_info
        Vec::new(),                                 // ldc_string_info
        Vec::new(),                                 // ldc_class_info
        Vec::new(),                                 // ldc2w_info
        Default::default(),                         // ldc_fp_pcs
        HashMap::new(),
        HashMap::new(),
        &helpers,
        std::collections::HashSet::new(),
        sites,
        HashMap::new(), // inline_guard_variants (PGO-02)
        None,
        &[0],
        1,
        0,
        Vec::new(),
        "T.f:(I)LT;",
        None, // despec: no VM
        Vec::new(),
        None, // elidable_init_pcs
        crate::x64::BackendRequest::default(),
    );
    crate::x64::licm::ALLOC_SINK_FORCE.with(|c| c.set(None));
    compiled.expect("the constructor shape compiles")
}

/// With no recursive call between `new` and its constructor there is nothing
/// to sink past: `new T(null, null)` allocates at `new`, as before.
#[test]
pub(in crate::x64::tests) fn a_new_with_no_call_before_its_constructor_is_not_sunk() {
    // 0: new T ; 3: dup ; 4: aconst_null ; 5: aconst_null ;
    // 6: invokespecial T.<init>(TT)V ; 9: areturn
    let leaf = [0xbb, 0x00, 0x02, 0x59, 0x01, 0x01, 0xb7, 0x00, 0x04, 0xb0];
    let on = compile_ctor_shape(true, &leaf, 6);
    let off = compile_ctor_shape(false, &leaf, 6);
    assert!(
        on.oop_maps.iter().any(|e| e.bytecode_pc == 0),
        "allocates at `new`"
    );
    assert_eq!(
        on.code_bytes().len(),
        off.code_bytes().len(),
        "and emits the same code"
    );
}

/// Allocation sinking moves `new`'s allocation — and its safepoint — to the
/// constructor call, so neither recursive call publishes the half-built
/// object, and a frame state taken at either call describes it as a virtual
/// object the resume can rebuild.
#[test]
pub(in crate::x64::tests) fn a_sunk_new_allocates_at_its_constructor_and_is_virtual_in_between() {
    let slots_at = |m: &CompiledMethod, pc: u32| {
        m.oop_maps
            .iter()
            .find(|e| e.bytecode_pc == pc)
            .map(|e| e.frame_slot_offsets.len())
    };
    let off = compile_sunk_ctor_method(false);
    let on = compile_sunk_ctor_method(true);

    // Where the allocation's safepoint is.
    assert!(
        slots_at(&off, 0).is_some(),
        "unsunk: `new` allocates at pc 0"
    );
    assert!(
        slots_at(&on, 0).is_none(),
        "sunk: nothing allocates at pc 0"
    );
    assert_eq!(
        slots_at(&on, 12),
        Some(2),
        "sunk: the allocation at the constructor call publishes both argument subtrees"
    );

    // What the two recursive calls publish.
    assert_eq!(slots_at(&off, 5), Some(2), "unsunk: the object and its dup");
    assert_eq!(
        slots_at(&on, 5),
        Some(0),
        "sunk: nothing is live at the first call"
    );
    assert_eq!(
        slots_at(&off, 9),
        Some(3),
        "unsunk: object, dup, first subtree"
    );
    assert_eq!(slots_at(&on, 9), Some(1), "sunk: only the first subtree");

    // A frame state at either call rebuilds the uninitialized object.
    let at_calls: Vec<_> = on
        .deopt_points
        .iter()
        .filter(|p| p.bci == 5 || p.bci == 9)
        .collect();
    assert_eq!(
        at_calls.len(),
        2,
        "each recursive call records a deopt snapshot"
    );
    for p in &at_calls {
        let st = &p.frame_state.stack;
        match &st[0] {
            crate::deopt::FrameValue::VirtualObject(v) => {
                assert_eq!((v.id, v.class_id, v.num_fields), (0, 7, 2));
                assert!(v
                    .field_values
                    .iter()
                    .all(|f| matches!(f, crate::deopt::FrameValue::Undefined)));
            }
            other => panic!("bci {}: the `new` slot is {other:?}", p.bci),
        }
        assert!(
            matches!(st[1], crate::deopt::FrameValue::VirtualObjectRef(0)),
            "bci {}: the dup is a second reference to the same object",
            p.bci
        );
        assert!(crate::deopt::frame_state_is_resumable(&p.frame_state));
    }
}

#[test]
pub(in crate::x64::tests) fn self_recursive_reference_return_is_published_as_an_oop() {
    let map = self_recursive_second_call_map("T.f:(I)Ljava/lang/Object;")
        .expect("the second self-call is a GC-capable safepoint and records a map");
    assert!(
        !map.frame_slot_offsets.is_empty(),
        "the first call's result is a live reference on the operand stack across \
         the second call; leaving it untagged is the measured bt18 moving-young \
         heap corruption (defects-and-fixes/fixed-suite-bugs/app-jvm-bugs/\
             moving-young-gen-drops-jit-held-oops-FIXED.md)",
    );
}

#[test]
pub(in crate::x64::tests) fn self_recursive_primitive_return_is_not_published_as_an_oop() {
    let map = self_recursive_second_call_map("T.f:(I)I")
        .expect("the second self-call is a GC-capable safepoint and records a map");
    assert!(
        map.frame_slot_offsets.is_empty(),
        "an int result must NOT be tagged: publishing a primitive as a movable \
         root would have the collector relocate whatever its bit pattern names",
    );
}

#[test]
pub(in crate::x64::tests) fn self_recursive_return_without_a_descriptor_fails_closed() {
    // The legacy `compile()` wrapper passes an empty `method_key`, so the
    // return type is unknown. Guessing either way is unsafe, so the frame
    // stops certifying moving-young coverage instead and the collector
    // takes the non-moving sweep for that cycle.
    let map = self_recursive_second_call_map("")
        .expect("the second self-call is a GC-capable safepoint and records a map");
    assert!(
        !map.moving_young_coverage_complete,
        "an unknown self-call return type must mark the safepoint's coverage \
         INCOMPLETE, not silently assume the pushed value is a primitive",
    );
}

/// t1 wave 3, lane X (`docs/feature-designs/jit-ir-optimizations.md`):
/// compile `static int f(int n) { return f(n) + 1; }` (the `invokestatic` at
/// pc 1) with the given request and `direct_calls`.
pub(in crate::x64::tests) fn compile_static_self_call_site(
    backend: crate::x64::BackendRequest,
    direct_calls: Vec<(usize, crate::JitDirectCall)>,
) -> Option<CompiledMethod> {
    //  0: iload_0
    //  1: invokestatic #0
    //  4: iconst_1
    //  5: iadd
    //  6: ireturn
    let code = [0x1a, 0xb8, 0x00, 0x00, 0x04, 0x60, 0xac, 0x00];
    let helpers = test_helpers();
    compile_with_param_slots(
        &crate::compile_gate::CompileAdmission::for_backend_test(),
        &code,
        7,
        1,
        1,
        true,
        Vec::new(),         // multianewarray_info
        Vec::new(),         // field_info
        Vec::new(),         // typecheck_info
        Vec::new(),         // static_field_info
        Vec::new(),         // new_info
        Vec::new(),         // new_deferred_info
        Vec::new(),         // anewarray_info
        Vec::new(),         // anewarray_deferred_info
        Vec::new(),         // invoke_info: none at the site
        direct_calls,       // direct_calls
        Vec::new(),         // mic_slots
        Vec::new(),         // pic_slots
        Vec::new(),         // ldc_info
        Vec::new(),         // ldc_string_info
        Vec::new(),         // ldc_class_info
        Vec::new(),         // ldc2w_info
        Default::default(), // ldc_fp_pcs
        HashMap::new(),
        HashMap::new(),
        &helpers,
        std::collections::HashSet::new(),
        HashMap::new(),
        HashMap::new(), // inline_guard_variants (PGO-02)
        None,
        &[0],
        1,
        0,
        Vec::new(),
        "T.f:(I)I",
        None, // despec: no VM
        Vec::new(),
        None, // elidable_init_pcs
        backend,
    )
}

/// A door that marks its static self-calls makes the self-recursive arm
/// positive: the site with neither a row, nor an info, nor the marker is
/// REFUSED (it could be any callee a table builder forgot -- the SBR4
/// `binarySort` shape), the marked site compiles to the raw self-call, and a
/// request that does not vouch (the test wrapper, the unroll fixture) keeps
/// the old inference.
#[test]
pub(in crate::x64::tests) fn t1x_a_marking_door_refuses_an_unregistered_invokestatic() {
    let marking = || crate::x64::BackendRequest {
        static_self_calls_registered: true,
        ..crate::x64::BackendRequest::default()
    };
    assert!(
        compile_static_self_call_site(marking(), Vec::new()).is_none(),
        "no row, no info, no marker: refused, never a CALL of this body's own entry"
    );
    let marked = compile_static_self_call_site(
        marking(),
        vec![(1, crate::static_self_call_marker(1, b'I'))],
    );
    assert!(marked.is_some(), "the marked self-call lowers as before");
    assert!(
        compile_static_self_call_site(crate::x64::BackendRequest::default(), Vec::new()).is_some(),
        "an unvouched request keeps the inference"
    );
}

/// The marker row is the instance twin's guard word with no entry, and the
/// body-shape gates that ask "no direct-call rows" do not count it at an
/// `invokestatic` (they still count it at an instance site, and count every
/// routing row).
#[test]
pub(in crate::x64::tests) fn t1x_a_static_self_call_marker_is_no_routing_row() {
    let marker = crate::static_self_call_marker(2, b'J');
    assert_eq!(marker.guard_class_id, crate::JIT_SELF_CALL_GUARD);
    assert_eq!((marker.entry, marker.num_params, marker.return_type), (0, 2, b'J'));
    // 0: iload_0 ; 1: invokestatic ; 4: aload_0 ; 5: invokevirtual ; 8: ireturn
    let code = [0x1a, 0xb8, 0x00, 0x00, 0x2a, 0xb6, 0x00, 0x01, 0xac];
    let routing = crate::JitDirectCall {
        entry: 0x1000,
        needs_context: true,
        num_params: 1,
        return_type: b'I',
        guard_class_id: 0,
    };
    assert!(!super::driver::has_routing_direct_calls(&code, &[]));
    assert!(!super::driver::has_routing_direct_calls(&code, &[(1, marker.clone())]));
    assert!(super::driver::has_routing_direct_calls(&code, &[(5, marker.clone())]));
    assert!(super::driver::has_routing_direct_calls(
        &code,
        &[(1, marker), (1, routing)]
    ));
}

/// `CRATONVM_JIT_STATIC_SELF_CALL_MARKER` is a default-ON kill switch.
#[test]
pub(in crate::x64::tests) fn cooperative_poll_runs_in_a_pure_compiled_method() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    if !jit_safepoint_polls_enabled() {
        return;
    }
    static FLAG: u8 = 1;
    static HITS: AtomicUsize = AtomicUsize::new(0);
    // The real helper's shape; see `cooperative_poll_covers_a_conditional_only_backedge`.
    extern "C" fn slow_poll(_flag_addr: i64, _body_id: i64) -> i64 {
        HITS.fetch_add(1, Ordering::SeqCst);
        0
    }

    let mut helpers = test_helpers();
    helpers.safepoint_flag_addr = &FLAG as *const u8 as usize;
    helpers.safepoint_slow_path = slow_poll as *const () as usize;
    HITS.store(0, Ordering::SeqCst);

    // iconst_1; ireturn -- deliberately no heap/context dependency.
    let code = [0x04, 0xac];
    let compiled = compile(
        &code,
        code.len(),
        0,
        0,
        false,
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
        &helpers,
        std::collections::HashSet::new(),
        HashMap::new(),
        None,
    )
    .expect("pure method should compile with a cooperative poll");

    // SAFETY: the generated function has no arguments and returns int 1.
    assert_eq!(unsafe { compiled.try_call(&[]) }, Ok(1));
    assert_eq!(HITS.load(Ordering::SeqCst), 1);
}

#[test]
pub(in crate::x64::tests) fn cooperative_poll_covers_a_conditional_only_backedge() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    if !jit_safepoint_polls_enabled() {
        // `CRATONVM_JIT_SAFEPOINT_POLLS=0`: no poll is emitted, HITS stays 0.
        return;
    }
    static FLAG: u8 = 1;
    static HITS: AtomicUsize = AtomicUsize::new(0);
    // The real helper's shape (`HelperFnSafepointSlowPath`: `(i64, i64) ->
    // i64`), answering "keep running". This stub used to be `extern "C" fn()`,
    // which leaves in RAX whatever the caller had there. Since interpreter
    // round i1 waves 13/15 a METHOD-ENTRY body's conditional back-edge poll
    // (`emit_branch_back_edge_safepoint_poll`) reads that RAX as the verdict
    // and leaves on `SAFEPOINT_VERDICT_POLLING_BODY` (bit 1) through the
    // `ifne`'s own `OsrExit` map, and the reason-7 stub returns the deopt
    // sentinel `i64::MIN` — the value this test failed with (w36 page
    // `cooperative-poll-conditional-backedge-test-returns-i64-min`). Not a
    // codegen defect: the VM's `jit_safepoint_slow_path` always returns a
    // verdict.
    extern "C" fn slow_poll(_flag_addr: i64, _body_id: i64) -> i64 {
        HITS.fetch_add(1, Ordering::SeqCst);
        0
    }

    let mut helpers = test_helpers();
    helpers.safepoint_flag_addr = &FLAG as *const u8 as usize;
    helpers.safepoint_slow_path = slow_poll as *const () as usize;
    HITS.store(0, Ordering::SeqCst);

    // int i=3; do { --i; } while (i != 0); return i;
    // The loop has no goto: its only backedge is the conditional ifne.
    let code = [
        0x06, 0x3b, 0x84, 0x00, 0xff, 0x1a, 0x9a, 0xff, 0xfc, 0x1a, 0xac,
    ];
    let compiled = compile(
        &code,
        code.len(),
        0,
        1,
        false,
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
        &helpers,
        std::collections::HashSet::new(),
        HashMap::new(),
        None,
    )
    .expect("conditional loop should compile");

    // SAFETY: the generated function has no arguments and returns int 0.
    assert_eq!(unsafe { compiled.try_call(&[]) }, Ok(0));
    assert_eq!(
        HITS.load(Ordering::SeqCst),
        4,
        "one method-entry poll plus one poll at each of three loop tests"
    );
}

#[test]
pub(in crate::x64::tests) fn compiled_prologue_emits_stack_headroom_bang() {
    if !jit_stack_bang_enabled() {
        return;
    }
    let code: Vec<u8> = vec![0x1a, 0xac, 0, 0];
    let compiled = compile(
        &code,
        2,
        1,
        1,
        false,
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
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None,
    )
    .expect("simple method should compile");

    let mut expected = vec![0x8B, 0x84, 0x24];
    expected.extend_from_slice(&(-STACK_BANG_PAGE_SIZE).to_le_bytes());
    assert!(
        compiled
            .code_bytes()
            .windows(expected.len())
            .any(|w| w == expected.as_slice()),
        "compiled prologue should contain MOV EAX, [RSP-4096]"
    );
}

#[test]
pub(in crate::x64::tests) fn compiled_entry_accepts_stack_passed_java_arguments() {
    // `iload 4; ireturn`: on Windows the fifth no-context argument is
    // stack-passed, while the fourth argument of a context method is
    // stack-passed because the hidden context consumes RCX.
    let fifth_arg = [0x15, 0x04, 0xac];
    let no_context = compile(
        &fifth_arg,
        fifth_arg.len(),
        5,
        5,
        false,
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
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None,
    )
    .expect("five-argument method should compile");
    // SAFETY: `no_context` was compiled from the valid method above.
    assert_eq!(unsafe { no_context.try_call(&[1, 2, 3, 4, 55]) }, Ok(55));

    let fourth_arg = [0x1d, 0xac];
    let with_context = compile(
        &fourth_arg,
        fourth_arg.len(),
        4,
        4,
        true,
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
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None,
    )
    .expect("four-argument context method should compile");
    // SAFETY: `with_context` was compiled from the valid method above.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { with_context.try_call_with_context(0, &[1, 2, 3, 44]) },
        Ok(44)
    );
}

#[test]
pub(in crate::x64::tests) fn test_compile_simple_return() {
    // Method: int f(int x) { return x; }
    // Bytecode: iload_0, ireturn
    let code: Vec<u8> = vec![0x1a, 0xac, 0, 0]; // + 2 padding bytes
    let code_len = 2;

    assert!(is_jit_compatible(&code, code_len, "(I)I"));

    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        false,
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    );
    assert!(compiled.is_some());

    let method = compiled.unwrap();
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { method.try_call(&[42]).expect("test JIT call") };
    assert_eq!(result, 42);
}

#[test]
pub(in crate::x64::tests) fn test_compile_add() {
    // Method: int add(int a, int b) { return a + b; }
    // Bytecode: iload_0, iload_1, iadd, ireturn
    let code: Vec<u8> = vec![0x1a, 0x1b, 0x60, 0xac, 0, 0];
    let code_len = 4;

    assert!(is_jit_compatible(&code, code_len, "(II)I"));

    let compiled = compile(
        &code,
        code_len,
        2,
        2,
        false,
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[10, 32]).expect("test JIT call") };
    assert_eq!(result, 42);
}

#[test]
pub(in crate::x64::tests) fn test_compile_sub_mul() {
    // Method: int f(int a, int b) { return (a - b) * a; }
    // iload_0, iload_1, isub, iload_0, imul, ireturn
    let code: Vec<u8> = vec![0x1a, 0x1b, 0x64, 0x1a, 0x68, 0xac, 0, 0];
    let code_len = 6;

    let compiled = compile(
        &code,
        code_len,
        2,
        2,
        false,
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[10, 3]).expect("test JIT call") };
    assert_eq!(result, (10 - 3) * 10); // 70
}

#[test]
pub(in crate::x64::tests) fn test_compile_iconst() {
    // Method: int f() { return 5; }
    // iconst_5, ireturn
    let code: Vec<u8> = vec![0x08, 0xac, 0, 0];
    let code_len = 2;

    let compiled = compile(
        &code,
        code_len,
        0,
        0,
        false,
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[]).expect("test JIT call") };
    assert_eq!(result, 5);
}

#[test]
pub(in crate::x64::tests) fn test_compile_branch() {
    // Method: int f(int n) { if (n <= 1) return n; return n + 1; }
    // iload_0        // 0
    // iconst_1       // 1
    // if_icmpgt 5    // 2 (jump to offset 7)
    // iload_0        // 5
    // ireturn        // 6
    // iload_0        // 7
    // iconst_1       // 8
    // iadd           // 9
    // ireturn        // 10
    // if_icmpgt offset=5 → target = 2 + 5 = 7
    let code: Vec<u8> = vec![
        0x1a, // 0: iload_0
        0x04, // 1: iconst_1
        0xa3, 0x00, 0x05, // 2: if_icmpgt +5 → target=7
        0x1a, // 5: iload_0
        0xac, // 6: ireturn
        0x1a, // 7: iload_0
        0x04, // 8: iconst_1
        0x60, // 9: iadd
        0xac, // 10: ireturn
        0, 0, // padding
    ];
    let code_len = 11;

    assert!(is_jit_compatible(&code, code_len, "(I)I"));
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        false,
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();

    // n=0: 0 <= 1, return 0
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[0]).expect("test JIT call") },
        0
    );
    // n=1: 1 <= 1, return 1
    // SAFETY: `compiled` is executable JIT code from valid bytecode; the single
    // i64 argument matches the compiled method's one-parameter ABI.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[1]).expect("test JIT call") },
        1
    );
    // n=5: 5 > 1, return 5+1=6
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[5]).expect("test JIT call") },
        6
    );
}

/// The spill census must be WIRED, not merely defined.
///
/// Every column here was a `pub fn` away from being another counter nobody
/// calls — the exact defect the census exists to fix one level up. This drives
/// a real compile and asserts the columns move, so losing a `note_spill_*` call
/// site fails a test instead of silently reporting zeros forever.
///
/// It asserts DIRECTION, not magnitude: the numbers are global across every
/// compile the test binary has done, so an exact value would be an assertion
/// about test ordering.
#[test]
pub(in crate::x64::tests) fn the_spill_census_is_wired_to_the_cursor() {
    let before = crate::spill_cursor_counts();

    // Any body with an operand stack will do; `push_stack` is on the path of
    // essentially every opcode that produces a value.
    let code: Vec<u8> = vec![
        0x1a, // 0: iload_0
        0x1b, // 1: iload_1
        0x60, // 2: iadd
        0xac, // 3: ireturn
        0, 0,
    ];
    let compiled = compile_array_test(&code, 4, 2, 2);
    // SAFETY: JIT-compiled code from valid bytecode; mmap region executable.
    let got = unsafe { compiled.try_call(&[20, 22]).expect("test JIT call") };
    assert_eq!(got, 42, "the compiled body must still compute the sum");

    let after = crate::spill_cursor_counts();
    assert!(
        after[crate::SPILL_RES_TOTAL] > before[crate::SPILL_RES_TOTAL],
        "res-total did not move across a compile that pushes operands: the \
         census is defined but not wired ({} -> {})",
        before[crate::SPILL_RES_TOTAL],
        after[crate::SPILL_RES_TOTAL]
    );
    assert!(
        after[crate::SPILL_RES_PUSH] > before[crate::SPILL_RES_PUSH],
        "res-push did not move, so `push_stack` no longer reports"
    );
    assert!(
        after[crate::SPILL_RES_TOTAL] >= after[crate::SPILL_RES_PUSH],
        "res-total must bound the columns attributed out of it"
    );
    assert!(
        after[crate::SPILL_MIN_HEADROOM] < u64::MAX,
        "min-headroom is still its unset sentinel after a successful \
         reservation, so nothing is recording it"
    );
    // `res-total` is DERIVED from the reason columns, so the partition needs no
    // assertion -- it cannot be false. What can still break is a reservation
    // reaching the cursor through a column nobody reads, which is what the
    // `res-push` check above catches, and a stale name table, which this does.
    assert_eq!(
        crate::SPILL_RES_REASON_COLUMNS.len(),
        7,
        "a `SpillReason` variant was added or removed without updating the \
         columns that partition `res-total`"
    );
    for &c in crate::SPILL_RES_REASON_COLUMNS.iter() {
        assert!(
            c < crate::SPILL_CURSOR_SLOT_NAMES.len(),
            "reason column {c} has no name in SPILL_CURSOR_SLOT_NAMES"
        );
    }

    assert_eq!(
        after[crate::SPILL_FLUSH_CANONICAL],
        0,
        "flush-canonical is a retired column and must stay zero; if this fires, \
         someone revived the canonical-home flush without revisiting the \
         measurement that withdrew it (see `Compiler::flush_home`)"
    );
}

#[test]
pub(in crate::x64::tests) fn test_compile_fib() {
    // int fib(int n) {
    //     if (n <= 1) return n;
    //     return fib(n-1) + fib(n-2);
    // }
    //
    // Bytecode:
    //  0: iload_0
    //  1: iconst_1
    //  2: if_icmpgt +5  → target=7
    //  5: iload_0
    //  6: ireturn
    //  7: iload_0
    //  8: iconst_1
    //  9: isub
    // 10: invokestatic (self) cp=0
    // 13: iload_0
    // 14: iconst_2
    // 15: isub
    // 16: invokestatic (self) cp=0
    // 19: iadd
    // 20: ireturn
    let code: Vec<u8> = vec![
        0x1a, // 0: iload_0
        0x04, // 1: iconst_1
        0xa3, 0x00, 0x05, // 2: if_icmpgt +5 → target=7
        0x1a, // 5: iload_0
        0xac, // 6: ireturn
        0x1a, // 7: iload_0
        0x04, // 8: iconst_1
        0x64, // 9: isub
        0xb8, 0x00, 0x00, // 10: invokestatic (self-call)
        0x1a, // 13: iload_0
        0x05, // 14: iconst_2
        0x64, // 15: isub
        0xb8, 0x00, 0x00, // 16: invokestatic (self-call)
        0x60, // 19: iadd
        0xac, // 20: ireturn
        0, 0, // padding
    ];
    let code_len = 21;

    assert!(is_jit_compatible(&code, code_len, "(I)I"));
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        false,
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();

    // fib(0) = 0, fib(1) = 1, fib(10) = 55, fib(20) = 6765
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[0]).expect("test JIT call") },
        0
    );
    // SAFETY: `compiled` is executable JIT code from valid bytecode; each call below
    // passes a single i64 matching the compiled method's one-parameter ABI.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[1]).expect("test JIT call") },
        1
    );
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[10]).expect("test JIT call") },
        55
    );
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[20]).expect("test JIT call") },
        6765
    );
}

#[test]
pub(in crate::x64::tests) fn test_compile_long_add() {
    // long f(long a, long b) { return a + b; }
    // lload_0, lload_1, ladd, lreturn
    let code: Vec<u8> = vec![0x1e, 0x1f, 0x61, 0xad, 0, 0];
    let code_len = 4;

    assert!(is_jit_compatible(&code, code_len, "(JJ)J"));
    let compiled = compile(
        &code,
        code_len,
        2,
        2,
        false,
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            .try_call(&[100_000_000_000i64, 200_000_000_000i64])
            .expect("test JIT call")
    };
    assert_eq!(result, 300_000_000_000i64);
}

#[test]
pub(in crate::x64::tests) fn test_compile_iinc() {
    // int f(int x) { x += 10; return x; }
    // iload_0, iinc 0 10, iload_0, ireturn
    // Wait, iinc doesn't use the stack. Let's do:
    // iinc 0, 10
    // iload_0
    // ireturn
    let code: Vec<u8> = vec![
        0x84, 0x00, 0x0A, // iinc local=0, inc=10
        0x1a, // iload_0
        0xac, // ireturn
        0, 0, // padding
    ];
    let code_len = 5;

    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        false,
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[5]).expect("test JIT call") },
        15
    );
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[-3]).expect("test JIT call") },
        7
    );
}

#[test]
pub(in crate::x64::tests) fn test_compile_idiv_irem() {
    // int f(int a, int b) { return a / b + a % b; }
    // iload_0, iload_1, idiv, iload_0, iload_1, irem, iadd, ireturn
    let code: Vec<u8> = vec![
        0x1a, 0x1b, 0x6c, // iload_0, iload_1, idiv
        0x1a, 0x1b, 0x70, // iload_0, iload_1, irem
        0x60, // iadd
        0xac, // ireturn
        0, 0,
    ];
    let code_len = 8;

    let compiled = compile(
        &code,
        code_len,
        2,
        2,
        false,
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();
    // 17 / 5 = 3, 17 % 5 = 2, total = 5
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[17, 5]).expect("test JIT call") },
        5
    );
    // -7 / 2 = -3, -7 % 2 = -1, total = -4
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[-7, 2]).expect("test JIT call") },
        -4
    );
}

#[test]
pub(in crate::x64::tests) fn test_not_jit_compatible() {
    // invokeinterface (0xb9) is not supported
    let code: Vec<u8> = vec![0x2a, 0xb9, 0x00, 0x01, 0xac, 0, 0];
    assert!(!is_jit_compatible(&code, 5, "(Ljava/lang/Object;)I"));
}

#[test]
pub(in crate::x64::tests) fn test_jit_scan_field_ops() {
    // getfield (0xb4) is now JIT-compatible
    let code: Vec<u8> = vec![0x2a, 0xb4, 0x00, 0x01, 0xac, 0, 0];
    let result = jit_scan(&code, 5, "(Ljava/lang/Object;)I").unwrap();
    assert!(
        result.needs_heap,
        "default checked getfield helper needs the hidden VM pointer"
    );
    assert_eq!(result.field_ops.len(), 1);
    assert_eq!(result.field_ops[0], (1, 1)); // pc=1, cp_idx=1

    // putfield (0xb5) is JIT-compatible and sets needs_heap via resolver
    let pcode: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x1b, // 1: iload_1
        0xb5, 0x00, 0x02, // 2: putfield #2
        0xb1, // 5: return
        0, 0,
    ];
    let result = jit_scan(&pcode, 6, "(Ljava/lang/Object;I)V").unwrap();
    assert_eq!(result.field_ops.len(), 1);
    assert_eq!(result.field_ops[0], (2, 2)); // pc=2, cp_idx=2
}

#[test]
pub(in crate::x64::tests) fn test_jit_scan_array_needs_heap() {
    // newarray (0xbc) requires heap; method with just arithmetic doesn't
    let pure_code: Vec<u8> = vec![0x1a, 0x1b, 0x60, 0xac, 0, 0]; // iload_0, iload_1, iadd, ireturn
    let result = jit_scan(&pure_code, 4, "(II)I").unwrap();
    assert!(!result.needs_heap);
    assert!(!result.has_newarray);
    assert!(result.multianewarray_ops.is_empty());

    // Method with newarray needs heap
    let array_code: Vec<u8> = vec![
        0x1a, // 0: iload_0 (count)
        0xbc, 0x04, // 1: newarray T_BOOLEAN
        0x4c, // 3: astore_1
        0x2c, // 4: aload_2
        0xbe, // 5: arraylength
        0xac, // 6: ireturn
        0, 0,
    ];
    let result = jit_scan(&array_code, 7, "(I)I").unwrap();
    assert!(result.needs_heap);
    assert!(result.has_newarray);
}

#[test]
pub(in crate::x64::tests) fn test_jit_scan_areturn_and_multianewarray() {
    // areturn is accepted for object return types
    let areturn_code: Vec<u8> = vec![0x2a, 0xb0, 0, 0]; // aload_0, areturn
    let result = jit_scan(&areturn_code, 2, "([[I)[[I").unwrap();
    assert!(!result.needs_heap);

    // multianewarray 2D is accepted
    let mna_code: Vec<u8> = vec![
        0x1a, // 0: iload_0 (dim1)
        0x1b, // 1: iload_1 (dim2)
        0xc5, 0x00, 0x0d, 0x02, // 2: multianewarray #13, 2
        0xb0, // 6: areturn
        0, 0,
    ];
    let result = jit_scan(&mna_code, 7, "(II)[[I").unwrap();
    assert!(result.needs_heap);
    assert_eq!(result.multianewarray_ops.len(), 1);
    assert_eq!(result.multianewarray_ops[0], (2, 13, 2));

    // multianewarray 3D is accepted too. It was refused until 2026-09-22,
    // and a `jit_scan` refusal is PERMANENT and whole-method: one
    // `new byte[3][4][4]` made `RomulusEngine.skinny_128_384_plus_enc` (138x
    // HotSpot, 63% of `RomulusTest`'s samples) uncompilable at every door for
    // the life of the process. The emitter now pins the dimensions into frame
    // scratch words and passes their ADDRESS, so arity costs no argument
    // registers.
    let mna3d_code: Vec<u8> = vec![
        0x1a, 0x1b, 0x1c, 0xc5, 0x00, 0x0d, 0x03, // ndims=3
        0xb0, 0, 0,
    ];
    let _ = crate::take_jit_bail_site(); // clear any residue from the rows above
    let result = jit_scan(&mna3d_code, 8, "(III)[[[I").expect("3-D is compilable");
    assert_eq!(result.multianewarray_ops, vec![(3, 13, 3)]);
    assert_eq!(crate::take_jit_bail_site(), None);

    // `ndims == 0` is still refused: it is not a legal classfile, and an
    // emitter that popped nothing and pushed an array would leave the stack
    // model one entry high.
    let mna0d_code: Vec<u8> = vec![0xc5, 0x00, 0x0d, 0x00, 0xb0, 0, 0];
    assert!(jit_scan(&mna0d_code, 5, "()[[I").is_none());
    // ...and it NAMES the bytecode it refused at. A `jit_scan` `None` is
    // permanent — every door bail-lists the method on it — so a silent
    // refusal is a method that runs interpreted for the life of the process
    // with nothing in any log to say why. `jit_bridge`'s osr-DENY line used
    // to print `<unrecorded> @pc=0 op=0x00` for all 40 refusal arms.
    assert_eq!(
        crate::take_jit_bail_site(),
        Some(("jit_scan/unsupported-opcode", 0, 0xc5)),
        "a scan refusal must carry the pc and opcode that caused it"
    );

    // ...as is anything past the emitter's frame-scratch budget.
    let mna9d_code: Vec<u8> = vec![0xc5, 0x00, 0x0d, 0x09, 0xb0, 0, 0];
    assert!(
        (crate::x64::bytecode_compat::MAX_JIT_MULTIANEWARRAY_DIMS) < 9,
        "this row assumes 9 dimensions is over the budget"
    );
    assert!(jit_scan(&mna9d_code, 5, "()[[I").is_none());
    let _ = crate::take_jit_bail_site();

    // A scan that SUCCEEDS must leave no bail site behind for the next
    // compile to misattribute.
    assert!(jit_scan(&mna_code, 7, "(II)[[I").is_some());
    assert_eq!(crate::take_jit_bail_site(), None);
}

#[test]
pub(in crate::x64::tests) fn test_compile_fconst() {
    // float f() { return 1.0f; }
    // fconst_1 (0x0c), freturn (0xae)
    let code: Vec<u8> = vec![0x0c, 0xae, 0, 0];
    let code_len = 2;

    assert!(is_jit_compatible(&code, code_len, "()F"));
    let compiled = compile(
        &code,
        code_len,
        0,
        0,
        false,
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[]).expect("test JIT call") };
    // Result is f32 bit pattern as i64
    assert_eq!(f32::from_bits(result as u32), 1.0f32); // Cast: JIT ABI convention
}

#[test]
pub(in crate::x64::tests) fn test_compile_fconst_all() {
    // Test fconst_0 (0x0b), fconst_1 (0x0c), fconst_2 (0x0d)
    for (op, expected) in [(0x0bu8, 0.0f32), (0x0c, 1.0f32), (0x0d, 2.0f32)] {
        let code: Vec<u8> = vec![op, 0xae, 0, 0]; // fconst_N, freturn
        let compiled = compile(
            &code,
            2,
            0,
            0,
            false,
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
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
            None, // string_layout — not needed for this test
        )
        .unwrap();
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.try_call(&[]).expect("test JIT call") };
        assert_eq!(f32::from_bits(result as u32), expected); // Cast: JIT ABI convention
    }
}

#[test]
pub(in crate::x64::tests) fn test_compile_dconst() {
    // double f() { return 1.0; }
    // dconst_1 (0x0f), dreturn (0xaf)
    let code: Vec<u8> = vec![0x0f, 0xaf, 0, 0];
    let code_len = 2;

    assert!(is_jit_compatible(&code, code_len, "()D"));
    let compiled = compile(
        &code,
        code_len,
        0,
        0,
        false,
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[]).expect("test JIT call") };
    assert_eq!(f64::from_bits(result as u64), 1.0f64); // Cast: JIT ABI convention
}

#[test]
pub(in crate::x64::tests) fn test_compile_dconst_all() {
    // dconst_0 (0x0e), dconst_1 (0x0f)
    for (op, expected) in [(0x0eu8, 0.0f64), (0x0f, 1.0f64)] {
        let code: Vec<u8> = vec![op, 0xaf, 0, 0]; // dconst_N, dreturn
        let compiled = compile(
            &code,
            2,
            0,
            0,
            false,
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
            Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
            None, // string_layout — not needed for this test
        )
        .unwrap();
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let result = unsafe { compiled.try_call(&[]).expect("test JIT call") };
        assert_eq!(f64::from_bits(result as u64), expected); // Cast: JIT ABI convention
    }
}

#[test]
pub(in crate::x64::tests) fn test_compile_float_load_store() {
    // float f(float x) { float y = x; return y; }
    // fload_0 (0x22), fstore_1 (0x44), fload_1 (0x23), freturn (0xae)
    let code: Vec<u8> = vec![0x22, 0x44, 0x23, 0xae, 0, 0];
    let code_len = 4;

    assert!(is_jit_compatible(&code, code_len, "(F)F"));
    let compiled = compile(
        &code,
        code_len,
        1,
        2,
        false,
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();
    let input = 3.15f32.to_bits() as i64; // Cast: JIT ABI convention
                                          // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                          // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[input]).expect("test JIT call") };
    assert_eq!(f32::from_bits(result as u32), 3.15f32); // Cast: JIT ABI convention
}

#[test]
pub(in crate::x64::tests) fn test_compile_double_load_store() {
    // double f(double x) { double y = x; return y; }
    // dload_0 (0x26), dstore_1 (0x48), dload_1 (0x27), dreturn (0xaf)
    let code: Vec<u8> = vec![0x26, 0x48, 0x27, 0xaf, 0, 0];
    let code_len = 4;

    assert!(is_jit_compatible(&code, code_len, "(D)D"));
    let compiled = compile(
        &code,
        code_len,
        1,
        2,
        false,
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();
    let input = 2.719f64.to_bits() as i64; // Cast: JIT ABI convention
                                           // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                           // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[input]).expect("test JIT call") };
    assert_eq!(f64::from_bits(result as u64), 2.719f64); // Cast: JIT ABI convention
}

#[test]
pub(in crate::x64::tests) fn test_compile_float_load_indexed() {
    // float f(int dummy, float x) { return x; }
    // fload 1 (0x17, 0x01), freturn (0xae)
    let code: Vec<u8> = vec![0x17, 0x01, 0xae, 0, 0];
    let code_len = 3;

    assert!(is_jit_compatible(&code, code_len, "(IF)F"));
    let compiled = compile(
        &code,
        code_len,
        2,
        2,
        false,
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();
    let input = 42.5f32.to_bits() as i64; // Cast: JIT ABI convention
                                          // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                          // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[0, input]).expect("test JIT call") };
    assert_eq!(f32::from_bits(result as u32), 42.5f32); // Cast: JIT ABI convention
}

#[test]
pub(in crate::x64::tests) fn test_compile_i2b() {
    // int f(int x) { return (byte) x; }
    // iload_0, i2b (0x91), ireturn
    let code: Vec<u8> = vec![0x1a, 0x91, 0xac, 0, 0];
    let code_len = 3;

    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        false,
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();
    // Positive value within byte range
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[42]).expect("test JIT call") },
        42
    );
    // Truncation: 0x1FF → (byte) = -1
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[0x1FF]).expect("test JIT call") },
        -1
    );
    // Truncation: 300 → (byte) = 44
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[300]).expect("test JIT call") },
        44
    );
    // Negative: -128
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[-128]).expect("test JIT call") },
        -128
    );
}

#[test]
pub(in crate::x64::tests) fn test_compile_i2c() {
    // int f(int x) { return (char) x; }
    // iload_0, i2c (0x92), ireturn
    let code: Vec<u8> = vec![0x1a, 0x92, 0xac, 0, 0];
    let code_len = 3;

    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        false,
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();
    // Positive value within char range
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[65]).expect("test JIT call") },
        65
    ); // 'A'
       // 0xFFFF stays as 65535 (unsigned)
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[0xFFFF]).expect("test JIT call") },
        65535
    );
    // Truncation: 0x10041 → 0x0041 = 65
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[0x10041]).expect("test JIT call") },
        65
    );
    // Negative: -1 → 0xFFFF = 65535
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[-1]).expect("test JIT call") },
        65535
    );
}

#[test]
pub(in crate::x64::tests) fn test_compile_i2s() {
    // int f(int x) { return (short) x; }
    // iload_0, i2s (0x93), ireturn
    let code: Vec<u8> = vec![0x1a, 0x93, 0xac, 0, 0];
    let code_len = 3;

    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        false,
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();
    // Positive within short range
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[1000]).expect("test JIT call") },
        1000
    );
    // Truncation: 0x18000 → (short) = -32768
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[0x18000]).expect("test JIT call") },
        -32768
    );
    // 32767 stays
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[32767]).expect("test JIT call") },
        32767
    );
    // -32768 stays
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[-32768]).expect("test JIT call") },
        -32768
    );
}

#[test]
pub(in crate::x64::tests) fn test_compile_void_return() {
    // void f() { return; }
    // return (0xb1)
    let code: Vec<u8> = vec![0xb1, 0, 0];
    let code_len = 1;

    assert!(is_jit_compatible(&code, code_len, "()V"));
    let compiled = compile(
        &code,
        code_len,
        0,
        0,
        false,
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();
    // Void return — result is undefined, but should not crash
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let _ = unsafe { compiled.try_call(&[]).expect("test JIT call") };
}

#[test]
pub(in crate::x64::tests) fn test_compile_freturn() {
    // float f(float x) { return x; }
    // fload_0 (0x22), freturn (0xae)
    let code: Vec<u8> = vec![0x22, 0xae, 0, 0];
    let code_len = 2;

    assert!(is_jit_compatible(&code, code_len, "(F)F"));
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        false,
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();
    let input = (-3.5f32).to_bits() as i64; // Cast: JIT ABI convention
                                            // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                            // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[input]).expect("test JIT call") };
    assert_eq!(f32::from_bits(result as u32), -3.5f32); // Cast: JIT ABI convention
}

#[test]
pub(in crate::x64::tests) fn test_compile_dup2() {
    // int f(int a, int b) { [a,b] → dup2 → [a,b,a,b] → iadd → [a,b,a+b] → iadd → [a,b+a+b] → iadd → [2a+2b] }
    // iload_0 (0x1a), iload_1 (0x1b), dup2 (0x5c), iadd (0x60), iadd (0x60), iadd (0x60), ireturn (0xac)
    let code: Vec<u8> = vec![0x1a, 0x1b, 0x5c, 0x60, 0x60, 0x60, 0xac, 0, 0];
    let code_len = 7;
    let compiled = compile(
        &code,
        code_len,
        2,
        2,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout — not needed for this test
    )
    .unwrap();
    // f(3, 5) = 2*3 + 2*5 = 16
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[3, 5]).expect("test JIT call") };
    assert_eq!(result, 16);
    // f(10, 7) = 2*10 + 2*7 = 34
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[10, 7]).expect("test JIT call") };
    assert_eq!(result, 34);
}

#[test]
pub(in crate::x64::tests) fn test_compile_swap_arithmetic() {
    // int f(int a, int b) { return a - b; }, computed via swap:
    // [a, b] → swap → [b, a] → isub → [b - a] … but we want `a - b`, so:
    // [a, b] → swap → [b, a] → swap → [a, b] → isub → [a - b]. Two swaps
    // exercise the codegen twice and verify the operand identities survive
    // unchanged. f(10, 3) = 7; f(-4, 6) = -10.
    // iload_0 (0x1a), iload_1 (0x1b), swap (0x5f), swap (0x5f),
    // isub (0x64), ireturn (0xac)
    let code: Vec<u8> = vec![0x1a, 0x1b, 0x5f, 0x5f, 0x64, 0xac, 0, 0];
    let code_len = 6;
    let compiled = compile(
        &code,
        code_len,
        2,
        2,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .expect("swap must JIT-compile");
    // SAFETY: Calling JIT-compiled machine code in a test; produced from valid bytecode.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[10, 3]).expect("test JIT call") },
        7
    );
    // SAFETY: same as above.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[-4, 6]).expect("test JIT call") },
        -10
    );
}

/// A SINGLE swap of two frame-resident values must actually swap them.
/// The double-swap test above is identity-blind: the old (Frame, Frame)
/// arm exchanged the slot memory AND pushed the entries in (a, b) order,
/// which re-paired each entry with its original value — a net no-op that
/// two swaps cannot distinguish from correct code.
#[test]
pub(in crate::x64::tests) fn test_swap_frame_frame_single() {
    // iconst_5, iconst_2, swap, isub, ireturn
    // [5, 2] → swap → [2, 5]; isub = 2 - 5 = -3. (No-op swap gives 3.)
    let code: Vec<u8> = vec![0x08, 0x05, 0x5f, 0x64, 0xac, 0, 0];
    let code_len = 5;
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .expect("single swap must JIT-compile");
    // SAFETY: Calling JIT-compiled machine code in a test; produced from valid bytecode.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[0]).expect("test JIT call") },
        -3
    );
}

/// After a swap, the spill cursor must not hand a later push a slot the
/// swapped values still occupy (the pops rewound `next_spill_offset`
/// below the re-pushed entries; without the post-swap reset a following
/// iconst landed on the below-top value's frame slot).
#[test]
pub(in crate::x64::tests) fn test_swap_then_push_no_live_slot_reuse() {
    // iconst_5, iconst_2, swap, iconst_1, isub, isub, ireturn
    // [5,2] → swap → [2,5] → push 1 → [2,5,1] → isub → [2,4] → isub → -2.
    let code: Vec<u8> = vec![0x08, 0x05, 0x5f, 0x04, 0x64, 0x64, 0xac, 0, 0];
    let code_len = 7;
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .expect("swap+push must JIT-compile");
    // SAFETY: Calling JIT-compiled machine code in a test; produced from valid bytecode.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[0]).expect("test JIT call") },
        -2
    );
}

/// Mixed register/frame swap (register-allocated local below a
/// frame-resident constant) — exercises the pure entry-reorder arm.
#[test]
pub(in crate::x64::tests) fn test_swap_mixed_reg_frame() {
    // iload_0, iconst_3, swap, isub, ireturn
    // [k, 3] → swap → [3, k]; isub = 3 - k. f(10) = -7.
    let code: Vec<u8> = vec![0x1a, 0x06, 0x5f, 0x64, 0xac, 0, 0];
    let code_len = 5;
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .expect("mixed swap must JIT-compile");
    // SAFETY: Calling JIT-compiled machine code in a test; produced from valid bytecode.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[10]).expect("test JIT call") },
        -7
    );
    // SAFETY: same as above.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[-4]).expect("test JIT call") },
        7
    );
}

/// if_icmp with a register-resident slot BELOW two frame-resident
/// operands: canonicalizing the remaining stack must not clobber the
/// popped operands. Pre-fix, the handler popped first and the
/// canonicalization stored the register local to canonical slot 0 —
/// exactly val1's frame slot (register slots shift the offsets of frame
/// slots above them down by one position) — so the CMP compared the
/// register local's value instead of val1.
#[test]
pub(in crate::x64::tests) fn test_if_icmp_canonicalize_preserves_popped_operands() {
    // int f(int k) { return k + (k+5 < k+3 ? 9 : 7); }
    //  0: iload_0            [k]            (register-allocated → CalleeSaved)
    //  1: iload_0            [k, k]
    //  2: iconst_5
    //  3: iadd               [k, k+5]       (frame slot base+0)
    //  4: iload_0
    //  5: iconst_3
    //  6: iadd               [k, k+5, k+3]  (frame slot base+8)
    //  7: if_icmplt +8 → 15  [k]
    // 10: bipush 7
    // 12: goto +5 → 17
    // 15: bipush 9
    // 17: iadd               [k+7]   (k+5 < k+3 is always false)
    // 18: ireturn
    let code: Vec<u8> = vec![
        0x1a, 0x1a, 0x08, 0x60, 0x1a, 0x06, 0x60, 0xa1, 0x00, 0x08, 0x10, 0x07, 0xa7, 0x00, 0x05,
        0x10, 0x09, 0x60, 0xac, 0, 0,
    ];
    let code_len = 19;
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .expect("deep-stack if_icmp must JIT-compile");
    // k+5 < k+3 is false for every k → always the 7 arm. The clobbered
    // compare read k itself as val1: k < k+3 is true → 9 arm.
    // SAFETY: Calling JIT-compiled machine code in a test; produced from valid bytecode.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[10]).expect("test JIT call") },
        17
    );
    // SAFETY: same as above.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[-2]).expect("test JIT call") },
        5
    );
}

/// Same shape for the one-operand ifXX family: [CalleeSaved, Frame]
/// at an ifle. Pre-fix the canonicalization stored the register local
/// over the just-popped operand's frame slot before the TEST read it.
#[test]
pub(in crate::x64::tests) fn test_ifxx_canonicalize_preserves_popped_operand() {
    // int f(int k) { return k + (k+3 <= 0 ? 9 : 7); }
    //  0: iload_0           [k]
    //  1: iload_0           [k, k]
    //  2: iconst_3
    //  3: iadd              [k, k+3]   (frame slot base+0)
    //  4: ifle +8 → 12      [k]
    //  7: bipush 7
    //  9: goto +5 → 14
    // 12: bipush 9
    // 14: iadd
    // 15: ireturn
    let code: Vec<u8> = vec![
        0x1a, 0x1a, 0x06, 0x60, 0x9e, 0x00, 0x08, 0x10, 0x07, 0xa7, 0x00, 0x05, 0x10, 0x09, 0x60,
        0xac, 0, 0,
    ];
    let code_len = 16;
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .expect("deep-stack ifle must JIT-compile");
    // f(-1): k+3 = 2 > 0 → 7 arm → 6. The clobbered TEST read k = -1
    // (≤ 0) → 9 arm → 8.
    // SAFETY: Calling JIT-compiled machine code in a test; produced from valid bytecode.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[-1]).expect("test JIT call") },
        6
    );
    // SAFETY: same as above.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[5]).expect("test JIT call") },
        12
    );
}

#[test]
pub(in crate::x64::tests) fn test_compile_dup2_form2_long() {
    // long f(long a) { return a + a; }  — FORM-2 dup2: the top is a single
    // category-2 long (ONE slot in this single-slot model), so `dup2` must
    // duplicate ONE slot (`[…, a] → […, a, a]`), not two. Pre-fix the
    // codegen always did FORM-1 (indexing `stack[len-2]` with height 1 →
    // underflow/desync); the `dup2_category_safe` gate masked it by
    // de-JITing such methods (which regressed bintrees18). `dup2_top_cat2`
    // now classifies the `lload_0`-produced top as category-2 and emits the
    // single-slot duplication.
    // lload_0 (0x1e), dup2 (0x5c), ladd (0x61), lreturn (0xad)
    let code: Vec<u8> = vec![0x1e, 0x5c, 0x61, 0xad, 0, 0];
    let code_len = 4;
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .expect("FORM-2 dup2 method must JIT-compile (not bail/reject)");
    // f(5) = 10, f(-7) = -14
    // SAFETY: Calling JIT-compiled machine code in a test; produced from valid bytecode.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[5]).expect("test JIT call") },
        10
    );
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[-7]).expect("test JIT call") },
        -14
    );
}

#[test]
#[allow(deprecated)] // uses MATH_*_INTRINSIC aliases
pub(in crate::x64::tests) fn test_compile_math_sqrt_intrinsic() {
    // double f(double x) { return Math.sqrt(x); }
    // dload_0 (0x26), invokestatic (0xb8, 0x00, 0x01), dreturn (0xaf)
    let code: Vec<u8> = vec![0x26, 0xb8, 0x00, 0x01, 0xaf, 0, 0];
    let code_len = 5;
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        vec![(
            1,
            crate::JitDirectCall {
                entry: crate::MATH_SQRT_INTRINSIC,
                needs_context: false,
                num_params: 1,
                return_type: b'D',
                guard_class_id: 0,
            },
        )],
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();
    // sqrt(4.0) == 2.0
    let input = 4.0f64.to_bits() as i64; // Cast: JIT ABI convention
                                         // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                         // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[input]).expect("test JIT call") };
    assert_eq!(f64::from_bits(result as u64), 2.0f64); // Cast: JIT ABI convention
                                                       // sqrt(2.0) ≈ 1.4142135623730951
    let input2 = 2.0f64.to_bits() as i64; // Cast: JIT ABI convention
                                          // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                          // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result2 = unsafe { compiled.try_call(&[input2]).expect("test JIT call") };
    // Bits, not a tolerance. IEEE 754 requires `sqrt` to be CORRECTLY ROUNDED,
    // so there is exactly one right answer and `SQRT_2` is it. The `1e-14` this
    // used to allow is about 45 million ULP at 1.414 — wide enough to accept a
    // wrong instruction, a wrong operand width, or a lost rounding mode. The
    // `sqrt(4.0)` assertion six lines up was already exact, so this was the odd
    // one out rather than a considered choice.
    // W7-54-strictmath-fdlibm-family-RETIRED-20260917.md.
    assert_eq!(
        f64::from_bits(result2 as u64).to_bits(), // Cast: JIT ABI convention
        std::f64::consts::SQRT_2.to_bits(),
        "sqrtsd must be exactly rounded"
    );
}

#[test]
#[allow(deprecated)] // uses MATH_*_INTRINSIC aliases
pub(in crate::x64::tests) fn test_compile_math_min_max_int_intrinsic() {
    // int min_f(int a, int b) { return Math.min(a, b); }
    // int max_f(int a, int b) { return Math.max(a, b); }
    // Bytecode: iload_0 (0x1a), iload_1 (0x1b), invokestatic (0xb8, 0x00, 0x01),
    //           ireturn (0xac)
    // Round-9 CRIT regression test for the swapped CMOVL/CMOVG opcodes in
    // the MATH_MIN_INT_INTRINSIC / MATH_MAX_INT_INTRINSIC arms. Before the
    // fix, Math.min(3, 5) returned 5 and Math.max(3, 5) returned 3.
    let code: Vec<u8> = vec![0x1a, 0x1b, 0xb8, 0x00, 0x01, 0xac, 0, 0];
    let code_len = 6;

    // Math.min variant
    let compiled_min = compile(
        &code,
        code_len,
        2,
        2,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        vec![(
            2,
            crate::JitDirectCall {
                entry: crate::MATH_MIN_INT_INTRINSIC,
                needs_context: false,
                num_params: 2,
                return_type: b'I',
                guard_class_id: 0,
            },
        )],
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout — not needed for this test
    )
    .unwrap();
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let r1 = unsafe { compiled_min.try_call(&[3, 5]).expect("test JIT call") };
    assert_eq!(r1, 3, "Math.min(3, 5) must be 3 (was {r1})");
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let r2 = unsafe { compiled_min.try_call(&[5, 3]).expect("test JIT call") };
    assert_eq!(r2, 3, "Math.min(5, 3) must be 3 (was {r2})");
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let r3 = unsafe { compiled_min.try_call(&[-7, 4]).expect("test JIT call") };
    assert_eq!(r3, -7, "Math.min(-7, 4) must be -7 (was {r3})");

    // Math.max variant
    let compiled_max = compile(
        &code,
        code_len,
        2,
        2,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        vec![(
            2,
            crate::JitDirectCall {
                entry: crate::MATH_MAX_INT_INTRINSIC,
                needs_context: false,
                num_params: 2,
                return_type: b'I',
                guard_class_id: 0,
            },
        )],
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout — not needed for this test
    )
    .unwrap();
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let m1 = unsafe { compiled_max.try_call(&[3, 5]).expect("test JIT call") };
    assert_eq!(m1, 5, "Math.max(3, 5) must be 5 (was {m1})");
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let m2 = unsafe { compiled_max.try_call(&[5, 3]).expect("test JIT call") };
    assert_eq!(m2, 5, "Math.max(5, 3) must be 5 (was {m2})");
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let m3 = unsafe { compiled_max.try_call(&[-7, 4]).expect("test JIT call") };
    assert_eq!(m3, 4, "Math.max(-7, 4) must be 4 (was {m3})");
}

/// Round-11 HIGH-3 — `peephole-cmov`: verify the user-written
/// equivalent of `Math.min(a, b)` — written as `(a < b) ? a : b`
/// — gets lowered to a branchless CMOV by
/// `try_cmov_minmax_peephole`. The bytecode is what javac emits
/// for that source expression:
///
///     [0] 0x1A             iload_0           a
///     [1] 0x1B             iload_1           b
///     [2] 0xa2 0x00 0x07   if_icmpge → PC 9  (taken when a >= b)
///     [5] 0x1A             iload_0           "take a"
///     [6] 0xa7 0x00 0x04   goto    → PC 10
///     [9] 0x1B             iload_1           "take b"
///     [10] 0xac            ireturn
#[test]
pub(in crate::x64::tests) fn test_compile_user_written_min_idiom_cmov() {
    let code: Vec<u8> = vec![
        0x1A, // 0:  iload_0
        0x1B, // 1:  iload_1
        0xa2, 0x00, 0x07, // 2:  if_icmpge → PC 9
        0x1A, // 5:  iload_0   (fall-through: a < b → take a)
        0xa7, 0x00, 0x04, // 6:  goto → PC 10
        0x1B, // 9:  iload_1   (taken: a >= b → take b)
        0xac, // 10: ireturn
        0, 0, // padding
    ];
    let code_len = 11;

    let compiled = compile(
        &code,
        code_len,
        2,
        2,
        false,
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
        Vec::new(), // pic_slots
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout — not needed for this test
    )
    .unwrap();

    // Semantics: (a < b) ? a : b == min(a, b).
    // SAFETY: Calling JIT-compiled machine code in a test; the
    // CompiledMethod was produced by the JIT compiler from valid
    // bytecode and the mmap region is executable.
    let r1 = unsafe { compiled.try_call(&[3, 5]).expect("test JIT call") };
    assert_eq!(r1, 3, "user-min(3, 5) must be 3 (was {r1})");
    // SAFETY: same as above
    let r2 = unsafe { compiled.try_call(&[5, 3]).expect("test JIT call") };
    assert_eq!(r2, 3, "user-min(5, 3) must be 3 (was {r2})");
    // SAFETY: same as above
    let r3 = unsafe { compiled.try_call(&[-7, 4]).expect("test JIT call") };
    assert_eq!(r3, -7, "user-min(-7, 4) must be -7 (was {r3})");
    // Equal inputs: (a < b) is false → take b == a.
    // SAFETY: same as above
    let r4 = unsafe { compiled.try_call(&[42, 42]).expect("test JIT call") };
    assert_eq!(r4, 42, "user-min(42, 42) must be 42 (was {r4})");
}

#[test]
#[allow(deprecated)] // uses MATH_*_INTRINSIC aliases
pub(in crate::x64::tests) fn test_compile_math_min_max_long_intrinsic() {
    // long min_f(long a, long b) { return Math.min(a, b); }
    // long max_f(long a, long b) { return Math.max(a, b); }
    // Bytecode: lload_0 (0x1e), lload_1 (0x1f), invokestatic (0xb8, 0x00, 0x01),
    //           lreturn (0xad). This JIT models every parameter — long
    //           included — as a single 64-bit local slot (the prologue
    //           maps Java param `i` to local slot `i`), so the second
    //           arg lives at slot 1 (lload_1) and max_locals is 2. Using
    //           the JVM-classic `lload_2` here would read an
    //           uninitialized slot 2 and miscompare against garbage.
    // Round-9 CRIT regression test for the swapped CMOVL/CMOVG opcodes in
    // the MATH_MIN_LONG_INTRINSIC / MATH_MAX_LONG_INTRINSIC arms — same
    // bug as the int variants but on the 64-bit REX.W CMOV path.
    let code: Vec<u8> = vec![0x1e, 0x1f, 0xb8, 0x00, 0x01, 0xad, 0, 0];
    let code_len = 6;

    // Math.min(long, long) variant
    let compiled_min = compile(
        &code,
        code_len,
        2, // num_params: 2 long args, one local slot each
        2, // max_locals
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        vec![(
            2,
            crate::JitDirectCall {
                entry: crate::MATH_MIN_LONG_INTRINSIC,
                needs_context: false,
                num_params: 2,
                return_type: b'J',
                guard_class_id: 0,
            },
        )],
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout — not needed for this test
    )
    .unwrap();
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let r1 = unsafe { compiled_min.try_call(&[3i64, 5i64]).expect("test JIT call") };
    assert_eq!(r1, 3, "Math.min(3L, 5L) must be 3 (was {r1})");
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let r2 = unsafe { compiled_min.try_call(&[5i64, 3i64]).expect("test JIT call") };
    assert_eq!(r2, 3, "Math.min(5L, 3L) must be 3 (was {r2})");
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let r3 = unsafe {
        compiled_min
            .try_call(&[-7i64, 4i64])
            .expect("test JIT call")
    };
    assert_eq!(r3, -7, "Math.min(-7L, 4L) must be -7 (was {r3})");

    // Math.max(long, long) variant
    let compiled_max = compile(
        &code,
        code_len,
        2, // num_params: 2 long args, one local slot each
        2, // max_locals
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        vec![(
            2,
            crate::JitDirectCall {
                entry: crate::MATH_MAX_LONG_INTRINSIC,
                needs_context: false,
                num_params: 2,
                return_type: b'J',
                guard_class_id: 0,
            },
        )],
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout — not needed for this test
    )
    .unwrap();
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let m1 = unsafe { compiled_max.try_call(&[3i64, 5i64]).expect("test JIT call") };
    assert_eq!(m1, 5, "Math.max(3L, 5L) must be 5 (was {m1})");
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let m2 = unsafe { compiled_max.try_call(&[5i64, 3i64]).expect("test JIT call") };
    assert_eq!(m2, 5, "Math.max(5L, 3L) must be 5 (was {m2})");
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let m3 = unsafe {
        compiled_max
            .try_call(&[-7i64, 4i64])
            .expect("test JIT call")
    };
    assert_eq!(m3, 4, "Math.max(-7L, 4L) must be 4 (was {m3})");
}

/// `Math.min`/`Math.max` for float and double, at the corners SSE gets
/// wrong.
///
/// The point of this test is not that `min(3, 5) == 3` -- a bare `MINSS`
/// gets that right. It is the two cases `MINSS` gets WRONG, both of which
/// are silently invisible to `==`:
///
///   * `-0.0` vs `+0.0`. `MINSS` treats them as equal and returns its
///     second operand, so a naive lowering makes `Math.min` order-dependent
///     when Java says the answer is `-0.0` either way round. Asserted on
///     raw bits, since `-0.0 == 0.0` is true.
///   * NaN. `MINSS` returns the OTHER operand when either input is NaN;
///     Java returns the NaN one, payload included. Two distinct payloads
///     are used so "returned some NaN" cannot pass for "returned the right
///     argument".
#[test]
#[allow(deprecated)] // uses MATH_*_INTRINSIC aliases
pub(in crate::x64::tests) fn test_compile_math_min_max_float_double_intrinsic() {
    // float f(float a, float b) { return Math.min(a, b); }
    // fload_0 (0x22), fload_1 (0x23), invokestatic (0xb8 0x00 0x01), freturn (0xae)
    let code_f: Vec<u8> = vec![0x22, 0x23, 0xb8, 0x00, 0x01, 0xae, 0, 0];
    // double g(double a, double b) { return Math.min(a, b); }
    // dload_0 (0x26), dload_1 (0x27), invokestatic, dreturn (0xaf). This
    // JIT keeps every value in one 64-bit slot, so a double is dload_1 and
    // not dload_2 -- see test_compile_dadd for the same convention.
    let code_d: Vec<u8> = vec![0x26, 0x27, 0xb8, 0x00, 0x01, 0xaf, 0, 0];

    let build = |code: &[u8], entry: usize, ret: u8| {
        compile(
            code,
            6,
            2,
            2,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            vec![(
                2,
                crate::JitDirectCall {
                    entry,
                    needs_context: false,
                    num_params: 2,
                    return_type: ret,
                    guard_class_id: 0,
                },
            )],
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
            None, // string_layout
        )
        .unwrap()
    };

    let min_f = build(&code_f, crate::MATH_MIN_FLOAT_INTRINSIC, b'F');
    let max_f = build(&code_f, crate::MATH_MAX_FLOAT_INTRINSIC, b'F');
    let min_d = build(&code_d, crate::MATH_MIN_DOUBLE_INTRINSIC, b'D');
    let max_d = build(&code_d, crate::MATH_MAX_DOUBLE_INTRINSIC, b'D');

    // The JIT value ABI carries floats as raw bits in an integer slot.
    // Cast: f32 bits -> i64 slot (zero-extended, no truncation).
    let fbits = |v: f32| v.to_bits() as i64;
    let dbits = |v: f64| v.to_bits() as i64;
    let call_f = |m: &crate::CompiledMethod, a: f32, b: f32| -> u32 {
        // SAFETY: calling JIT-compiled machine code produced by `compile`
        // above from valid bytecode, in an executable mmap region.
        let r = unsafe { m.try_call(&[fbits(a), fbits(b)]).expect("test JIT call") };
        r as u32 // Cast: JIT ABI convention -- low 32 bits are the float
    };
    let call_d = |m: &crate::CompiledMethod, a: f64, b: f64| -> u64 {
        // SAFETY: as above.
        let r = unsafe { m.try_call(&[dbits(a), dbits(b)]).expect("test JIT call") };
        r as u64 // Cast: JIT ABI convention
    };

    // Two distinct NaN payloads.
    let nan_a = f32::from_bits(0x7FC0_0001);
    let nan_b = f32::from_bits(0x7FC0_0002);
    let nan_a_d = f64::from_bits(0x7FF8_0000_0000_0001);
    let nan_b_d = f64::from_bits(0x7FF8_0000_0000_0002);

    // --- ordinary ordering ---
    assert_eq!(call_f(&min_f, 3.0, 5.0), 3.0f32.to_bits());
    assert_eq!(call_f(&min_f, 5.0, 3.0), 3.0f32.to_bits());
    assert_eq!(call_f(&max_f, 3.0, 5.0), 5.0f32.to_bits());
    assert_eq!(call_f(&max_f, 5.0, 3.0), 5.0f32.to_bits());
    assert_eq!(call_f(&min_f, -7.0, 4.0), (-7.0f32).to_bits());
    assert_eq!(call_f(&max_f, -7.0, 4.0), 4.0f32.to_bits());

    // --- signed zero: -0.0 is strictly smaller than +0.0 ---
    assert_eq!(
        call_f(&min_f, 0.0, -0.0),
        (-0.0f32).to_bits(),
        "Math.min(+0.0f, -0.0f) must be -0.0f, not +0.0f (bare MINSS returns its second operand)"
    );
    assert_eq!(call_f(&min_f, -0.0, 0.0), (-0.0f32).to_bits());
    assert_eq!(call_f(&min_f, -0.0, -0.0), (-0.0f32).to_bits());
    assert_eq!(call_f(&min_f, 0.0, 0.0), 0.0f32.to_bits());
    assert_eq!(
        call_f(&max_f, 0.0, -0.0),
        0.0f32.to_bits(),
        "Math.max(+0.0f, -0.0f) must be +0.0f"
    );
    assert_eq!(call_f(&max_f, -0.0, 0.0), 0.0f32.to_bits());
    assert_eq!(call_f(&max_f, -0.0, -0.0), (-0.0f32).to_bits());
    // A zero against a non-zero must not take the zero path.
    assert_eq!(call_f(&min_f, -0.0, 1.0), (-0.0f32).to_bits());
    assert_eq!(call_f(&min_f, 1.0, -0.0), (-0.0f32).to_bits());

    // --- NaN: the result is the NaN ARGUMENT, payload and all ---
    assert_eq!(
        call_f(&min_f, nan_a, 5.0),
        nan_a.to_bits(),
        "Math.min(NaN, x) must return the NaN argument with its payload"
    );
    assert_eq!(call_f(&min_f, 5.0, nan_b), nan_b.to_bits());
    assert_eq!(call_f(&max_f, nan_a, 5.0), nan_a.to_bits());
    assert_eq!(call_f(&max_f, 5.0, nan_b), nan_b.to_bits());
    // Both NaN: the JDK body tests `a != a` first, so `a` wins.
    assert_eq!(call_f(&min_f, nan_a, nan_b), nan_a.to_bits());
    assert_eq!(call_f(&max_f, nan_a, nan_b), nan_a.to_bits());

    // --- infinities ---
    assert_eq!(
        call_f(&min_f, f32::NEG_INFINITY, 0.0),
        f32::NEG_INFINITY.to_bits()
    );
    assert_eq!(call_f(&max_f, f32::INFINITY, 0.0), f32::INFINITY.to_bits());
    assert_eq!(call_f(&min_f, f32::INFINITY, nan_a), nan_a.to_bits());

    // --- the same grid for double ---
    assert_eq!(call_d(&min_d, 3.0, 5.0), 3.0f64.to_bits());
    assert_eq!(call_d(&max_d, 3.0, 5.0), 5.0f64.to_bits());
    assert_eq!(call_d(&min_d, -7.0, 4.0), (-7.0f64).to_bits());
    assert_eq!(call_d(&min_d, 0.0, -0.0), (-0.0f64).to_bits());
    assert_eq!(call_d(&min_d, -0.0, 0.0), (-0.0f64).to_bits());
    assert_eq!(call_d(&max_d, 0.0, -0.0), 0.0f64.to_bits());
    assert_eq!(call_d(&max_d, -0.0, 0.0), 0.0f64.to_bits());
    assert_eq!(call_d(&max_d, -0.0, -0.0), (-0.0f64).to_bits());
    assert_eq!(call_d(&min_d, nan_a_d, 5.0), nan_a_d.to_bits());
    assert_eq!(call_d(&min_d, 5.0, nan_b_d), nan_b_d.to_bits());
    assert_eq!(call_d(&max_d, nan_a_d, 5.0), nan_a_d.to_bits());
    assert_eq!(call_d(&max_d, 5.0, nan_b_d), nan_b_d.to_bits());
    assert_eq!(
        call_d(&min_d, f64::NEG_INFINITY, 0.0),
        f64::NEG_INFINITY.to_bits()
    );
}

#[test]
pub(in crate::x64::tests) fn test_compile_dreturn() {
    // double f(double x) { return x; }
    // dload_0 (0x26), dreturn (0xaf)
    let code: Vec<u8> = vec![0x26, 0xaf, 0, 0];
    let code_len = 2;

    assert!(is_jit_compatible(&code, code_len, "(D)D"));
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        false,
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();
    let input = std::f64::consts::PI.to_bits() as i64; // Cast: JIT ABI convention
                                                       // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                                       // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[input]).expect("test JIT call") };
    assert_eq!(f64::from_bits(result as u64), std::f64::consts::PI); // Cast: JIT ABI convention
}

#[test]
pub(in crate::x64::tests) fn test_jit_scan_float_double_compatible() {
    // Verify float/double opcodes pass the scan
    // fconst_1, fstore_0, fload_0, freturn
    let fcode: Vec<u8> = vec![0x0c, 0x43, 0x22, 0xae, 0, 0];
    assert!(jit_scan(&fcode, 4, "()F").is_some());

    // dconst_1, dstore_0, dload_0, dreturn
    let dcode: Vec<u8> = vec![0x0f, 0x47, 0x26, 0xaf, 0, 0];
    assert!(jit_scan(&dcode, 4, "()D").is_some());

    // i2b, i2c, i2s are compatible
    let cast_code: Vec<u8> = vec![0x1a, 0x91, 0x92, 0x93, 0xac, 0, 0];
    assert!(jit_scan(&cast_code, 5, "(I)I").is_some());

    // void return is compatible
    let void_code: Vec<u8> = vec![0xb1, 0, 0];
    assert!(jit_scan(&void_code, 1, "()V").is_some());

    // SSE float arithmetic is compatible
    let fadd_code: Vec<u8> = vec![0x22, 0x23, 0x62, 0xae, 0, 0]; // fload_0, fload_1, fadd, freturn
    assert!(jit_scan(&fadd_code, 4, "(FF)F").is_some());

    // SSE double arithmetic is compatible
    let dadd_code: Vec<u8> = vec![0x26, 0x27, 0x63, 0xaf, 0, 0]; // dload_0, dload_1, dadd, dreturn
    assert!(jit_scan(&dadd_code, 4, "(DD)D").is_some());

    // All conversions are compatible
    let conv_code: Vec<u8> = vec![0x1a, 0x86, 0x8d, 0x8e, 0xac, 0, 0]; // iload_0, i2f, f2d, d2i, ireturn
    assert!(jit_scan(&conv_code, 5, "(I)I").is_some());

    // fcmpl/fcmpg/dcmpl/dcmpg are compatible
    let fcmp_code: Vec<u8> = vec![0x22, 0x23, 0x95, 0xac, 0, 0]; // fload_0, fload_1, fcmpl, ireturn
    assert!(jit_scan(&fcmp_code, 4, "(FF)I").is_some());

    // fneg/dneg are compatible
    let fneg_code: Vec<u8> = vec![0x22, 0x76, 0xae, 0, 0]; // fload_0, fneg, freturn
    assert!(jit_scan(&fneg_code, 3, "(F)F").is_some());
}

#[test]
pub(in crate::x64::tests) fn test_compile_fadd() {
    // float f(float a, float b) { return a + b; }
    // fload_0 (0x22), fload_1 (0x23), fadd (0x62), freturn (0xae)
    let code: Vec<u8> = vec![0x22, 0x23, 0x62, 0xae, 0, 0];
    let code_len = 4;

    let compiled = compile(
        &code,
        code_len,
        2,
        2,
        false,
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();
    let a = 3.5f32.to_bits() as i64; // Cast: JIT ABI convention
    let b = 2.25f32.to_bits() as i64; // Cast: JIT ABI convention
                                      // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                      // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[a, b]).expect("test JIT call") };
    assert_eq!(f32::from_bits(result as u32), 5.75f32); // Cast: JIT ABI convention
}

#[test]
pub(in crate::x64::tests) fn test_compile_fsub_fmul_fdiv() {
    // float fsub(float a, float b) { return a - b; }
    let sub_code: Vec<u8> = vec![0x22, 0x23, 0x66, 0xae, 0, 0]; // fsub
    let compiled = compile(
        &sub_code,
        4,
        2,
        2,
        false,
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();
    let a = 10.0f32.to_bits() as i64; // Cast: JIT ABI convention
    let b = 3.0f32.to_bits() as i64; // Cast: JIT ABI convention
                                     // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                     // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[a, b]).expect("test JIT call") };
    assert_eq!(f32::from_bits(result as u32), 7.0f32); // Cast: JIT ABI convention

    // float fmul(float a, float b) { return a * b; }
    let mul_code: Vec<u8> = vec![0x22, 0x23, 0x6a, 0xae, 0, 0]; // fmul
    let compiled = compile(
        &mul_code,
        4,
        2,
        2,
        false,
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[a, b]).expect("test JIT call") };
    assert_eq!(f32::from_bits(result as u32), 30.0f32); // Cast: JIT ABI convention

    // float fdiv(float a, float b) { return a / b; }
    let div_code: Vec<u8> = vec![0x22, 0x23, 0x6e, 0xae, 0, 0]; // fdiv
    let compiled = compile(
        &div_code,
        4,
        2,
        2,
        false,
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();
    let a = 15.0f32.to_bits() as i64; // Cast: JIT ABI convention
    let b = 4.0f32.to_bits() as i64; // Cast: JIT ABI convention
                                     // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                     // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[a, b]).expect("test JIT call") };
    assert_eq!(f32::from_bits(result as u32), 3.75f32); // Cast: JIT ABI convention
}

#[test]
pub(in crate::x64::tests) fn test_compile_dadd() {
    // double f(double a, double b) { return a + b; }
    // dload_0 (0x26), dload_1 (0x27), dadd (0x63), dreturn (0xaf)
    let code: Vec<u8> = vec![0x26, 0x27, 0x63, 0xaf, 0, 0];
    let code_len = 4;

    let compiled = compile(
        &code,
        code_len,
        2,
        2,
        false,
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();
    let a = 1.5f64.to_bits() as i64; // Cast: JIT ABI convention
    let b = 2.5f64.to_bits() as i64; // Cast: JIT ABI convention
                                     // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                     // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[a, b]).expect("test JIT call") };
    assert_eq!(f64::from_bits(result as u64), 4.0f64); // Cast: JIT ABI convention
}

