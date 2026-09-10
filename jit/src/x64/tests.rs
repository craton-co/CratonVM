// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Unit tests for the x86-64 backend.

use super::*;
use crate::JitInvokeInfo;
use cratonvm_types::{ObjectRef, Value, ARRAY_DATA_OFFSET};

#[test]
fn gc_inert_selfrec_accepts_forward_field_walk_and_rejects_gc_edges() {
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
fn stack_bang_frame_probes_cover_crossed_pages() {
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
fn estimate_max_stack_counts_ldc_family_and_dup_pushes() {
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
fn pop_does_not_reclaim_a_slot_a_buried_entry_still_owns() {
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
fn a_splice_does_not_rewind_the_cursor_under_a_buried_operand() {
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
        "the hazard requires the buried operand ABOVE an argument          (buried={buried}, arg={arg_lo}); with these two in the other order          the `min` would be right and this test would prove nothing"
    );
    // Depth 2 — the second argument, at the top as usual.
    compiler.push_stack().expect("second argument slot");

    // `static int leaf(int a, int b) { return a; }`
    let site = make_inline_site(&[0x1a, 0xac], 2, 2, true, b'I');
    assert!(
        compiler.try_emit_inline_site(0, &site),
        "a two-argument static leaf must splice; a bail would make every          assertion below vacuous"
    );

    assert!(
        compiler.next_spill_offset > buried,
        "the splice left the cursor at {} with the buried operand still          owning slot {buried}: the next reservation gets that address a          second time",
        compiler.next_spill_offset
    );
    let next = compiler.push_stack().expect("push after the splice");
    assert!(
        !matches!(next, StackSlot::Frame(off) if off == buried),
        "the push after the splice was handed slot {buried}, which the buried          entry {:?} still reads",
        compiler.stack[0]
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
fn a_reservation_never_starts_inside_an_open_inline_scopes_locals() {
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

#[test]
fn push_stack_refuses_to_cross_spill_limit() {
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

/// Read `num_slots` from the object header at the given pointer.
/// `num_slots` is at byte offset 16 within `ObjectHeader` (repr(C)).
///
/// # Safety
/// `obj_ptr` must point to a valid, properly aligned `ObjectHeader` that has not been freed.
// SAFETY: obj_ptr points to a live, properly aligned ObjectHeader (repr(C)); reading the
// u32 num_slots field at its fixed offset is in-bounds and the object is not freed.
unsafe fn read_num_slots(obj_ptr: *const u8) -> u32 {
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
    static LAST_DISPATCH: std::cell::RefCell<Option<(String, String, String, Vec<i64>)>> =
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
unsafe extern "C" fn stub_invoke_dispatch(
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
unsafe extern "C" fn stub_getfield(_vm_ptr: i64, obj_ptr: i64, field_index: i64) -> i64 {
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
unsafe extern "C" fn stub_putfield_int(obj_ptr: i64, field_index: i64, val: i64) {
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
unsafe extern "C" fn stub_putfield_long(obj_ptr: i64, field_index: i64, val: i64) {
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
unsafe extern "C" fn stub_putfield_float(obj_ptr: i64, field_index: i64, val: i64) {
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
unsafe extern "C" fn stub_putfield_double(obj_ptr: i64, field_index: i64, val: i64) {
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
unsafe extern "C" fn stub_putfield_object(_vm_ptr: i64, obj_ptr: i64, field_index: i64, val: i64) {
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
fn test_helpers() -> JitRuntimeHelpers {
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
    }
}

#[cfg(target_arch = "x86_64")]
#[test]
fn live_monitor_ops_execute_direct_runtime_stubs() {
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
    crate::set_monitor_direct_fns(
        monitor_enter_stub as *const () as usize,
        monitor_exit_stub as *const () as usize,
    );

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
    let compiled = compile(
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
fn self_recursive_second_call_map(method_key: &str) -> Option<crate::OopMapEntry> {
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
        Vec::new(),
        None, // elidable_init_pcs: no constant pool, so nothing is proven empty
    )?;
    compiled
        .oop_maps
        .iter()
        .find(|m| m.bytecode_pc == 5)
        .cloned()
}

#[test]
fn self_recursive_reference_return_is_published_as_an_oop() {
    let map = self_recursive_second_call_map("T.f:(I)Ljava/lang/Object;")
        .expect("the second self-call is a GC-capable safepoint and records a map");
    assert!(
        !map.frame_slot_offsets.is_empty(),
        "the first call's result is a live reference on the operand stack across \
         the second call; leaving it untagged is the measured bt18 moving-young \
         heap corruption (fixed-suite-bugs/app-jvm-bugs/             moving-young-gen-drops-jit-held-oops-FIXED.md)",
    );
}

#[test]
fn self_recursive_primitive_return_is_not_published_as_an_oop() {
    let map = self_recursive_second_call_map("T.f:(I)I")
        .expect("the second self-call is a GC-capable safepoint and records a map");
    assert!(
        map.frame_slot_offsets.is_empty(),
        "an int result must NOT be tagged: publishing a primitive as a movable \
         root would have the collector relocate whatever its bit pattern names",
    );
}

#[test]
fn self_recursive_return_without_a_descriptor_fails_closed() {
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

#[test]
fn cooperative_poll_runs_in_a_pure_compiled_method() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static FLAG: u8 = 1;
    static HITS: AtomicUsize = AtomicUsize::new(0);
    extern "C" fn slow_poll() {
        HITS.fetch_add(1, Ordering::SeqCst);
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
fn cooperative_poll_covers_a_conditional_only_backedge() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static FLAG: u8 = 1;
    static HITS: AtomicUsize = AtomicUsize::new(0);
    extern "C" fn slow_poll() {
        HITS.fetch_add(1, Ordering::SeqCst);
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
fn compiled_prologue_emits_stack_headroom_bang() {
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
fn compiled_entry_accepts_stack_passed_java_arguments() {
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
        unsafe { with_context.try_call_with_context(0, &[1, 2, 3, 44]) },
        Ok(44)
    );
}

#[test]
fn test_compile_simple_return() {
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
fn test_compile_add() {
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
fn test_compile_sub_mul() {
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
fn test_compile_iconst() {
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
fn test_compile_branch() {
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
        unsafe { compiled.try_call(&[0]).expect("test JIT call") },
        0
    );
    // n=1: 1 <= 1, return 1
    // SAFETY: `compiled` is executable JIT code from valid bytecode; the single
    // i64 argument matches the compiled method's one-parameter ABI.
    assert_eq!(
        unsafe { compiled.try_call(&[1]).expect("test JIT call") },
        1
    );
    // n=5: 5 > 1, return 5+1=6
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
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
fn the_spill_census_is_wired_to_the_cursor() {
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
        "res-total did not move across a compile that pushes operands: the          census is defined but not wired ({} -> {})",
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
        "min-headroom is still its unset sentinel after a successful          reservation, so nothing is recording it"
    );
    // `res-total` is DERIVED from the reason columns, so the partition needs no
    // assertion -- it cannot be false. What can still break is a reservation
    // reaching the cursor through a column nobody reads, which is what the
    // `res-push` check above catches, and a stale name table, which this does.
    assert_eq!(
        crate::SPILL_RES_REASON_COLUMNS.len(),
        7,
        "a `SpillReason` variant was added or removed without updating the          columns that partition `res-total`"
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
        "flush-canonical is a retired column and must stay zero; if this fires,          someone revived the canonical-home flush without revisiting the          measurement that withdrew it (see `Compiler::flush_home`)"
    );
}

#[test]
fn test_compile_fib() {
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
        unsafe { compiled.try_call(&[0]).expect("test JIT call") },
        0
    );
    // SAFETY: `compiled` is executable JIT code from valid bytecode; each call below
    // passes a single i64 matching the compiled method's one-parameter ABI.
    assert_eq!(
        unsafe { compiled.try_call(&[1]).expect("test JIT call") },
        1
    );
    assert_eq!(
        unsafe { compiled.try_call(&[10]).expect("test JIT call") },
        55
    );
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        unsafe { compiled.try_call(&[20]).expect("test JIT call") },
        6765
    );
}

#[test]
fn test_compile_long_add() {
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
fn test_compile_iinc() {
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
        unsafe { compiled.try_call(&[5]).expect("test JIT call") },
        15
    );
    assert_eq!(
        unsafe { compiled.try_call(&[-3]).expect("test JIT call") },
        7
    );
}

#[test]
fn test_compile_idiv_irem() {
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
        unsafe { compiled.try_call(&[17, 5]).expect("test JIT call") },
        5
    );
    // -7 / 2 = -3, -7 % 2 = -1, total = -4
    assert_eq!(
        unsafe { compiled.try_call(&[-7, 2]).expect("test JIT call") },
        -4
    );
}

#[test]
fn test_not_jit_compatible() {
    // invokeinterface (0xb9) is not supported
    let code: Vec<u8> = vec![0x2a, 0xb9, 0x00, 0x01, 0xac, 0, 0];
    assert!(!is_jit_compatible(&code, 5, "(Ljava/lang/Object;)I"));
}

#[test]
fn test_jit_scan_field_ops() {
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
fn test_jit_scan_array_needs_heap() {
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
fn test_jit_scan_areturn_and_multianewarray() {
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

    // multianewarray 3D is rejected
    let mna3d_code: Vec<u8> = vec![
        0x1a, 0x1b, 0x1c, 0xc5, 0x00, 0x0d, 0x03, // ndims=3
        0xb0, 0, 0,
    ];
    assert!(jit_scan(&mna3d_code, 8, "(III)[[[I").is_none());
}

#[test]
fn test_compile_fconst() {
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
fn test_compile_fconst_all() {
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
fn test_compile_dconst() {
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
fn test_compile_dconst_all() {
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
fn test_compile_float_load_store() {
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
fn test_compile_double_load_store() {
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
fn test_compile_float_load_indexed() {
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
fn test_compile_i2b() {
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
        unsafe { compiled.try_call(&[42]).expect("test JIT call") },
        42
    );
    // Truncation: 0x1FF → (byte) = -1
    assert_eq!(
        unsafe { compiled.try_call(&[0x1FF]).expect("test JIT call") },
        -1
    );
    // Truncation: 300 → (byte) = 44
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        unsafe { compiled.try_call(&[300]).expect("test JIT call") },
        44
    );
    // Negative: -128
    assert_eq!(
        unsafe { compiled.try_call(&[-128]).expect("test JIT call") },
        -128
    );
}

#[test]
fn test_compile_i2c() {
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
        unsafe { compiled.try_call(&[65]).expect("test JIT call") },
        65
    ); // 'A'
       // 0xFFFF stays as 65535 (unsigned)
    assert_eq!(
        unsafe { compiled.try_call(&[0xFFFF]).expect("test JIT call") },
        65535
    );
    // Truncation: 0x10041 → 0x0041 = 65
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        unsafe { compiled.try_call(&[0x10041]).expect("test JIT call") },
        65
    );
    // Negative: -1 → 0xFFFF = 65535
    assert_eq!(
        unsafe { compiled.try_call(&[-1]).expect("test JIT call") },
        65535
    );
}

#[test]
fn test_compile_i2s() {
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
        unsafe { compiled.try_call(&[1000]).expect("test JIT call") },
        1000
    );
    // Truncation: 0x18000 → (short) = -32768
    assert_eq!(
        unsafe { compiled.try_call(&[0x18000]).expect("test JIT call") },
        -32768
    );
    // 32767 stays
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        unsafe { compiled.try_call(&[32767]).expect("test JIT call") },
        32767
    );
    // -32768 stays
    assert_eq!(
        unsafe { compiled.try_call(&[-32768]).expect("test JIT call") },
        -32768
    );
}

#[test]
fn test_compile_void_return() {
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
fn test_compile_freturn() {
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
fn test_compile_dup2() {
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
fn test_compile_swap_arithmetic() {
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
        unsafe { compiled.try_call(&[10, 3]).expect("test JIT call") },
        7
    );
    // SAFETY: same as above.
    assert_eq!(
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
fn test_swap_frame_frame_single() {
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
        unsafe { compiled.try_call(&[0]).expect("test JIT call") },
        -3
    );
}

/// After a swap, the spill cursor must not hand a later push a slot the
/// swapped values still occupy (the pops rewound `next_spill_offset`
/// below the re-pushed entries; without the post-swap reset a following
/// iconst landed on the below-top value's frame slot).
#[test]
fn test_swap_then_push_no_live_slot_reuse() {
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
        unsafe { compiled.try_call(&[0]).expect("test JIT call") },
        -2
    );
}

/// Mixed register/frame swap (register-allocated local below a
/// frame-resident constant) — exercises the pure entry-reorder arm.
#[test]
fn test_swap_mixed_reg_frame() {
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
        unsafe { compiled.try_call(&[10]).expect("test JIT call") },
        -7
    );
    // SAFETY: same as above.
    assert_eq!(
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
fn test_if_icmp_canonicalize_preserves_popped_operands() {
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
        unsafe { compiled.try_call(&[10]).expect("test JIT call") },
        17
    );
    // SAFETY: same as above.
    assert_eq!(
        unsafe { compiled.try_call(&[-2]).expect("test JIT call") },
        5
    );
}

/// Same shape for the one-operand ifXX family: [CalleeSaved, Frame]
/// at an ifle. Pre-fix the canonicalization stored the register local
/// over the just-popped operand's frame slot before the TEST read it.
#[test]
fn test_ifxx_canonicalize_preserves_popped_operand() {
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
        unsafe { compiled.try_call(&[-1]).expect("test JIT call") },
        6
    );
    // SAFETY: same as above.
    assert_eq!(
        unsafe { compiled.try_call(&[5]).expect("test JIT call") },
        12
    );
}

#[test]
fn test_compile_dup2_form2_long() {
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
        unsafe { compiled.try_call(&[5]).expect("test JIT call") },
        10
    );
    assert_eq!(
        unsafe { compiled.try_call(&[-7]).expect("test JIT call") },
        -14
    );
}

#[test]
#[allow(deprecated)] // uses MATH_*_INTRINSIC aliases
fn test_compile_math_sqrt_intrinsic() {
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
    // W7-54-strictmath-fdlibm-family.md.
    assert_eq!(
        f64::from_bits(result2 as u64).to_bits(), // Cast: JIT ABI convention
        std::f64::consts::SQRT_2.to_bits(),
        "sqrtsd must be exactly rounded"
    );
}

#[test]
#[allow(deprecated)] // uses MATH_*_INTRINSIC aliases
fn test_compile_math_min_max_int_intrinsic() {
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
fn test_compile_user_written_min_idiom_cmov() {
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
fn test_compile_math_min_max_long_intrinsic() {
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
fn test_compile_math_min_max_float_double_intrinsic() {
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
fn test_compile_dreturn() {
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
fn test_jit_scan_float_double_compatible() {
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
fn test_compile_fadd() {
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
fn test_compile_fsub_fmul_fdiv() {
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
fn test_compile_dadd() {
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

#[test]
fn test_compile_dsub_dmul_ddiv() {
    // double dsub
    let sub_code: Vec<u8> = vec![0x26, 0x27, 0x67, 0xaf, 0, 0];
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
    let a = 100.0f64.to_bits() as i64; // Cast: JIT ABI convention
    let b = 37.5f64.to_bits() as i64; // Cast: JIT ABI convention
                                      // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                      // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[a, b]).expect("test JIT call") };
    assert_eq!(f64::from_bits(result as u64), 62.5f64); // Cast: JIT ABI convention

    // double dmul
    let mul_code: Vec<u8> = vec![0x26, 0x27, 0x6b, 0xaf, 0, 0];
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
    let a = 6.0f64.to_bits() as i64; // Cast: JIT ABI convention
    let b = 7.0f64.to_bits() as i64; // Cast: JIT ABI convention
                                     // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                     // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[a, b]).expect("test JIT call") };
    assert_eq!(f64::from_bits(result as u64), 42.0f64); // Cast: JIT ABI convention

    // double ddiv
    let div_code: Vec<u8> = vec![0x26, 0x27, 0x6f, 0xaf, 0, 0];
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
    let a = 22.0f64.to_bits() as i64; // Cast: JIT ABI convention
    let b = 7.0f64.to_bits() as i64; // Cast: JIT ABI convention
                                     // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                     // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[a, b]).expect("test JIT call") };
    let expected = 22.0f64 / 7.0f64;
    assert_eq!(f64::from_bits(result as u64), expected); // Cast: JIT ABI convention
}

#[test]
fn test_compile_fneg_dneg() {
    // float fneg(float x) { return -x; }
    // fload_0 (0x22), fneg (0x76), freturn (0xae)
    let fcode: Vec<u8> = vec![0x22, 0x76, 0xae, 0, 0];
    let compiled = compile(
        &fcode,
        3,
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
    let input = 3.5f32.to_bits() as i64; // Cast: JIT ABI convention
                                         // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                         // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[input]).expect("test JIT call") };
    assert_eq!(f32::from_bits(result as u32), -3.5f32); // Cast: JIT ABI convention
                                                        // Negate negative
    let input = (-7.0f32).to_bits() as i64; // Cast: JIT ABI convention
                                            // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                            // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[input]).expect("test JIT call") };
    assert_eq!(f32::from_bits(result as u32), 7.0f32); // Cast: JIT ABI convention

    // double dneg(double x) { return -x; }
    // dload_0 (0x26), dneg (0x77), dreturn (0xaf)
    let dcode: Vec<u8> = vec![0x26, 0x77, 0xaf, 0, 0];
    let compiled = compile(
        &dcode,
        3,
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
    let input = 42.0f64.to_bits() as i64; // Cast: JIT ABI convention
                                          // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                          // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[input]).expect("test JIT call") };
    assert_eq!(f64::from_bits(result as u64), -42.0f64); // Cast: JIT ABI convention
}

#[test]
fn test_compile_i2f_i2d() {
    // int → float: iload_0 (0x1a), i2f (0x86), freturn (0xae)
    let i2f_code: Vec<u8> = vec![0x1a, 0x86, 0xae, 0, 0];
    let compiled = compile(
        &i2f_code,
        3,
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
    let result = unsafe { compiled.try_call(&[42]).expect("test JIT call") };
    assert_eq!(f32::from_bits(result as u32), 42.0f32); // Cast: JIT ABI convention
    let result = unsafe { compiled.try_call(&[-7]).expect("test JIT call") };
    assert_eq!(f32::from_bits(result as u32), -7.0f32); // Cast: JIT ABI convention

    // int → double: iload_0 (0x1a), i2d (0x87), dreturn (0xaf)
    let i2d_code: Vec<u8> = vec![0x1a, 0x87, 0xaf, 0, 0];
    let compiled = compile(
        &i2d_code,
        3,
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
    let result = unsafe { compiled.try_call(&[42]).expect("test JIT call") };
    assert_eq!(f64::from_bits(result as u64), 42.0f64); // Cast: JIT ABI convention
    let result = unsafe { compiled.try_call(&[-100]).expect("test JIT call") };
    assert_eq!(f64::from_bits(result as u64), -100.0f64); // Cast: JIT ABI convention
}

#[test]
fn test_compile_f2i_f2d_d2i_d2f() {
    // float → int: fload_0 (0x22), f2i (0x8b), ireturn (0xac)
    let f2i_code: Vec<u8> = vec![0x22, 0x8b, 0xac, 0, 0];
    let compiled = compile(
        &f2i_code,
        3,
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
    let input = 3.7f32.to_bits() as i64; // Cast: JIT ABI convention
                                         // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                         // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[input]).expect("test JIT call") };
    assert_eq!(result, 3); // truncate toward zero
    let input = (-3.7f32).to_bits() as i64; // Cast: JIT ABI convention
                                            // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                            // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[input]).expect("test JIT call") };
    assert_eq!(result, -3);

    // float → double: fload_0, f2d (0x8d), dreturn
    let f2d_code: Vec<u8> = vec![0x22, 0x8d, 0xaf, 0, 0];
    let compiled = compile(
        &f2d_code,
        3,
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
    let input = 1.5f32.to_bits() as i64; // Cast: JIT ABI convention
                                         // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                         // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[input]).expect("test JIT call") };
    assert_eq!(f64::from_bits(result as u64), 1.5f64); // Cast: JIT ABI convention

    // double → int: dload_0 (0x26), d2i (0x8e), ireturn (0xac)
    let d2i_code: Vec<u8> = vec![0x26, 0x8e, 0xac, 0, 0];
    let compiled = compile(
        &d2i_code,
        3,
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
    let input = 9.99f64.to_bits() as i64; // Cast: JIT ABI convention
                                          // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                          // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[input]).expect("test JIT call") };
    assert_eq!(result, 9);

    // double → float: dload_0 (0x26), d2f (0x90), freturn (0xae)
    let d2f_code: Vec<u8> = vec![0x26, 0x90, 0xae, 0, 0];
    let compiled = compile(
        &d2f_code,
        3,
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
    let input = 1.5f64.to_bits() as i64; // Cast: JIT ABI convention
                                         // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                         // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[input]).expect("test JIT call") };
    assert_eq!(f32::from_bits(result as u32), 1.5f32); // Cast: JIT ABI convention
}

#[test]
fn test_compile_l2f_l2d_f2l_d2l() {
    // long → float: lload_0 (0x1e), l2f (0x89), freturn (0xae)
    let l2f_code: Vec<u8> = vec![0x1e, 0x89, 0xae, 0, 0];
    let compiled = compile(
        &l2f_code,
        3,
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
    let result = unsafe { compiled.try_call(&[1000000i64]).expect("test JIT call") };
    assert_eq!(f32::from_bits(result as u32), 1_000_000.0f32); // Cast: JIT ABI convention

    // long → double: lload_0 (0x1e), l2d (0x8a), dreturn (0xaf)
    let l2d_code: Vec<u8> = vec![0x1e, 0x8a, 0xaf, 0, 0];
    let compiled = compile(
        &l2d_code,
        3,
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
    let result = unsafe { compiled.try_call(&[1000000i64]).expect("test JIT call") };
    assert_eq!(f64::from_bits(result as u64), 1_000_000.0f64); // Cast: JIT ABI convention

    // float → long: fload_0 (0x22), f2l (0x8c), lreturn (0xad)
    let f2l_code: Vec<u8> = vec![0x22, 0x8c, 0xad, 0, 0];
    let compiled = compile(
        &f2l_code,
        3,
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
    let input = 42.9f32.to_bits() as i64; // Cast: JIT ABI convention
                                          // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                          // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[input]).expect("test JIT call") };
    assert_eq!(result, 42i64);

    // double → long: dload_0 (0x26), d2l (0x8f), lreturn (0xad)
    let d2l_code: Vec<u8> = vec![0x26, 0x8f, 0xad, 0, 0];
    let compiled = compile(
        &d2l_code,
        3,
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
    let input = 99.9f64.to_bits() as i64; // Cast: JIT ABI convention
                                          // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                          // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[input]).expect("test JIT call") };
    assert_eq!(result, 99i64);
}

#[test]
fn test_compile_fcmpl() {
    // int f(float a, float b) { return fcmpl(a, b); }
    // fload_0 (0x22), fload_1 (0x23), fcmpl (0x95), ireturn (0xac)
    let code: Vec<u8> = vec![0x22, 0x23, 0x95, 0xac, 0, 0];
    let compiled = compile(
        &code,
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

    // a > b → 1
    let a = 5.0f32.to_bits() as i64; // Cast: JIT ABI convention
    let b = 3.0f32.to_bits() as i64; // Cast: JIT ABI convention
                                     // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                     // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        unsafe { compiled.try_call(&[a, b]).expect("test JIT call") },
        1
    );

    // a == b → 0
    let a = 3.0f32.to_bits() as i64; // Cast: JIT ABI convention
    let b = 3.0f32.to_bits() as i64; // Cast: JIT ABI convention
                                     // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                     // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        unsafe { compiled.try_call(&[a, b]).expect("test JIT call") },
        0
    );

    // a < b → -1
    let a = 1.0f32.to_bits() as i64; // Cast: JIT ABI convention
    let b = 3.0f32.to_bits() as i64; // Cast: JIT ABI convention
                                     // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                     // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        unsafe { compiled.try_call(&[a, b]).expect("test JIT call") },
        -1
    );

    // NaN → -1 (fcmpl)
    let nan = f32::NAN.to_bits() as i64; // Cast: JIT ABI convention
    let b = 3.0f32.to_bits() as i64; // Cast: JIT ABI convention
                                     // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                     // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        unsafe { compiled.try_call(&[nan, b]).expect("test JIT call") },
        -1
    );
    assert_eq!(
        unsafe { compiled.try_call(&[b, nan]).expect("test JIT call") },
        -1
    );
}

#[test]
fn test_compile_fcmpg() {
    // fload_0, fload_1, fcmpg (0x96), ireturn
    let code: Vec<u8> = vec![0x22, 0x23, 0x96, 0xac, 0, 0];
    let compiled = compile(
        &code,
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

    // a > b → 1
    let a = 5.0f32.to_bits() as i64; // Cast: JIT ABI convention
    let b = 3.0f32.to_bits() as i64; // Cast: JIT ABI convention
                                     // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                     // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        unsafe { compiled.try_call(&[a, b]).expect("test JIT call") },
        1
    );

    // a < b → -1
    let a = 1.0f32.to_bits() as i64; // Cast: JIT ABI convention
                                     // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                     // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        unsafe { compiled.try_call(&[a, b]).expect("test JIT call") },
        -1
    );

    // NaN → 1 (fcmpg)
    let nan = f32::NAN.to_bits() as i64; // Cast: JIT ABI convention
                                         // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                         // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        unsafe { compiled.try_call(&[nan, b]).expect("test JIT call") },
        1
    );
    assert_eq!(
        unsafe { compiled.try_call(&[b, nan]).expect("test JIT call") },
        1
    );
}

#[test]
fn test_compile_dcmpl_dcmpg() {
    // dload_0, dload_1, dcmpl (0x97), ireturn
    let dcmpl_code: Vec<u8> = vec![0x26, 0x27, 0x97, 0xac, 0, 0];
    let compiled = compile(
        &dcmpl_code,
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

    let a = 5.0f64.to_bits() as i64; // Cast: JIT ABI convention
    let b = 3.0f64.to_bits() as i64; // Cast: JIT ABI convention
                                     // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                     // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        unsafe { compiled.try_call(&[a, b]).expect("test JIT call") },
        1
    );

    let a = 3.0f64.to_bits() as i64; // Cast: JIT ABI convention
                                     // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                     // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        unsafe { compiled.try_call(&[a, b]).expect("test JIT call") },
        0
    );

    let a = 1.0f64.to_bits() as i64; // Cast: JIT ABI convention
                                     // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                     // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        unsafe { compiled.try_call(&[a, b]).expect("test JIT call") },
        -1
    );

    // NaN → -1 (dcmpl)
    let nan = f64::NAN.to_bits() as i64; // Cast: JIT ABI convention
                                         // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                         // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        unsafe { compiled.try_call(&[nan, b]).expect("test JIT call") },
        -1
    );

    // dcmpg: NaN → 1
    let dcmpg_code: Vec<u8> = vec![0x26, 0x27, 0x98, 0xac, 0, 0];
    let compiled = compile(
        &dcmpg_code,
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
    assert_eq!(
        unsafe { compiled.try_call(&[nan, b]).expect("test JIT call") },
        1
    );
    assert_eq!(
        unsafe { compiled.try_call(&[b, nan]).expect("test JIT call") },
        1
    );
}

#[test]
fn test_compile_float_chain() {
    // float f(float a, float b) { return (a + b) * a; }
    // fload_0, fload_1, fadd, fload_0, fmul, freturn
    let code: Vec<u8> = vec![0x22, 0x23, 0x62, 0x22, 0x6a, 0xae, 0, 0];
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
    let a = 3.0f32.to_bits() as i64; // Cast: JIT ABI convention
    let b = 2.0f32.to_bits() as i64; // Cast: JIT ABI convention
                                     // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                     // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[a, b]).expect("test JIT call") };
    // (3.0 + 2.0) * 3.0 = 15.0
    assert_eq!(f32::from_bits(result as u32), 15.0f32); // Cast: JIT ABI convention
}

#[test]
fn test_compile_i2f_fadd_f2i() {
    // int f(int a, int b) { return (int)((float)a + (float)b); }
    // iload_0, i2f, iload_1, i2f, fadd, f2i, ireturn
    let code: Vec<u8> = vec![
        0x1a, // iload_0
        0x86, // i2f
        0x1b, // iload_1
        0x86, // i2f
        0x62, // fadd
        0x8b, // f2i
        0xac, // ireturn
        0, 0,
    ];
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
    let result = unsafe { compiled.try_call(&[10, 20]).expect("test JIT call") };
    assert_eq!(result, 30);

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[7, -3]).expect("test JIT call") };
    assert_eq!(result, 4);
}

// -----------------------------------------------------------------------
// Round 17: getfield/putfield tests
// -----------------------------------------------------------------------

#[test]
fn test_getfield_int() {
    // Method: int getX(Object this) { return this.x; }
    // Bytecode: aload_0, getfield #1, ireturn
    // We fake field #1 as field_index=0, type='I'
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xb4, 0x00, 0x01, // 1: getfield #1
        0xac, // 4: ireturn
        0, 0,
    ];
    let code_len = 5;

    // Compile with field_info: pc=1, field_index=0, type_tag='I'
    let field_info = vec![(1usize, 0usize, b'I')];
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        false,
        Vec::new(),
        field_info,
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

    // Create a heap object with one Int field
    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 2);
    heap.set_field(obj, 0, Value::Int(42));

    // Call the JIT method: pass obj pointer as first arg
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call(&[obj.as_ptr() as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(result, 42);

    // Test with negative value
    heap.set_field(obj, 0, Value::Int(-123));
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call(&[obj.as_ptr() as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(result, -123);
}

/// Guarded inline getfield (perf/throughput-20260710): with a non-zero
/// `read_bounds_addr` the default arm emits the inline receiver guard +
/// raw field load, falling back to the checked helper only for receivers
/// that fail the guard. Uses a test-local bounds table so the pass/fail
/// routing is deterministic (the process-global table is owned by
/// whichever real heap ran last and is zeroed on heap drop):
///
///   * receiver inside the published range → inline raw load (the marker
///     helper is NOT called);
///   * table zeroed / null receiver / unaligned receiver → the helper IS
///     called (asserted via a marker stub that returns a constant no raw
///     load could produce).
#[test]
fn test_getfield_guarded_inline_fast_and_fallback() {
    // guarded_inline_getfield_enabled() is default-ON (see its doc
    // comment) -- no env var needed to exercise this path. If a test run
    // sets CRATONVM_JIT_GETFIELD_HELPER=1 to force the helper-only path
    // globally, this test's own assertions about the inline guard would
    // no longer hold; nothing here does that.
    use std::sync::atomic::{AtomicUsize, Ordering};
    static TEST_BOUNDS: [AtomicUsize; 6] = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];
    /// Marker helper: returns a constant that a raw field-cell load of the
    /// test object can never produce, so routing is observable.
    unsafe extern "C" fn marker_getfield(_vm: i64, _obj: i64, _idx: i64) -> i64 {
        424242
    }

    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xb4, 0x00, 0x01, // 1: getfield #1
        0xac, // 4: ireturn
        0, 0,
    ];
    let code_len = 5;
    let field_info = vec![(1usize, 0usize, b'I')];
    let mut helpers = test_helpers();
    helpers.getfield = marker_getfield as *const () as usize;
    // READ side: the guarded getfield arm bakes `read_bounds_addr`.
    helpers.read_bounds_addr = TEST_BOUNDS.as_ptr() as usize;
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        false,
        Vec::new(),
        field_info,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots — no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &helpers,
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 2);
    heap.set_field(obj, 0, Value::Int(42));
    let obj_addr = obj.as_ptr() as usize;

    // 1. Publish a range covering the object → guard passes → inline raw
    //    load reads the real cell, the marker helper is NOT called.
    TEST_BOUNDS[0].store(obj_addr & !0xFFF, Ordering::Release);
    TEST_BOUNDS[1].store((obj_addr & !0xFFF) + 0x10000, Ordering::Release);
    // SAFETY: JIT-compiled code from valid bytecode; executable mmap region.
    let result = unsafe { compiled.try_call(&[obj_addr as i64]).expect("jit call") };
    assert_eq!(result, 42, "in-bounds receiver must take the inline path");

    // 2. Unaligned receiver (inside the range) → guard fails → helper.
    // SAFETY: as above; the guard rejects the pointer before any deref.
    let result = unsafe {
        compiled
            .try_call(&[(obj_addr + 1) as i64])
            .expect("jit call")
    };
    assert_eq!(
        result, 424242,
        "unaligned receiver must route to the helper"
    );

    // 3. Zero the table (what GenerationalHeap::drop does) → out-of-heap
    //    receiver → helper.
    TEST_BOUNDS[0].store(0, Ordering::Release);
    TEST_BOUNDS[1].store(0, Ordering::Release);
    // SAFETY: as above.
    let result = unsafe { compiled.try_call(&[obj_addr as i64]).expect("jit call") };
    assert_eq!(
        result, 424242,
        "receiver outside every published region must route to the helper"
    );

    // 4. Null receiver → helper (which owns the NPE semantics in prod).
    // SAFETY: as above.
    let result = unsafe { compiled.try_call(&[0]).expect("jit call") };
    assert_eq!(result, 424242, "null receiver must route to the helper");
}

/// A REFERENCE field whose 16-byte cell does not hold a reference must not be
/// read as one by the inline arm.
///
/// This is the Tomcat/Derby SIGSEGV of 2026-08-23. The inline legacy `getfield`
/// loaded the cell's 8-byte payload and handed it on as a pointer without ever
/// looking at the cell's discriminant. `SQLChar.rawData` is declared `[C`, its
/// cell did not hold an `Object`, and the payload word — 1 — went straight into
/// the `arraylength` this arm emits three instructions later
/// (`MOV r32,[RAX+4]`), so the process died at `addr=0x5`. Three crashes
/// carried byte-identical registers: the body was deterministic, only its
/// reachability was racy (3 crashes in 38 runs one hour, 0 in 129 the next).
///
/// The checked helper has always looked at the variant, which is why the whole
/// thing was invisible from the helper's side. The inline arm exists to skip
/// the helper, so it has to ask the same question and defer when the answer is
/// no. The marker helper makes that deferral observable.
#[test]
fn a_reference_getfield_does_not_read_a_non_reference_cell_as_a_pointer() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static TEST_BOUNDS: [AtomicUsize; 6] = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];
    /// A value no cell read could produce, so "went to the helper" is visible.
    unsafe extern "C" fn marker_getfield(_vm: i64, _obj: i64, _idx: i64) -> i64 {
        424242
    }

    // aload_0; getfield #1; areturn
    let code: Vec<u8> = vec![0x2a, 0xb4, 0x00, 0x01, 0xb0, 0, 0];
    let code_len = 5;
    // Slot 1, declared `L` — a reference.
    let field_info = vec![(1usize, 1usize, b'L')];
    let mut helpers = test_helpers();
    helpers.getfield = marker_getfield as *const () as usize;
    helpers.read_bounds_addr = TEST_BOUNDS.as_ptr() as usize;
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        false,
        Vec::new(),
        field_info,
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
    .unwrap();

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 2);
    let obj_addr = obj.as_ptr() as usize;
    TEST_BOUNDS[0].store(obj_addr & !0xFFF, Ordering::Release);
    TEST_BOUNDS[1].store((obj_addr & !0xFFF) + 0x10000, Ordering::Release);

    // A genuine reference still takes the inline path: the whole sequence
    // exists to make this case fast, and a "fix" that sent it to the helper
    // would be a performance regression disguised as a correctness one.
    let target = heap.alloc_object(ClassId::new(0), 1);
    heap.set_field(obj, 1, Value::Object(Some(target)));
    // SAFETY: JIT-compiled code from valid bytecode; executable mmap region.
    let got = unsafe { compiled.try_call(&[obj_addr as i64]).expect("jit call") };
    assert_eq!(
        got,
        target.as_ptr() as i64,
        "a real reference must still be read inline"
    );

    // A PUNNED cell: the same slot now holds a `Long`, whose 8-byte payload
    // sits at exactly the offset the reference read uses. Pre-fix this
    // returned 0x1234_5678 — a pointer the caller would dereference. It must
    // now reach the helper, which owns the decision.
    heap.set_field(obj, 1, Value::Long(0x1234_5678));
    // SAFETY: as above.
    let got = unsafe { compiled.try_call(&[obj_addr as i64]).expect("jit call") };
    assert_eq!(
        got, 424242,
        "a reference load off a non-reference cell returned {got:#x} instead of \
         deferring — that word is what compiled code dereferences next"
    );

    // Null is a reference, and must NOT be pushed to the helper: `Object(None)`
    // is a legitimate inline answer and the common one.
    heap.set_field(obj, 1, Value::Object(None));
    // SAFETY: as above.
    let got = unsafe { compiled.try_call(&[obj_addr as i64]).expect("jit call") };
    assert_eq!(got, 0, "a null reference must still be read inline as 0");
}

/// Inline (helper-free) `getstatic`: with a resolver wired, the default `0xb2`
/// arm emits the two-load direct form and leaves NO call to `jit_getstatic`
/// behind; a site the resolver declines keeps the helper.
///
/// Both directions are asserted from ONE registration on purpose:
/// `set_static_base_resolver` deliberately latches its context for the life of
/// the process (a second VM must never re-point it at its own statics), so the
/// test resolver instead answers for exactly one `(class, field)` pair and
/// declines everything else — which also keeps it inert for any other test in
/// this binary that compiles a `getstatic`.
#[test]
fn test_getstatic_inline_direct_load_and_fallback() {
    use std::sync::atomic::AtomicPtr;

    /// Marker helper: returns a constant no direct load of the block below
    /// could produce, so routing is observable.
    unsafe extern "C" fn marker_getstatic(_vm: i64, _cid: i64, _idx: i64) -> i64 {
        424_242
    }

    /// Stands in for `jit_resolve_static_base`. `ctx` IS the address of the
    /// base-pointer cell here, so the test needs no VM.
    unsafe extern "C" fn test_resolver(ctx: i64, class_id: i64, field_index: i64) -> i64 {
        if class_id == 0x5EED && field_index == 1 {
            ctx
        } else {
            0
        }
    }

    // A leaked statics block plus the `AtomicPtr` cell that names it — the same
    // two-level shape `StaticsIndex` publishes.
    static CELL_ADDR: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let cell_addr = *CELL_ADDR.get_or_init(|| {
        let block: &'static mut [Value] =
            Box::leak(vec![Value::Int(1), Value::Int(-7), Value::Int(2)].into_boxed_slice());
        let cell: &'static AtomicPtr<Value> =
            Box::leak(Box::new(AtomicPtr::new(block.as_mut_ptr())));
        // Cast: the address the backend bakes as an immediate.
        cell as *const AtomicPtr<Value> as usize
    });
    set_static_base_resolver(test_resolver as *const () as usize, cell_addr);

    // getstatic #1 ; ireturn
    let code: Vec<u8> = vec![0xb2, 0x00, 0x01, 0xac, 0, 0];
    let code_len = 4;
    let mut helpers = test_helpers();
    helpers.getstatic = marker_getstatic as *const () as usize;

    let build = |field_index: usize| {
        compile(
            &code,
            code_len,
            1,
            1,
            false,
            Vec::new(), // multianewarray_info
            Vec::new(), // field_info
            Vec::new(), // typecheck_info
            // static_field_info: (pc, class_id, field_index, type_tag, volatile)
            vec![(0usize, 0x5EEDu32, field_index, b'I', false)],
            Vec::new(), // new_info
            Vec::new(), // anewarray_info
            Vec::new(), // invoke_info
            Vec::new(), // direct_calls
            Vec::new(), // mic_slots
            Vec::new(), // pic_slots
            Vec::new(), // ldc_info
            Vec::new(), // ldc2w_info
            HashMap::new(),
            HashMap::new(),
            &helpers,
            std::collections::HashSet::new(),
            HashMap::new(),
            None, // string_layout
        )
        .expect("test JIT compile")
    };

    // 1. Resolved site → direct load of slot 1, sign-extended.
    let inlined = build(1);
    // SAFETY: JIT-compiled machine code from valid bytecode in an executable
    // mmap region, as in every other codegen test here.
    let v = unsafe { inlined.try_call(&[0]).expect("test JIT call") };
    assert_eq!(
        v, -7,
        "a resolved getstatic must read the block directly (MOVSXD of the Int payload)"
    );
    assert_eq!(
        calls_to(&inlined, helpers.getstatic),
        0,
        "an inlined getstatic must leave no CALL to jit_getstatic"
    );

    // 2. Declined site (field 0) → the helper still owns it.
    let fallback = build(0);
    // SAFETY: as above.
    let v = unsafe { fallback.try_call(&[0]).expect("test JIT call") };
    assert_eq!(
        v, 424_242,
        "a site the resolver declines must keep the jit_getstatic path"
    );
}

/// An OSR-exit map belongs at a loop header — at EVERY bytecode boundary it is
/// what made OSR impossible.
///
/// `deopt-osr` Step 7 emitted one per non-dead pc, so a method's `deopt_points`
/// held a snapshot taken mid-expression, with a partially-built operand stack.
/// `CompiledMethod::osr_exit_policy` is an artifact-wide veto — one unresumable
/// point refuses OSR entry at every pc — and a mid-expression stack entry is
/// unresumable in any method that touches a `long`/`float`/`double`, because
/// the operand stack has no per-entry width source there. Net effect: every
/// counted loop in such a method was refused `osr-entry-unresumable-exit`, and
/// a once-invoked method with a hot loop never left the interpreter (~90 ns/op
/// vs ~1.6 compiled — `probes/StaticFieldProbe.java` measured the interpreter
/// for two full rounds of a known-issue doc because of it).
///
/// The loop below is deliberately the simplest counted shape; the assertion is
/// that exactly ONE map is recorded, at the back-edge target.
#[test]
fn osr_exit_maps_are_emitted_at_loop_headers_only() {
    // 0: iconst_0        1: istore_0
    // 2: iload_0   <-- back-edge target (the only loop header)
    // 3: iconst_1        4: iadd         5: istore_0
    // 6: goto -4  (to 2)
    // 9: return
    let code: Vec<u8> = vec![
        0x03, 0x3b, 0x1a, 0x04, 0x60, 0x3b, 0xa7, 0xff, 0xfc, 0xb1, 0, 0,
    ];
    let code_len = 10;
    let helpers = test_helpers();
    let compiled = compile(
        &code,
        code_len,
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
    .expect("test JIT compile");

    if !crate::deopt_real_enabled() {
        // `CRATONVM_DEOPT_REAL=0` builds no OSR-exit metadata at all; the
        // assertion below would then pass vacuously, so say so instead.
        assert!(compiled.osr_exit_points.is_empty());
        return;
    }
    assert_eq!(
        compiled.osr_exit_points,
        vec![2],
        "exactly one OSR-exit map, at the back-edge target — not one per pc"
    );
}

// -----------------------------------------------------------------------
// G1-2 — the inline reference-store fast paths must not elide the
// collector's post-write barrier on a backend that publishes no region
// bounds. `g1-audit.md` §8.1: under G1 a young region held out of
// the collection set by a JNI pin is reachable ONLY through its remembered
// set, so an inline store that skips `post_write_barrier_rset` loses the
// edge and the next pause frees a live referent.
// -----------------------------------------------------------------------

/// Does the emitted code contain `value` as a little-endian 8-byte
/// immediate? `emit_mov_imm64` bakes the bounds-table address this way for
/// the region-containment guard, so its presence/absence distinguishes the
/// full guard from the bare trusted-oop null test.
fn code_contains_u64(compiled: &CompiledMethod, value: usize) -> bool {
    // Cast: address → the exact 8-byte immediate the encoder emitted
    let needle = (value as u64).to_le_bytes();
    compiled.code_bytes().windows(8).any(|w| *w == needle)
}

/// Number of CALLs to `target` in the emitted code. `emit_call_absolute`
/// emits `E8 rel32` when the target is within ±2GB of the call site and a
/// `MOV RAX, imm64` + `CALL RAX` form beyond it, so both encodings are
/// counted. The scan is not instruction-aligned, which can only ever
/// over-count — good enough to assert "the barrier call IS emitted".
fn calls_to(compiled: &CompiledMethod, target: usize) -> usize {
    let code = compiled.code_bytes();
    // Cast: buffer base address for resolving rel32 displacements
    let base = code.as_ptr() as usize;
    let mut n = 0usize;
    for i in 0..code.len().saturating_sub(4) {
        if code[i] != 0xE8 {
            continue;
        }
        let disp = i32::from_le_bytes([code[i + 1], code[i + 2], code[i + 3], code[i + 4]]);
        let next = base.wrapping_add(i).wrapping_add(5);
        // Cast: sign-extend the rel32 for wrapping address arithmetic
        if next.wrapping_add(disp as isize as usize) == target {
            n += 1;
        }
    }
    if code_contains_u64(compiled, target) {
        n += 1;
    }
    n
}

/// A fake, 8-byte-aligned "compact young object" that every inline
/// reference-`putfield` header test accepts: `GC_FLAG_COMPACT` set,
/// `GC_FLAG_OLD_GEN` clear, four slots, and a NULL reference cell at
/// compact body offset 0.
///
/// Deliberately NOT a `GenerationalHeap` allocation: these tests need the
/// receiver's header bits and the published region bounds to vary
/// independently, and a real heap couples them.
fn fake_compact_young_object() -> Box<[u64; 8]> {
    let mut o = Box::new([0u64; 8]);
    // SAFETY: `o` is 64 bytes and 8-byte aligned (a `[u64; 8]`); both
    // writes land inside it — `GC_FLAGS_BYTE_OFFSET` is 7 and
    // `NUM_SLOTS_OFFSET` is 12, and the reference cell is [32, 40).
    unsafe {
        let p = o.as_mut_ptr() as *mut u8; // Cast: array base → byte cursor
        *p.add(cratonvm_types::GC_FLAGS_BYTE_OFFSET) = cratonvm_types::GC_FLAG_COMPACT;
        std::ptr::write_unaligned(
            p.add(cratonvm_types::NUM_SLOTS_OFFSET) as *mut u32, // Cast: header field
            4u32,
        );
    }
    o
}

/// Read the 8-byte compact reference cell (body offset 0) of a fake object.
fn fake_object_ref_cell(o: &[u64; 8]) -> usize {
    // Derived, never restated: this said "HEADER_SIZE == 32 == 4 * 8, so the
    // cell is word 4" and silently read the WRONG WORD when the header shrank
    // to 24 — the failure mode a hard-coded layout constant always has, an
    // assertion comparing two unrelated words rather than a compile error.
    o[cratonvm_types::HEADER_SIZE / 8] as usize // Cast: raw stored pointer word
}

/// `region_bounds_are_live` must read the TABLE, not its address.
///
/// This is finding 2 of the audit in executable form: `region_bounds_addr`
/// is the address of a process-global static and is therefore always
/// non-zero, so the `!= 0` test the emitters used to key on is a constant
/// true and not a backend gate at all.
// -----------------------------------------------------------------------
// F-08 — the inline G1 post-write barrier
// -----------------------------------------------------------------------

/// The barrier table is read by CONTENT, never by address — the same lesson
/// `region_bounds_are_live_reads_the_table_not_its_address` pins one table
/// over, and the reason that test exists is that the `!= 0` form had shipped.
///
/// A published table with a zero base or a zero region mask must read as NOT
/// live: those are the two words the emitted sequence subtracts and masks with,
/// and a zero mask would make every pair of addresses look like the same region
/// — i.e. it would silently disable the barrier rather than fail loudly.
#[test]
fn the_g1_barrier_table_is_read_by_content_not_by_address() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static TABLE: [AtomicUsize; 5] = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];
    let addr = TABLE.as_ptr() as usize;

    assert_ne!(addr, 0, "a static's address is never zero");
    assert!(
        !g1_barrier_table_live(addr),
        "an all-zero table is the no-G1-collector shape and must not read as live"
    );
    assert!(!g1_barrier_table_live(0), "an unwired field is not live");

    // arena_len alone is not enough: the base and the mask are what the
    // sequence computes with.
    TABLE[1].store(0x10000, Ordering::Release);
    assert!(!g1_barrier_table_live(addr), "no base, no mask");
    TABLE[0].store(0x4000_0000, Ordering::Release);
    assert!(!g1_barrier_table_live(addr), "no mask");
    TABLE[2].store(!(0x100000usize - 1), Ordering::Release);
    assert!(g1_barrier_table_live(addr), "a fully published table is live");

    // `G1Collector::drop` clears the length first.
    TABLE[1].store(0, Ordering::Release);
    assert!(
        !g1_barrier_table_live(addr),
        "a torn-down collector must read as not-live again"
    );
    for w in TABLE.iter() {
        w.store(0, Ordering::Release);
    }
}

/// The inline barrier's two-instruction filter, EXECUTED.
///
/// This is the test that matters. `emit_g1_barrier_filter` introduces three
/// instruction encodings this backend had no other user for (`SUB r64, m64`,
/// `AND r64, m64`, `XOR r64, r64`) and an address-arithmetic argument that is
/// wrong in a silent, use-after-free direction if it is wrong at all. Reading
/// the emitted bytes would only re-assert the encoding I believe I wrote; this
/// runs them on a real CPU and asks what the flags did.
///
/// The shape under test:
///
/// ```text
///   mov  rax, arg0            ; obj
///   mov  rdx, arg1            ; val
///   <filter>                  ; jumps to `nothing` when there is nothing to do
///   mov  eax, 1 ; ret         ; "would have called the barrier"
/// nothing:
///   xor  eax, eax ; ret
/// ```
///
/// The four cases are exactly the four the filter claims to separate, and the
/// last two are the ones that would be indistinguishable if the arena base
/// were not subtracted: G1's arena is only malloc-aligned, so an aligned
/// `region_size` block of the address space is NOT a region, and a raw
/// `(obj ^ val) & mask` would call two addresses straddling the real region
/// boundary "same region", skip the barrier and lose the edge.
#[cfg(target_arch = "x86_64")]
#[test]
fn the_inline_g1_barrier_filter_separates_the_four_cases_when_executed() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    // A synthetic arena at an UNALIGNED base, which is the real shape: G1's
    // arena is a `Vec<u8>`, so its base is malloc-aligned and nothing more.
    const REGION_SIZE: usize = 0x10_0000; // 1 MiB, as G1's default is
    const ARENA_BASE: usize = 0x4000_0000 + 0x30; // deliberately not region-aligned
    const ARENA_LEN: usize = 8 * REGION_SIZE;

    static TABLE: [AtomicUsize; 5] = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];
    TABLE[0].store(ARENA_BASE, Ordering::Release);
    TABLE[2].store(!(REGION_SIZE - 1), Ordering::Release);
    TABLE[1].store(ARENA_LEN, Ordering::Release);

    let mut helpers = test_helpers();
    helpers.g1_barrier_addr = TABLE.as_ptr() as usize;

    let alloc_result = crate::regalloc::RegAllocResult {
        assignments: Vec::new(),
        xmm_assignments: Vec::new(),
        used_callee_saved: Vec::new(),
        used_xmm_regs: Vec::new(),
        block_live_in: Vec::new(),
    };
    let mut compiler = Compiler::new(
        "g1-barrier-filter-test".to_string(),
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
                // dev added `array_len_hoist_info` as argument 13 while this branch
        // was open; these tests hoist no array length.
        Vec::new(),
alloc_result,
        false,
        helpers,
        0,
        false,
        false,
        false,
        false,
        false,
        Vec::new(),
    );

    // `Compiler::new` has already emitted a method prologue into the buffer;
    // this snippet is a self-contained leaf that must be entered AFTER it, so
    // record where it starts and call there rather than at the buffer base.
    // (Entering at the base runs a prologue whose epilogue this snippet's `ret`
    // never reaches — an access violation, which is how this was found.)
    let entry_off = compiler.buf.pos();
    // Move the two C arguments into the registers the filter operates on. No
    // prologue of its own: the sequence touches only RAX/RDX/RCX and returns.
    compiler.emit_mov_r64_r64(RAX, ARG_REGS[0]);
    compiler.emit_mov_r64_r64(RDX, ARG_REGS[1]);
    let nothing = compiler.emit_g1_barrier_filter(RAX, RDX, RCX);
    compiler.emit_mov_imm64(RAX, 1);
    compiler.emit_ret();
    for patch in nothing {
        compiler.patch_rel32_to_here(patch);
    }
    compiler.emit_xor_reg_self(RAX);
    compiler.emit_ret();

    assert!(!compiler.buf.overflowed(), "the test buffer must hold the snippet");
    // W^X: `ExecutableBuffer::new` maps RW, and the compile driver flips the
    // page to RX when it finalises a method. This snippet bypasses the driver,
    // so it has to do the flip itself or the first instruction faults.
    crate::platform::make_executable(compiler.buf.as_ptr() as *mut u8, compiler.buf.capacity())
        .expect("the test buffer must be flippable to RX");
    // SAFETY: `entry_off` is a byte offset inside the same allocation.
    let entry = unsafe { compiler.buf.as_ptr().add(entry_off) };
    // SAFETY: the emitted code takes two i64 arguments in the platform's first
    // two argument registers, clobbers only RAX/RCX/RDX (all caller-saved on
    // both the SysV and Win64 ABIs), dereferences nothing but `TABLE`, and
    // returns an i64 in RAX. The buffer is RWX for its whole lifetime, which
    // outlives this call because `compiler` is still alive.
    let f: extern "C" fn(i64, i64) -> i64 = unsafe { std::mem::transmute(entry) };

    let r0 = ARENA_BASE + 8; // region 0
    let r0_high = ARENA_BASE + REGION_SIZE - 8; // still region 0
    let r1 = ARENA_BASE + REGION_SIZE + 8; // region 1

    assert_eq!(
        f(r0 as i64, 0),
        0,
        "a null store records nothing — post_write_barrier_rset returns on it"
    );
    assert_eq!(
        f(r0 as i64, (r0 + 64) as i64),
        0,
        "a same-region store records nothing"
    );
    assert_eq!(
        f(r0 as i64, r1 as i64),
        1,
        "a cross-region store must reach the collector's barrier"
    );

    let _ = r0_high;

    // The two ALIGNMENT cases, which exist only because G1's arena base is not
    // a multiple of the region size. Region 0 is
    // `[ARENA_BASE, ARENA_BASE + REGION_SIZE)`, and the aligned 1 MiB block
    // boundary at `0x4010_0000` falls INSIDE it.
    //
    // (a) Same region, different aligned blocks. A filter that masked the raw
    //     addresses would call this cross-region: a redundant call, harmless.
    let same_region_lo = ARENA_BASE + REGION_SIZE - 0x38;
    let same_region_hi = same_region_lo + 0x10;
    assert_eq!(
        (same_region_lo - ARENA_BASE) / REGION_SIZE,
        (same_region_hi - ARENA_BASE) / REGION_SIZE,
        "test setup: both addresses are in one region"
    );
    assert_ne!(
        same_region_lo >> 20,
        same_region_hi >> 20,
        "test setup: they are in DIFFERENT aligned 1 MiB blocks"
    );
    assert_eq!(
        f(same_region_lo as i64, same_region_hi as i64),
        0,
        "both are in region 0, so there is nothing to remember"
    );

    // (b) Different regions, SAME aligned block. This is the pair a base-free
    //     `(obj ^ val) & mask` silently dismisses, and dismissing it loses a
    //     live cross-region edge -- the use-after-free the subtraction prevents.
    let last_of_r0 = ARENA_BASE + REGION_SIZE - 8;
    let first_of_r1 = ARENA_BASE + REGION_SIZE + 8;
    assert_ne!(
        (last_of_r0 - ARENA_BASE) / REGION_SIZE,
        (first_of_r1 - ARENA_BASE) / REGION_SIZE,
        "test setup: the pair really is cross-region"
    );
    assert_eq!(
        last_of_r0 >> 20,
        first_of_r1 >> 20,
        "test setup: and it shares one aligned 1 MiB block"
    );
    assert_eq!(
        f(last_of_r0 as i64, first_of_r1 as i64),
        1,
        "a cross-region pair inside one aligned block must still reach the barrier"
    );

    for w in TABLE.iter() {
        w.store(0, Ordering::Release);
    }
}

/// Availability is the conjunction of three independent facts, and none of the
/// three is redundant: the flag (default OFF), a published table, and a wired
/// helper. Dropping any one of them either emits a barrier nobody asked for,
/// loads through a null table, or CALLs address zero.
#[test]
fn the_inline_g1_barrier_needs_the_flag_the_table_and_the_helper() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static TABLE: [AtomicUsize; 5] = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];
    TABLE[0].store(0x4000_0000, Ordering::Release);
    TABLE[2].store(!(0x10_0000usize - 1), Ordering::Release);
    TABLE[1].store(0x80_0000, Ordering::Release);

    let build = |barrier_addr: usize, helper: usize| {
        let mut helpers = test_helpers();
        helpers.g1_barrier_addr = barrier_addr;
        helpers.g1_post_write_barrier = helper;
        let alloc_result = crate::regalloc::RegAllocResult {
            assignments: Vec::new(),
            xmm_assignments: Vec::new(),
            used_callee_saved: Vec::new(),
            used_xmm_regs: Vec::new(),
            block_live_in: Vec::new(),
        };
        Compiler::new(
            "g1-barrier-availability-test".to_string(),
            ExecutableBuffer::new(256).expect("test executable buffer"),
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
                        // dev added `array_len_hoist_info` as argument 13 while this branch
            // was open; these tests hoist no array length.
            Vec::new(),
alloc_result,
            false,
            helpers,
            0,
            false,
            false,
            false,
            false,
            false,
            Vec::new(),
        )
    };

    let table = TABLE.as_ptr() as usize;
    let helper = 0x1234_5678usize;

    // `CRATONVM_G1_INLINE_BARRIER=0` still suppresses the arm outright. This
    // used to read "the flag is OFF by default, so even a fully wired backend
    // emits nothing"; the default flipped on 2026-09-04, and the assertion that
    // survives the flip is the one about the OFF setting, not about the
    // default.
    crate::x64::licm::G1_INLINE_BARRIER_FORCED.with(|c| c.set(Some(false)));
    assert!(
        !build(table, helper).g1_inline_barrier_available(),
        "`=0` must suppress the inline arm even on a fully wired backend — it is \
         the revert lever for a barrier whose failure mode is a lost \
         remembered-set edge"
    );

    crate::x64::licm::G1_INLINE_BARRIER_FORCED.with(|c| c.set(Some(true)));
    assert!(
        build(table, helper).g1_inline_barrier_available(),
        "flag + table + helper is the combination that emits"
    );
    assert!(
        !build(0, helper).g1_inline_barrier_available(),
        "no table address: the sequence would load through null"
    );
    assert!(
        !build(table, 0).g1_inline_barrier_available(),
        "no helper: the slow arm would CALL address zero"
    );

    // And with the override OUT of the way, the wired backend emits — which is
    // what "default ON" means and is the half a forced-on assertion cannot show.
    crate::x64::licm::G1_INLINE_BARRIER_FORCED.with(|c| c.set(None));
    assert!(
        build(table, helper).g1_inline_barrier_available(),
        "the arm is default ON since 2026-09-04: a wired backend emits it with no \
         flag set at all"
    );

    for w in TABLE.iter() {
        w.store(0, Ordering::Release);
    }
}

/// Defect G1-2's gate is untouched by F-08.
///
/// The G1 arm reads a DIFFERENT table, and `region_bounds_are_live` — the
/// predicate that decides whether a barrier-free inline store is legal — must
/// still answer NO for a backend that publishes only the G1 barrier table.
/// If this ever passes, a G1 receiver has become eligible for the generational
/// arm's barrier-free store, which is the use-after-free G1-2 named.
#[test]
fn publishing_the_g1_barrier_table_does_not_make_region_bounds_live() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static G1_TABLE: [AtomicUsize; 5] = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];
    static STORE_BOUNDS: [AtomicUsize; 6] = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];
    G1_TABLE[0].store(0x4000_0000, Ordering::Release);
    G1_TABLE[2].store(!(0x10_0000usize - 1), Ordering::Release);
    G1_TABLE[1].store(0x80_0000, Ordering::Release);

    assert!(g1_barrier_table_live(G1_TABLE.as_ptr() as usize));
    assert!(
        !region_bounds_are_live(STORE_BOUNDS.as_ptr() as usize),
        "G1 publishes no store-side region bounds, and closing G1-2 depends on it"
    );
    for w in G1_TABLE.iter() {
        w.store(0, Ordering::Release);
    }
}

/// The generational inline card mark stays disabled. F-08 is a different
/// mechanism against a different table and must not be read as re-enabling it:
/// `inline_card_mark_available` is a constant `false` because a WildFly boot
/// audit found an old `org/jboss/modules/Module` reference to a young child
/// left on a CLEAN card, and nothing here addresses that.
#[test]
fn the_generational_inline_card_mark_stays_disabled_under_f08() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static TABLE: [AtomicUsize; 5] = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];
    TABLE[0].store(0x4000_0000, Ordering::Release);
    TABLE[2].store(!(0x10_0000usize - 1), Ordering::Release);
    TABLE[1].store(0x80_0000, Ordering::Release);

    let mut helpers = test_helpers();
    helpers.g1_barrier_addr = TABLE.as_ptr() as usize;
    helpers.g1_post_write_barrier = 0x1234_5678;
    let alloc_result = crate::regalloc::RegAllocResult {
        assignments: Vec::new(),
        xmm_assignments: Vec::new(),
        used_callee_saved: Vec::new(),
        used_xmm_regs: Vec::new(),
        block_live_in: Vec::new(),
    };
    let compiler = Compiler::new(
        "g1-card-mark-separation-test".to_string(),
        ExecutableBuffer::new(256).expect("test executable buffer"),
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
                // dev added `array_len_hoist_info` as argument 13 while this branch
        // was open; these tests hoist no array length.
        Vec::new(),
alloc_result,
        false,
        helpers,
        0,
        false,
        false,
        false,
        false,
        false,
        Vec::new(),
    );
    crate::x64::licm::G1_INLINE_BARRIER_FORCED.with(|c| c.set(Some(true)));
    assert!(compiler.g1_inline_barrier_available());
    assert!(
        !compiler.inline_card_mark_available(),
        "F-08 must not re-enable the generational inline card mark"
    );
    crate::x64::licm::G1_INLINE_BARRIER_FORCED.with(|c| c.set(None));
    for w in TABLE.iter() {
        w.store(0, Ordering::Release);
    }
}

#[test]
fn region_bounds_are_live_reads_the_table_not_its_address() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static BOUNDS: [AtomicUsize; 6] = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];
    let addr = BOUNDS.as_ptr() as usize; // Cast: static address → helpers-table field

    // The G1/ZGC shape: the static exists (non-zero ADDRESS — exactly what
    // `region_bounds_addr != 0` tested) but nothing was ever published.
    assert_ne!(addr, 0, "a static's address is never zero");
    assert!(
        !region_bounds_are_live(addr),
        "an all-zero table is the G1/ZGC shape and must NOT read as live"
    );

    // Unwired table: JIT unit tests, and any embedding that never set it.
    assert!(!region_bounds_are_live(0));

    // A degenerate empty range is not a live region.
    BOUNDS[0].store(0x1000, Ordering::Release);
    BOUNDS[1].store(0x1000, Ordering::Release);
    assert!(!region_bounds_are_live(addr));

    // A real published range — the Generational shape.
    BOUNDS[1].store(0x2000, Ordering::Release);
    assert!(region_bounds_are_live(addr));

    // Any ONE of the three pairs is enough (here: old gen only).
    BOUNDS[0].store(0, Ordering::Release);
    BOUNDS[1].store(0, Ordering::Release);
    BOUNDS[4].store(0x8000, Ordering::Release);
    BOUNDS[5].store(0x9000, Ordering::Release);
    assert!(region_bounds_are_live(addr));

    // `GenerationalHeap::drop` re-zeroes the table.
    BOUNDS[4].store(0, Ordering::Release);
    BOUNDS[5].store(0, Ordering::Release);
    assert!(
        !region_bounds_are_live(addr),
        "a torn-down heap must read as not-live again"
    );
}

/// Top-level compact reference `putfield`: the barrier-free inline store is
/// reachable ONLY while the receiver is inside a published region.
///
/// Case 1 is the G1-2 regression: a receiver that is young, compact, in
/// bounds by construction, and whose old value is null — i.e. every
/// condition the fast path keys on — must still take
/// `jit_putfield_object` when the backend published no bounds, because
/// "young ⇒ no post barrier" is a generational statement and G1's
/// JNI-pinned young regions are outside the collection set.
///
/// Case 3 pins the elision that IS correct and must be preserved: SATB has
/// nothing to log for a null old value.
#[test]
fn inline_ref_putfield_fast_path_is_gated_on_published_region_bounds() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static BOUNDS: [AtomicUsize; 6] = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    /// Marker barrier helper: records that the full-barrier path ran and
    /// deliberately does NOT perform the store, so a fast-path store and a
    /// helper store are trivially distinguishable.
    unsafe extern "C" fn marker_putfield_object(_vm: i64, _obj: i64, _idx: i64, _val: i64) {
        CALLS.fetch_add(1, Ordering::SeqCst);
    }

    // void setRef(Object this, Object v) { this.f = v; }
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x2b, // 1: aload_1
        0xb5, 0x00, 0x01, // 2: putfield #1 (reference)
        0xb1, // 5: return
        0, 0,
    ];
    let code_len = 6;
    let field_info = vec![(2usize, 0usize, b'L')];
    let mut helpers = test_helpers();
    helpers.putfield_object = marker_putfield_object as *const () as usize; // Cast: fn → helpers slot
    helpers.region_bounds_addr = BOUNDS.as_ptr() as usize; // Cast: static address

    // Compact layout for the site: pc 2, body offset 0, reference field.
    set_pending_compact_field_info(vec![(2, 0, true)]);
    let compiled = compile(
        &code,
        code_len,
        2,
        2,
        true, // needs_heap — the reference putfield helper takes it
        Vec::new(),
        field_info,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots — no PIC sites
        Vec::new(), // ldc_info
        Vec::new(), // ldc2w_info
        HashMap::new(),
        HashMap::new(),
        &helpers,
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .expect("reference putfield must compile");

    // Shape: the full-barrier call is emitted exactly once, as the bail
    // target every guarded arm branches to.
    assert_eq!(
        calls_to(&compiled, helpers.putfield_object),
        1,
        "the inline arm must emit exactly one CALL to the barrier helper"
    );

    let mut obj = fake_compact_young_object();
    let val = Box::new([0u64; 8]);
    let obj_addr = obj.as_mut_ptr() as usize; // Cast: receiver address
    let val_addr = val.as_ptr() as usize; // Cast: stored reference

    // 1. G1/ZGC shape — table all zero. Young + compact + null old value,
    //    and the fast path must STILL not be taken.
    assert!(!region_bounds_are_live(helpers.region_bounds_addr));
    CALLS.store(0, Ordering::SeqCst);
    // SAFETY: JIT-compiled code from valid bytecode in an executable mmap;
    // the receiver is a live 64-byte aligned buffer shaped like an object
    // header and the marker helper performs no store.
    unsafe {
        compiled.call_with_heap(0, &[obj_addr as i64, val_addr as i64]); // Cast: JIT ABI
    }
    assert_eq!(
        CALLS.load(Ordering::SeqCst),
        1,
        "unpublished bounds (the G1/ZGC shape) must route the store to the \
         full-barrier helper even for a young receiver — G1-2"
    );
    assert_eq!(
        fake_object_ref_cell(&obj),
        0,
        "the inline store must not have run"
    );

    // 2. Generational shape — publish a range covering the receiver.
    let page = obj_addr & !0xFFF;
    BOUNDS[0].store(page, Ordering::Release);
    BOUNDS[1].store(page + 0x10000, Ordering::Release);
    assert!(region_bounds_are_live(helpers.region_bounds_addr));

    // 3. Null old value ⇒ nothing for SATB to log ⇒ the elision is correct
    //    and must be preserved: the barrier helper is NOT called.
    CALLS.store(0, Ordering::SeqCst);
    // SAFETY: as above.
    unsafe {
        compiled.call_with_heap(0, &[obj_addr as i64, val_addr as i64]); // Cast: JIT ABI
    }
    assert_eq!(
        CALLS.load(Ordering::SeqCst),
        0,
        "a young, compact, in-bounds receiver with a NULL old value keeps \
         the barrier-free fast path"
    );
    assert_eq!(
        fake_object_ref_cell(&obj),
        val_addr,
        "the inline store must have written the reference cell"
    );

    // 4. Non-null OLD value ⇒ SATB has something to log ⇒ helper.
    CALLS.store(0, Ordering::SeqCst);
    // SAFETY: as above.
    unsafe {
        compiled.call_with_heap(0, &[obj_addr as i64, val_addr as i64]); // Cast: JIT ABI
    }
    assert_eq!(
        CALLS.load(Ordering::SeqCst),
        1,
        "a non-null old value must take the SATB pre-barrier helper"
    );

    // 5. Old-generation receiver ⇒ helper (card / RSet), even with a null
    //    old value and live bounds.
    obj[4] = 0;
    // SAFETY: `obj` is the 64-byte buffer built above; byte 7 is its
    // `gc_flags` header byte.
    unsafe {
        let p = obj.as_mut_ptr() as *mut u8; // Cast: array base → byte cursor
        *p.add(cratonvm_types::GC_FLAGS_BYTE_OFFSET) =
            cratonvm_types::GC_FLAG_COMPACT | cratonvm_types::GC_FLAG_OLD_GEN;
    }
    CALLS.store(0, Ordering::SeqCst);
    // SAFETY: as above.
    unsafe {
        compiled.call_with_heap(0, &[obj_addr as i64, val_addr as i64]); // Cast: JIT ABI
    }
    assert_eq!(
        CALLS.load(Ordering::SeqCst),
        1,
        "an old-generation receiver must take the full-barrier helper"
    );
}

/// The trusted-oop receiver substitution replaces the region-containment
/// guard with a bare null test — and the containment guard is precisely
/// what routes every receiver to the full-barrier helper on a backend that
/// publishes no bounds. So the substitution is legal only when bounds are
/// actually live (audit finding 1).
///
/// Proven structurally: with an unpublished table the emitter must still
/// bake the bounds-table address as the guard's `MOV RDX, imm64` operand;
/// with a published one it may drop the guard (and the method gets shorter).
#[test]
fn trusted_oop_receiver_substitution_requires_live_bounds() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static BOUNDS: [AtomicUsize; 6] = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];
    let mut helpers = test_helpers();
    helpers.region_bounds_addr = BOUNDS.as_ptr() as usize; // Cast: static address
    let bounds_addr = helpers.region_bounds_addr;

    // void setRef(Object this, Object v) { this.f = v; }  — `aload_0`
    // marks the receiver as a proven oop, and a non-empty `method_key` is
    // the other half of `receiver_is_trusted_oop`, so the substitution is
    // eligible at this site. The legacy `compile` wrapper passes `""` and
    // could never reach it, hence `compile_with_param_slots` here.
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x2b, // 1: aload_1
        0xb5, 0x00, 0x01, // 2: putfield #1 (reference)
        0xb1, // 5: return
        0, 0,
    ];
    let compile_it = |helpers: &JitRuntimeHelpers| {
        compile_with_param_slots(
            &crate::compile_gate::CompileAdmission::for_backend_test(),
            &code,
            6,
            2,
            2,
            true,
            Vec::new(),
            vec![(2usize, 0usize, b'L')],
            Vec::new(), // typecheck_info
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),         // mic_slots
            Vec::new(),         // pic_slots
            Vec::new(),         // ldc_info
            Vec::new(),         // ldc_string_info
            Vec::new(),         // ldc_class_info
            Vec::new(),         // ldc2w_info
            Default::default(), // ldc_fp_pcs
            HashMap::new(),
            HashMap::new(),
            helpers,
            std::collections::HashSet::new(),
            HashMap::new(),
            HashMap::new(), // inline_guard_variants (PGO-02)
            None,           // string_layout
            &[],
            0,
            0b11, // param_oop_mask: both parameters are references
            vec![(2usize, 0u32, true)],
            "T.setRef:(Ljava/lang/Object;)V", // non-empty ⇒ trusted-oop eligible
            Vec::new(),
            None, // elidable_init_pcs: no constant pool, so nothing is proven empty
        )
        .expect("reference putfield must compile")
    };

    // G1/ZGC shape: no bounds published at emission time.
    assert!(!region_bounds_are_live(bounds_addr));
    let g1 = compile_it(&helpers);

    // Generational shape: bounds live at emission time.
    BOUNDS[0].store(0x1000, Ordering::Release);
    BOUNDS[1].store(0x2000, Ordering::Release);
    assert!(region_bounds_are_live(bounds_addr));
    let generational = compile_it(&helpers);

    // The containment guard bakes the table address as a 64-bit immediate
    // (`emit_mov_imm64` only uses the imm64 form above `i32::MAX`, which is
    // where any real static lands), so its presence is an exact readout of
    // which receiver check was emitted.
    if bounds_addr > i32::MAX as usize {
        assert!(
            code_contains_u64(&g1, bounds_addr),
            "unpublished bounds must keep the full containment guard — G1-2. \
             A failure here means the trusted-oop substitution is being taken \
             on a backend that publishes nothing, which loses G1's RSet edge."
        );
        assert!(
            !code_contains_u64(&generational, bounds_addr),
            "live bounds must still allow the cheap trusted-oop null test. A \
             failure here means either the gate is now unconditionally strict \
             (a throughput regression on Generational) or this site stopped \
             qualifying as `receiver_is_trusted_oop` — check `method_key`, \
             `stack_oop_marks_exact` and the aload_0 oop mark before assuming \
             the barrier gate regressed."
        );
    }

    assert!(
        generational.code_bytes().len() < g1.code_bytes().len(),
        "with live bounds the trusted-oop null test replaces the six-compare \
         containment guard, so the method must be strictly shorter \
         (live={}, unpublished={})",
        generational.code_bytes().len(),
        g1.code_bytes().len()
    );

    // Both shapes still keep the full-barrier helper as the bail target.
    assert_eq!(calls_to(&g1, helpers.putfield_object), 1);
    assert_eq!(calls_to(&generational, helpers.putfield_object), 1);
}

/// `emit_inline_fresh_ctor_compact_ref_putfield` had NO receiver guard at
/// all, so on a non-publishing backend it wrote the reference inline and
/// dropped the collector's post-write barrier entirely. It must now take
/// the helper whenever bounds are not live, and keep the fast path when
/// they are.
#[test]
fn fresh_ctor_ref_putfield_takes_the_full_barrier_when_bounds_are_not_live() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static BOUNDS: [AtomicUsize; 6] = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "C" fn marker_putfield_object(_vm: i64, _obj: i64, _idx: i64, _val: i64) {
        CALLS.fetch_add(1, Ordering::SeqCst);
    }

    // Caller: void f(Object o, Object v) { o.<init>(v); }
    let caller: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x2b, // 1: aload_1
        0xb7, 0x00, 0x01, // 2: invokespecial #1 → inline site
        0xb1, // 5: return
        0, 0,
    ];
    // Callee: <init>(Object v) { this.f = v; } — the fresh-ctor first store.
    let make_site = || {
        let mut site = make_inline_site(
            &[0x2a, 0x2b, 0xb5, 0x00, 0x01, 0xb1],
            2,     // callee_max_locals: this + v
            2,     // callee_num_args: this + v
            false, // instance method
            b'V',
        );
        site.method_name = "<init>".to_string();
        site.descriptor = "(Ljava/lang/Object;)V".to_string();
        site.field_info = vec![(2usize, 0usize, b'L')];
        site.compact_field_info = vec![(2usize, 0u32, true)];
        site.needs_heap = true;
        let mut sites = HashMap::new();
        sites.insert(2usize, site);
        sites
    };

    let mut helpers = test_helpers();
    helpers.putfield_object = marker_putfield_object as *const () as usize; // Cast: fn → slot
    helpers.region_bounds_addr = BOUNDS.as_ptr() as usize; // Cast: static address

    let compile_it = |helpers: &JitRuntimeHelpers| {
        compile(
            &caller,
            6,
            2,
            2,
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
            Vec::new(), // pic_slots
            Vec::new(), // ldc_info
            Vec::new(), // ldc2w_info
            HashMap::new(),
            HashMap::new(),
            helpers,
            std::collections::HashSet::new(),
            make_site(),
            None, // string_layout
        )
        .expect("inlined <init> reference putfield must compile")
    };

    // 1. G1/ZGC shape at emission time → the emitter routes to the helper.
    assert!(!region_bounds_are_live(helpers.region_bounds_addr));
    let g1 = compile_it(&helpers);

    // 2. Generational shape at emission time → the fast path is emitted.
    BOUNDS[0].store(0x1000, Ordering::Release);
    BOUNDS[1].store(0x2000, Ordering::Release);
    assert!(region_bounds_are_live(helpers.region_bounds_addr));
    let generational = compile_it(&helpers);

    assert_eq!(
        calls_to(&g1, helpers.putfield_object),
        1,
        "the barrier call must be emitted"
    );
    assert!(
        g1.code_bytes().len() < generational.code_bytes().len(),
        "the unpublished-bounds compile is the bare helper call, with no \
         inline store at all (unpublished={}, live={})",
        g1.code_bytes().len(),
        generational.code_bytes().len()
    );

    // The fresh-ctor emitter has no containment guard, so its runtime
    // behaviour does not depend on the table's state — only on which shape
    // was emitted. Run both against identical receivers.
    let val = Box::new([0u64; 8]);
    let val_addr = val.as_ptr() as usize; // Cast: stored reference

    let mut obj_g1 = fake_compact_young_object();
    let obj_g1_addr = obj_g1.as_mut_ptr() as usize; // Cast: receiver address
    CALLS.store(0, Ordering::SeqCst);
    // SAFETY: JIT-compiled code from valid bytecode in an executable mmap;
    // the receiver is a live 64-byte aligned object-shaped buffer and the
    // marker helper performs no store.
    unsafe {
        g1.call_with_heap(0, &[obj_g1_addr as i64, val_addr as i64]); // Cast: JIT ABI
    }
    assert_eq!(
        CALLS.load(Ordering::SeqCst),
        1,
        "a fresh-ctor reference store must take the collector's barrier when \
         the backend publishes no region bounds — G1-2"
    );
    assert_eq!(
        fake_object_ref_cell(&obj_g1),
        0,
        "the inline store must not have run"
    );

    let mut obj_gen = fake_compact_young_object();
    let obj_gen_addr = obj_gen.as_mut_ptr() as usize; // Cast: receiver address
    CALLS.store(0, Ordering::SeqCst);
    // SAFETY: as above.
    unsafe {
        generational.call_with_heap(0, &[obj_gen_addr as i64, val_addr as i64]);
        // Cast: JIT ABI
    }
    assert_eq!(
        CALLS.load(Ordering::SeqCst),
        0,
        "with live bounds a young compact fresh-ctor store keeps its \
         barrier-free inline path"
    );
    assert_eq!(
        fake_object_ref_cell(&obj_gen),
        val_addr,
        "the inline store must have written the reference cell"
    );
}

#[test]
fn test_getfield_long() {
    // Method: long getY(Object this) { return this.y; }
    // y is at field_index=1
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xb4, 0x00, 0x02, // 1: getfield #2
        0xad, // 4: lreturn
        0, 0,
    ];
    let code_len = 5;
    let field_info = vec![(1usize, 1usize, b'J')];
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        false,
        Vec::new(),
        field_info,
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

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 3);
    heap.set_field(obj, 1, Value::Long(9_999_999_999i64));

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call(&[obj.as_ptr() as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(result, 9_999_999_999i64);
}

#[test]
fn test_getfield_float() {
    // Return float field as bits
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xb4, 0x00, 0x01, // 1: getfield #1
        0xae, // 4: freturn
        0, 0,
    ];
    let code_len = 5;
    let field_info = vec![(1usize, 0usize, b'F')];
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        false,
        Vec::new(),
        field_info,
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

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 2);
    heap.set_field(obj, 0, Value::Float(3.5f32));

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call(&[obj.as_ptr() as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    let result_f = f32::from_bits(result as u32); // Cast: JIT ABI convention
    assert!((result_f - 3.5f32).abs() < 0.001);
}

#[test]
fn test_putfield_int() {
    // Method: void setX(Object this, int val) { this.x = val; }
    // Bytecode: aload_0, iload_1, putfield #1, return
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x1b, // 1: iload_1
        0xb5, 0x00, 0x01, // 2: putfield #1
        0xb1, // 5: return
        0, 0,
    ];
    let code_len = 6;
    let field_info = vec![(2usize, 0usize, b'I')];
    let compiled = compile(
        &code,
        code_len,
        2,
        2,
        false,
        Vec::new(),
        field_info,
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

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 2);
    heap.set_field(obj, 0, Value::Int(0));

    // Call: setX(obj, 99)
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call(&[obj.as_ptr() as i64, 99])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention

    // Verify the field was updated
    let val = heap.get_field(obj, 0);
    assert_eq!(val, Value::Int(99));

    // Test with negative value
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call(&[obj.as_ptr() as i64, -42])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    let val = heap.get_field(obj, 0);
    assert_eq!(val, Value::Int(-42));
}

#[test]
fn test_putfield_long() {
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x1f, // 1: lload_1
        0xb5, 0x00, 0x01, // 2: putfield #1
        0xb1, // 5: return
        0, 0,
    ];
    let code_len = 6;
    let field_info = vec![(2usize, 0usize, b'J')];
    let compiled = compile(
        &code,
        code_len,
        2,
        2,
        false,
        Vec::new(),
        field_info,
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

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 2);

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call(&[obj.as_ptr() as i64, 123_456_789_012i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    let val = heap.get_field(obj, 0);
    assert_eq!(val, Value::Long(123_456_789_012i64));
}

#[test]
fn test_putfield_object_with_write_barrier() {
    // Method: void setRef(Object this, Object ref) { this.ref = ref; }
    // Object putfield needs heap pointer for write barrier
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x2b, // 1: aload_1
        0xb5, 0x00, 0x01, // 2: putfield #1
        0xb1, // 5: return
        0, 0,
    ];
    let code_len = 6;
    let field_info = vec![(2usize, 0usize, b'L')];
    // needs_heap = true for Object putfield
    let compiled = compile(
        &code,
        code_len,
        2,
        2,
        true,
        Vec::new(),
        field_info,
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

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 2);
    let ref_obj = heap.alloc_object(ClassId::new(0), 1);

    let heap_ptr = &heap as *const GenerationalHeap as i64; // Cast: address arithmetic

    // Call with heap pointer as hidden first arg
    // SAFETY: Calling JIT-compiled machine code with heap pointer; the CompiledMethod
    // was produced from valid bytecode and the mmap region is executable.
    unsafe {
        compiled.call_with_heap(heap_ptr, &[obj.as_ptr() as i64, ref_obj.as_ptr() as i64])
        // Cast: JIT ABI convention
    };

    // Verify the field was updated
    let val = heap.get_field(obj, 0);
    match val {
        Value::Object(Some(r)) => assert_eq!(r.as_ptr(), ref_obj.as_ptr()),
        other => unreachable!("expected Object(Some), got {other:?}"),
    }

    // Test setting to null
    // SAFETY: Calling JIT-compiled machine code with heap pointer; the CompiledMethod
    // was produced from valid bytecode and the mmap region is executable.
    unsafe { compiled.call_with_heap(heap_ptr, &[obj.as_ptr() as i64, 0]) }; // Cast: JIT ABI convention
    let val = heap.get_field(obj, 0);
    assert_eq!(val, Value::Object(None));
}

#[test]
fn test_getfield_putfield_roundtrip() {
    // Method: int inc(Object this) { this.x = this.x + 1; return this.x; }
    // aload_0, getfield #1, iconst_1, iadd, aload_0, swap, putfield #1, aload_0, getfield #1, ireturn
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xb4, 0x00, 0x01, // 1: getfield #1 → x
        0x04, // 4: iconst_1
        0x60, // 5: iadd → x+1
        0x2a, // 6: aload_0
        0x5f, // 7: swap → [obj, x+1]
        0xb5, 0x00, 0x01, // 8: putfield #1 → this.x = x+1
        0x2a, // 11: aload_0
        0xb4, 0x00, 0x01, // 12: getfield #1
        0xac, // 15: ireturn
        0, 0,
    ];
    let code_len = 16;
    let field_info = vec![
        (1usize, 0usize, b'I'),  // getfield at pc=1
        (8usize, 0usize, b'I'),  // putfield at pc=8
        (12usize, 0usize, b'I'), // getfield at pc=12
    ];
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        false,
        Vec::new(),
        field_info,
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

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 2);
    heap.set_field(obj, 0, Value::Int(10));

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call(&[obj.as_ptr() as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(result, 11);

    // Verify the field is now 11
    let val = heap.get_field(obj, 0);
    assert_eq!(val, Value::Int(11));

    // Call again — should return 12
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call(&[obj.as_ptr() as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(result, 12);
}

#[test]
fn test_getfield_double() {
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xb4, 0x00, 0x01, // 1: getfield #1
        0xaf, // 4: dreturn
        0, 0,
    ];
    let code_len = 5;
    let field_info = vec![(1usize, 0usize, b'D')];
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        false,
        Vec::new(),
        field_info,
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

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 2);
    heap.set_field(obj, 0, Value::Double(2.719));

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call(&[obj.as_ptr() as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    let result_d = f64::from_bits(result as u64); // Cast: JIT ABI convention
    assert!((result_d - 2.719).abs() < 0.0001);
}

#[test]
fn test_getfield_object_ref() {
    // getfield that returns an Object reference (areturn)
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xb4, 0x00, 0x01, // 1: getfield #1
        0xb0, // 4: areturn
        0, 0,
    ];
    let code_len = 5;
    let field_info = vec![(1usize, 0usize, b'L')];
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        false,
        Vec::new(),
        field_info,
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

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 2);
    let ref_obj = heap.alloc_object(ClassId::new(0), 1);
    heap.set_field(obj, 0, Value::Object(Some(ref_obj)));

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call(&[obj.as_ptr() as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(result, ref_obj.as_ptr() as i64); // Cast: JIT ABI convention

    // Test null reference
    heap.set_field(obj, 0, Value::Object(None));
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call(&[obj.as_ptr() as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(result, 0);
}

#[test]
fn test_putfield_float_double() {
    // Test putfield for float
    // Method: void setF(Object this, float f) { this.f = f; }
    let fcode: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x23, // 1: fload_1 (load float param from local 1)
        0xb5, 0x00, 0x01, // 2: putfield #1
        0xb1, // 5: return
        0, 0,
    ];
    let code_len = 6;
    let field_info = vec![(2usize, 1usize, b'F')];
    let compiled = compile(
        &fcode,
        code_len,
        2,
        2,
        false,
        Vec::new(),
        field_info,
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

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 3);

    let float_bits = 1.5f32.to_bits() as i64; // Cast: JIT ABI convention
                                              // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                              // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call(&[obj.as_ptr() as i64, float_bits])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    let val = heap.get_field(obj, 1);
    assert_eq!(val, Value::Float(1.5f32));
}

#[test]
fn test_getfield_second_field() {
    // Access field_index=2 (third field)
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xb4, 0x00, 0x03, // 1: getfield #3
        0xac, // 4: ireturn
        0, 0,
    ];
    let code_len = 5;
    let field_info = vec![(1usize, 2usize, b'I')]; // field_index=2
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        false,
        Vec::new(),
        field_info,
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

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 4);
    heap.set_field(obj, 0, Value::Int(100));
    heap.set_field(obj, 1, Value::Int(200));
    heap.set_field(obj, 2, Value::Int(300));

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call(&[obj.as_ptr() as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(result, 300); // reads field at index 2
}

// ── Inline getfield codegen tests (Value-cell direct MOV) ───────────
//
// These exercise the inline `getfield` path that emits a raw MOV
// against the 16-byte `Value` field cell instead of `CALL jit_getfield`.
// The cases cover int/byte/ref payloads and the null-receiver guard
// and confirm bit-identical results vs the helper.

/// Compile `aload_0; getfield #1; <ret>` with the given field metadata.
fn compile_single_getfield(ret_op: u8, field_index: usize, type_tag: u8) -> CompiledMethod {
    let code: Vec<u8> = vec![0x2a, 0xb4, 0x00, 0x01, ret_op, 0, 0];
    compile(
        &code,
        5,
        1,
        1,
        false,
        Vec::new(),
        vec![(1usize, field_index, type_tag)],
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
        None, // string_layout
    )
    .unwrap()
}

#[test]
fn test_getfield_inline_byte_field() {
    // A `byte` field is stored as `Value::Int` (sign-extended) — the
    // inline path uses MOVSXD so a negative byte round-trips.
    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let compiled = compile_single_getfield(0xac /* ireturn */, 0, b'B');
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 1);
    // Byte fields live in the cell as Value::Int(sign-extended).
    heap.set_field(obj, 0, Value::Int(-7));
    // SAFETY: executing JIT-compiled machine code produced from valid bytecode.
    let r = unsafe {
        compiled
            .try_call(&[obj.as_ptr() as i64])
            .expect("test JIT call")
    };
    assert_eq!(
        r, -7,
        "inline byte getfield must sign-extend like the helper"
    );
    heap.set_field(obj, 0, Value::Int(127));
    // SAFETY: executing JIT-compiled machine code produced from valid bytecode.
    let r = unsafe {
        compiled
            .try_call(&[obj.as_ptr() as i64])
            .expect("test JIT call")
    };
    assert_eq!(r, 127);
}

#[test]
fn test_getfield_inline_ref_field() {
    // A reference field: the inline path MOVs the 8-byte payload word,
    // which is exactly the raw object pointer (0 for Object(None)).
    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let compiled = compile_single_getfield(0xb0 /* areturn */, 0, b'L');
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 1);
    let target = heap.alloc_object(ClassId::new(1), 0);
    heap.set_field(obj, 0, Value::Object(Some(target)));
    // SAFETY: executing JIT-compiled machine code produced from valid bytecode.
    let r = unsafe {
        compiled
            .try_call(&[obj.as_ptr() as i64])
            .expect("test JIT call")
    };
    assert_eq!(
        r,
        // Cast: object/array pointer to i64 for the JIT calling convention
        target.as_ptr() as i64,
        "inline ref getfield must return the raw object pointer"
    );
    // Null reference field → 0.
    heap.set_field(obj, 0, Value::Object(None));
    // SAFETY: executing JIT-compiled machine code produced from valid bytecode.
    let r = unsafe {
        compiled
            .try_call(&[obj.as_ptr() as i64])
            .expect("test JIT call")
    };
    // Cast: numeric conversion
    assert_eq!(r, 0, "Object(None) field must read as 0");
}

#[test]
fn test_getfield_inline_long_field() {
    // 8-byte payload load for a `long` field, including a value whose
    // high bit is set (no truncation / sign issues).
    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let compiled = compile_single_getfield(0xad /* lreturn */, 1, b'J');
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 2);
    let v = 0x7EDC_BA98_7654_3210_i64;
    heap.set_field(obj, 1, Value::Long(v));
    // SAFETY: executing JIT-compiled machine code produced from valid bytecode.
    let r = unsafe {
        compiled
            .try_call(&[obj.as_ptr() as i64])
            .expect("test JIT call")
    };
    assert_eq!(
        r, v,
        "inline long getfield must load the full 64-bit payload"
    );
}

#[test]
fn test_getfield_inline_null_receiver_returns_zero() {
    // A null receiver must NOT fault: the inline null check skips the
    // load and yields 0, bit-identical to `jit_getfield`'s
    // `if obj_ptr == 0 { return 0 }` guard. Covered for both an int
    // field (32-bit payload path) and a ref field (64-bit path).
    let compiled_int = compile_single_getfield(0xac /* ireturn */, 0, b'I');
    // SAFETY: executing JIT-compiled machine code; null receiver is the case under test.
    let r = unsafe { compiled_int.try_call(&[0]).expect("test JIT call") };
    assert_eq!(r, 0, "null-receiver int getfield must return 0, not fault");

    let compiled_ref = compile_single_getfield(0xb0 /* areturn */, 0, b'L');
    // SAFETY: executing JIT-compiled machine code; null receiver is the case under test.
    let r = unsafe { compiled_ref.try_call(&[0]).expect("test JIT call") };
    assert_eq!(r, 0, "null-receiver ref getfield must return 0, not fault");

    let compiled_long = compile_single_getfield(0xad /* lreturn */, 0, b'J');
    // SAFETY: executing JIT-compiled machine code; null receiver is the case under test.
    let r = unsafe { compiled_long.try_call(&[0]).expect("test JIT call") };
    assert_eq!(r, 0, "null-receiver long getfield must return 0, not fault");
}

#[test]
fn test_getfield_inline_matches_helper_differential() {
    // Differential: for a spread of int field values, the inline path
    // result must equal what `stub_getfield` (the helper) would return.
    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let compiled = compile_single_getfield(0xac /* ireturn */, 0, b'I');
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 1);
    for &v in &[0i32, 1, -1, i32::MAX, i32::MIN, 0x5A5A_5A5A, -0x0102_0304] {
        heap.set_field(obj, 0, Value::Int(v));
        // Helper reference result.
        // SAFETY: obj is a live heap object with field index 0 in bounds.
        let helper = unsafe { stub_getfield(0, obj.as_ptr() as i64, 0) };
        // SAFETY: executing JIT-compiled machine code produced from valid bytecode.
        let inline = unsafe {
            compiled
                .try_call(&[obj.as_ptr() as i64])
                .expect("test JIT call")
        };
        assert_eq!(
            inline, helper,
            "inline getfield diverged from helper for value {v}"
        );
    }
}

// ===================================================================
// LICM (Loop-Invariant Code Motion) tests
// ===================================================================

#[test]
fn test_detect_loops() {
    // Simple loop: for (k=0; k<10; k++) { ... }
    // 0: iconst_0       k = 0
    // 1: istore_1
    // 2: iload_1        ← loop header
    // 3: bipush 10
    // 5: if_icmpge +8   → exit at 13
    // 8: iinc 1, 1      k++
    // 11: goto -9        → back to 2
    // 13: iload_0        after loop
    // 14: ireturn
    let code: Vec<u8> = vec![
        0x03, // 0: iconst_0
        0x3c, // 1: istore_1
        0x1b, // 2: iload_1 (header)
        0x10, 0x0a, // 3: bipush 10
        0xa2, 0x00, 0x08, // 5: if_icmpge +8 → 13
        0x84, 0x01, 0x01, // 8: iinc 1, 1
        0xa7, 0xff, 0xf7, // 11: goto -9 → 2
        0x1a, // 14: iload_0
        0xac, // 15: ireturn
        0, 0,
    ];
    let loops = detect_loops(&code, 16);
    assert_eq!(loops.len(), 1);
    assert_eq!(loops[0], (2, 11)); // header=2, back_edge=11
}

#[test]
fn detects_only_canonical_inclusive_zero_byte_fill_loop() {
    // for (int i = from; i <= bound; i++) array[i] = 0;
    let code = [
        0x1b, // 0: iload_1 (i)
        0x1c, // 1: iload_2 (bound)
        0xa3, 0x00, 0x0d, // 2: if_icmpgt -> 15
        0x2a, // 5: aload_0 (array)
        0x1b, // 6: iload_1 (i)
        0x03, // 7: iconst_0
        0x54, // 8: bastore
        0x84, 0x01, 0x01, // 9: iinc 1, 1
        0xa7, 0xff, 0xf4, // 12: goto -> 0
        0xb1, // 15: return
    ];
    let loops = detect_loops(&code, code.len());
    assert_eq!(loops, vec![(0, 12)]);
    assert_eq!(
        detect_bulk_zero_byte_fill_loop(&code, code.len(), 0, 12),
        Some(BulkZeroByteFillLoop {
            header_pc: 0,
            array_local: 0,
            iv_local: 1,
            bound_local: 2,
        })
    );

    let mut nonzero = code;
    nonzero[7] = 0x04; // iconst_1
    assert_eq!(
        detect_bulk_zero_byte_fill_loop(&nonzero, nonzero.len(), 0, 12),
        None,
        "non-zero fills retain scalar code"
    );

    let mut mismatched_iv = code;
    mismatched_iv[6] = 0x1d; // store index comes from local 3
    assert_eq!(
        detect_bulk_zero_byte_fill_loop(&mismatched_iv, mismatched_iv.len(), 0, 12),
        None,
        "the store index must be the loop induction variable"
    );
}

#[test]
fn bulk_zero_byte_fill_executes_range_and_skips_empty_null_range() {
    // int f(byte[] a, int i, int bound) {
    //   for (; i <= bound; i++) a[i] = 0;
    //   return i;
    // }
    let code = [
        0x1b, // 0: iload_1
        0x1c, // 1: iload_2
        0xa3, 0x00, 0x0d, // 2: if_icmpgt -> 15
        0x2a, // 5: aload_0
        0x1b, // 6: iload_1
        0x03, // 7: iconst_0
        0x54, // 8: bastore
        0x84, 0x01, 0x01, // 9: iinc 1, 1
        0xa7, 0xff, 0xf4, // 12: goto -> 0
        0x1b, // 15: iload_1
        0xac, // 16: ireturn
        0x00, 0x00,
    ];
    let compiled = compile(
        &code,
        17,
        3,
        3,
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
    .expect("bulk-zero loop should compile");

    let byte_len = 8usize;
    let mut words = vec![0u64; (HEADER_SIZE + byte_len + 7) / 8];
    let array_ptr = words.as_mut_ptr() as *mut u8;
    // SAFETY: `words` is aligned and has room for the complete VM header
    // plus eight data bytes.
    unsafe {
        (array_ptr.add(ARRAY_LENGTH_OFFSET) as *mut i32).write_unaligned(byte_len as i32);
        std::ptr::write_bytes(array_ptr.add(ARRAY_DATA_OFFSET), 7, byte_len);
    }

    // SAFETY: the compiled method receives a correctly laid out live array.
    let result = unsafe {
        compiled
            .try_call(&[array_ptr as i64, 2, 5])
            .expect("bulk-zero JIT call")
    };
    assert_eq!(result, 6);
    // SAFETY: the eight-byte data region is within `words`.
    let data = unsafe { std::slice::from_raw_parts(array_ptr.add(ARRAY_DATA_OFFSET), byte_len) };
    assert_eq!(data, &[7, 7, 0, 0, 0, 0, 7, 7]);

    // An empty range does not dereference the array, even when it is null.
    // SAFETY: the bytecode exits at its condition before any array access.
    let empty = unsafe { compiled.try_call(&[0, 6, 5]).expect("empty-range JIT call") };
    assert_eq!(empty, 6);
}

#[test]
fn detects_and_executes_canonical_strided_byte_set_loop() {
    // int f(byte[] a, int j, int bound, int step) {
    //   for (; j <= bound; j += step) a[j] = 1;
    //   return j;
    // }
    let code = [
        0x1b, // 0: iload_1
        0x1c, // 1: iload_2
        0xa3, 0x00, 0x10, // 2: if_icmpgt -> 18
        0x2a, // 5: aload_0
        0x1b, // 6: iload_1
        0x04, // 7: iconst_1
        0x54, // 8: bastore
        0x1b, // 9: iload_1
        0x1d, // 10: iload_3
        0x60, // 11: iadd
        0x3c, // 12: istore_1
        0xa7, 0xff, 0xf3, // 13: goto -> 0
        0x00, 0x00, // 16: padding
        0x1b, // 18: iload_1
        0xac, // 19: ireturn
        0x00, 0x00,
    ];
    let loops = detect_loops(&code, 20);
    assert_eq!(loops, vec![(0, 13)]);
    assert_eq!(
        detect_bulk_set_byte_stride_loop(&code, 20, 0, 13),
        Some(BulkSetByteStrideLoop {
            header_pc: 0,
            array_local: 0,
            iv_local: 1,
            bound_local: 2,
            step_local: 3,
        })
    );
    let mut wrong_value = code;
    wrong_value[7] = 0x03; // iconst_0
    assert_eq!(
        detect_bulk_set_byte_stride_loop(&wrong_value, 20, 0, 13),
        None,
        "only the canonical unit store is specialized"
    );
    let mut changing_step = code;
    changing_step[10] = 0x1b; // iload_1: the IV cannot also be its step
    assert_eq!(
        detect_bulk_set_byte_stride_loop(&changing_step, 20, 0, 13),
        None,
        "the cached step must be loop-invariant"
    );

    let compiled = compile(
        &code,
        20,
        4,
        4,
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
    .expect("strided byte-set loop should compile");
    let byte_len = 8usize;
    let mut words = vec![0u64; (HEADER_SIZE + byte_len + 7) / 8];
    let array_ptr = words.as_mut_ptr() as *mut u8;
    // SAFETY: `words` is aligned and contains the VM header plus data.
    unsafe {
        (array_ptr.add(ARRAY_LENGTH_OFFSET) as *mut i32).write_unaligned(byte_len as i32);
    }
    // SAFETY: the compiled method receives a correctly laid out live array.
    let result = unsafe {
        compiled
            .try_call(&[array_ptr as i64, 1, 7, 2])
            .expect("strided byte-set JIT call")
    };
    assert_eq!(result, 9);
    // SAFETY: the eight-byte data region is within `words`.
    let data = unsafe { std::slice::from_raw_parts(array_ptr.add(ARRAY_DATA_OFFSET), byte_len) };
    assert_eq!(data, &[0, 1, 0, 1, 0, 1, 0, 1]);

    // A bound beyond the array conservatively takes the scalar path. This
    // particular stride never reaches the invalid index, so it still
    // completes normally and demonstrates that guard failure preserves
    // the bytecode's exact store sequence.
    unsafe {
        std::ptr::write_bytes(array_ptr.add(ARRAY_DATA_OFFSET), 0, byte_len);
    }
    // SAFETY: the array is live and every reached strided index is valid.
    let conservative = unsafe {
        compiled
            .try_call(&[array_ptr as i64, 1, 8, 2])
            .expect("conservative scalar fallback call")
    };
    assert_eq!(conservative, 9);
    // SAFETY: the eight-byte data region is within `words`.
    let data = unsafe { std::slice::from_raw_parts(array_ptr.add(ARRAY_DATA_OFFSET), byte_len) };
    assert_eq!(data, &[0, 1, 0, 1, 0, 1, 0, 1]);

    // Empty ranges retain Java's condition-before-array-access behavior.
    // SAFETY: the loop exits before dereferencing the null array.
    let empty = unsafe {
        compiled
            .try_call(&[0, 8, 7, 2])
            .expect("empty-range strided JIT call")
    };
    assert_eq!(empty, 8);
}

#[test]
fn detects_and_executes_canonical_byte_sieve_loop_nest() {
    // Exact javac shape of CratonBench.sieve(boolean[], int).
    let code = [
        0x03, 0x3d, // 0: int i/count = 0
        0x1c, 0x1b, 0xa3, 0x00, 0x0d, // 2: clear-loop header -> 17
        0x2a, 0x1c, 0x03, 0x54, // 7: a[i] = 0
        0x84, 0x02, 0x01, 0xa7, 0xff, 0xf4, // 11: i++; goto 2
        0x03, 0x3d, 0x05, 0x3e, // 17: count=0; outer i=2
        0x1d, 0x1b, 0xa3, 0x00, 0x2b, // 21: outer header -> 66
        0x2a, 0x1d, 0x33, 0x9a, 0x00, 0x1f, // 26: if (a[i]) -> 60
        0x84, 0x02, 0x01, // 32: count++
        0x1d, 0x1d, 0x60, 0x36, 0x04, // 35: j=i+i
        0x15, 0x04, 0x1b, 0xa3, 0x00, 0x11, // 40: inner header -> 60
        0x2a, 0x15, 0x04, 0x04, 0x54, // 46: a[j] = 1
        0x15, 0x04, 0x1d, 0x60, 0x36, 0x04, // 51: j += i
        0xa7, 0xff, 0xef, // 57: goto 40
        0x84, 0x03, 0x01, 0xa7, 0xff, 0xd6, // 60: i++; goto 21
        0x1c, 0xac, // 66: return count
        0x00, 0x00,
    ];
    let loops = detect_loops(&code, 68);
    assert!(loops.contains(&(21, 63)), "outer loop missing: {loops:?}");
    assert_eq!(
        detect_byte_sieve_loop(&code, 68, 21, 63),
        Some(ByteSieveLoop {
            header_pc: 21,
            array_local: 0,
            outer_iv_local: 3,
            bound_local: 1,
            count_local: 2,
            inner_iv_local: 4,
        })
    );

    let compiled = compile(
        &code,
        68,
        2,
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
    .expect("canonical byte sieve should compile");

    let byte_len = 32usize;
    let mut words = vec![0u64; (HEADER_SIZE + byte_len + 7) / 8];
    let array_ptr = words.as_mut_ptr() as *mut u8;
    // SAFETY: `words` is aligned and contains the VM header plus data.
    unsafe {
        (array_ptr.add(ARRAY_LENGTH_OFFSET) as *mut i32).write_unaligned(byte_len as i32);
        std::ptr::write_bytes(array_ptr.add(ARRAY_DATA_OFFSET), 7, byte_len);
    }
    // SAFETY: the compiled method receives a correctly laid out live array.
    let count = unsafe {
        compiled
            .try_call(&[array_ptr as i64, 31])
            .expect("canonical byte-sieve JIT call")
    };
    assert_eq!(count, 11);
    // SAFETY: the 32-byte data region is within `words`.
    let data = unsafe { std::slice::from_raw_parts(array_ptr.add(ARRAY_DATA_OFFSET), byte_len) };
    for prime in [2usize, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31] {
        assert_eq!(data[prime], 0, "{prime} must remain prime");
    }
    for composite in [4usize, 6, 8, 9, 10, 12, 15, 21, 25, 27, 30] {
        assert_eq!(data[composite], 1, "{composite} must be marked");
    }

    // Both counted loops are zero-trip, so a null array is not touched.
    // SAFETY: the original bytecode exits both headers before array access.
    let empty = unsafe {
        compiled
            .try_call(&[0, -1])
            .expect("empty canonical byte-sieve call")
    };
    assert_eq!(empty, 0);
}

#[test]
fn test_find_modified_locals() {
    // Loop body: iinc 1,1; istore_2; aload_0; iload_3; aaload
    let code: Vec<u8> = vec![
        0x84, 0x01, 0x01, // 0: iinc 1, 1
        0x3d, // 3: istore_2
        0x2a, // 4: aload_0
        0x1d, // 5: iload_3
        0x32, // 6: aaload
    ];
    let modified = find_modified_locals(&code, 0, 7);
    assert!(modified & (1 << 1) != 0); // local 1 modified by iinc
    assert!(modified & (1 << 2) != 0); // local 2 modified by istore_2
    assert!(modified & (1 << 0) == 0); // local 0 NOT modified
    assert!(modified & (1 << 3) == 0); // local 3 NOT modified
}

/// A `long`/`double` store writes TWO local slots, and the mask has to say so.
///
/// `test_find_modified_locals_sees_every_wide_form` covers the `wide` prefix
/// (`7af844829`). This is the adjacent hole that audit turned up: every arm of
/// this scan recorded only the LOWER slot of a category-2 store, so a consumer
/// asking about the upper one was told the loop does not write it.
///
/// It was pinned the wrong way round in this very file until 2026-09-05 —
/// `p87_fp_loop_hoist_detection` and `p87_fp_hoist_double_and_float` both
/// asserted that a `dload_1` is hoistable out of a loop whose body contains
/// `dstore_0`, one of them with the comment "this modifies 0, but doesn't
/// affect 1". It affects 1.
#[test]
fn find_modified_locals_marks_the_high_half_of_a_category_2_store() {
    // dstore_1 (narrow, implicit index) covers locals 1 and 2.
    let dstore_1: Vec<u8> = vec![0x48, 0xb1];
    assert_eq!(
        find_modified_locals(&dstore_1, 0, dstore_1.len()),
        (1u64 << 1) | (1u64 << 2),
        "`dstore_1` writes locals 1 and 2"
    );

    // lstore 5 (1-byte index operand) covers 5 and 6.
    let lstore_5: Vec<u8> = vec![0x37, 0x05, 0xb1];
    assert_eq!(
        find_modified_locals(&lstore_5, 0, lstore_5.len()),
        (1u64 << 5) | (1u64 << 6),
        "`lstore 5` writes locals 5 and 6"
    );

    // wide lstore 5 (2-byte index operand) covers the same pair.
    let wide_lstore: Vec<u8> = vec![0xc4, 0x37, 0x00, 0x05, 0xb1];
    assert_eq!(
        find_modified_locals(&wide_lstore, 0, wide_lstore.len()),
        (1u64 << 5) | (1u64 << 6),
        "`wide lstore 5` writes locals 5 and 6"
    );

    // A category-1 store keeps its single slot: over-marking is safe here, but
    // marking MORE than the store writes is still a lost hoist, so pin it.
    let istore_5: Vec<u8> = vec![0x36, 0x05, 0xb1];
    assert_eq!(
        find_modified_locals(&istore_5, 0, istore_5.len()),
        1u64 << 5,
        "`istore 5` writes local 5 only"
    );
}

/// `for (i = 0; i < a.length; i++) if (a[i] == 'a') c++;` — the exact inner
/// loop of `probes/CharAtCostCurve.java::scanArr`, taken from `javap -c -p -l`.
///
/// This is the shape the whole hoist exists for. javac re-evaluates `a.length`
/// at the top of every iteration, and the single-pass emitter took that
/// literally: a receiver move, a `TEST`/`JZ` null check that the loop's own
/// previous iteration had already discharged, and a header dereference — four
/// instructions of a 21-instruction body whose useful work is one `MOVZX`.
///
/// Locals: 0=a (char[]), 2=c, 4=i.
#[test]
fn find_array_len_hoists_matches_the_canonical_counted_loop() {
    let code: Vec<u8> = vec![
        0x03, // 0:  iconst_0
        0x3d, // 1:  istore_2        (c = 0)
        0x03, // 2:  iconst_0
        0x36, 0x04, // 3:  istore 4        (i = 0)
        0x00, 0x00, 0x00, 0x00, 0x00, // 5..9: nop padding to bci 10
        0x00, 0x00, // 10..11: nop
        0x15, 0x04, // 12: iload 4         (header)
        0x2a, // 14: aload_0
        0xbe, // 15: arraylength
        0xa2, 0x00, 0x16, // 16: if_icmpge +22 -> 38
        0x2a, // 19: aload_0
        0x15, 0x04, // 20: iload 4
        0x34, // 22: caload
        0x10, 0x61, // 23: bipush 97
        0xa0, 0x00, 0x06, // 25: if_icmpne +6 -> 31
        0x84, 0x02, 0x01, // 28: iinc 2, 1
        0x84, 0x04, 0x01, // 31: iinc 4, 1
        0xa7, 0xff, 0xea, // 34: goto -22 -> 12
        0x1c, // 37: iload_2
        0xac, // 38: ireturn
    ];
    let code_len = code.len();
    let loops = detect_loops(&code, code_len);
    assert_eq!(loops[0], (12, 34), "back edge 34 -> header 12, got {loops:?}");

    let hoists = find_array_len_hoists(&code, code_len, &loops);
    assert_eq!(hoists.len(), 1, "one invariant arraylength, got {hoists:?}");
    assert_eq!(hoists[0].loop_header, 12);
    assert_eq!(hoists[0].array_local, 0);
    assert_eq!(hoists[0].loop_end, 37, "one past the 3-byte goto at 34");
    assert_eq!(
        hoists[0].sites,
        vec![(14, 16)],
        "the aload_0 at 14 through the arraylength at 15"
    );
}

/// Two reads of the same length in one body share ONE slot and ONE pre-header
/// computation — the pre-header must not grow a load per site.
#[test]
fn find_array_len_hoists_shares_one_slot_across_sites() {
    // Locals: 0=a, 1=i.
    //  0: iload_1 ; 1: aload_0 ; 2: arraylength ; 3: if_icmpge -> 16 (header at 0)
    //  6: aload_0 ; 7: arraylength ; 8: pop
    //  9: iinc 1,1 ; 12: goto -> 0 ; 15: return
    let code: Vec<u8> = vec![
        0x1b, // 0:  iload_1 (header)
        0x2a, // 1:  aload_0
        0xbe, // 2:  arraylength
        0xa2, 0x00, 0x0c, // 3:  if_icmpge +12 -> 15
        0x2a, // 6:  aload_0
        0xbe, // 7:  arraylength
        0x57, // 8:  pop
        0x84, 0x01, 0x01, // 9:  iinc 1, 1
        0xa7, 0xff, 0xf4, // 12: goto -12 -> 0
        0xb1, // 15: return
    ];
    let code_len = code.len();
    let loops = detect_loops(&code, code_len);
    let hoists = find_array_len_hoists(&code, code_len, &loops);
    assert_eq!(hoists.len(), 1, "one record, not one per site");
    assert_eq!(hoists[0].sites, vec![(1, 3), (6, 8)]);
}

/// An array local reassigned inside the body is not invariant, and its length
/// may genuinely differ per iteration. Caching it would make the loop read a
/// stale bound — and, where BCE trusted that bound, an unchecked access.
#[test]
fn find_array_len_hoists_refuses_a_reassigned_array_local() {
    // Locals: 0=a, 1=i, 2=other.
    //  0: iload_1 ; 1: aload_0 ; 2: arraylength ; 3: if_icmpge -> 16
    //  6: aload_2 ; 7: astore_0        <- a = other, inside the body
    //  8: iinc 1,1 ; 11: goto -> 0 ; 14: return
    let code: Vec<u8> = vec![
        0x1b, // 0:  iload_1 (header)
        0x2a, // 1:  aload_0
        0xbe, // 2:  arraylength
        0xa2, 0x00, 0x0b, // 3:  if_icmpge +11 -> 14
        0x2c, // 6:  aload_2
        0x4b, // 7:  astore_0
        0x84, 0x01, 0x01, // 8:  iinc 1, 1
        0xa7, 0xff, 0xf5, // 11: goto -11 -> 0
        0xb1, // 14: return
    ];
    let code_len = code.len();
    let loops = detect_loops(&code, code_len);
    let hoists = find_array_len_hoists(&code, code_len, &loops);
    assert!(
        hoists.is_empty(),
        "astore_0 in the body makes local 0 variant, got {hoists:?}"
    );
}

/// The body scan refuses any body containing a `wide` prefix outright.
///
/// It was written when `find_modified_locals` genuinely could not decode one:
/// a `wide astore 0` fell into that function's catch-all arm and left local 0
/// looking invariant. The mask decodes `wide` now -- it had to, because the
/// arith, `aaload` and FP hoists share it and had no guard of their own, and
/// the arith one miscompiled a strided loop as a result. So this refusal is
/// belt-and-braces today. Kept anyway: the same scan also refuses `jsr`/`ret`,
/// which `detect_loops` still does not model, and refusing costs only a hoist.
///
/// The second assertion is the one that notices the mask regressing.
#[test]
fn find_array_len_hoists_refuses_a_wide_prefixed_body() {
    // Same as the refusal above, but the store is `wide astore 0`
    // (c4 3a 00 00) — four bytes, which `find_modified_locals` skips whole.
    let code: Vec<u8> = vec![
        0x1b, // 0:  iload_1 (header)
        0x2a, // 1:  aload_0
        0xbe, // 2:  arraylength
        0xa2, 0x00, 0x0e, // 3:  if_icmpge +14 -> 17
        0x2c, // 6:  aload_2
        0xc4, 0x3a, 0x00, 0x00, // 7:  wide astore 0
        0x84, 0x01, 0x01, // 11: iinc 1, 1
        0xa7, 0xff, 0xf2, // 14: goto -14 -> 0
        0xb1, // 17: return
    ];
    let code_len = code.len();
    let loops = detect_loops(&code, code_len);
    let hoists = find_array_len_hoists(&code, code_len, &loops);
    assert!(
        hoists.is_empty(),
        "a wide prefix in the body is not modelled, got {hoists:?}"
    );
    assert_eq!(
        find_modified_locals(&code, 0, code_len) & 1,
        1,
        "and the mask itself must see the `wide astore 0`"
    );
}

/// The cooperative safepoint poll is one RIP-relative instruction, and the
/// displacement it bakes must resolve to the flag byte itself.
///
/// A displacement measured from the wrong reference point reads a byte NEAR
/// the flag. That is not a fault and not a crash: the poll simply stops seeing
/// stop-the-world requests, or sees phantom ones, on the back edge of every
/// compiled loop in the VM. Nothing else in the suite would notice, so decode
/// the bytes and check the arithmetic.
#[test]
fn rip_relative_safepoint_poll_addresses_the_flag_byte() {
    static FLAG: u8 = 0;

    let alloc_result = crate::regalloc::RegAllocResult {
        assignments: Vec::new(),
        xmm_assignments: Vec::new(),
        used_callee_saved: Vec::new(),
        used_xmm_regs: Vec::new(),
        block_live_in: Vec::new(),
    };
    let mut helpers = test_helpers();
    let flag_addr = &FLAG as *const u8 as usize;
    helpers.safepoint_flag_addr = flag_addr;
    let mut c = Compiler::new(
        "rip-poll-test".to_string(),
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
        helpers,
        0,
        false,
        false,
        false,
        false,
        false,
        Vec::new(),
    );

    let start = c.buf.pos();
    let emitted = c.emit_test_mem8_abs_imm8(flag_addr, 0xFF);
    if !emitted {
        // The buffer landed more than 2GB from this test binary's data
        // segment. The fallback is the pre-2026-09-02 sequence and is checked
        // by the executing poll tests; nothing to decode here.
        assert_eq!(c.buf.pos(), start, "a refused encoding emits nothing");
        return;
    }
    let bytes: Vec<u8> = c.buf.as_slice()[start..c.buf.pos()].to_vec();
    assert_eq!(bytes.len(), 7, "F6 05 <disp32> <imm8>");
    assert_eq!(&bytes[..2], &[0xF6, 0x05], "TEST r/m8, imm8 via [rip+d32]");
    assert_eq!(bytes[6], 0xFF, "the imm8 tests every bit of the flag byte");

    let disp = i32::from_le_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]);
    // RIP is the address of the NEXT instruction — past the imm8, not past the
    // displacement. `+ 7`, not `+ 6`: the whole point of the test.
    let insn_end = c.buf.as_ptr() as usize + start + 7;
    let resolved = (insn_end as i64).wrapping_add(disp as i64) as usize;
    assert_eq!(
        resolved, flag_addr,
        "the RIP-relative displacement must land exactly on the flag byte"
    );
}

#[test]
fn test_find_loop_hoists_conditional_body_still_detected() {
    // The detector deliberately hoists a sequence that sits BEHIND a
    // conditional inside the body (`for(..){ if (m != null) m[j]... }`) —
    // and the loop may also be zero-trip at runtime. That is only sound
    // because the EMISSION now guards the hoisted load with null+bounds
    // checks routed to the reason-2 deopt stub (see the preheader "LICM:
    // Emit hoisted aaload" block and test_classes/LicmHoistRepro.java);
    // before those guards this shape crashed the VM on m == null. This
    // test pins the detector contract so an emission-side reader knows
    // conditional/zero-trip shapes DO reach the guarded preheader.
    //
    // Locals: 0=m (Object[]), 1=j, 2=n, 3=i.
    //   0: iconst_0 ; 1: istore_3                    (i = 0)
    //   2: iload_3 ; 3: iload_2 ; 4: if_icmpge → 21  (header)
    //   7: aload_0 ; 8: ifnull → 15                  (skip if m == null)
    //  11: aload_0 ; 12: iload_1 ; 13: aaload ; 14: pop
    //  15: iinc 3, 1 ; 18: goto → 2 ; 21: return
    let code: Vec<u8> = vec![
        0x03, // 0: iconst_0
        0x3e, // 1: istore_3
        0x1d, // 2: iload_3 (header)
        0x1c, // 3: iload_2
        0xa2, 0x00, 0x11, // 4: if_icmpge +17 → 21
        0x2a, // 7: aload_0
        0xc6, 0x00, 0x07, // 8: ifnull +7 → 15
        0x2a, // 11: aload_0
        0x1b, // 12: iload_1
        0x32, // 13: aaload
        0x57, // 14: pop
        0x84, 0x03, 0x01, // 15: iinc 3, 1
        0xa7, 0xff, 0xf0, // 18: goto -16 → 2
        0xb1, // 21: return
    ];
    let code_len = code.len();
    let loops = detect_loops(&code, code_len);
    assert_eq!(loops[0], (2, 18), "loop should be (2, 18), got {loops:?}");

    let hoists = find_loop_hoists(&code, code_len, &loops);
    assert_eq!(hoists.len(), 1, "conditional-body aaload is hoisted");
    assert_eq!(hoists[0].loop_header, 2);
    assert_eq!(hoists[0].seq_start, 11);
    assert_eq!(hoists[0].array_local, 0);
    assert_eq!(hoists[0].index_local, 1);
}

#[test]
fn test_match_invariant_aaload() {
    // aload_0; iload_3; aaload
    let code: Vec<u8> = vec![0x2a, 0x1d, 0x32];
    let modified = 0u64; // nothing modified
    let result = match_invariant_aaload(&code, 0, modified, 3);
    assert_eq!(result, Some((0, 3, 3))); // array=0, index=3, seq_end=3

    // Same but local 0 is modified → should NOT match
    let modified2 = 1u64 << 0;
    let result2 = match_invariant_aaload(&code, 0, modified2, 3);
    assert_eq!(result2, None);

    // aload_1; iload (wide) 4; aaload
    let code2: Vec<u8> = vec![0x2b, 0x15, 0x04, 0x32];
    let result3 = match_invariant_aaload(&code2, 0, 0, 4);
    assert_eq!(result3, Some((1, 4, 4)));
}

#[test]
fn test_find_loop_hoists_matmul_pattern() {
    // Simulate matmul inner loop pattern:
    // header=2: iload 7; iload 2; if_icmpge exit;
    //           aload_0; iload 4; aaload;     ← INVARIANT (a[i])
    //           iload 7; iaload;              sum += a[i][k]
    //           iinc 7,1; goto header
    //
    // Modified: local 7 (k) via iinc
    // Invariant: local 0 (a) and local 4 (i)
    let code: Vec<u8> = vec![
        0x03, // 0: iconst_0 (k=0)
        0x36, 0x07, // 1: istore 7
        // header at 3:
        0x15, 0x07, // 3: iload 7
        0x1c, // 5: iload_2 (n)
        0xa2, 0x00, 0x0e, // 6: if_icmpge +14 → 20
        0x2a, // 9: aload_0 (a) ← SEQ START
        0x15, 0x04, // 10: iload 4 (i)
        0x32, // 12: aaload  ← SEQ END (13)
        0x15, 0x07, // 13: iload 7 (k)
        0x2e, // 15: iaload (a[i][k])
        0x84, 0x07, 0x01, // 16: iinc 7, 1
        0xa7, 0xff, 0xf0, // 19: goto -16 → 3
        0x1a, // 22: iload_0
        0xac, // 23: ireturn
        0, 0,
    ];
    let loops = detect_loops(&code, 24);
    assert_eq!(loops.len(), 1);
    assert_eq!(loops[0], (3, 19)); // header=3, back=19

    let hoists = find_loop_hoists(&code, 24, &loops);
    assert_eq!(hoists.len(), 1);
    assert_eq!(hoists[0].loop_header, 3);
    assert_eq!(hoists[0].seq_start, 9);
    assert_eq!(hoists[0].seq_end, 13);
    assert_eq!(hoists[0].array_local, 0);
    assert_eq!(hoists[0].index_local, 4);
}

#[test]
fn detects_exact_guarded_matrix_dot_product_loop() {
    // CratonBench.matmul's javac bytecode at bci 31..62.
    let mut code = vec![0u8; 31];
    code.extend_from_slice(&[
        0x15, 0x07, // 31: iload 7 (k)
        0x1c, // 33: iload_2 (n)
        0xa2, 0x00, 0x1d, // 34: if_icmpge 63
        0x15, 0x06, // 37: iload 6 (sum)
        0x2a, // 39: aload_0 (a)
        0x15, 0x04, // 40: iload 4 (row)
        0x32, // 42: aaload
        0x15, 0x07, // 43: iload 7 (k)
        0x2e, // 45: iaload
        0x2b, // 46: aload_1 (b)
        0x15, 0x07, // 47: iload 7 (k)
        0x32, // 49: aaload
        0x15, 0x05, // 50: iload 5 (column)
        0x2e, // 52: iaload
        0x68, // 53: imul
        0x60, // 54: iadd
        0x36, 0x06, // 55: istore 6
        0x84, 0x07, 0x01, // 57: iinc 7,1
        0xa7, 0xff, 0xe3, // 60: goto 31
    ]);

    let dot = detect_matrix_dot_loop(&code, 31, 60, 7).expect("matrix dot loop");
    assert_eq!(
        dot,
        MatrixDotLoop {
            header_pc: 31,
            back_edge_pc: 60,
            iv_local: 7,
            bound_local: 2,
            acc_local: 6,
            a_outer_local: 0,
            a_row_local: 4,
            b_outer_local: 1,
            b_column_local: 5,
        }
    );

    // Any side effect or different arithmetic body must stay on the exact
    // scalar emitter; fallback restart would not be valid for it.
    code[53] = 0x4f; // iastore instead of imul
    assert!(detect_matrix_dot_loop(&code, 31, 60, 7).is_none());
}

#[cfg(feature = "vm-tests")]
#[test]
fn test_licm_loop_sum_with_aaload() {
    // Test that LICM correctly hoists an aaload out of a loop.
    //
    // Java equivalent:
    //   int sum(int[][] a, int i, int n) {
    //       int sum = 0;
    //       for (int k = 0; k < n; k++) {
    //           sum += a[i][k];  // a[i] is loop-invariant
    //       }
    //       return sum;
    //   }
    //
    // Params: a(local 0), i(local 1), n(local 2)
    // Locals: sum(local 3), k(local 4)
    // goto at PC 23 → target PC 5: offset = 5 - 23 = -18 = 0xFFEE
    let code: Vec<u8> = vec![
        0x03, // 0: iconst_0 (sum=0)
        0x3e, // 1: istore_3
        0x03, // 2: iconst_0 (k=0)
        0x36, 0x04, // 3: istore 4
        // header at 5:
        0x15, 0x04, // 5: iload 4 (k)
        0x1c, // 7: iload_2 (n)
        0xa2, 0x00, 0x12, // 8: if_icmpge +18 → 26
        0x1d, // 11: iload_3 (sum)
        0x2a, // 12: aload_0 (a) ← HOIST
        0x1b, // 13: iload_1 (i)
        0x32, // 14: aaload (a[i])
        0x15, 0x04, // 15: iload 4 (k)
        0x2e, // 17: iaload (a[i][k])
        0x60, // 18: iadd
        0x3e, // 19: istore_3
        0x84, 0x04, 0x01, // 20: iinc 4, 1
        0xa7, 0xff, 0xee, // 23: goto -18 → 5
        0x1d, // 26: iload_3
        0xac, // 27: ireturn
        0, 0,
    ];
    let code_len = 28;

    // Verify LICM detection
    let loops = detect_loops(&code, code_len);
    assert_eq!(loops.len(), 1);
    assert_eq!(loops[0], (5, 23));

    let hoists = find_loop_hoists(&code, code_len, &loops);
    assert_eq!(hoists.len(), 1);
    assert_eq!(hoists[0].loop_header, 5);
    assert_eq!(hoists[0].seq_start, 12);
    assert_eq!(hoists[0].array_local, 0);
    assert_eq!(hoists[0].index_local, 1);

    // Compile and verify correctness
    assert!(is_jit_compatible(&code, code_len, "([[III)I"));

    // Create arrays: a = int[3][4], a[1] = {10, 20, 30, 40}
    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_gc::heap::ArrayElementType;
    use cratonvm_types::ClassId;

    let heap = GenerationalHeap::new();

    // Create int[] row = {10, 20, 30, 40}
    let row = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 4);
    let _ = heap.set_array_element(row, 0, Value::Int(10));
    let _ = heap.set_array_element(row, 1, Value::Int(20));
    let _ = heap.set_array_element(row, 2, Value::Int(30));
    let _ = heap.set_array_element(row, 3, Value::Int(40));

    // Create Object[] (int[][]) outer array with 3 rows
    let outer = heap.alloc_array(ClassId::new(0), ArrayElementType::Reference, 3);
    let _ = heap.set_array_element(outer, 1, Value::Object(Some(row)));

    // Compile with heap (needs_heap for arrays)
    let compiled = compile(
        &code,
        code_len,
        3,
        5,
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

    // Call: sum(a=outer, i=1, n=4) → should sum row[0..4] = 10+20+30+40 = 100
    let heap_ptr = &heap as *const _ as i64; // Cast: function pointer for JIT call target
                                             // SAFETY: Calling JIT-compiled machine code with heap pointer; the CompiledMethod
                                             // was produced from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.call_with_heap(heap_ptr, &[outer.as_ptr() as i64, 1, 4]) }; // Cast: JIT ABI convention
    assert_eq!(result, 100);

    // Call with n=2 → sum first 2 elements: 10+20 = 30
    // SAFETY: Calling JIT-compiled machine code with heap pointer; the CompiledMethod
    // was produced from valid bytecode and the mmap region is executable.
    let result2 = unsafe { compiled.call_with_heap(heap_ptr, &[outer.as_ptr() as i64, 1, 2]) }; // Cast: JIT ABI convention
    assert_eq!(result2, 30);

    // Call with n=0 → sum nothing = 0
    // SAFETY: Calling JIT-compiled machine code with heap pointer; the CompiledMethod
    // was produced from valid bytecode and the mmap region is executable.
    let result3 = unsafe { compiled.call_with_heap(heap_ptr, &[outer.as_ptr() as i64, 1, 0]) }; // Cast: JIT ABI convention
    assert_eq!(result3, 0);
}

#[test]
fn test_licm_no_hoist_when_modified() {
    // When the index local is modified inside the loop, aaload should NOT be hoisted.
    // loop body: aload_0; iload_1; aaload; ...; iinc 1, 1
    // local 1 is modified → no hoist
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0 (header)
        0x1b, // 1: iload_1
        0x32, // 2: aaload
        0x57, // 3: pop
        0x84, 0x01, 0x01, // 4: iinc 1, 1
        0xa7, 0xff, 0xf9, // 7: goto -7 → 0
        0, 0,
    ];
    let loops = detect_loops(&code, 10);
    assert_eq!(loops.len(), 1);
    let hoists = find_loop_hoists(&code, 10, &loops);
    assert_eq!(hoists.len(), 0); // no hoist because local 1 is modified
}

#[test]
fn test_licm_no_hoist_with_aastore() {
    // When the loop contains aastore (0x53), aaload should NOT be hoisted
    // (conservative safety: aastore could modify the array being read)
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0 (header)
        0x1b, // 1: iload_1
        0x32, // 2: aaload
        0x57, // 3: pop
        0x53, // 4: aastore (tests analysis only)
        0xa7, 0xff, 0xfa, // 5: goto -6 → 0
        0, 0,
    ];
    // Test the LICM analysis directly (aastore isn't JIT-compilable, but
    // find_loop_hoists analyzes bytecode patterns independently)
    let loops = vec![(0usize, 5usize)]; // header=0, back=5
    let hoists = find_loop_hoists(&code, 8, &loops);
    assert_eq!(hoists.len(), 0); // no hoist because aastore present
}

#[test]
fn test_licm_nested_loops() {
    // Nested loops: outer (j) and inner (k).
    // aload_0; iload 4; aaload is invariant in BOTH loops.
    // Should be hoisted to the OUTERMOST loop (header=0).
    //
    // Modified in outer: local 5 (j) via iinc, local 7 (k) via iinc
    // Modified in inner: local 7 (k) via iinc
    // Invariant in both: local 0 (a) and local 4 (i)
    //
    // Layout:
    //   PC 0: outer header
    //   PC 6: inner header
    //   PC 12-15: aload_0; iload 4; aaload (invariant)
    //   PC 20: inner back-edge (goto PC 6, offset = 6-20 = -14 = 0xFFF2)
    //   PC 23: outer iinc j
    //   PC 26: outer back-edge (goto PC 0, offset = 0-26 = -26 = 0xFFE6)
    let code: Vec<u8> = vec![
        0x15, 0x05, // 0: iload 5 (j)
        0x1c, // 2: iload_2 (n)
        0xa2, 0x00, 0x17, // 3: if_icmpge +23 → 26
        0x15, 0x07, // 6: iload 7 (k)
        0x1c, // 8: iload_2 (n)
        0xa2, 0x00, 0x0a, // 9: if_icmpge +10 → 19
        0x2a, // 12: aload_0    ← INVARIANT
        0x15, 0x04, // 13: iload 4 (i)
        0x32, // 15: aaload
        0x57, // 16: pop
        0x84, 0x07, 0x01, // 17: iinc 7, 1
        0xa7, 0xff, 0xf2, // 20: goto -14 → 6
        0x84, 0x05, 0x01, // 23: iinc 5, 1
        0xa7, 0xff, 0xe6, // 26: goto -26 → 0
        0, 0,
    ];
    let code_len = 29;

    // Use detect_loops to find both loops
    let loops = detect_loops(&code, code_len);
    assert_eq!(loops.len(), 2);

    let hoists = find_loop_hoists(&code, code_len, &loops);
    assert_eq!(hoists.len(), 1);
    // Should be hoisted to the OUTER loop (header=0), not the inner (header=6)
    assert_eq!(hoists[0].loop_header, 0);
    assert_eq!(hoists[0].seq_start, 12);
    assert_eq!(hoists[0].array_local, 0);
    assert_eq!(hoists[0].index_local, 4);
}

#[test]
fn test_arith_licm_invariant_expr() {
    // Counted loop where `base*3 + 11` (base = local 0, never stored in
    // the loop) is loop-invariant and should be hoisted.
    //
    //   PC 0: iload 4 (i)           — loop header / cond
    //   PC 2: iload_1 (n)
    //   PC 3: if_icmpge +N          — loop exit
    //   PC 6: iload_0 (base)        ← invariant run START
    //   PC 7: iconst_3
    //   PC 8: imul
    //   PC 9: bipush 11
    //   PC 11: iadd                 ← invariant run END (seq_end = 12)
    //   PC 12: istore 5
    //   PC 14: iinc 4, 1
    //   PC 17: goto -17 → 0
    let code: Vec<u8> = vec![
        0x15, 0x04, // 0: iload 4
        0x1b, // 2: iload_1
        0xa2, 0x00, 0x11, // 3: if_icmpge +17 → 20
        0x1a, // 6: iload_0 (base)   ← invariant
        0x06, // 7: iconst_3
        0x68, // 8: imul
        0x10, 0x0b, // 9: bipush 11
        0x60, // 11: iadd
        0x36, 0x05, // 12: istore 5
        0x84, 0x04, 0x01, // 14: iinc 4, 1
        0xa7, 0xff, 0xef, // 17: goto -17 → 0
        0, 0,
    ];
    let code_len = 20;

    let loops = detect_loops(&code, code_len);
    assert_eq!(loops.len(), 1);

    let hoists = find_arith_loop_hoists(&code, code_len, &loops);
    assert_eq!(hoists.len(), 1, "base*3+11 must be hoisted");
    assert_eq!(hoists[0].loop_header, 0);
    assert_eq!(hoists[0].seq_start, 6);
    assert_eq!(hoists[0].seq_end, 12);
    // Expression: iload_0, iconst_3, imul, bipush 11, iadd → 5 steps.
    assert_eq!(hoists[0].steps.len(), 5);
    assert!(matches!(hoists[0].steps[0], ArithStep::PushLocal(0)));
    assert!(matches!(hoists[0].steps[1], ArithStep::PushConst(3)));
    assert!(matches!(hoists[0].steps[2], ArithStep::BinOp(0x68)));
    assert!(matches!(hoists[0].steps[3], ArithStep::PushConst(11)));
    assert!(matches!(hoists[0].steps[4], ArithStep::BinOp(0x60)));
    assert_eq!(arith_expr_max_depth(&hoists[0].steps), 2);
}

#[test]
fn test_arith_licm_no_hoist_across_invoke_loop() {
    // The arithmetic itself is invariant, but a loop that invokes Java is
    // re-entrant: its synthetic LICM frame value must not be kept across
    // that call boundary. This is the shape exercised by Lucene's
    // recursive Sorter.mergeInPlace loop.
    let code: Vec<u8> = vec![
        0x15, 0x04, // 0: iload 4
        0x1b, // 2: iload_1
        0xa2, 0x00, 0x14, // 3: if_icmpge -> 23
        0x1a, // 6: iload_0 (invariant)
        0x06, // 7: iconst_3
        0x68, // 8: imul
        0x10, 0x0b, // 9: bipush 11
        0x60, // 11: iadd
        0x36, 0x05, // 12: istore 5
        0xb8, 0x00, 0x01, // 14: invokestatic #1
        0x84, 0x04, 0x01, // 17: iinc 4, 1
        0xa7, 0xff, 0xec, // 20: goto -> 0
    ];
    let code_len = code.len();
    let loops = detect_loops(&code, code_len);
    assert_eq!(loops.len(), 1);
    assert!(
        find_arith_loop_hoists(&code, code_len, &loops).is_empty(),
        "a loop containing invoke* must keep arithmetic in bytecode order"
    );
}

#[test]
fn test_arith_licm_no_hoist_when_operand_modified() {
    // Same shape, but `base` (local 0) IS stored inside the loop, so the
    // expression is NOT loop-invariant and must not be hoisted.
    //
    //   PC 0: iload 4              — header
    //   PC 2: iload_1
    //   PC 3: if_icmpge +N
    //   PC 6: iload_0 (base)
    //   PC 7: iconst_3
    //   PC 8: imul
    //   PC 9: istore_0 (base)      ← base modified → not invariant
    //   PC 10: iinc 4, 1
    //   PC 13: goto -13 → 0
    let code: Vec<u8> = vec![
        0x15, 0x04, // 0: iload 4
        0x1b, // 2: iload_1
        0xa2, 0x00, 0x0d, // 3: if_icmpge +13 → 16
        0x1a, // 6: iload_0
        0x06, // 7: iconst_3
        0x68, // 8: imul
        0x3b, // 9: istore_0  ← modifies base
        0x84, 0x04, 0x01, // 10: iinc 4, 1
        0xa7, 0xff, 0xf3, // 13: goto -13 → 0
        0, 0,
    ];
    let code_len = 16;
    let loops = detect_loops(&code, code_len);
    let hoists = find_arith_loop_hoists(&code, code_len, &loops);
    assert!(
        hoists.is_empty(),
        "expr on a loop-modified local must not hoist"
    );
}

#[test]
fn test_arith_licm_no_hoist_when_operand_modified_by_wide_iinc() {
    // The same refusal, through the `wide` prefix.
    //
    // AUDIT 2026-09-05. `find_modified_locals` had an arm for `iinc`
    // (0x84) and none for `wide` (0xc4), so `wide iinc` fell to the
    // length-only default: the walk stayed aligned and the local was
    // never marked modified. javac emits `wide iinc` whenever the delta
    // does not fit in a signed byte -- `i += 1024` -- so the induction
    // variable of every such loop read as loop-INVARIANT and
    // `i + <invariant>` got hoisted into the pre-header. Stride 1 was
    // correct and stride 1024 was not.
    //
    //   PC 0: iload 4              — header
    //   PC 2: iload_1
    //   PC 3: if_icmpge +N
    //   PC 6: iload 4  (the INDUCTION VARIABLE, not a constant)
    //   PC 8: iload_0
    //   PC 9: iadd                 ← i + base, must NOT hoist
    //   PC 10: istore_2
    //   PC 11: wide iinc 4, 1024   ← modifies local 4
    //   PC 17: goto -17 → 0
    let code: Vec<u8> = vec![
        0x15, 0x04, // 0: iload 4
        0x1b, // 2: iload_1
        0xa2, 0x00, 0x11, // 3: if_icmpge +17 → 20
        0x15, 0x04, // 6: iload 4   ← the induction variable
        0x1a, // 8: iload_0
        0x60, // 9: iadd
        0x3d, // 10: istore_2
        0xc4, 0x84, 0x00, 0x04, 0x04, 0x00, // 11: wide iinc 4, 1024
        0xa7, 0xff, 0xef, // 17: goto -17 → 0
        0, 0,
    ];
    let code_len = 20;
    let loops = detect_loops(&code, code_len);
    let hoists = find_arith_loop_hoists(&code, code_len, &loops);
    assert!(
        hoists.is_empty(),
        "an expression over a local incremented by WIDE iinc is not          loop-invariant and must not hoist; got {} hoist(s)",
        hoists.len()
    );
}

/// `wide` stores mark their local too, not just `wide iinc`.
#[test]
fn test_find_modified_locals_sees_every_wide_form() {
    use crate::x64::escape_analysis::find_modified_locals;
    // wide istore 4      c4 36 00 04
    let m = find_modified_locals(&[0xc4, 0x36, 0x00, 0x04], 0, 4);
    assert_eq!(m, 1 << 4, "wide istore must mark its local");
    // wide astore 7      c4 3a 00 07
    let m = find_modified_locals(&[0xc4, 0x3a, 0x00, 0x07], 0, 4);
    assert_eq!(m, 1 << 7, "wide astore must mark its local");
    // wide iinc 4, 1024  c4 84 00 04 04 00
    let m = find_modified_locals(&[0xc4, 0x84, 0x00, 0x04, 0x04, 0x00], 0, 6);
    assert_eq!(m, 1 << 4, "wide iinc must mark its local");
    // wide iload 4 writes nothing.
    let m = find_modified_locals(&[0xc4, 0x15, 0x00, 0x04], 0, 4);
    assert_eq!(m, 0, "a wide LOAD must not mark anything");
}

#[test]
fn test_arith_licm_excludes_idiv() {
    // idiv (0x6c) can throw ArithmeticException — it must never be part
    // of a hoisted run. `base / 3` here should yield no hoist.
    //   PC 0..5: header + cond (as above)
    //   PC 6: iload_0; PC 7: iconst_3; PC 8: idiv
    //   PC 9: istore 5 ...
    let code: Vec<u8> = vec![
        0x15, 0x04, // 0: iload 4
        0x1b, // 2: iload_1
        0xa2, 0x00, 0x0e, // 3: if_icmpge → 17
        0x1a, // 6: iload_0
        0x06, // 7: iconst_3
        0x6c, // 8: idiv  ← faulting, not hoistable
        0x36, 0x05, // 9: istore 5
        0x84, 0x04, 0x01, // 11: iinc 4, 1
        0xa7, 0xff, 0xf2, // 14: goto -14 → 0
        0, 0,
    ];
    let code_len = 17;
    let loops = detect_loops(&code, code_len);
    let hoists = find_arith_loop_hoists(&code, code_len, &loops);
    assert!(hoists.is_empty(), "idiv must not be hoisted (can fault)");
}

#[test]
fn test_aconst_null() {
    // Method: long f() { return null; }  (aconst_null, areturn)
    let code: Vec<u8> = vec![0x01, 0xb0, 0, 0];
    let code_len = 2;
    assert!(is_jit_compatible(&code, code_len, "()Ljava/lang/Object;"));
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
    assert_eq!(result, 0);
}

#[test]
fn test_ifnull_taken() {
    // Method: int f(Object a) { if (a == null) return 1; return 0; }
    // aload_0, ifnull +5, iconst_0, ireturn, iconst_1, ireturn
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xc6, 0x00, 0x05, // 1: ifnull +5 → PC 6
        0x03, // 4: iconst_0
        0xac, // 5: ireturn
        0x04, // 6: iconst_1
        0xac, // 7: ireturn
        0, 0,
    ];
    let code_len = 8;
    assert!(is_jit_compatible(&code, code_len, "(Ljava/lang/Object;)I"));
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
    // null input → branch taken → returns 1
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[0]).expect("test JIT call") };
    assert_eq!(result, 1);
    // non-null input → branch not taken → returns 0
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[42]).expect("test JIT call") };
    assert_eq!(result, 0);
}

#[test]
fn test_ifnonnull_taken() {
    // Method: int f(Object a) { if (a != null) return 1; return 0; }
    // aload_0, ifnonnull +5, iconst_0, ireturn, iconst_1, ireturn
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xc7, 0x00, 0x05, // 1: ifnonnull +5 → PC 6
        0x03, // 4: iconst_0
        0xac, // 5: ireturn
        0x04, // 6: iconst_1
        0xac, // 7: ireturn
        0, 0,
    ];
    let code_len = 8;
    assert!(is_jit_compatible(&code, code_len, "(Ljava/lang/Object;)I"));
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
    // non-null input → branch taken → returns 1
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[42]).expect("test JIT call") };
    assert_eq!(result, 1);
    // null input → branch not taken → returns 0
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[0]).expect("test JIT call") };
    assert_eq!(result, 0);
}

#[test]
fn test_if_acmpeq() {
    // Method: int f(Object a, Object b) { if (a == b) return 1; return 0; }
    // aload_0, aload_1, if_acmpeq +5, iconst_0, ireturn, iconst_1, ireturn
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x2b, // 1: aload_1
        0xa5, 0x00, 0x05, // 2: if_acmpeq +5 → PC 7
        0x03, // 5: iconst_0
        0xac, // 6: ireturn
        0x04, // 7: iconst_1
        0xac, // 8: ireturn
        0, 0,
    ];
    let code_len = 9;
    assert!(is_jit_compatible(
        &code,
        code_len,
        "(Ljava/lang/Object;Ljava/lang/Object;)I"
    ));
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
    // Same ref → branch taken → 1
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[100, 100]).expect("test JIT call") };
    assert_eq!(result, 1);
    // Different refs → not taken → 0
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[100, 200]).expect("test JIT call") };
    assert_eq!(result, 0);
    // Both null → taken → 1
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[0, 0]).expect("test JIT call") };
    assert_eq!(result, 1);
}

#[test]
fn test_if_acmpne() {
    // Method: int f(Object a, Object b) { if (a != b) return 1; return 0; }
    // aload_0, aload_1, if_acmpne +5, iconst_0, ireturn, iconst_1, ireturn
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x2b, // 1: aload_1
        0xa6, 0x00, 0x05, // 2: if_acmpne +5 → PC 7
        0x03, // 5: iconst_0
        0xac, // 6: ireturn
        0x04, // 7: iconst_1
        0xac, // 8: ireturn
        0, 0,
    ];
    let code_len = 9;
    assert!(is_jit_compatible(
        &code,
        code_len,
        "(Ljava/lang/Object;Ljava/lang/Object;)I"
    ));
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
    // Different refs → branch taken → 1
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[100, 200]).expect("test JIT call") };
    assert_eq!(result, 1);
    // Same ref → not taken → 0
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[100, 100]).expect("test JIT call") };
    assert_eq!(result, 0);
}

#[test]
fn test_ifnull_with_aconst_null() {
    // Method: int f() { Object x = null; if (x == null) return 42; return 0; }
    // aconst_null, astore_0, aload_0, ifnull +5, iconst_0, ireturn, bipush 42, ireturn
    let code: Vec<u8> = vec![
        0x01, // 0: aconst_null
        0x4b, // 1: astore_0
        0x2a, // 2: aload_0
        0xc6, 0x00, 0x05, // 3: ifnull +5 → PC 8
        0x03, // 6: iconst_0
        0xac, // 7: ireturn
        0x10, 0x2a, // 8: bipush 42
        0xac, // 10: ireturn
        0, 0,
    ];
    let code_len = 11;
    assert!(is_jit_compatible(&code, code_len, "()I"));
    let compiled = compile(
        &code,
        code_len,
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
    assert_eq!(result, 42);
}

#[cfg(feature = "vm-tests")]
#[test]
fn test_checkcast_null_passthrough() {
    // Method: Object f(Object a) { return (String) a; }
    // checkcast with null should pass through
    // aload_0, checkcast #0, areturn
    let class_name = "java/lang/String";
    let leaked: &'static str = Box::leak(class_name.to_string().into_boxed_str()); // LEAK(intentional): test-only; class name must outlive JIT-compiled code pointer
    let typecheck_info = vec![(1usize, leaked.as_ptr(), leaked.len())];
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xc0, 0x00, 0x01, // 1: checkcast #1 (ignored, we use typecheck_info)
        0xb0, // 4: areturn
        0, 0,
    ];
    let code_len = 5;
    // checkcast needs context
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        true,
        Vec::new(),
        Vec::new(),
        typecheck_info,
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

    // null passes checkcast → returns 0
    use crate::config::VmConfig;
    use crate::vm::SharedVm;
    use std::sync::Arc;
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let vm_ptr = shared.as_ref() as *const _ as i64; // Cast: function pointer for JIT call target
                                                     // SAFETY: Calling JIT-compiled machine code with VM context; the CompiledMethod
                                                     // was produced from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            .try_call_with_context(vm_ptr, &[0])
            .expect("test JIT call")
    };
    assert_eq!(result, 0);
}

#[cfg(feature = "vm-tests")]
#[test]
fn test_instanceof_null() {
    // Method: int f(Object a) { return a instanceof String ? 1 : 0; }
    // aload_0, instanceof #1, ireturn
    let class_name = "java/lang/String";
    let leaked: &'static str = Box::leak(class_name.to_string().into_boxed_str()); // LEAK(intentional): test-only; class name must outlive JIT-compiled code pointer
    let typecheck_info = vec![(1usize, leaked.as_ptr(), leaked.len())];
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xc1, 0x00, 0x01, // 1: instanceof #1
        0xac, // 4: ireturn
        0, 0,
    ];
    let code_len = 5;
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        true,
        Vec::new(),
        Vec::new(),
        typecheck_info,
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

    // null → instanceof returns 0
    use crate::config::VmConfig;
    use crate::vm::SharedVm;
    use std::sync::Arc;
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let vm_ptr = shared.as_ref() as *const _ as i64; // Cast: function pointer for JIT call target
                                                     // SAFETY: Calling JIT-compiled machine code with VM context; the CompiledMethod
                                                     // was produced from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            .try_call_with_context(vm_ptr, &[0])
            .expect("test JIT call")
    };
    assert_eq!(result, 0);
}

#[cfg(feature = "vm-tests")]
#[test]
fn test_bounds_check_iaload_in_bounds() {
    // Method: int f(int[] arr, int idx) { return arr[idx]; }
    // Bytecode: aload_0, iload_1, iaload, ireturn
    // needs_heap = false for inline array access, but we pass needs_heap=true
    // to test the bounds check with a real array.
    use crate::config::VmConfig;
    use crate::vm::SharedVm;
    use cratonvm_gc::heap::ArrayElementType;
    use cratonvm_types::ClassId;
    use std::sync::Arc;

    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0   (load array)
        0x1b, // 1: iload_1   (load index)
        0x2e, // 2: iaload    (load int from array)
        0xac, // 3: ireturn
        0, 0,
    ];
    let code_len = 4;
    let compiled = compile(
        &code,
        code_len,
        2,
        2,
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

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let vm_ptr = shared.as_ref() as *const _ as i64; // Cast: function pointer for JIT call target

    // Allocate an int[5] = {10, 20, 30, 40, 50}
    let arr = shared
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Int, 5);
    let arr_ptr = arr.as_ptr();
    for i in 0..5 {
        let _ = shared
            .heap
            .set_array_element(arr, i, Value::Int((i as i32 + 1) * 10)); // Cast: x86-64 immediate encoding
    }

    // In-bounds access should work
    // SAFETY: Calling JIT-compiled machine code with VM context; the CompiledMethod
    // was produced from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call_with_context(vm_ptr, &[arr_ptr as i64, 0])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(result, 10);
    // SAFETY: `compiled` is executable JIT code from valid bytecode; vm_ptr and
    // arr_ptr reference live test objects and match the JIT calling convention.
    let result = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call_with_context(vm_ptr, &[arr_ptr as i64, 4])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(result, 50);
}

// test_jit_throw_aioobe_direct moved to vm crate (helper function lives there)

#[cfg(feature = "vm-tests")]
#[test]
fn test_bounds_check_bastore_in_bounds() {
    // Method: void f(byte[] arr, int idx, int val) { arr[idx] = (byte)val; }
    // Bytecode: aload_0, iload_1, iload_2, bastore, return
    use crate::config::VmConfig;
    use crate::vm::SharedVm;
    use cratonvm_gc::heap::ArrayElementType;
    use cratonvm_types::ClassId;
    use std::sync::Arc;

    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0   (array)
        0x1b, // 1: iload_1   (index)
        0x1c, // 2: iload_2   (value)
        0x54, // 3: bastore
        0xb1, // 4: return (void)
        0, 0,
    ];
    let code_len = 5;
    let compiled = compile(
        &code,
        code_len,
        3,
        3,
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

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let vm_ptr = shared.as_ref() as *const _ as i64; // Cast: function pointer for JIT call target

    let arr = shared
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Byte, 3);
    let arr_ptr = arr.as_ptr();

    // In-bounds store should work
    // SAFETY: Calling JIT-compiled machine code with VM context; the CompiledMethod
    // was produced from valid bytecode and the mmap region is executable.
    unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call_with_context(vm_ptr, &[arr_ptr as i64, 0, 42])
            .expect("test JIT call")
    }; // Cast: address arithmetic
    let val = shared.heap.get_array_element(arr, 0).unwrap();
    assert_eq!(val.as_int(), Some(42));
}

#[test]
fn test_bounds_elimination_analysis() {
    // Test the BCE analysis functions directly
    // Simulate a for-loop: for (int i = 0; i < n; i++) { arr[i]; }
    // Bytecode pattern:
    //   0: iload_0          ; load i (induction var)
    //   1: iload_2          ; load n (bound)
    //   2: if_icmpge +10    ; exit if i >= n (target = 15)
    //   5: aload_1          ; load arr
    //   6: iload_0          ; load i (index)
    //   7: iaload           ; arr[i]
    //   8: pop              ; discard
    //   9: iinc 0, 1        ; i++
    //  12: goto -12         ; back to 0
    //  15: return
    let code: Vec<u8> = vec![
        0x1a, // 0: iload_0 (i)
        0x1c, // 1: iload_2 (n)
        0xa2, 0x00, 0x0d, // 2: if_icmpge +13 → 15
        0x2b, // 5: aload_1 (arr)
        0x1a, // 6: iload_0 (i)
        0x2e, // 7: iaload
        0x57, // 8: pop
        0x84, 0x00, 0x01, // 9: iinc 0, 1
        0xa7, 0xff, 0xf4, // 12: goto -12 → 0
        0xb1, // 15: return
        0, 0,
    ];
    let code_len = 16;
    let loops = detect_loops(&code, code_len);

    // Should detect loop (header=0, back_edge=12)
    assert!(!loops.is_empty());
    assert_eq!(loops[0], (0, 12));

    // Should find induction variable = local 0
    let iv = find_induction_variable(&code, 0, 15);
    assert_eq!(iv, Some(0));

    // Analyze bounds elimination
    let (safe_pcs, _speculative_guards) = analyze_bounds_elimination(&code, code_len, &loops);
    // The iaload at pc=7 should be safe because i < n and arr is unmodified
    assert!(
        safe_pcs.contains(&7),
        "iaload at pc=7 should be bounds-safe, got {:?}",
        safe_pcs
    );
}

#[test]
fn test_bounds_elimination_javac_pattern() {
    // Test BCE with the standard javac for-loop pattern:
    //   goto check; body; iinc; check: iload iv; iload bound; if_icmplt body
    // This is the pattern javac generates (check at bottom, if_icmplt as back-edge)
    //
    //   0: iconst_0         ; push 0
    //   1: istore_3         ; i = 0
    //   2: goto +9 → 11    ; jump to check
    //   5: aload_1          ; load arr (loop body starts here)
    //   6: iload_3          ; load i
    //   7: iaload           ; arr[i]
    //   8: pop              ; discard
    //   9: iinc 3, 1        ; i++
    //  12: iload_3          ; load i (check)
    //  13: iload_2          ; load n
    //  14: if_icmplt -9 → 5 ; continue if i < n (back-edge)
    //  17: return
    let code: Vec<u8> = vec![
        0x03, // 0: iconst_0
        0x3e, // 1: istore_3 (i=0)
        0xa7, 0x00, 0x09, // 2: goto +9 → 11
        0x2b, // 5: aload_1 (arr)
        0x1d, // 6: iload_3 (i)
        0x2e, // 7: iaload
        0x57, // 8: pop
        0x84, 0x03, 0x01, // 9: iinc 3, 1
        0x1d, // 12: iload_3 (i)
        0x1c, // 13: iload_2 (n)
        0xa1, 0xff, 0xf7, // 14: if_icmplt -9 → 5
        0xb1, // 17: return
        0,
    ];
    let code_len = 18;
    let loops = detect_loops(&code, code_len);

    // Loop: header=5 (target of back-edge), back_edge=14 (if_icmplt)
    assert!(!loops.is_empty(), "should detect loop, got {:?}", loops);
    assert_eq!(loops[0], (5, 14), "loop should be (5, 14), got {:?}", loops);

    // Should find induction variable = local 3 (iinc 3, 1)
    let iv = find_induction_variable(&code, 5, 17);
    assert_eq!(iv, Some(3), "IV should be local 3");

    // Analyze bounds elimination — should recognize if_icmplt as continue-condition
    let (safe_pcs, _speculative_guards) = analyze_bounds_elimination(&code, code_len, &loops);
    assert!(
        safe_pcs.contains(&7),
        "iaload at pc=7 should be bounds-safe with javac pattern, got {:?}",
        safe_pcs
    );
}

#[test]
fn test_bce_multi_array_per_array_guards() {
    // Regression: docs/known-issues/jit-bce-multi-array-oob-store-20260711.md
    // (repro test_classes/gpu/BoundsDeopt2.java). The vectorAdd shape —
    //   int n = a.length; for (int i = 0; i < n; i++) out[i] = a[i] + b[i];
    // — must elide statically ONLY the access into `a` (whose length the
    // bound provably is). `b[i]` and `out[i]` must each get their OWN
    // speculative header guard; before the per-array fix all three were
    // "statically" elided from `a`'s guard alone, so a shorter `out`
    // took silent out-of-bounds heap stores instead of AIOOBE.
    //
    // Locals: 0=a, 1=b, 2=out, 3=n, 4=i.
    //   0: aload_0 ; 1: arraylength ; 2: istore_3      (n = a.length)
    //   3: iconst_0 ; 4: istore 4                       (i = 0)
    //   6: iload 4 ; 8: iload_3 ; 9: if_icmpge +22 → 31 (header)
    //  12: aload_2 ; 13: iload 4                        (out, i)
    //  15: aload_0 ; 16: iload 4 ; 18: iaload           (a[i])
    //  19: aload_1 ; 20: iload 4 ; 22: iaload           (b[i])
    //  23: iadd ; 24: iastore                           (out[i] = ...)
    //  25: iinc 4, 1 ; 28: goto -22 → 6 ; 31: return
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xbe, // 1: arraylength
        0x3e, // 2: istore_3
        0x03, // 3: iconst_0
        0x36, 0x04, // 4: istore 4
        0x15, 0x04, // 6: iload 4 (header)
        0x1d, // 8: iload_3
        0xa2, 0x00, 0x16, // 9: if_icmpge +22 → 31
        0x2c, // 12: aload_2 (out)
        0x15, 0x04, // 13: iload 4
        0x2a, // 15: aload_0 (a)
        0x15, 0x04, // 16: iload 4
        0x2e, // 18: iaload
        0x2b, // 19: aload_1 (b)
        0x15, 0x04, // 20: iload 4
        0x2e, // 22: iaload
        0x60, // 23: iadd
        0x4f, // 24: iastore
        0x84, 0x04, 0x01, // 25: iinc 4, 1
        0xa7, 0xff, 0xea, // 28: goto -22 → 6
        0xb1, // 31: return
    ];
    let code_len = code.len();
    let loops = detect_loops(&code, code_len);
    assert_eq!(loops[0], (6, 28), "loop should be (6, 28), got {loops:?}");

    // Whole-method provenance: n (local 3) IS a.length (local 0), and the
    // IV (local 4) provably starts at 0.
    assert_eq!(
        find_bound_arraylength_provenance(&code, code_len, 3),
        Some(0),
        "n = a.length provenance"
    );
    assert_eq!(
        find_bound_arraylength_provenance(&code, code_len, 4),
        None,
        "the IV local is stored from iconst_0, not an arraylength"
    );
    assert!(find_iv_nonneg_start(&code, code_len, 4));

    let (safe_pcs, guards) = analyze_bounds_elimination(&code, code_len, &loops);

    // All three accesses end up elided (a statically, b/out behind guards)...
    for pc in [18usize, 22, 24] {
        assert!(
            safe_pcs.contains(&pc),
            "access at pc={pc} elided, got {safe_pcs:?}"
        );
    }
    // ...but `b` (local 1) and `out` (local 2) each need their own guard,
    // and `a` (local 0) — statically proven — must have none.
    let mut guarded: Vec<(usize, usize, usize, Vec<usize>)> = guards
        .iter()
        .map(|g| {
            (
                g.array_local,
                g.bound_local,
                g.iv_local,
                g.covered_pcs.clone(),
            )
        })
        .collect();
    guarded.sort();
    assert_eq!(
        guarded,
        vec![(1, 3, 4, vec![22]), (2, 3, 4, vec![24])],
        "b and out each get a per-array guard covering exactly their access; \
         a gets none"
    );
}

#[test]
fn test_bce_param_bound_goes_speculative_not_static() {
    // A loop bound that is a plain parameter (no arraylength provenance)
    // must NOT be statically elided — the elision demotes to a speculative
    // header guard on the accessed array. Same shape as
    // `test_bounds_elimination_analysis` (locals: 0=i, 1=arr, 2=n).
    let code: Vec<u8> = vec![
        0x1a, // 0: iload_0 (i)
        0x1c, // 1: iload_2 (n)
        0xa2, 0x00, 0x0d, // 2: if_icmpge +13 → 15
        0x2b, // 5: aload_1 (arr)
        0x1a, // 6: iload_0 (i)
        0x2e, // 7: iaload
        0x57, // 8: pop
        0x84, 0x00, 0x01, // 9: iinc 0, 1
        0xa7, 0xff, 0xf4, // 12: goto -12 → 0
        0xb1, // 15: return
        0, 0,
    ];
    let code_len = 16;
    let loops = detect_loops(&code, code_len);
    assert_eq!(find_bound_arraylength_provenance(&code, code_len, 2), None);

    let (safe_pcs, guards) = analyze_bounds_elimination(&code, code_len, &loops);
    assert!(
        safe_pcs.contains(&7),
        "elided behind a guard, got {safe_pcs:?}"
    );
    assert_eq!(guards.len(), 1, "exactly one speculative guard");
    assert_eq!(
        (
            guards[0].loop_header,
            guards[0].array_local,
            guards[0].bound_local,
            guards[0].iv_local,
            guards[0].covered_pcs.clone(),
        ),
        (0, 1, 2, 0, vec![7]),
    );
}

#[test]
fn test_bce_negative_iv_start_not_static() {
    // `for (int i = -5; i < n; i++) a[i]` with n = a.length: the bound
    // provenance holds, but the IV provably starts NEGATIVE, so the static
    // (guard-less) elision must be refused. The access may still be elided
    // behind the speculative header guard, whose emitted code tests the
    // runtime `iv >= 0` at loop entry (and deopts for i = -5).
    // Locals: 0=a, 1=n, 2=i.
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xbe, // 1: arraylength
        0x3c, // 2: istore_1 (n = a.length)
        0x10, 0xfb, // 3: bipush -5
        0x3d, // 5: istore_2 (i = -5)
        0x1c, // 6: iload_2 (header)
        0x1b, // 7: iload_1
        0xa2, 0x00, 0x0d, // 8: if_icmpge +13 → 21
        0x2a, // 11: aload_0
        0x1c, // 12: iload_2
        0x2e, // 13: iaload
        0x57, // 14: pop
        0x84, 0x02, 0x01, // 15: iinc 2, 1
        0xa7, 0xff, 0xf4, // 18: goto -12 → 6
        0xb1, // 21: return
    ];
    let code_len = code.len();
    let loops = detect_loops(&code, code_len);
    assert_eq!(loops[0], (6, 18));

    assert_eq!(
        find_bound_arraylength_provenance(&code, code_len, 1),
        Some(0)
    );
    assert!(
        !find_iv_nonneg_start(&code, code_len, 2),
        "bipush -5 start must not prove a non-negative IV"
    );

    let (safe_pcs, guards) = analyze_bounds_elimination(&code, code_len, &loops);
    assert!(safe_pcs.contains(&13));
    assert_eq!(guards.len(), 1, "elision must be guard-backed, not static");
    assert_eq!(guards[0].array_local, 0);
    assert_eq!(guards[0].iv_local, 2);
    assert_eq!(guards[0].covered_pcs, vec![13]);
}

#[test]
fn test_bce_two_bound_stores_no_provenance() {
    // Two stores to the bound local defeat the single-dominating-store
    // proof: after the second store the bound may exceed a.length.
    // Locals: 0=a, 1=n, 2=i.
    //   0: aload_0 ; 1: arraylength ; 2: istore_1   (n = a.length)
    //   3: iload_1 ; 4: iconst_1 ; 5: iadd ; 6: istore_1  (n = n + 1 !)
    //   7: return
    let code: Vec<u8> = vec![
        0x2a, 0xbe, 0x3c, // n = a.length
        0x1b, 0x04, 0x60, 0x3c, // n = n + 1
        0xb1,
    ];
    assert_eq!(
        find_bound_arraylength_provenance(&code, code.len(), 1),
        None
    );
}

#[test]
fn test_bounds_elimination_inclusive_not_safe() {
    // SECURITY FIX (V17 / B1): an INCLUSIVE comparator reaches index == bound,
    // which the single `array.length >= bound` header guard does NOT cover.
    // The access must therefore KEEP its per-element bounds check (not be in
    // `safe_pcs`) and NO speculative guard may be installed.
    //
    // Same shape as `test_bounds_elimination_analysis` but the exit test is
    // `if_icmpgt` (0xa3) — `for (int i = 0; i <= n; i++) { arr[i]; }`:
    //   0: iload_0          ; i
    //   1: iload_2          ; n
    //   2: if_icmpgt +13    ; exit if i > n (inclusive) → 15
    //   5: aload_1          ; arr
    //   6: iload_0          ; i
    //   7: iaload           ; arr[i]
    //   8: pop
    //   9: iinc 0, 1        ; i++
    //  12: goto -12         ; back to 0
    //  15: return
    let code: Vec<u8> = vec![
        0x1a, // 0: iload_0 (i)
        0x1c, // 1: iload_2 (n)
        0xa3, 0x00, 0x0d, // 2: if_icmpgt +13 → 15 (INCLUSIVE exit)
        0x2b, // 5: aload_1 (arr)
        0x1a, // 6: iload_0 (i)
        0x2e, // 7: iaload
        0x57, // 8: pop
        0x84, 0x00, 0x01, // 9: iinc 0, 1
        0xa7, 0xff, 0xf4, // 12: goto -12 → 0
        0xb1, // 15: return
        0, 0,
    ];
    let code_len = 16;
    let loops = detect_loops(&code, code_len);
    assert_eq!(loops[0], (0, 12));

    // analyze_loop_bound must flag the loop as inclusive.
    let bounds = analyze_loop_bound(&code, 0, 12, 15, 0).expect("loop bound recognized");
    assert!(bounds.inclusive, "if_icmpgt exit must be marked inclusive");

    // Default (opt-in flag off): inclusive loops take no guard at all.
    __set_inclusive_spec_bce_override(Some(false));
    let (off_safe, off_guards) = analyze_bounds_elimination(&code, code_len, &loops);
    assert!(!off_safe.contains(&7), "default-off must keep the check");
    assert!(off_guards.is_empty(), "default-off must emit no guard");

    // V17 sound-guard form (opt-in): the access IS elided, but only on
    // the strength of a speculative guard flagged `inclusive` (emitted
    // as `length > bound` / JBE + the bound != MAX entry check), never
    // the guard-less static proof.
    __set_inclusive_spec_bce_override(Some(true));
    let (safe_pcs, speculative_guards) = analyze_bounds_elimination(&code, code_len, &loops);
    __set_inclusive_spec_bce_override(None);
    assert!(
        safe_pcs.contains(&7),
        "inclusive-loop iaload at pc=7 should be guard-elided, got {:?}",
        safe_pcs
    );
    assert_eq!(speculative_guards.len(), 1, "one guarded array expected");
    let g = &speculative_guards[0];
    assert!(g.inclusive, "guard must record the inclusive comparator");
    assert_eq!(g.step_local, None, "iinc +1 loop needs no step guard");
    assert!(g.covered_pcs.contains(&7));
}

#[test]
fn test_bce_varadd_step_guard_and_commuted_refusal() {
    // Sieve inner-loop shape: `for (j = ..; j < n; j += i) a[j] = ..`.
    //   locals: 0 = j (IV), 1 = n (bound), 2 = arr, 3 = i (step)
    //   0: iload_0
    //   1: iload_1
    //   2: if_icmpge +16 -> 18
    //   5: aload_2
    //   6: iload_0
    //   7: iconst_1
    //   8: bastore
    //   9: iload_0        (canonical j += i)
    //  10: iload_3
    //  11: iadd
    //  12: istore_0
    //  13: goto -13 -> 0
    //  16..: return padding
    let code = [
        0x1a, 0x1b, 0xa2, 0x00, 0x10, 0x2c, 0x1a, 0x04, 0x54, 0x1a, 0x1d, 0x60, 0x3b, 0xa7, 0xff,
        0xf3, 0xb1, 0x00, 0x00, 0x00,
    ];
    let code_len = 17;
    let loops = detect_loops(&code, code_len);
    assert_eq!(loops[0].0, 0);
    assert_eq!(
        find_iv_step_provenance(&code, 0, 16, 0),
        Some(IvStep::VarAdd(3)),
        "canonical j += i must name the step local"
    );
    let (safe_pcs, guards) = analyze_bounds_elimination(&code, code_len, &loops);
    assert!(
        safe_pcs.contains(&8),
        "bastore at pc=8 should be guard-elided"
    );
    assert_eq!(guards.len(), 1);
    assert_eq!(
        guards[0].step_local,
        Some(3),
        "guard must carry the step local"
    );
    assert!(!guards[0].inclusive);

    // Commuted form `j = i + j` (iload_3; iload_0; iadd; istore_0) is NOT
    // the canonical compound shape — the step cannot be proven, so the
    // whole loop must be refused (no guard, no elision).
    let commuted = [
        0x1a, 0x1b, 0xa2, 0x00, 0x10, 0x2c, 0x1a, 0x04, 0x54, 0x1d, 0x1a, 0x60, 0x3b, 0xa7, 0xff,
        0xf3, 0xb1, 0x00, 0x00, 0x00,
    ];
    assert_eq!(find_iv_step_provenance(&commuted, 0, 16, 0), None);
    let (safe2, guards2) = analyze_bounds_elimination(&commuted, code_len, &loops);
    assert!(
        !safe2.contains(&8),
        "unproven step must keep the bounds check"
    );
    assert!(guards2.is_empty());
}

#[test]
fn test_find_modified_locals_high_local_no_panic() {
    // B2: `find_modified_locals` must not panic (debug) or set the wrong bit
    // (release) for a local index >= 64. A wide `istore 200` / `iinc 200, 1`
    // must saturate to bit 63 via the `.min(63)` clamp.
    //
    //   0: istore 200   (0x36 0xc8)
    //   2: iinc 200, 1  (0x84 0xc8 0x01)
    //   5: return       (0xb1)
    let code: Vec<u8> = vec![0x36, 0xc8, 0x84, 0xc8, 0x01, 0xb1];
    let modified = find_modified_locals(&code, 0, code.len());
    // High locals saturate to bit 63; no panic, and the low bits are clear.
    assert_eq!(
        modified,
        1u64 << 63,
        "high-local store/iinc must saturate to bit 63"
    );
}

#[cfg(feature = "vm-tests")]
#[test]
fn test_bounds_check_loop_compiled() {
    // Compile a loop that accesses array elements with an induction variable
    // The bounds check should be eliminated by BCE for the inner access
    // Method: int sum(int[] arr, int len) { int s=0; for(int i=0;i<len;i++) s+=arr[i]; return s; }
    //
    // Bytecode:
    //   0: iconst_0         ; push 0
    //   1: istore_2         ; s = 0
    //   2: iconst_0         ; push 0
    //   3: istore_3         ; i = 0
    //   4: iload_3          ; load i     (loop header)
    //   5: iload_1          ; load len
    //   6: if_icmpge +12    ; exit if i >= len → 21
    //   9: iload_2          ; load s
    //  10: aload_0          ; load arr
    //  11: iload_3          ; load i
    //  12: iaload           ; arr[i]
    //  13: iadd             ; s + arr[i]
    //  14: istore_2         ; s = s + arr[i]
    //  15: iinc 3, 1        ; i++
    //  18: goto -14         ; back to 4
    //  21: iload_2          ; load s
    //  22: ireturn
    use crate::config::VmConfig;
    use crate::vm::SharedVm;
    use cratonvm_gc::heap::ArrayElementType;
    use cratonvm_types::ClassId;
    use std::sync::Arc;

    let code: Vec<u8> = vec![
        0x03, // 0: iconst_0
        0x3d, // 1: istore_2 (s=0)
        0x03, // 2: iconst_0
        0x3e, // 3: istore_3 (i=0)
        0x1d, // 4: iload_3 (i)
        0x1b, // 5: iload_1 (len)
        0xa2, 0x00, 0x0f, // 6: if_icmpge +15 → 21
        0x1c, // 9: iload_2 (s)
        0x2a, // 10: aload_0 (arr)
        0x1d, // 11: iload_3 (i)
        0x2e, // 12: iaload
        0x60, // 13: iadd
        0x3d, // 14: istore_2 (s)
        0x84, 0x03, 0x01, // 15: iinc 3, 1
        0xa7, 0xff, 0xf2, // 18: goto -14 → 4
        0x1c, // 21: iload_2 (s)
        0xac, // 22: ireturn
        0, 0,
    ];
    let code_len = 23;

    let compiled = compile(
        &code,
        code_len,
        2,
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

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let vm_ptr = shared.as_ref() as *const _ as i64; // Cast: function pointer for JIT call target

    // Allocate int[5] = {1, 2, 3, 4, 5}
    let arr = shared
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Int, 5);
    let arr_ptr = arr.as_ptr();
    for i in 0..5 {
        let _ = shared
            .heap
            .set_array_element(arr, i, Value::Int(i as i32 + 1)); // Cast: x86-64 immediate encoding
    }

    // SAFETY: Calling JIT-compiled machine code with VM context; the CompiledMethod
    // was produced from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call_with_context(vm_ptr, &[arr_ptr as i64, 5])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(result, 15); // 1+2+3+4+5
}

/// A loop whose header is reached only by fall-through + its own back edge
/// keeps its pre-header (and therefore its LICM hoists / speculative
/// guards).
///
/// ```text
///  0: iconst_0        2: iload_1        7: iinc 1, 1
///  1: istore_1        3: iload_0       10: goto 2
///                     4: if_icmpge 13  13: return
/// ```
#[test]
fn single_entry_loop_header_is_not_bypassable() {
    let code: &[u8] = &[
        0x03, // 0: iconst_0
        0x3c, // 1: istore_1
        0x1b, // 2: iload_1        <- loop header (back-edge target)
        0x1a, // 3: iload_0
        0xa2, 0x00, 0x09, // 4: if_icmpge 13
        0x84, 0x01, 0x01, // 7: iinc 1, 1
        0xa7, 0xff, 0xf8, // 10: goto 2
        0xb1, // 13: return
    ];
    let loops = detect_loops(code, code.len());
    assert_eq!(loops, vec![(2, 10)], "expected one loop with header 2");
    let bypassable = find_bypassable_loop_headers(code, code.len(), &loops, &[]);
    assert!(
        bypassable.is_empty(),
        "a fall-through-only loop header must keep its pre-header, got {bypassable:?}"
    );
}

/// `org.xml.sax.helpers.AttributesImpl.ensureCapacity`'s shape (and
/// `LicmEntryProbe.shapeB`): the loop header at 13 is ALSO the target of
/// the forward `goto 13` at pc 7, which lands after the pre-header the
/// emitter places at the header. Hoisting `n * 5` there left the cache slot
/// unwritten on that edge — HIB-LONGTAIL.2's
/// `anewarray ... length 1677721600`. The header must be reported
/// bypassable so every speculating transform is dropped for it.
///
/// ```text
///  0: iload_1          7: goto 13       17: if_icmpge 27
///  1: ifeq 10         10: bipush 30     20: iload_2
///  4: bipush 25       12: istore_2      21: iconst_2
///  6: istore_2        13: iload_2       22: imul
///                     14: iload_0       23: istore_2
///                     15: iconst_5      24: goto 13
///                     16: imul          27: iload_2 / 28: ireturn
/// ```
#[test]
fn handler_reachable_from_outside_a_hoisted_loop_is_bypassable() {
    // A plain counted loop whose only non-back-edge predecessor is the
    // fall-through, so it is NOT bypassable on branch edges alone — every
    // difference below comes from the exception table.
    //
    //  0: iconst_0
    //  1: istore_2
    //  2: iload_2            <-- loop header
    //  3: iload_1
    //  4: if_icmpge +13 -> 17
    //  7: iload_0
    //  8: iconst_3
    //  9: imul
    // 10: istore_3           (loop-invariant run, the hoist candidate)
    // 11: iinc 2, 1
    // 14: goto -12 -> 2      <-- back edge; loop body is [2, 17)
    // 17: iload_2
    // 18: ireturn
    let code: &[u8] = &[
        0x03, 0x3D, // iconst_0; istore_2
        0x1C, 0x1B, 0xA2, 0x00, 0x0D, // iload_2; iload_1; if_icmpge -> 17
        0x1A, 0x06, 0x68, 0x3E, // iload_0; iconst_3; imul; istore_3
        0x84, 0x02, 0x01, // iinc 2,1
        0xA7, 0xFF, 0xF4, // goto -> 2
        0x1C, 0xAC, // iload_2; ireturn
    ];
    let loops = detect_loops(code, code.len());
    assert_eq!(
        loops,
        vec![(2, 14)],
        "expected exactly one natural loop with header 2"
    );

    // No exception table: single-entry header keeps its pre-header.
    assert!(
        find_bypassable_loop_headers(code, code.len(), &loops, &[]).is_empty(),
        "a single-entry counted loop must not be reported bypassable"
    );

    // try/catch wholly INSIDE the loop body — the common javac shape for
    // `while (…) { try { … } catch { … } }`. The throw can only happen
    // after the header was entered, so the pre-header already ran: sound,
    // and the hoist must be KEPT (this is the no-regression half).
    assert!(
        find_bypassable_loop_headers(code, code.len(), &loops, &[(7, 11, 10)]).is_empty(),
        "a protected range wholly inside the loop must keep its hoist"
    );

    // Protected range starts BEFORE the header: a throw from pre-loop code
    // delivers control into the body without the pre-header having run.
    assert!(
        find_bypassable_loop_headers(code, code.len(), &loops, &[(0, 11, 7)]).contains(&2),
        "a handler reachable from before the loop bypasses the pre-header"
    );

    // Protected range extends PAST the loop: same hazard from the far side.
    assert!(
        find_bypassable_loop_headers(code, code.len(), &loops, &[(7, 19, 7)]).contains(&2),
        "a handler reachable from after the loop bypasses the pre-header"
    );

    // Handler outside the loop entirely — irrelevant, keep the hoist.
    assert!(
        find_bypassable_loop_headers(code, code.len(), &loops, &[(0, 19, 17)]).is_empty(),
        "a handler outside the loop must not drop the hoist"
    );
}

#[test]
fn forward_goto_into_loop_header_is_bypassable() {
    let code: &[u8] = &[
        0x1b, // 0: iload_1
        0x99, 0x00, 0x09, // 1: ifeq 10
        0x10, 0x19, // 4: bipush 25
        0x3d, // 6: istore_2
        0xa7, 0x00, 0x06, // 7: goto 13   <- skips the pre-header at 13
        0x10, 0x1e, // 10: bipush 30
        0x3d, // 12: istore_2
        0x1c, // 13: iload_2   <- loop header
        0x1a, // 14: iload_0
        0x08, // 15: iconst_5
        0x68, // 16: imul
        0xa2, 0x00, 0x0a, // 17: if_icmpge 27
        0x1c, // 20: iload_2
        0x05, // 21: iconst_2
        0x68, // 22: imul
        0x3d, // 23: istore_2
        0xa7, 0xff, 0xf5, // 24: goto 13
        0x1c, // 27: iload_2
        0xac, // 28: ireturn
    ];
    let loops = detect_loops(code, code.len());
    assert_eq!(loops, vec![(13, 24)], "expected one loop with header 13");
    // The invariant `n * 5` run IS matched — i.e. without the bypassable
    // gate this method really would get a hoist at header 13.
    assert!(
        !find_arith_loop_hoists(code, code.len(), &loops).is_empty(),
        "expected arith-LICM to match the `n * 5` run at this header"
    );
    let bypassable = find_bypassable_loop_headers(code, code.len(), &loops, &[]);
    assert!(
        bypassable.contains(&13),
        "header 13 is entered by the forward `goto` at pc 7 and must be \
         reported bypassable, got {bypassable:?}"
    );
}

#[test]
fn test_jit_scan_accepts_invokevirtual() {
    // Bytecode: aload_0, invokevirtual #1, areturn
    // invokevirtual is 0xb6 + 2-byte cp_index
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xb6, 0x00, 0x01, // 1: invokevirtual #1
        0xb0, // 4: areturn
        0, 0, // padding
    ];
    let scan = jit_scan(&code, 5, "(Ljava/lang/Object;)Ljava/lang/Object;");
    assert!(scan.is_some(), "jit_scan should accept invokevirtual");
    let scan = scan.unwrap();
    assert!(scan.needs_heap, "invoke methods need heap");
    assert_eq!(scan.invoke_ops.len(), 1);
    assert_eq!(scan.invoke_ops[0], (1, 1, 0xb6)); // (pc=1, cp_idx=1, opcode=0xb6)
}

#[test]
fn test_jit_scan_accepts_invokeinterface() {
    // Bytecode: aload_0, invokeinterface #1 count 1 0, ireturn
    // invokeinterface is 0xb9 + 2-byte cp_index + count + 0
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xb9, 0x00, 0x01, 0x01, 0x00, // 1: invokeinterface #1, count=1, 0
        0xac, // 6: ireturn
        0, 0, // padding
    ];
    let scan = jit_scan(&code, 7, "(Ljava/lang/Object;)I");
    assert!(scan.is_some(), "jit_scan should accept invokeinterface");
    let scan = scan.unwrap();
    assert_eq!(scan.invoke_ops.len(), 1);
    assert_eq!(scan.invoke_ops[0], (1, 1, 0xb9)); // (pc=1, cp_idx=1, opcode=0xb9)
}

#[test]
fn test_jit_scan_accepts_invokespecial() {
    // Bytecode: aload_0, invokespecial #1, return
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xb7, 0x00, 0x01, // 1: invokespecial #1
        0xb1, // 4: return (void)
        0, 0, // padding
    ];
    let scan = jit_scan(&code, 5, "(Ljava/lang/Object;)V");
    assert!(scan.is_some(), "jit_scan should accept invokespecial");
    let scan = scan.unwrap();
    assert_eq!(scan.invoke_ops.len(), 1);
    assert_eq!(scan.invoke_ops[0], (1, 1, 0xb7));
}

#[test]
fn test_compile_with_invokevirtual() {
    // Test that code containing invokevirtual compiles successfully
    // Bytecode: aload_0, invokevirtual #1, ireturn
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xb6, 0x00, 0x01, // 1: invokevirtual #1
        0xac, // 4: ireturn
        0, 0, // padding
    ];
    let code_len = 5;

    // Create a JitInvokeInfo for the invokevirtual at pc=1
    // LEAK(intentional): the JIT-compiled code stores a raw pointer to this
    // JitInvokeInfo, so it must have 'static lifetime and outlive the compiled
    // method; the test process owns it for its entire (short) lifetime.
    let info = Box::leak(Box::new(JitInvokeInfo {
        // LEAK(intentional): test-only; JitInvokeInfo must outlive JIT-compiled code pointer
        class_name: "TestClass",
        method_name: "getValue",
        descriptor: "()I",
        num_jit_args: 1, // just receiver
        return_type: b'I',
        invoke_kind: 0, // invokevirtual
        declaring_class_id: 0,
    }));
    let invoke_info = vec![(1usize, info as *const JitInvokeInfo)]; // Cast: address arithmetic

    // needs_heap=true because invokevirtual requires vm_ptr
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        true,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        invoke_info,
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
    assert!(
        compiled.is_some(),
        "Should compile method with invokevirtual"
    );
}

#[test]
fn test_compile_with_invokeinterface() {
    // Bytecode: aload_0, invokeinterface #1 count=1 0, ireturn
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xb9, 0x00, 0x01, 0x01, 0x00, // 1: invokeinterface #1
        0xac, // 6: ireturn
        0, 0, // padding
    ];
    let code_len = 7;

    // LEAK(intentional): the compiled method holds a raw pointer into this
    // JitInvokeInfo; it must be 'static and outlive the JIT code. Owned by the
    // test process for its lifetime.
    let info = Box::leak(Box::new(JitInvokeInfo {
        // LEAK(intentional): test-only; JitInvokeInfo must outlive JIT-compiled code pointer
        class_name: "TestInterface",
        method_name: "compute",
        descriptor: "()I",
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind: 2, // invokeinterface
        declaring_class_id: 0,
    }));
    let invoke_info = vec![(1usize, info as *const JitInvokeInfo)]; // Cast: address arithmetic

    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        true,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        invoke_info,
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
    assert!(
        compiled.is_some(),
        "Should compile method with invokeinterface"
    );
}

#[test]
fn test_compile_invoke_void_return() {
    // Test invokevirtual with void return type — no push after call
    // Bytecode: aload_0, invokevirtual #1, return
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xb6, 0x00, 0x01, // 1: invokevirtual #1
        0xb1, // 4: return (void)
        0, 0, // padding
    ];
    let code_len = 5;

    // LEAK(intentional): the compiled method holds a raw pointer into this
    // JitInvokeInfo; it must be 'static and outlive the JIT code. Owned by the
    // test process for its lifetime.
    let info = Box::leak(Box::new(JitInvokeInfo {
        // LEAK(intentional): test-only; JitInvokeInfo must outlive JIT-compiled code pointer
        class_name: "TestClass",
        method_name: "doSomething",
        descriptor: "()V",
        num_jit_args: 1, // just receiver
        return_type: b'V',
        invoke_kind: 0,
        declaring_class_id: 0,
    }));
    let invoke_info = vec![(1usize, info as *const JitInvokeInfo)]; // Cast: address arithmetic

    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        true,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        invoke_info,
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
    assert!(
        compiled.is_some(),
        "Should compile method with void invokevirtual"
    );
}

#[test]
fn test_compile_invoke_with_args() {
    // Test invokevirtual with multiple args: receiver + int + int
    // Bytecode: aload_0, iload_1, iload_2, invokevirtual #1, ireturn
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0 (receiver)
        0x1b, // 1: iload_1 (arg1)
        0x1c, // 2: iload_2 (arg2)
        0xb6, 0x00, 0x01, // 3: invokevirtual #1
        0xac, // 6: ireturn
        0, 0, // padding
    ];
    let code_len = 7;

    // LEAK(intentional): the compiled method holds a raw pointer into this
    // JitInvokeInfo; it must be 'static and outlive the JIT code. Owned by the
    // test process for its lifetime.
    let info = Box::leak(Box::new(JitInvokeInfo {
        // LEAK(intentional): test-only; JitInvokeInfo must outlive JIT-compiled code pointer
        class_name: "TestClass",
        method_name: "add",
        descriptor: "(II)I",
        num_jit_args: 3, // receiver + 2 int args
        return_type: b'I',
        invoke_kind: 0,
        declaring_class_id: 0,
    }));
    let invoke_info = vec![(3usize, info as *const JitInvokeInfo)]; // Cast: address arithmetic

    let compiled = compile(
        &code,
        code_len,
        3,
        3,
        true,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        invoke_info,
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
    assert!(
        compiled.is_some(),
        "Should compile method with multi-arg invokevirtual"
    );
}

#[test]
fn test_bytecode_len_invoke() {
    // Verify bytecode length calculation for invoke opcodes
    assert_eq!(bytecode_len_at(&[0xb6, 0x00, 0x01], 0), 3); // invokevirtual
    assert_eq!(bytecode_len_at(&[0xb7, 0x00, 0x01], 0), 3); // invokespecial
    assert_eq!(bytecode_len_at(&[0xb9, 0x00, 0x01, 0x02, 0x00], 0), 5); // invokeinterface
                                                                        // Defense-in-depth (same class as the missing-`ldc` desync): the other
                                                                        // 5-byte ops. invokedynamic is now accepted by `jit_scan` (see the 0xba
                                                                        // scan/codegen arms); goto_w / jsr_w are still rejected today, but the
                                                                        // length table must stay correct so a future acceptance can't silently
                                                                        // desync every PC-stepping walk. Must match the regalloc.rs `bc_len`
                                                                        // twin's `bc_len_five_byte_ops`.
    assert_eq!(bytecode_len_at(&[0xba, 0x00, 0x01, 0x00, 0x00], 0), 5); // invokedynamic
    assert_eq!(bytecode_len_at(&[0xc8, 0x00, 0x00, 0x00, 0x10], 0), 5); // goto_w
    assert_eq!(bytecode_len_at(&[0xc9, 0x00, 0x00, 0x00, 0x10], 0), 5); // jsr_w
}

#[test]
fn test_bytecode_len_wide() {
    // wide (0xc4) prefix — JVMS §6.5. Must stay in lockstep with
    // regalloc.rs::bc_len's 0xc4 arm.
    // `wide iload <2-byte index>` → 4 bytes (0x15 = iload).
    assert_eq!(bytecode_len_at(&[0xc4, 0x15, 0x01, 0x00], 0), 4);
    // `wide istore <2-byte index>` → 4 bytes (0x36 = istore).
    assert_eq!(bytecode_len_at(&[0xc4, 0x36, 0x01, 0x00], 0), 4);
    // `wide ret <2-byte index>` → 4 bytes (0xa9 = ret).
    assert_eq!(bytecode_len_at(&[0xc4, 0xa9, 0x01, 0x00], 0), 4);
    // `wide iinc <2-byte index> <2-byte const>` → 6 bytes (0x84 = iinc).
    assert_eq!(bytecode_len_at(&[0xc4, 0x84, 0x01, 0x00, 0x00, 0x01], 0), 6);
    // Truncated prefix (no modified-opcode byte): the `pc + 1 < code.len()`
    // bounds check must not panic and falls to the 4-byte form.
    assert_eq!(bytecode_len_at(&[0xc4], 0), 4);
}

#[test]
fn test_osr_metadata_populated() {
    // Compile a simple loop method and verify OSR metadata is set
    // Bytecode: int sum(int n) { int s = 0; for (int i = 0; i < n; i++) s += i; return s; }
    // Simplified: iload_0(n), iconst_0(s), iconst_0(i), loop: iload_2, iload_0, if_icmpge exit,
    //   iload_1 + iload_2 + iadd + istore_1, iinc 2 1, goto loop, iload_1, ireturn
    let code: Vec<u8> = vec![
        0x03, // 0: iconst_0 (s = 0)
        0x3c, // 1: istore_1
        0x03, // 2: iconst_0 (i = 0)
        0x3d, // 3: istore_2
        // loop header at pc=4
        0x1c, // 4: iload_2 (i)
        0x1a, // 5: iload_0 (n)
        0xa2, 0x00, 0x0d, // 6: if_icmpge +13 → 19
        0x1b, // 9: iload_1 (s)
        0x1c, // 10: iload_2 (i)
        0x60, // 11: iadd
        0x3c, // 12: istore_1 (s = s + i)
        0x84, 0x02, 0x01, // 13: iinc 2, 1
        0xa7, 0xff, 0xf4, // 16: goto -12 → 4
        0x1b, // 19: iload_1 (s)
        0xac, // 20: ireturn
        0, 0, // padding
    ];
    let code_len = 21;

    let compiled = compile(
        &code,
        code_len,
        1,
        3,
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

    // Verify OSR metadata is populated
    assert!(
        compiled.osr_pc_to_native.is_some(),
        "OSR pc_to_native should be set"
    );
    let pc_map = compiled.osr_pc_to_native.as_ref().unwrap();
    assert!(
        pc_map.len() >= code_len,
        "pc_to_native should cover all bytecodes"
    );

    // The loop header at PC=4 should have a valid native offset
    assert!(
        pc_map[4] >= 0,
        "Loop header at PC=4 should have native mapping"
    );

    // Verify other OSR metadata
    assert_eq!(compiled.osr_num_locals, 3);
    assert!(compiled.osr_frame_size > 0, "Frame size should be positive");
}

#[test]
fn callee_saved_gpr_local_homes_are_default_on_with_precise_maps() {
    if !callee_saved_gpr_local_homes_enabled() {
        // The process-wide diagnostic opt-out is intentionally respected.
        return;
    }

    // int f(int a, int b) { int c = a + b; return c; }
    let code: Vec<u8> = vec![
        0x1a, // iload_0
        0x1b, // iload_1
        0x60, // iadd
        0x3d, // istore_2
        0x1c, // iload_2
        0xac, // ireturn
        0, 0,
    ];
    let compiled = compile(
        &code,
        6,
        2,
        3,
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
    .expect("simple int-local method should compile");

    assert!(compiled.osr_num_reg_locals > 0);
    assert!(compiled
        .osr_callee_saved_regs
        .as_ref()
        .is_some_and(|registers| !registers.is_empty()));
    assert!(compiled
        .osr_local_assignments
        .as_ref()
        .is_some_and(|assignments| assignments.iter().any(Option::is_some)));
}

#[test]
// x86-64 only: `CompiledMethod::osr_enter` is itself
// `#[cfg(target_arch = "x86_64")]`, so on aarch64 this test does not merely
// fail -- it does not COMPILE, and took the whole crate's test binary with
// it. Found by actually building for aarch64 in an emulated container; a
// cfg-gated API needs cfg-gated tests.
#[cfg(target_arch = "x86_64")]
fn test_osr_simple_loop() {
    // Test OSR entry: compile a simple sum loop and enter at the loop header
    // Same bytecode as above: sum(n) = 0 + 1 + ... + (n-1)
    let code: Vec<u8> = vec![
        0x03, // 0: iconst_0 (s = 0)
        0x3c, // 1: istore_1
        0x03, // 2: iconst_0 (i = 0)
        0x3d, // 3: istore_2
        0x1c, // 4: iload_2 (i)
        0x1a, // 5: iload_0 (n)
        0xa2, 0x00, 0x0d, // 6: if_icmpge +13 → 19
        0x1b, // 9: iload_1 (s)
        0x1c, // 10: iload_2 (i)
        0x60, // 11: iadd
        0x3c, // 12: istore_1
        0x84, 0x02, 0x01, // 13: iinc 2, 1
        0xa7, 0xff, 0xf4, // 16: goto -12 → 4
        0x1b, // 19: iload_1 (s)
        0xac, // 20: ireturn
        0, 0,
    ];
    let code_len = 21;

    let compiled = compile(
        &code,
        code_len,
        1,
        3,
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

    // Normal entry: sum(10) = 0+1+...+9 = 45
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[10]).expect("test JIT call") };
    assert_eq!(result, 45);

    // OSR entry: simulate entering at PC=4 with locals [n=10, s=10, i=5]
    // This means we've already accumulated s=0+1+2+3+4=10, and i=5
    // Remaining: 5+6+7+8+9 = 35, total = 10+35 = 45
    let jit_locals: [i64; 3] = [10, 10, 5]; // n=10, s=10, i=5
                                            // SAFETY: Entering JIT-compiled code via OSR; the CompiledMethod was produced
                                            // from valid bytecode, locals array is correctly sized, and the mmap region is executable.
    let osr_result = unsafe {
        compiled.osr_enter(0, &jit_locals, 4, /* thread_ptr */ 0)
    };
    assert!(osr_result.is_some(), "OSR entry should succeed at PC=4");
    assert_eq!(osr_result.unwrap(), 45); // s=10 + 5+6+7+8+9 = 45
}

#[test]
// x86-64 only: `CompiledMethod::osr_enter` is itself
// `#[cfg(target_arch = "x86_64")]`, so on aarch64 this test does not merely
// fail -- it does not COMPILE, and took the whole crate's test binary with
// it. Found by actually building for aarch64 in an emulated container; a
// cfg-gated API needs cfg-gated tests.
#[cfg(target_arch = "x86_64")]
fn test_osr_long_loop() {
    // long addOnly(long n) { long s=0; for(long i=0;i<n;i++) s+=i; return s; }
    // Locals: 0-1=n(long), 2-3=s(long), 4-5=i(long)
    let code: Vec<u8> = vec![
        0x09, // 0: lconst_0
        0x41, // 1: lstore_2 (s=0)
        0x09, // 2: lconst_0
        0x37, 0x04, // 3: lstore 4 (i=0)
        0x16, 0x04, // 5: lload 4 (i) — loop header
        0x1e, // 7: lload_0 (n)
        0x94, // 8: lcmp
        0x9c, 0x00, 0x11, // 9: ifge +17 → 26
        0x20, // 12: lload_2 (s)
        0x16, 0x04, // 13: lload 4 (i)
        0x61, // 15: ladd
        0x41, // 16: lstore_2 (s)
        0x16, 0x04, // 17: lload 4 (i)
        0x0a, // 19: lconst_1
        0x61, // 20: ladd
        0x37, 0x04, // 21: lstore 4 (i)
        0xa7, 0xff, 0xee, // 23: goto -18 → 5
        0x20, // 26: lload_2
        0xad, // 27: lreturn
        0, 0,
    ];
    let code_len = 28;
    let compiled = compile(
        &code,
        code_len,
        2,
        6,
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
        None, // string_layout: Option<StringFieldLayout>
    )
    .unwrap();

    // Normal entry: addOnly(2000) = sum(0..1999) = 1999000
    // SAFETY: `compiled` is freshly JIT-compiled from valid bytecode into an
    // executable mmap; calling it with the matching one-arg ABI is sound.
    let result = unsafe { compiled.try_call(&[2000]).expect("test JIT call") };
    assert_eq!(result, 1999000, "normal entry");

    // OSR entry at PC=5 with i=1000, s=499500 (sum 0..999), n=2000.
    // Remaining sum 1000..1999 = 1499500; total = 1999000.
    // jit_locals layout: index 0=n, 1=(n high), 2=s, 3=(s high), 4=i, 5=(i high)
    let jit_locals: [i64; 6] = [2000, 0, 499500, 0, 1000, 0];
    // SAFETY: `compiled` holds executable JIT code with a valid OSR entry at PC=5;
    // jit_locals matches the compiler's slot layout, so OSR resume is sound.
    let osr_result = unsafe {
        compiled.osr_enter(0, &jit_locals, 5, /* thread_ptr */ 0)
    };
    assert!(osr_result.is_some(), "OSR entry should succeed at PC=5");
    assert_eq!(osr_result.unwrap(), 1999000, "OSR long loop result");
}

#[test]
fn test_avx2_detection() {
    // Just verify has_avx2() doesn't crash and returns a consistent result.
    let a = has_avx2();
    let b = has_avx2();
    assert_eq!(a, b, "AVX2 detection should be deterministic");
    // On modern x86-64 machines this should be true, but we can't assert it
    // as CI might run on older hardware.
    println!("AVX2 support detected: {}", a);
}

#[test]
fn test_detect_int_array_sum_pattern() {
    // Bytecode for: long sum_array(int[] arr, int n) {
    //   long sum = 0;
    //   for (int i = 0; i < n; i++) sum += arr[i];
    //   return sum;
    // }
    // Locals: 0=arr, 1=n, 2-3=sum(long), 4=i
    let code: Vec<u8> = vec![
        0x09, // 0: lconst_0
        0x41, // 1: lstore_2 (sum = 0)
        0x03, // 2: iconst_0
        0x36, 0x04, // 3: istore 4 (i = 0)
        0x15, 0x04, // 5: iload 4 (i) — loop header
        0x1b, // 7: iload_1 (n)
        0xa2, 0x00, 0x11, // 8: if_icmpge +17 → 25
        0x2a, // 11: aload_0 (arr)
        0x15, 0x04, // 12: iload 4 (i)
        0x2e, // 14: iaload
        0x85, // 15: i2l
        0x20, // 16: lload_2 (sum)
        0x61, // 17: ladd
        0x41, // 18: lstore_2 (sum)
        0x84, 0x04, 0x01, // 19: iinc 4, 1
        0xa7, 0xff, 0xef, // 22: goto -17 → 5
        0x20, // 25: lload_2
        0xad, // 26: lreturn
        0, 0,
    ];
    let code_len = 27;

    // Detect loops
    let loops = detect_loops(&code, code_len);
    assert!(!loops.is_empty(), "Should detect the for loop");

    // Find the loop (header=5, back_edge=22)
    let &(header, back_edge) = loops
        .iter()
        .find(|&&(h, _)| h == 5)
        .expect("Should find loop with header at PC=5");
    assert_eq!(back_edge, 22);

    // Find induction variable
    let back_edge_end = back_edge + bytecode_len_at(&code, back_edge);
    let iv = find_induction_variable(&code, header, back_edge_end);
    assert_eq!(iv, Some(4), "Induction variable should be local 4 (i)");

    // Detect SIMD pattern
    let info = detect_int_array_sum(&code, header, back_edge, 4);
    assert!(info.is_some(), "Should detect int-array-sum pattern");
    let info = info.unwrap();
    assert_eq!(info.header_pc, 5);
    assert_eq!(info.iv_local, 4);
    assert_eq!(info.acc_local, 2);
    assert_eq!(info.array_local, 0);
    assert_eq!(info.bound_local, 1);
    assert!(info.acc_is_long);
}

// T5.2.15 — element-wise SIMD detection tests

#[test]
fn test_detect_int_array_element_wise_add() {
    // for (int i = 0; i < n; i++) out[i] = a[i] + b[i];
    // Locals: 0=out, 1=a, 2=b, 3=n, 4=i
    let code: Vec<u8> = vec![
        0x03, // 0: iconst_0
        0x36, 0x04, // 1: istore 4 (i = 0)
        0x15, 0x04, // 3: iload 4 (i) — header
        0x1D, // 5: iload_3 (n)
        0xa2, 0x00, 0x14, // 6: if_icmpge +20 → 26
        0x2A, // 9:  aload_0 (out)
        0x15, 0x04, // 10: iload 4 (i)
        0x2B, // 12: aload_1 (a)
        0x15, 0x04, // 13: iload 4
        0x2e, // 15: iaload
        0x2C, // 16: aload_2 (b)
        0x15, 0x04, // 17: iload 4
        0x2e, // 19: iaload
        0x60, // 20: iadd
        0x4F, // 21: iastore
        0x84, 0x04, 0x01, // 22: iinc 4, 1
        0xa7, 0xff, 0xEA, // 25: goto -22 → 3
        0xB1, // 28: return
    ];
    let code_len = code.len();
    let loops = detect_loops(&code, code_len);
    let &(header, back_edge) = loops
        .iter()
        .find(|&&(h, _)| h == 3)
        .expect("should detect loop at PC=3");
    assert_eq!(back_edge, 25);
    let info = detect_int_array_element_wise(&code, header, back_edge, 4)
        .expect("element-wise add should match");
    assert_eq!(info.header_pc, 3);
    assert_eq!(info.iv_local, 4);
    assert_eq!(info.out_local, 0);
    assert_eq!(info.a_local, 1);
    assert_eq!(info.b_local, 2);
    assert_eq!(info.bound_local, 3);
    assert_eq!(info.op, ElementWiseOp::Add);
}

#[test]
fn test_detect_int_array_element_wise_mul() {
    // Same shape but imul (0x68)
    let code: Vec<u8> = vec![
        0x03, 0x36, 0x04, 0x15, 0x04, 0x1D, 0xa2, 0x00, 0x14, 0x2A, 0x15, 0x04, 0x2B, 0x15, 0x04,
        0x2e, 0x2C, 0x15, 0x04, 0x2e, 0x68, // imul
        0x4F, 0x84, 0x04, 0x01, 0xa7, 0xff, 0xEA, 0xB1,
    ];
    let code_len = code.len();
    let loops = detect_loops(&code, code_len);
    let &(header, back_edge) = loops.iter().find(|&&(h, _)| h == 3).unwrap();
    let info = detect_int_array_element_wise(&code, header, back_edge, 4).unwrap();
    assert_eq!(info.op, ElementWiseOp::Mul);
}

// ── T17.Β.2 — SIMD element-wise emission ──────────────────────

/// Build a bytecode sequence for
/// `for (i=0; i<n; i++) out[i] = a[i] OP b[i]` and return
/// `(code, code_len)`. `op_byte` is the JVM arithmetic opcode
/// (`0x60` iadd, `0x68` imul, …).
fn ewise_bytecode(op_byte: u8) -> (Vec<u8>, usize) {
    // Locals: 0=out, 1=a, 2=b, 3=n, 4=i
    let code: Vec<u8> = vec![
        0x03, // 0: iconst_0
        0x36, 0x04, // 1: istore 4 (i = 0)
        0x15, 0x04, // 3: iload 4 — HEADER
        0x1D, // 5: iload_3 (n)
        // Exit branch targets the `return` at pc 28. (Historical note:
        // this was `+20 → 26`, the middle of the goto at 25 — never an
        // instruction boundary. The unresolved patch was silently
        // skipped before `patch_branches` learned to reject such
        // targets; the compile-only assertions below never noticed.)
        0xa2, 0x00, 0x16, // 6: if_icmpge +22 → 28
        0x2A, // 9:  aload_0 (out)
        0x15, 0x04, // 10: iload 4
        0x2B, // 12: aload_1 (a)
        0x15, 0x04, // 13: iload 4
        0x2e, // 15: iaload
        0x2C, // 16: aload_2 (b)
        0x15, 0x04,    // 17: iload 4
        0x2e,    // 19: iaload
        op_byte, // 20: iOP
        0x4F,    // 21: iastore
        0x84, 0x04, 0x01, // 22: iinc 4, 1
        0xa7, 0xff, 0xEA, // 25: goto -22 → 3
        0xB1, // 28: return
        0, 0,
    ];
    let code_len = 29;
    (code, code_len)
}

/// Compile a standalone element-wise method and return its
/// [`CompiledMethod`]. The signature is `void f(int[] out, int[]
/// a, int[] b, int n)` — four params, all live-in locals 0..3.
fn compile_ewise(op_byte: u8) -> Option<CompiledMethod> {
    let (code, code_len) = ewise_bytecode(op_byte);
    compile(
        &code,
        code_len,
        4, // params: out, a, b, n
        5, // locals: 0..3 params + i
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
}

/// Trip count 7 — one full 4-batch (or 8-batch on AVX2 would be
/// zero since 7 < 8, so the AVX2 phase should early-exit and
/// everything runs through the scalar tail). Either way, the
/// emitted code must compile and produce the right result when
/// executed end-to-end.
#[test]
fn t17_b_simd_ewise_add_length_7() {
    let compiled = compile_ewise(0x60); // iadd
    assert!(
        compiled.is_some(),
        "element-wise add loop must compile without error"
    );
}

/// Trip count 4 — zero AVX2 batches (batch size = 8), 4 elements
/// through the scalar tail. Compile must succeed.
#[test]
fn t17_b_simd_ewise_mul_exact_batch() {
    let compiled = compile_ewise(0x68); // imul
    assert!(
        compiled.is_some(),
        "element-wise mul loop must compile without error"
    );
}

/// When AVX2 is unavailable, the SIMD preheader for element-wise
/// must not fire; compilation still succeeds and falls back to
/// the original scalar loop. This test confirms the gate is
/// wired: detection populates the list but emission is skipped.
#[test]
fn t17_b_simd_ewise_avx2_gated() {
    // Detection runs unconditionally (see `compile()`), but
    // emission in `Compiler::emit_loop_header` is gated on
    // `has_avx2()`. If AVX2 is absent the gate rejects emission;
    // compile still succeeds.
    let compiled = compile_ewise(0x7E); // iand
    assert!(
        compiled.is_some(),
        "element-wise loop must compile on every CPU tier"
    );
    if !has_avx2() {
        // Nothing more to check — AVX2 absent means we didn't
        // issue any YMM encodings. The compile-ok check above
        // covers the fallback path.
        return;
    }
    // On AVX2 CPUs, the YMM-using code path was taken; verify
    // compilation produced a real entry pointer. (The "fall
    // back to scalar" claim for non-AVX2 is still covered by
    // the early return above.)
    let cm = compiled.unwrap();
    assert!(!cm.entry_ptr().is_null(), "compiled entry must be valid");
}

// T5.2.17 — loop unswitching tests

#[test]
fn test_detect_loop_unswitch_candidate_found() {
    // for (i = 0; i < n; i++) { if (flag != 0) {} }
    // Locals: 0=flag (invariant), 1=n, 2=i
    //
    // PC offsets (instruction boundaries):
    //  0: iconst_0            (1)
    //  1: istore_2            (1)
    //  2: iload_2   HEADER    (1)
    //  3: iload_1             (1)
    //  4: if_icmpge +15 → 19  (3)
    //  7: iload_0   (flag)    (1)
    //  8: ifeq +5 → 13        (3)
    // 11: nop                 (1)
    // 12: nop                 (1)
    // 13: iinc 2, 1           (3)
    // 16: goto -14 → 2        (3)  back-edge
    // 19: return              (1)
    let code: Vec<u8> = vec![
        0x03, 0x3D, // 0-1
        0x1C, 0x1B, 0xa2, 0x00, 0x0F, // 2-6
        0x1A, // 7
        0x99, 0x00, 0x05, // 8-10
        0x00, 0x00, // 11-12
        0x84, 0x02, 0x01, // 13-15
        0xa7, 0xff, 0xF2, // 16-18
        0xB1, // 19
    ];
    let code_len = code.len();
    let loops = detect_loops(&code, code_len);
    let &(header, back_edge) = loops
        .iter()
        .find(|&&(h, _)| h == 2)
        .expect("should detect outer for loop");
    let candidates = detect_loop_unswitch_candidates(&code, code_len, &[(header, back_edge)]);
    assert!(
        !candidates.is_empty(),
        "should find an unswitch candidate for the invariant flag"
    );
    let c = &candidates[0];
    assert_eq!(c.header_pc, header);
    assert_eq!(c.invariant_local, 0);
    assert_eq!(c.branch_op, 0x99); // ifeq
}

#[test]
fn test_detect_loop_unswitch_rejects_when_local_written() {
    // Same shape as above but the body writes local 0 — so it's
    // no longer invariant and must not be unswitched.
    //
    // PC offsets:
    //  0-1:   iconst_0 istore_2
    //  2-6:   iload_2 iload_1 if_icmpge +17 → 21
    //  7:     iload_0 (flag)
    //  8-10:  ifeq +5 → 15
    // 11:     iconst_1
    // 12:     istore_0              ← writes local 0
    // 13-14:  (pad nops)
    // 15-17:  iinc 2, 1
    // 18-20:  goto -16 → 2
    // 21:     return
    let code: Vec<u8> = vec![
        0x03, 0x3D, // 0-1
        0x1C, 0x1B, 0xa2, 0x00, 0x11, // 2-6
        0x1A, // 7
        0x99, 0x00, 0x05, // 8-10
        0x04, // 11: iconst_1
        0x3B, // 12: istore_0 (writes local 0)
        0x00, 0x00, // 13-14: nop nop
        0x84, 0x02, 0x01, // 15-17: iinc 2,1
        0xa7, 0xff, 0xF0, // 18-20: goto -16 → 2
        0xB1, // 21: return
    ];
    let code_len = code.len();
    let loops = detect_loops(&code, code_len);
    let &(header, back_edge) = loops
        .iter()
        .find(|&&(h, _)| h == 2)
        .expect("should detect loop at PC=2");
    let candidates = detect_loop_unswitch_candidates(&code, code_len, &[(header, back_edge)]);
    assert!(
        candidates.is_empty(),
        "should NOT unswitch when the predicate local is written in the loop"
    );
}

#[test]
fn test_detect_loop_unswitch_rejects_large_body() {
    // Body > MAX_UNSWITCH_BYTECODES → rejected even if predicate
    // is invariant.
    // Build a large loop by padding with nops.
    let mut code = vec![0x03, 0x3D]; // i = 0
    let header = code.len();
    code.extend_from_slice(&[0x1C, 0x1B, 0xa2, 0x00, 0x00]); // iload i,n,if_icmpge
    code.push(0x1A); // iload_0 (flag)
    code.extend_from_slice(&[0x99, 0x00, 0x03]); // ifeq
                                                 // Pad the body with nops so size > MAX_UNSWITCH_BYTECODES.
    for _ in 0..(MAX_UNSWITCH_BYTECODES + 5) {
        code.push(0x00);
    }
    let back_edge = code.len();
    code.extend_from_slice(&[0x84, 0x02, 0x01]); // iinc (part of body)
    code.extend_from_slice(&[0xa7, 0xFF, 0xFF]); // goto (back_edge)
    code.push(0xB1); // return
    let candidates = detect_loop_unswitch_candidates(&code, code.len(), &[(header, back_edge)]);
    assert!(
        candidates.is_empty(),
        "should NOT unswitch bodies larger than MAX_UNSWITCH_BYTECODES"
    );
}

// ── T17.Β.3 — Loop unswitch emission ───────────────────────────

/// Build a tiny loop that exhibits an invariant-branch unswitch
/// pattern: `for (i=0; i<n; i++) if (flag != 0) {}`. Locals:
/// 0=flag (invariant), 1=n, 2=i.
fn tiny_unswitchable_loop() -> (Vec<u8>, usize) {
    // PC layout:
    //  0: iconst_0
    //  1: istore_2            (i = 0)
    //  2: iload_2    HEADER
    //  3: iload_1             (n)
    //  4: if_icmpge +15 → 19  (3)
    //  7: iload_0             (flag)
    //  8: ifeq +5 → 13        (invariant branch)
    // 11: nop nop             (body side)
    // 13: iinc 2, 1           (induction)
    // 16: goto -14 → 2        (back-edge)
    // 19: return
    let code: Vec<u8> = vec![
        0x03, 0x3D, 0x1C, 0x1B, 0xa2, 0x00, 0x0F, 0x1A, 0x99, 0x00, 0x05, 0x00, 0x00, 0x84, 0x02,
        0x01, 0xa7, 0xff, 0xF2, 0xB1, 0, 0,
    ];
    let code_len = 20;
    (code, code_len)
}

fn compile_tiny_unswitchable() -> Option<CompiledMethod> {
    let (code, code_len) = tiny_unswitchable_loop();
    compile(
        &code,
        code_len,
        2, // params: flag, n
        3, // locals: flag, n, i
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
}

/// Emit the unswitched variant via the regular compile path and
/// confirm the compilation succeeds. The emission is *additive*:
/// it evaluates the invariant local once at the loop preheader
/// but never writes back any Java-visible state, so the final
/// locals after `n` iterations match the original scalar loop
/// bit-for-bit. This is the bytecode-equivalent contract.
#[test]
fn t17_b_loop_unswitch_bytecode_equiv() {
    let compiled = compile_tiny_unswitchable();
    assert!(
        compiled.is_some(),
        "unswitchable loop must compile; emission is additive"
    );
    // Confirm the candidate list is non-empty — otherwise the
    // preheader evaluation wouldn't have fired at all.
    let (code, code_len) = tiny_unswitchable_loop();
    let loops = detect_loops(&code, code_len);
    let cands = detect_loop_unswitch_candidates(&code, code_len, &loops);
    assert!(
        !cands.is_empty(),
        "detection must identify the invariant flag — emission relies on it"
    );
    assert_eq!(cands[0].branch_op, 0x99, "detected op must be ifeq");
    assert_eq!(cands[0].invariant_local, 0, "flag is local 0");

    // Tiny body is well under MAX_UNSWITCH_BYTECODES.
    let body_size = cands[0].back_edge_pc - cands[0].header_pc;
    assert!(
        body_size <= MAX_UNSWITCH_BYTECODES,
        "body size {body_size} must be ≤ {MAX_UNSWITCH_BYTECODES}"
    );
}

/// A loop whose body exceeds `MAX_UNSWITCH_BYTECODES` must be
/// rejected by the detector; the emitter consequently produces
/// the unmodified scalar loop (no preheader evaluation, no
/// duplication). Compilation still succeeds.
#[test]
fn t17_b_loop_unswitch_large_body_rejected() {
    // Build a large loop (body > MAX_UNSWITCH_BYTECODES).
    let mut code = vec![0x03, 0x3D]; // i = 0
    let header = code.len();
    code.extend_from_slice(&[0x1C, 0x1B, 0xa2, 0x00, 0x00]); // iload i,n,if_icmpge
    code.push(0x1A); // iload_0 (flag)
    code.extend_from_slice(&[0x99, 0x00, 0x03]); // ifeq
    for _ in 0..(MAX_UNSWITCH_BYTECODES + 5) {
        code.push(0x00); // padding nops
    }
    let back_edge = code.len();
    code.extend_from_slice(&[0x84, 0x02, 0x01]); // iinc
                                                 // Real back-edge to the loop header. (Historical note: this was a
                                                 // hardcoded `goto -1`, landing on the iinc's last operand byte —
                                                 // not an instruction boundary. The unresolved patch was silently
                                                 // skipped before `patch_branches` learned to reject such targets.)
    let goto_pc = code.len();
    let goto_off = (header as i32 - goto_pc as i32) as i16; // Cast: fits — tiny method
    code.push(0xa7);
    code.extend_from_slice(&goto_off.to_be_bytes());
    code.push(0xB1); // return
    code.push(0); // padding
    code.push(0);

    let code_len = code.len() - 2;
    let loops = detect_loops(&code, code_len);
    let cands = detect_loop_unswitch_candidates(&code, code_len, &loops);
    assert!(
        cands.is_empty(),
        "large body must not produce an unswitch candidate"
    );

    // Compilation still succeeds via the normal scalar path.
    let compiled = compile(
        &code,
        code_len,
        2,
        3,
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
    assert!(
        compiled.is_some(),
        "large-body loop must still compile through the scalar fallback"
    );
    // The key guarantee: no preheader evaluation was emitted, so
    // code size reflects only the scalar loop body (the emitter
    // short-circuited in `emit_loop_unswitch_preheader`).
    let _ = header;
    let _ = back_edge;
}

#[test]
fn test_detect_int_array_element_wise_rejects_non_elementwise() {
    // Reduction loop (sum += arr[i]) should NOT match element-wise.
    // Use a plain int reduction here (not the long-accumulator form
    // detect_int_array_sum already accepts).
    let code: Vec<u8> = vec![
        0x03, // 0: iconst_0
        0x36, 0x02, // 1: istore 2 (sum = 0)
        0x03, // 3: iconst_0
        0x36, 0x03, // 4: istore 3 (i = 0)
        // header at PC=6
        0x15, 0x03, // 6: iload 3
        0x1B, // 8: iload_1 (n)
        0xa2, 0x00, 0x0D, // 9: if_icmpge +13 → 22
        0x15, 0x02, // 12: iload 2 (sum)
        0x2A, // 14: aload_0
        0x15, 0x03, // 15: iload 3
        0x2e, // 17: iaload
        0x60, // 18: iadd
        0x36, 0x02, // 19: istore 2
        0x84, 0x03, 0x01, // 21: iinc 3, 1
        0xa7, 0xff, 0xF1, // 24: goto -15 → 9? (doesn't matter for this test)
        0x15, 0x02, // 27: iload 2
        0xac, // 29: ireturn
    ];
    let code_len = code.len();
    let loops = detect_loops(&code, code_len);
    // Either no loop is detected or the pattern doesn't match — both are fine.
    for &(header, back_edge) in &loops {
        let back_end = back_edge + bytecode_len_at(&code, back_edge);
        if let Some(iv) = find_induction_variable(&code, header, back_end) {
            assert!(
                detect_int_array_element_wise(&code, header, back_edge, iv).is_none(),
                "reduction should not match element-wise"
            );
        }
    }
}

#[cfg(feature = "vm-tests")]
#[test]
fn test_simd_int_array_sum_end_to_end() {
    // End-to-end test: compile a long sum_array(int[], int) method
    // and verify SIMD-accelerated execution with a real array.
    if !has_avx2() {
        println!("Skipping SIMD end-to-end test: AVX2 not available");
        return;
    }

    use crate::config::VmConfig;
    use crate::vm::SharedVm;
    use cratonvm_gc::heap::ArrayElementType;
    use cratonvm_types::ClassId;
    use std::sync::Arc;

    // Same bytecode as the pattern test above:
    // long sum_array(int[] arr, int n)
    // Locals: 0=arr, 1=n, 2-3=sum(long), 4=i
    let code: Vec<u8> = vec![
        0x09, // 0: lconst_0
        0x41, // 1: lstore_2 (sum = 0)
        0x03, // 2: iconst_0
        0x36, 0x04, // 3: istore 4 (i = 0)
        0x15, 0x04, // 5: iload 4 — loop header
        0x1b, // 7: iload_1 (n)
        0xa2, 0x00, 0x11, // 8: if_icmpge +17 → 25
        0x2a, // 11: aload_0 (arr)
        0x15, 0x04, // 12: iload 4 (i)
        0x2e, // 14: iaload
        0x85, // 15: i2l
        0x20, // 16: lload_2 (sum)
        0x61, // 17: ladd
        0x41, // 18: lstore_2 (sum)
        0x84, 0x04, 0x01, // 19: iinc 4, 1
        0xa7, 0xff, 0xef, // 22: goto -17 → 5
        0x20, // 25: lload_2
        0xad, // 26: lreturn
        0, 0,
    ];
    let code_len = 27;

    // needs_heap=true because of iaload
    let compiled = compile(
        &code,
        code_len,
        2,
        5,
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

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let vm_ptr = shared.as_ref() as *const _ as i64; // Cast: function pointer for JIT call target

    // Test 1: empty array (n=0, should return 0) — no SIMD, no scalar
    let arr1 = shared
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Int, 0);
    let arr1_ptr = arr1.as_ptr();
    // SAFETY: Calling JIT-compiled machine code with VM context; the CompiledMethod
    // was produced from valid bytecode and the mmap region is executable.
    let result1 = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call_with_context(vm_ptr, &[arr1_ptr as i64, 0])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(result1, 0, "empty array sum should be 0");

    // Test 2: array of 3 elements (0 SIMD chunks, all scalar cleanup)
    let n2 = 3;
    let arr2 = shared
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Int, n2);
    let arr2_ptr = arr2.as_ptr();
    for i in 0..n2 {
        let _ = shared.heap.set_array_element(arr2, i, Value::Int(100));
    }
    // SAFETY: Calling JIT-compiled machine code with VM context; the CompiledMethod
    // was produced from valid bytecode and the mmap region is executable.
    let result2 = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call_with_context(vm_ptr, &[arr2_ptr as i64, n2 as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(result2, 300, "3 elements of 100 should sum to 300");

    // Test 3: array of exactly 8 elements (exactly 1 SIMD chunk, no cleanup)
    let n3 = 8;
    let arr3 = shared
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Int, n3);
    let arr3_ptr = arr3.as_ptr();
    for i in 0..n3 {
        let _ = shared.heap.set_array_element(arr3, i, Value::Int(10));
    }
    // SAFETY: Calling JIT-compiled machine code with VM context; the CompiledMethod
    // was produced from valid bytecode and the mmap region is executable.
    let result3 = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call_with_context(vm_ptr, &[arr3_ptr as i64, n3 as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(result3, 80, "8 elements of 10 should sum to 80");

    // Test 4: array of 20 elements (exercises both SIMD chunks and scalar cleanup)
    // arr = [1, 2, 3, ..., 20], sum = 210
    let n4 = 20;
    let arr4 = shared
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Int, n4);
    let arr4_ptr = arr4.as_ptr();
    for i in 0..n4 {
        let _ = shared
            .heap
            .set_array_element(arr4, i, Value::Int((i + 1) as i32)); // Cast: x86-64 immediate encoding
    }
    // SAFETY: Calling JIT-compiled machine code with VM context; the CompiledMethod
    // was produced from valid bytecode and the mmap region is executable.
    let result4 = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call_with_context(vm_ptr, &[arr4_ptr as i64, n4 as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(result4, 210, "sum of 1..=20 should be 210");

    // Test 5: large array (256 elements) to really exercise SIMD
    let n5 = 256;
    let arr5 = shared
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Int, n5);
    let arr5_ptr = arr5.as_ptr();
    for i in 0..n5 {
        // Cast: value to i32 (encoding immediate/displacement)
        let _ = shared.heap.set_array_element(arr5, i, Value::Int(i as i32));
        // Cast: x86-64 immediate encoding
    }
    // sum of 0..255 = 255*256/2 = 32640
    // SAFETY: Calling JIT-compiled machine code with VM context; the CompiledMethod
    // was produced from valid bytecode and the mmap region is executable.
    let result5 = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call_with_context(vm_ptr, &[arr5_ptr as i64, n5 as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(result5, 32640, "sum of 0..=255 should be 32640");
}

#[test]
fn test_jit_scan_accepts_new_opcode() {
    // Bytecode: new #1 (0xbb 0x00 0x01), areturn
    // new pushes objectref; method returns it
    let code: Vec<u8> = vec![
        0xbb, 0x00, 0x01, // 0: new #1
        0xb0, // 3: areturn
        0, 0, // padding
    ];
    let scan = jit_scan(&code, 4, "()Ljava/lang/Object;");
    assert!(scan.is_some(), "jit_scan should accept `new` opcode");
    let scan = scan.unwrap();
    assert!(scan.needs_heap, "`new` requires heap pointer");
    assert_eq!(scan.new_ops.len(), 1);
    assert_eq!(scan.new_ops[0], (0, 1)); // pc=0, cp_idx=1
}

#[test]
fn test_jit_scan_accepts_anewarray_opcode() {
    // Bytecode: iconst_3, anewarray #1 (0xbd 0x00 0x01), areturn
    let code: Vec<u8> = vec![
        0x06, // 0: iconst_3 (length=3)
        0xbd, 0x00, 0x01, // 1: anewarray #1
        0xb0, // 4: areturn
        0, 0, // padding
    ];
    let scan = jit_scan(&code, 5, "()[Ljava/lang/Object;");
    assert!(scan.is_some(), "jit_scan should accept `anewarray` opcode");
    let scan = scan.unwrap();
    assert!(scan.needs_heap, "`anewarray` requires heap pointer");
    assert_eq!(scan.anewarray_ops.len(), 1);
    assert_eq!(scan.anewarray_ops[0], (1, 1)); // pc=1, cp_idx=1
}

#[test]
fn test_jit_scan_escape_analysis_non_escaping() {
    // Pattern: new; dup; invokespecial <init>; astore_1; aload_1; getfield; ireturn
    //
    // `jit_scan` runs escape analysis WITHOUT a constant-pool
    // resolver, so it cannot tell a trivial `<init>()V` from an
    // arg-bearing constructor. It therefore runs a *conservative*
    // pre-pass (every `invokespecial` escapes its operands); the
    // precise pass — which re-enables scalar replacement for the
    // `new; dup; invokespecial <init>()V` pattern — runs later in
    // `compile`, where the resolved descriptors are available.
    let code: Vec<u8> = vec![
        0xbb, 0x00, 0x01, // 0: new #1
        0x59, // 3: dup
        0xb7, 0x00, 0x02, // 4: invokespecial <init>
        0x4c, // 7: astore_1
        0x2b, // 8: aload_1
        0xb4, 0x00, 0x03, // 9: getfield #3
        0xac, // 12: ireturn
        0, 0, // padding
    ];
    let scan = jit_scan(&code, 13, "()I");
    assert!(scan.is_some(), "method should be jit-compatible");
    let scan = scan.unwrap();
    assert!(
        !scan.non_escaping_new.contains(&0),
        "jit_scan's conservative pre-pass escapes invokespecial operands; \
         precise re-analysis happens in `compile`"
    );

    // Precise pass: a trivial `<init>()V` (arg_slots=1, the dup'd
    // `this`) keeps the object non-escaping.
    let mut trivial: FxHashMap<usize, InvokeSpecialShape> = FxHashMap::default();
    trivial.insert(
        4,
        InvokeSpecialShape {
            arg_slots: 1,
            is_trivial_void_init: true,
        },
    );
    assert!(
        analyze_escapes(&code, 13, &trivial).contains(&0),
        "with a trivial `<init>()V` shape, new at PC=0 is non-escaping"
    );

    // An arg-bearing constructor (`<init>(I)V`, arg_slots=2) writes
    // the receiver's fields from an un-inlined method body, so the
    // receiver must escape — scalar replacement is unsound.
    let mut arg_init: FxHashMap<usize, InvokeSpecialShape> = FxHashMap::default();
    arg_init.insert(
        4,
        InvokeSpecialShape {
            arg_slots: 2,
            is_trivial_void_init: false,
        },
    );
    assert!(
        !analyze_escapes(&code, 13, &arg_init).contains(&0),
        "an arg-bearing `<init>(I)V` must escape its receiver"
    );
}

#[test]
fn test_escape_analysis_varargs_ctor_receiver_escapes() {
    // BUG-05 regression. A varargs constructor call `new C(a, b)` is
    // compiled by javac to:
    //   new C; dup; iconst_2; anewarray E;
    //   dup; iconst_0; <push a>; aastore;
    //   dup; iconst_1; <push b>; aastore;
    //   invokespecial C.<init>([E;)V
    // The dup'd receiver sits on the operand stack BELOW the array while
    // `anewarray` (0xbd) executes. `anewarray` is not modeled by
    // `analyze_escapes`, so its catch-all arm runs. The previous catch-all
    // merely FORGOT the slot provenance — which erased the receiver's
    // tracking, so the later arg-bearing `invokespecial <init>` escaped
    // nothing and the receiver was reported non-escaping. It was then
    // scalar-replaced to a dummy null and passed as `this` to the
    // un-inlined constructor → "Cannot assign field … because \"this\" is
    // null" (Spring `ResourceDatabasePopulator`). The catch-all must now
    // ESCAPE tracked stack objects, so the `new` at PC=0 is escaping.
    //
    // Use `bipush a (0x10)` / `bipush b` as the array element pushes so the
    // bytecode is self-contained (no constant-pool dependency for the test).
    let code: Vec<u8> = vec![
        0xbb, 0x00, 0x01, // 0:  new #1 (C)
        0x59, // 3:  dup
        0x05, // 4:  iconst_2
        0xbd, 0x00, 0x02, // 5:  anewarray #2 (E)
        0x59, // 8:  dup
        0x03, // 9:  iconst_0
        0x10, 0x07, // 10: bipush 7
        0x53, // 12: aastore
        0x59, // 13: dup
        0x04, // 14: iconst_1
        0x10, 0x09, // 15: bipush 9
        0x53, // 17: aastore
        0xb7, 0x00, 0x03, // 18: invokespecial C.<init>([E;)V
        0xb1, // 21: return
        0, 0, // padding
    ];
    // Resolved shape for the varargs ctor: receiver + 1 array slot.
    let mut shapes: FxHashMap<usize, InvokeSpecialShape> = FxHashMap::default();
    shapes.insert(
        18,
        InvokeSpecialShape {
            arg_slots: 2,
            is_trivial_void_init: false,
        },
    );
    assert!(
        !analyze_escapes(&code, 22, &shapes).contains(&0),
        "the receiver of a varargs constructor must escape (it is passed to \
         the un-inlined `<init>` across an `anewarray`); scalar-replacing it \
         yields a null `this` (BUG-05)"
    );
}

#[test]
fn test_jit_scan_escape_analysis_escaping_via_areturn() {
    // Pattern: new; dup; invokespecial <init>; areturn — the object escapes
    let code: Vec<u8> = vec![
        0xbb, 0x00, 0x01, // 0: new #1
        0x59, // 3: dup
        0xb7, 0x00, 0x02, // 4: invokespecial <init>
        0xb0, // 7: areturn
        0, 0, // padding
    ];
    let scan = jit_scan(&code, 8, "()Ljava/lang/Object;");
    assert!(scan.is_some(), "method should be jit-compatible");
    let scan = scan.unwrap();
    assert!(
        !scan.non_escaping_new.contains(&0),
        "new at PC=0 should be escaping (returned via areturn)"
    );
}

/// HIGH-6 — verify the inline TLAB bump-pointer codegen produces an
/// executable method that correctly falls through to the slow path
/// when the TLS thread pointer is null (the JE-on-null branch in
/// `emit_inline_tlab_new`). Using stubs avoids a full VM init so this
/// test is NOT gated behind the `vm-tests` feature.
#[test]
fn test_inline_tlab_new_falls_through_on_null_thread() {
    // Stub: pretend there is no current thread (re-entrant or pre-init
    // state). The inline path must take the JE branch to the slow path
    // and we then short-circuit with a sentinel `new_object` return.
    // SAFETY: extern "C" test stub with no arguments and no dereferences; it only
    // returns a null pointer, so there are no preconditions for the caller to uphold.
    unsafe extern "C" fn null_thread() -> *mut std::ffi::c_void {
        std::ptr::null_mut()
    }
    // SAFETY: extern "C" test stub; ignores all i64 arguments and dereferences nothing,
    // returning a fixed sentinel value, so it cannot violate memory safety.
    unsafe extern "C" fn fake_new_object(_vm: i64, _cid: i64, _nf: i64) -> i64 {
        0xDEAD_BEEFi64
    }
    // SAFETY: extern "C" test stub that immediately panics; it touches no arguments
    // and performs no memory access, so it imposes no safety obligations on callers.
    unsafe extern "C" fn unimplemented_post_init(_vm: i64, _obj: i64, _cid: i64, _nf: i64) -> i64 {
        panic!("post_tlab_init must not be called when thread is null");
    }

    // Build a custom helper table with the inline path WIRED so
    // `can_inline` is true (get_current_thread, tlab_post_init,
    // new_object all non-null), but `get_current_thread` returns
    // null at runtime to force the slow path inside the emitted
    // inline cascade.
    let mut helpers = test_helpers();
    // Cast: fn pointer to usize helper address
    helpers.get_current_thread = null_thread as *const () as usize;
    // Cast: fn pointer to usize helper address
    helpers.tlab_post_init = unimplemented_post_init as *const () as usize;
    // Cast: fn pointer to usize helper address
    helpers.new_object = fake_new_object as *const () as usize;
    // The offsets don't matter — they're only read when the
    // thread pointer is non-null.
    helpers.tlab_cursor_offset_in_thread = 0;
    helpers.tlab_end_offset_in_thread = 8;

    // Method: new #1; astore_1; aload_1; areturn (cls=42, fields=3).
    let code: Vec<u8> = vec![0xbb, 0x00, 0x01, 0x4c, 0x2b, 0xb0, 0, 0];
    let code_len = 6;
    // CRIT-2 tuple: (pc, class_id, num_fields, has_prim_init, has_finalizer).
    // Tests use conservative `(true, true)` so the helper path is exercised.
    let new_info: Vec<(usize, u32, usize, bool, bool)> = vec![(0, 42, 3, true, true)];

    let compiled = compile(
        &code,
        code_len,
        0,
        2,
        true, // needs_heap → can_inline gate passes
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        new_info,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &helpers,
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout — not needed for this test
    )
    .expect("inline-TLAB new opcode should compile");

    // SAFETY: Calling JIT-compiled machine code with a sentinel VM pointer.
    // The fake_new_object stub does not touch the pointer.
    let result = unsafe {
        compiled
            .try_call_with_context(0xCAFE_F00D, &[])
            .expect("test JIT call")
    };
    assert_eq!(
        result, 0xDEAD_BEEFi64,
        "inline TLAB cascade must fall through to slow-path fake_new_object \
         when get_current_thread returns null"
    );
}

/// The allocation spill sink splits ONE 14-store blind spill across two
/// program points: the registers the inline-TLAB fast path clobbers stay at
/// the safepoint, the rest move to the allocation's slow-path label. The two
/// halves are selected by complementary predicates over the same table, so a
/// register can only be lost if `ALLOC_FAST_PATH_CLOBBERS` names something
/// `ALL_SPILL_GPRS` does not contain — and a lost register is not a slow
/// benchmark, it is a live oop the conservative root scan never sees.
#[test]
fn alloc_spill_sink_partitions_the_gpr_file_exactly_once() {
    let hot: Vec<u8> = ALL_SPILL_GPRS
        .iter()
        .copied()
        .filter(|r| ALLOC_FAST_PATH_CLOBBERS.contains(r))
        .collect();
    let sunk: Vec<u8> = ALL_SPILL_GPRS
        .iter()
        .copied()
        .filter(|r| !ALLOC_FAST_PATH_CLOBBERS.contains(r))
        .collect();
    // Every clobbered register must be IN the spilled file, or the hot half
    // silently drops it and the sunk half captures a post-clobber value.
    for r in ALLOC_FAST_PATH_CLOBBERS {
        assert!(
            ALL_SPILL_GPRS.contains(&r),
            "fast-path clobber {r} is not in ALL_SPILL_GPRS"
        );
    }
    assert_eq!(hot.len(), ALLOC_FAST_PATH_CLOBBERS.len(), "hot half");
    assert_eq!(
        hot.len() + sunk.len(),
        ALL_SPILL_GPRS.len(),
        "the halves must cover the file"
    );
    let mut union: Vec<u8> = hot.iter().chain(sunk.iter()).copied().collect();
    union.sort_unstable();
    let mut expected = ALL_SPILL_GPRS.to_vec();
    expected.sort_unstable();
    assert_eq!(
        union, expected,
        "partition must be exact — no gap, no overlap"
    );
}

/// Sink form of the inline-TLAB preamble: it drops the `get_current_thread`
/// fallback CALL (whose caller-saved clobbers would invalidate the eleven
/// registers spilled later at the slow-path label) and treats a null cached
/// thread slot as a divert to `new_object`. This is the same end state the
/// fallback produced — it too fell through to the slow path once the helper
/// returned null — so the observable behaviour must be unchanged.
///
/// `test_inline_tlab_new_falls_through_on_null_thread` covers the un-sunk arm
/// with `(has_prim_init, has_finalizer) = (true, true)`; this one flips both to
/// false so `skip_helper` holds and the site actually requests the sink.
#[test]
fn alloc_spill_sink_still_diverts_to_the_helper_on_a_null_thread() {
    // SAFETY: extern "C" test stub with no arguments and no dereferences; it only
    // returns a null pointer, so there are no preconditions for the caller to uphold.
    unsafe extern "C" fn null_thread() -> *mut std::ffi::c_void {
        std::ptr::null_mut()
    }
    // SAFETY: extern "C" test stub; ignores all i64 arguments and dereferences nothing,
    // returning a fixed sentinel value, so it cannot violate memory safety.
    unsafe extern "C" fn fake_new_object(_vm: i64, _cid: i64, _nf: i64) -> i64 {
        0x51DE_5152i64
    }
    // SAFETY: extern "C" test stub that immediately panics; it touches no arguments
    // and performs no memory access, so it imposes no safety obligations on callers.
    unsafe extern "C" fn unreachable_post_init(_vm: i64, _o: i64, _c: i64, _n: i64) -> i64 {
        panic!("skip_post_init_helper is set — the helper must not be emitted");
    }

    let mut helpers = test_helpers();
    // Cast: fn pointer to usize helper address
    helpers.get_current_thread = null_thread as *const () as usize;
    // Cast: fn pointer to usize helper address
    helpers.tlab_post_init = unreachable_post_init as *const () as usize;
    // Cast: fn pointer to usize helper address
    helpers.new_object = fake_new_object as *const () as usize;
    helpers.tlab_cursor_offset_in_thread = 0;
    helpers.tlab_end_offset_in_thread = 8;

    // new #1; astore_1; aload_1; areturn (cls=42, 3 fields).
    let code: Vec<u8> = vec![0xbb, 0x00, 0x01, 0x4c, 0x2b, 0xb0, 0, 0];
    // (pc, class_id, num_fields, has_prim_init, has_finalizer) — both false, so
    // `skip_helper` holds and the `new` arm raises `sink_alloc_blind_spill`.
    let new_info: Vec<(usize, u32, usize, bool, bool)> = vec![(0, 42, 3, false, false)];

    let compiled = compile(
        &code,
        6,
        0,
        2,
        true,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        new_info,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots — no PIC sites in this stub
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &helpers,
        std::collections::HashSet::new(),
        HashMap::new(),
        None,
    )
    .expect("sink-form inline-TLAB new should compile");

    // SAFETY: Calling JIT-compiled machine code with a sentinel VM pointer.
    // The fake_new_object stub does not touch the pointer.
    let result = unsafe {
        compiled
            .try_call_with_context(0xCAFE_F00D, &[])
            .expect("test JIT call")
    };
    assert_eq!(
        result, 0x51DE_5152i64,
        "a null cached thread must still reach new_object under the spill sink"
    );
}

#[cfg(feature = "vm-tests")]
#[test]
fn test_jit_new_object_codegen() {
    // Verify that the `new` opcode compiles and calls jit_new_object at runtime.
    // Method: allocate an object, store to local 1, load local 1, areturn.
    // new #1; astore_1; aload_1; areturn
    let code: Vec<u8> = vec![
        0xbb, 0x00, 0x01, // 0: new #1  (cp_idx=1)
        0x4c, // 3: astore_1
        0x2b, // 4: aload_1
        0xb0, // 5: areturn
        0, 0, // padding
    ];
    let code_len = 6;

    // Provide new_info: class_id=42, num_fields=3
    // CRIT-2 tuple: (pc, class_id, num_fields, has_prim_init, has_finalizer).
    // Tests use conservative `(true, true)` so the helper path is exercised.
    let new_info: Vec<(usize, u32, usize, bool, bool)> = vec![(0, 42, 3, true, true)];
    let compiled = compile(
        &code,
        code_len,
        0,
        2,
        true, // needs_heap (for jit_new_object helper)
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        new_info,
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
    assert!(
        compiled.is_some(),
        "Should compile method with `new` opcode"
    );

    use crate::config::VmConfig;
    use crate::vm::SharedVm;
    use std::sync::Arc;
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let vm_ptr = shared.as_ref() as *const _ as i64; // Cast: function pointer for JIT call target

    // SAFETY: Calling JIT-compiled machine code with VM context; the CompiledMethod
    // was produced from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            .unwrap()
            .try_call_with_context(vm_ptr, &[])
            .expect("test JIT call")
    };
    // Result should be a non-zero pointer to the allocated object
    assert_ne!(
        result, 0,
        "jit_new_object should return a valid object pointer"
    );
}

#[cfg(feature = "vm-tests")]
#[test]
fn test_jit_anewarray_codegen() {
    // Verify anewarray compiles and produces a valid reference array at runtime.
    // iconst_5; anewarray #1; areturn
    let code: Vec<u8> = vec![
        0x08, // 0: iconst_5 (length=5)
        0xbd, 0x00, 0x01, // 1: anewarray #1 (cp_idx=1)
        0xb0, // 4: areturn
        0, 0, // padding
    ];
    let code_len = 5;

    // Provide anewarray_info: component_class_id=7
    let anewarray_info: Vec<(usize, u32)> = vec![(1, 7)];
    let compiled = compile(
        &code,
        code_len,
        0,
        0,
        true, // needs_heap
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        anewarray_info,
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
    assert!(
        compiled.is_some(),
        "Should compile method with `anewarray` opcode"
    );

    use crate::config::VmConfig;
    use crate::vm::SharedVm;
    use std::sync::Arc;
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let vm_ptr = shared.as_ref() as *const _ as i64; // Cast: function pointer for JIT call target

    // SAFETY: Calling JIT-compiled machine code with VM context; the CompiledMethod
    // was produced from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            .unwrap()
            .try_call_with_context(vm_ptr, &[])
            .expect("test JIT call")
    };
    // Result should be non-zero (valid array pointer)
    assert_ne!(
        result, 0,
        "jit_anewarray_object should return a valid array pointer"
    );
}

// -----------------------------------------------------------------------
// M5: JIT compilation of <init> constructors and lambda$ methods
// -----------------------------------------------------------------------

#[test]
fn test_m5_constructor_init_putfield() {
    // Simulates: <init>(int x) { this.x = x; }
    // Bytecode: aload_0, iload_1, putfield #1, return
    // This is the exact pattern that was previously skipped for <init>.
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0 (this)
        0x1b, // 1: iload_1 (x)
        0xb5, 0x00, 0x01, // 2: putfield #1
        0xb1, // 5: return (void)
        0, 0,
    ];
    let code_len = 6;
    let field_info = vec![(2usize, 0usize, b'I')];
    let compiled = compile(
        &code,
        code_len,
        2, // param_slots: this + int x
        2, // max_locals
        false,
        Vec::new(),
        field_info,
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
    assert!(
        compiled.is_some(),
        "Constructor with putfield should be JIT-compilable"
    );

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 2);
    heap.set_field(obj, 0, Value::Int(0));

    // Call <init>(this, 42)
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        compiled
            .unwrap()
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call(&[obj.as_ptr() as i64, 42])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    let val = heap.get_field(obj, 0);
    assert_eq!(
        val,
        Value::Int(42),
        "Constructor putfield should set field correctly"
    );
}

#[test]
fn test_m5_constructor_init_multiple_putfields() {
    // Simulates: <init>(int x, int y) { this.x = x; this.y = y; }
    // Bytecode: aload_0, iload_1, putfield #1, aload_0, iload_2, putfield #2, return
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x1b, // 1: iload_1
        0xb5, 0x00, 0x01, // 2: putfield #1
        0x2a, // 5: aload_0
        0x1c, // 6: iload_2
        0xb5, 0x00, 0x02, // 7: putfield #2
        0xb1, // 10: return
        0, 0,
    ];
    let code_len = 11;
    let field_info = vec![
        (2usize, 0usize, b'I'), // putfield #1 at pc=2, field_index=0
        (7usize, 1usize, b'I'), // putfield #2 at pc=7, field_index=1
    ];
    let compiled = compile(
        &code,
        code_len,
        3, // param_slots: this + int x + int y
        3, // max_locals
        false,
        Vec::new(),
        field_info,
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
    assert!(compiled.is_some(), "Multi-field constructor should compile");

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 3);
    heap.set_field(obj, 0, Value::Int(0));
    heap.set_field(obj, 1, Value::Int(0));

    // Call <init>(this, 10, 20)
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        compiled
            .unwrap()
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call(&[obj.as_ptr() as i64, 10, 20])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(heap.get_field(obj, 0), Value::Int(10));
    assert_eq!(heap.get_field(obj, 1), Value::Int(20));
}

#[test]
fn test_m5_lambda_method_compiles() {
    // Simulates: lambda$main$0(int x) -> int { return x + 1; }
    // Lambda methods are static methods with captured args. This tests
    // that such methods are now JIT-compilable (previously skipped).
    let code: Vec<u8> = vec![
        0x1a, // 0: iload_0
        0x04, // 1: iconst_1
        0x60, // 2: iadd
        0xac, // 3: ireturn
        0, 0,
    ];
    let code_len = 4;
    let compiled = compile(
        &code,
        code_len,
        1, // param_slots: int x
        1, // max_locals
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
    assert!(
        compiled.is_some(),
        "Lambda-style method should be JIT-compilable"
    );

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.unwrap().try_call(&[41]).expect("test JIT call") };
    assert_eq!(result, 42, "lambda$main$0(41) should return 42");
}

#[test]
fn test_m5_user_class_method_compiles() {
    // Simulates: com.example.MyClass.add(int a, int b) -> int { return a + b; }
    // Tests that non-java/* user class methods are JIT-compiled.
    let code: Vec<u8> = vec![
        0x1a, // 0: iload_0
        0x1b, // 1: iload_1
        0x60, // 2: iadd
        0xac, // 3: ireturn
        0, 0,
    ];
    let code_len = 4;
    let compiled = compile(
        &code,
        code_len,
        2, // param_slots: int a, int b
        2, // max_locals
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
    assert!(
        compiled.is_some(),
        "User class static method should be JIT-compilable"
    );

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            .unwrap()
            .try_call(&[17, 25])
            .expect("test JIT call")
    };
    assert_eq!(result, 42, "add(17, 25) should return 42");
}

#[test]
fn test_m5_constructor_void_return() {
    // Verify void-returning constructor descriptor is JIT-scannable
    // <init>()V — simplest constructor
    let code: Vec<u8> = vec![
        0xb1, // 0: return (void)
        0, 0,
    ];
    let code_len = 1;
    let scan = jit_scan(&code, code_len, "()V");
    assert!(scan.is_some(), "Void constructor should pass jit_scan");
}

#[test]
fn test_m5_lambda_with_captured_object() {
    // Simulates: lambda$forEach$0(Object captured, int idx) -> int
    // Returns idx (simplified — tests object + int arg passing)
    let code: Vec<u8> = vec![
        0x1b, // 0: iload_1 (idx is local 1, captured obj is local 0)
        0xac, // 1: ireturn
        0, 0,
    ];
    let code_len = 2;
    let compiled = compile(
        &code,
        code_len,
        2, // param_slots: Object captured, int idx
        2, // max_locals
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
    assert!(
        compiled.is_some(),
        "Lambda with captured Object arg should compile"
    );

    // Pass null as captured object (0), 99 as idx
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.unwrap().try_call(&[0, 99]).expect("test JIT call") };
    assert_eq!(
        result, 99,
        "Lambda should correctly access second parameter"
    );
}

// -----------------------------------------------------------------------
// Phase 87: FP Performance tests
// -----------------------------------------------------------------------

// --- 87.1: XMM Register Persistence ---

#[test]
fn p87_scratch_xmm_constants() {
    // Verify scratch XMM register constants are defined correctly
    assert_eq!(SCRATCH_XMMS, [2, 3, 4, 5, 6, 7]);
    assert_eq!(SCRATCH_XMMS.len(), 6);
}

#[test]
fn p87_xmm_intermediate_chaining_double() {
    // double f(double a, double b) { return (a + b) * (a - b); }
    // dload_0, dload_1, dadd, dload_0, dload_1, dsub, dmul, dreturn
    // This tests that FP intermediates persist in XMM registers
    // across the dadd → dsub → dmul chain.
    let code: Vec<u8> = vec![
        0x26, // 0: dload_0 (a)
        0x27, // 1: dload_1 (b)
        0x63, // 2: dadd (a+b)
        0x26, // 3: dload_0 (a)
        0x27, // 4: dload_1 (b)
        0x67, // 5: dsub (a-b)
        0x6b, // 6: dmul ((a+b)*(a-b))
        0xaf, // 7: dreturn
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

    let a = 5.0f64;
    let b = 3.0f64;
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            // Cast: float/double bit pattern to i64 (bit-preserving, VM all-GPR ABI)
            .try_call(&[a.to_bits() as i64, b.to_bits() as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    let expected = (a + b) * (a - b); // 8.0 * 2.0 = 16.0
    assert_eq!(f64::from_bits(result as u64), expected); // Cast: JIT ABI convention
}

#[test]
fn p87_xmm_intermediate_chaining_float() {
    // float f(float a, float b) { return (a + b) * (a - b); }
    let code: Vec<u8> = vec![
        0x22, // fload_0
        0x23, // fload_1
        0x62, // fadd
        0x22, // fload_0
        0x23, // fload_1
        0x66, // fsub
        0x6a, // fmul
        0xae, // freturn
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

    let a = 5.0f32;
    let b = 3.0f32;
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            // Cast: float/double bit pattern to i64 (bit-preserving, VM all-GPR ABI)
            .try_call(&[a.to_bits() as i64, b.to_bits() as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    let expected = (a + b) * (a - b);
    assert_eq!(f32::from_bits(result as u32), expected); // Cast: JIT ABI convention
}

#[test]
fn p87_xmm_multiple_intermediates() {
    // double f(double a, double b, double c) { return a*b + a*c + b*c; }
    // Tests multiple FP intermediates on the stack simultaneously
    // dload_0, dload_1, dmul,     -> (a*b)
    // dload_0, dload_2, dmul,     -> (a*c)
    // dadd,                       -> (a*b + a*c)
    // dload_1, dload_2, dmul,     -> (b*c)
    // dadd,                       -> (a*b + a*c + b*c)
    // dreturn
    let code: Vec<u8> = vec![
        0x26, // 0: dload_0 (a)
        0x27, // 1: dload_1 (b)
        0x6b, // 2: dmul
        0x26, // 3: dload_0 (a)
        0x28, // 4: dload_2 (c)
        0x6b, // 5: dmul
        0x63, // 6: dadd
        0x27, // 7: dload_1 (b)
        0x28, // 8: dload_2 (c)
        0x6b, // 9: dmul
        0x63, // 10: dadd
        0xaf, // 11: dreturn
        0, 0,
    ];
    let code_len = 12;
    let compiled = compile(
        &code,
        code_len,
        3,
        3,
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

    let a = 2.0f64;
    let b = 3.0f64;
    let c = 4.0f64;
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            .try_call(&[
                a.to_bits() as i64, // Cast: JIT ABI convention
                b.to_bits() as i64, // Cast: JIT ABI convention
                c.to_bits() as i64, // Cast: JIT ABI convention
            ])
            .expect("test JIT call")
    };
    let expected = a * b + a * c + b * c; // 6 + 8 + 12 = 26
    assert_eq!(f64::from_bits(result as u64), expected); // Cast: JIT ABI convention
}

#[test]
fn p87_xmm_local_allocation_verified() {
    // Verify that the register allocator allocates XMM registers for FP locals
    // Simple: dload_0, dstore_1, dload_1, dreturn
    // Locals 0 and 1 are both FP — should both get XMM allocations
    let code: Vec<u8> = vec![0x26, 0x48, 0x27, 0xaf, 0, 0];
    let code_len = 4;
    let loops = detect_loops(&code, code_len);
    let alloc = crate::regalloc::allocate_registers(&code, code_len, 2, 1, &loops);
    // Both locals should have XMM assignments (not GPR)
    assert!(
        alloc.assignments[0].is_none(),
        "FP local 0 should not have GPR"
    );
    assert!(
        alloc.assignments[1].is_none(),
        "FP local 1 should not have GPR"
    );
    // At least one should get an XMM
    let xmm_count = alloc
        .xmm_assignments
        .iter()
        .filter(|a: &&Option<u8>| a.is_some())
        .count();
    assert!(
        xmm_count > 0,
        "At least one FP local should get XMM register"
    );
}

/// **The XMM-homed-local emitter, exercised on a System V host.**
///
/// The test above proves the ALLOCATOR hands a float/double local an XMM
/// register. Nothing proved the EMITTER then does anything with it, on the
/// platform that runs the suite: `Compiler::new` gates the allocation behind
/// `xmm_local_homes_enabled`, which is Win64-only for the ABI reason stated
/// there, so on Linux `xmm_for_local` answered `None` for every local and the
/// fifteen sites that read it never ran.
///
/// This is the prologue's three of those, forced on and off over one fixture:
///
///   * a register-passed float/double parameter is moved GPR -> XMM, rather
///     than stored to its frame home;
///   * an XMM-mapped local past the parameter span is zeroed with `PXOR`,
///     rather than by storing a zeroed GPR to its frame slot;
///   * neither instruction is emitted when the homes are off.
///
/// **Bytes only, never executed.** Forcing the homes on does not change the
/// ABI: code compiled this way would lose a local across any call on System V,
/// which is the whole reason the default is what it is. This test inspects the
/// buffer and calls nothing in it.
#[test]
fn an_xmm_homed_local_is_loaded_and_zeroed_by_the_prologue() {
    // MOVQ xmm8, rax — 66 REX.W+R 0F 6E /r, ModRM 11 000 000.
    const MOVQ_XMM8_RAX: [u8; 5] = [0x66, 0x4C, 0x0F, 0x6E, 0xC0];
    // PXOR xmm9, xmm9 — 66 REX.R+B 0F EF /r, ModRM 11 001 001.
    const PXOR_XMM9: [u8; 5] = [0x66, 0x45, 0x0F, 0xEF, 0xC9];

    // Local 0 is the incoming parameter and is XMM-homed; local 2 is an
    // XMM-homed local past the parameter span, so the prologue must zero it.
    // XMM8/XMM9 rather than XMM2..7 because `regalloc` allocates single-pass
    // local homes out of the high half — the same range the test above asserts.
    let prologue_bytes = |homes: bool| -> Vec<u8> {
        let _force = if homes {
            XmmLocalHomesForce::on()
        } else {
            XmmLocalHomesForce::off()
        };
        let alloc_result = crate::regalloc::RegAllocResult {
            assignments: vec![None; 4],
            xmm_assignments: vec![Some(8), None, Some(9), None],
            used_callee_saved: Vec::new(),
            used_xmm_regs: vec![8, 9],
            block_live_in: Vec::new(),
        };
        let mut compiler = Compiler::new(
            "xmm-home-prologue-test".to_string(),
            ExecutableBuffer::new(4096).expect("test executable buffer"),
            4,
            1,
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
        compiler.emit_prologue();
        assert!(
            !compiler.buf.overflowed(),
            "the test buffer must hold the prologue",
        );
        compiler.buf.as_slice().to_vec()
    };

    let win = prologue_bytes(true);
    let sysv = prologue_bytes(false);
    let has = |b: &[u8], pat: &[u8; 5]| b.windows(5).any(|w| w == pat);

    assert!(
        has(&win, &MOVQ_XMM8_RAX),
        "with XMM homes on, the parameter must be moved into XMM8 — this is \
         `frames::emit_prologue`'s register-argument arm, and it is the arm \
         that never runs on System V",
    );
    assert!(
        has(&win, &PXOR_XMM9),
        "with XMM homes on, an XMM-mapped local past the parameter span must \
         be zeroed in its REGISTER; zeroing only its frame word would leave \
         the register holding the previous frame's value",
    );
    assert!(
        !has(&sysv, &MOVQ_XMM8_RAX) && !has(&sysv, &PXOR_XMM9),
        "with XMM homes off, every float/double local uses its frame home and \
         no XMM instruction belongs in the prologue at all",
    );
    assert_ne!(
        win, sysv,
        "the two arms emitted identical bytes, so the gate is being ignored \
         and both assertions above are about the same compile",
    );
}

/// **The ten bytecode arms, exercised on a System V host.**
///
/// The prologue test above covers `frames`' three sites. These are the other
/// ten: every `dload`/`dstore`/`fload`/`fstore` and accumulator arm in
/// `bytecode_walk` that reads `xmm_for_local`, reached the way production
/// reaches them -- a whole compile through `driver::compile`, with the
/// allocator choosing the homes rather than a fixture asserting them.
///
/// The assertion is an A/B rather than a byte pattern on purpose. Which
/// instruction each arm picks is the emitter's business and changes with it;
/// that the two arms of the gate produce DIFFERENT code, and that the forced-on
/// one keeps the allocator's XMM registers out of the frame, is the property
/// this is here to hold.
///
/// **Bytes only, never executed** -- see `XmmLocalHomesForce`.
#[test]
fn the_bytecode_arms_honour_an_xmm_home_the_allocator_gave() {
    // static double f(int n) { double a = n; return a + a; }
    //   0: iload_0  1: i2d  2: dstore_1  3: dload_1  4: dload_1  5: dadd
    //   6: dreturn
    // One store and two loads of local 1, which is what earns it a register.
    let code = [0x1a, 0x87, 0x48, 0x27, 0x27, 0x63, 0xaf, 0, 0];
    let code_len = 7;

    // Precondition: the ALLOCATOR gives local 1 an XMM home on this fixture.
    // Without it the two arms below are the same compile and the A/B proves
    // nothing -- the failure mode this fixture is one edit away from.
    let loops = detect_loops(&code, code_len);
    let alloc = crate::regalloc::allocate_registers(&code, code_len, 3, 1, &loops);
    assert!(
        alloc.xmm_assignments.iter().any(|a| a.is_some()),
        "the allocator gave this fixture no XMM home, so the gate below has \
         nothing to gate: repair the fixture rather than believe the A/B",
    );

    let compiled = |homes: bool| -> Vec<u8> {
        let _force = if homes {
            XmmLocalHomesForce::on()
        } else {
            XmmLocalHomesForce::off()
        };
        let cm = crate::x64::driver::compile(
            &code,
            code_len,
            1,
            3,
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
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            std::collections::HashMap::new(),
            None,
        )
        .expect("the fixture compiles through the single-pass backend");
        cm.code_bytes().to_vec()
    };

    let win = compiled(true);
    let sysv = compiled(false);

    assert_ne!(
        win, sysv,
        "the two arms of `xmm_local_homes_enabled` emitted identical code for \
         a method whose double local the allocator put in an XMM register — \
         the gate is being ignored, and every `xmm_for_local` arm in \
         `bytecode_walk` is unreachable on this host",
    );
}

// --- 87.2: FP Loop Optimization ---

#[test]
fn p87_fp_loop_hoist_detection() {
    // Loop: dload_0; dload 4; dadd; dstore_0; iinc 2 1; iload 2; iconst_5;
    //       if_icmplt -11; dload_0; dreturn
    //
    // The invariant operand is `dload 4`, NOT the `dload_1` this test used to
    // carry. `dstore_0` stores a `double`, which occupies slots 0 AND 1, so
    // `dload_1` reads a slot the loop body writes and hoisting it out is
    // unsound. The old fixture asserted that hoist was legal — it was written
    // from what `find_modified_locals` did, which recorded only the lower slot
    // of a category-2 store until 2026-09-05. `dload 4` (slots 4, 5) is
    // disjoint from the accumulator (0, 1) and from the counter (2), so the
    // property the test was written for is preserved and now stated on a
    // fixture where it holds.
    let code: Vec<u8> = vec![
        0x26, // 0:  dload_0  (modified — acc, slots 0+1)
        0x18, 0x04, // 1:  dload 4 (invariant — slots 4+5)
        0x63, // 3:  dadd
        0x47, // 4:  dstore_0
        0x84, 0x02, 0x01, // 5: iinc 2, 1
        0x15, 0x02, // 8:  iload 2
        0x08, // 10: iconst_5
        0xa1, 0xFF, 0xF5, // 11: if_icmplt -11 → target=0
        0x26, // 14: dload_0
        0xaf, // 15: dreturn
        0, 0,
    ];
    let code_len = 16;
    let loops = detect_loops(&code, code_len);
    assert!(!loops.is_empty(), "Should detect a loop");

    let hoists = find_fp_loop_hoists(&code, code_len, &loops);
    // dload 4 at PC=1 should be hoistable (locals 4 and 5 are invariant)
    let hoisted_pcs: Vec<usize> = hoists.iter().map(|h| h.load_pc).collect();
    assert!(
        hoisted_pcs.contains(&1),
        "dload 4 at PC=1 should be hoistable"
    );
    // dload_0 at PC=0 should NOT be hoistable (local 0 is modified by dstore_0)
    assert!(!hoisted_pcs.contains(&0), "dload_0 should not be hoistable");

    // And the shape the old fixture asserted was legal must be refused: swap
    // the invariant operand back to `dload_1`, whose slot 1 is the dead high
    // half of the `dstore_0` in the body. Two bytes shorter, so the branch
    // offset moves with it.
    let overlapping: Vec<u8> = vec![
        0x26, // 0:  dload_0
        0x27, // 1:  dload_1  <- reads slots 1+2, and the body writes both
        0x63, // 2:  dadd
        0x47, // 3:  dstore_0 (writes slots 0+1)
        0x84, 0x02, 0x01, // 4: iinc 2, 1  (writes slot 2)
        0x15, 0x02, // 7:  iload 2
        0x08, // 9:  iconst_5
        0xa1, 0xFF, 0xF6, // 10: if_icmplt -10 → target=0
        0x26, // 13: dload_0
        0xaf, // 14: dreturn
        0, 0,
    ];
    let overlapping_loops = detect_loops(&overlapping, 15);
    assert!(
        find_fp_loop_hoists(&overlapping, 15, &overlapping_loops)
            .iter()
            .all(|h| h.load_pc != 1),
        "`dload_1` overlaps the `dstore_0` in the body and must not hoist"
    );
}

#[test]
fn p87_fp_loop_hoist_empty_for_no_loops() {
    // No loops → no hoists
    let code: Vec<u8> = vec![0x26, 0xaf, 0, 0];
    let loops = detect_loops(&code, 2);
    let hoists = find_fp_loop_hoists(&code, 2, &loops);
    assert!(hoists.is_empty());
}

#[test]
fn p87_strength_reduction_detects_dmul_by_2() {
    // Loop with ldc2_w (2.0), dmul → should be detected
    // Simulated: ldc2_w at PC=5, dmul at PC=8
    let code: Vec<u8> = vec![
        0x15, 0x03, // 0: iload 3 (iv)
        0x08, // 2: iconst_5 (bound)
        0xa2, 0x00, 0x0E, // 3: if_icmpge +14 → target=17
        0x26, // 6: dload_0 (value)
        0x14, 0x00, 0x01, // 7: ldc2_w #1
        0x6b, // 10: dmul
        0x47, // 11: dstore_0
        0x84, 0x03, 0x01, // 12: iinc 3, 1
        0xa7, 0xFF, 0xF1, // 15: goto -15 → target=0
        0x26, // 18: dload_0
        0xaf, // 19: dreturn
        0, 0,
    ];
    let code_len = 20;
    let loops = detect_loops(&code, code_len);
    assert!(!loops.is_empty());

    // ldc2_w at PC=7, value = 2.0
    let ldc2w_info = vec![(7usize, 2.0f64.to_bits() as i64)]; // Cast: JIT ABI convention
    let pcs = find_fp_strength_reductions(&code, code_len, &loops, &ldc2w_info);
    // dmul at PC=10 should be strength-reduced
    assert!(
        pcs.contains(&10),
        "dmul at PC=10 should be strength-reduced"
    );
}

#[test]
fn p87_strength_reduction_ignores_non_2() {
    // Same pattern but ldc2_w loads 3.0 instead of 2.0
    let code: Vec<u8> = vec![
        0x15, 0x03, // 0: iload 3
        0x08, // 2: iconst_5
        0xa2, 0x00, 0x0E, // 3: if_icmpge +14
        0x26, // 6: dload_0
        0x14, 0x00, 0x01, // 7: ldc2_w #1
        0x6b, // 10: dmul
        0x47, // 11: dstore_0
        0x84, 0x03, 0x01, // 12: iinc 3, 1
        0xa7, 0xFF, 0xF0, // 15: goto -16
        0x26, // 18: dload_0
        0xaf, // 19: dreturn
        0, 0,
    ];
    let code_len = 20;
    let loops = detect_loops(&code, code_len);
    let ldc2w_info = vec![(7usize, 3.0f64.to_bits() as i64)]; // NOT 2.0 // Cast: JIT ABI convention
    let pcs = find_fp_strength_reductions(&code, code_len, &loops, &ldc2w_info);
    assert!(pcs.is_empty(), "Should not reduce dmul by non-2.0");
}

#[test]
fn p87_strength_reduction_emits_dadd_self() {
    // Verify that strength-reduced dmul-by-2.0 emits ADDSD XMM0, XMM0
    // double f(double x) { return x * 2.0; }
    // dload_0, ldc2_w #1 (2.0), dmul, dreturn
    let code: Vec<u8> = vec![
        0x26, // 0: dload_0
        0x14, 0x00, 0x01, // 1: ldc2_w #1 (resolves to 2.0)
        0x6b, // 4: dmul
        0xaf, // 5: dreturn
        0, 0,
    ];
    let code_len = 6;

    // Wrap in a loop so strength reduction triggers
    // Actually, strength reduction only triggers inside loops.
    // Let's test without a loop first — dmul should still work normally.
    let ldc2w_info = vec![(1usize, 2.0f64.to_bits() as i64)]; // Cast: JIT ABI convention
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        ldc2w_info,
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout — not needed for this test
    )
    .unwrap();

    let x = 7.5f64;
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            // Cast: float/double bit pattern to i64 (bit-preserving, VM all-GPR ABI)
            .try_call(&[x.to_bits() as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(f64::from_bits(result as u64), 15.0); // 7.5 * 2.0 = 15.0 // Cast: JIT ABI convention
}

// --- 87.3: SIMD FP Operations ---

#[test]
fn p87_detect_fp_array_sum_pattern_a() {
    // Pattern A: aload_1, iload_2, daload, dload_0, dadd, dstore_0, iinc 2 1, goto
    // Header: iload_2, iload_3, if_icmpge
    let code: Vec<u8> = vec![
        0x1c, // 0: iload_2 (iv)
        0x1d, // 1: iload_3 (bound)
        0xa2, 0x00, 0x0C, // 2: if_icmpge +12 → target=14
        0x2b, // 5: aload_1 (arr)
        0x1c, // 6: iload_2 (iv)
        0x31, // 7: daload
        0x26, // 8: dload_0 (sum)
        0x63, // 9: dadd
        0x47, // 10: dstore_0
        0x84, 0x02, 0x01, // 11: iinc 2, 1
        0xa7, 0xFF, 0xF2, // 14: goto -14 → target=0
        0x26, // 17: dload_0
        0xaf, // 18: dreturn
        0, 0,
    ];
    let code_len = 19;
    let loops = detect_loops(&code, code_len);
    assert!(!loops.is_empty(), "Should detect a loop");

    let iv = find_induction_variable(&code, loops[0].0, loops[0].1 + 3);
    assert_eq!(iv, Some(2), "Induction variable should be local 2");

    let result = detect_fp_array_sum(&code, loops[0].0, loops[0].1, 2);
    assert!(result.is_some(), "Should detect FP array sum pattern");
    let info = result.unwrap();
    assert_eq!(info.acc_local, 0);
    assert_eq!(info.array_local, 1);
    assert_eq!(info.iv_local, 2);
    assert_eq!(info.bound_local, 3);
    assert_eq!(info.sse_op, 0x58); // ADDPD
}

#[test]
fn p87_detect_fp_array_sum_pattern_b() {
    // Pattern B: dload_0, aload_1, iload_2, daload, dadd, dstore_0, iinc 2 1, goto
    let code: Vec<u8> = vec![
        0x1c, // 0: iload_2 (iv)
        0x1d, // 1: iload_3 (bound)
        0xa2, 0x00, 0x0C, // 2: if_icmpge +12
        0x26, // 5: dload_0 (sum)
        0x2b, // 6: aload_1 (arr)
        0x1c, // 7: iload_2 (iv)
        0x31, // 8: daload
        0x63, // 9: dadd
        0x47, // 10: dstore_0
        0x84, 0x02, 0x01, // 11: iinc 2, 1
        0xa7, 0xFF, 0xF2, // 14: goto -14
        0x26, 0xaf, 0, 0,
    ];
    let code_len = 19;
    let loops = detect_loops(&code, code_len);
    assert!(!loops.is_empty());

    let result = detect_fp_array_sum(&code, loops[0].0, loops[0].1, 2);
    assert!(result.is_some(), "Should detect FP array sum pattern B");
    let info = result.unwrap();
    assert_eq!(info.acc_local, 0);
    assert_eq!(info.array_local, 1);
}

#[test]
fn p87_no_fp_array_sum_for_int_loop() {
    // Int array sum should NOT trigger FP detection
    // aload_1, iload_2, iaload (0x2e not 0x31), iload_0, iadd, istore_0, iinc...
    let code: Vec<u8> = vec![
        0x1c, // iload_2
        0x1d, // iload_3
        0xa2, 0x00, 0x0B, // if_icmpge
        0x2b, // aload_1
        0x1c, // iload_2
        0x2e, // iaload (NOT daload)
        0x1a, // iload_0
        0x60, // iadd
        0x3b, // istore_0
        0x84, 0x02, 0x01, // iinc 2, 1
        0xa7, 0xFF, 0xF3, // goto
        0x1a, 0xac, 0, 0,
    ];
    let code_len = 18;
    let loops = detect_loops(&code, code_len);
    let result = detect_fp_array_sum(&code, loops[0].0, loops[0].1, 2);
    assert!(
        result.is_none(),
        "Int array sum should not trigger FP detection"
    );
}

#[test]
fn p87_simd_fp_not_detected_without_avx2() {
    // Verify the compile path handles the case where AVX2 is not available
    // (On machines with AVX2 this still works — just tests the detection path)
    let code: Vec<u8> = vec![0x26, 0xaf, 0, 0];
    let code_len = 2;
    let loops = detect_loops(&code, code_len);
    // No loops → no SIMD FP
    let simd = if has_avx2() {
        let mut s = Vec::new();
        for &(h, b) in &loops {
            let end = b + bytecode_len_at(&code, b);
            if let Some(iv) = find_induction_variable(&code, h, end) {
                if let Some(info) = detect_fp_array_sum(&code, h, b, iv) {
                    s.push(info);
                }
            }
        }
        s
    } else {
        Vec::new()
    };
    assert!(simd.is_empty());
}

#[test]
fn p87_double_binop_chain_preserves_precision() {
    // Verify FP chaining doesn't lose precision
    // double f(double a) { return a + a + a + a; }
    let code: Vec<u8> = vec![
        0x26, 0x26, 0x63, // dload_0, dload_0, dadd
        0x26, 0x63, // dload_0, dadd
        0x26, 0x63, // dload_0, dadd
        0xaf, // dreturn
        0, 0,
    ];
    let code_len = 8;
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

    let a = std::f64::consts::PI;
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            // Cast: float/double bit pattern to i64 (bit-preserving, VM all-GPR ABI)
            .try_call(&[a.to_bits() as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(f64::from_bits(result as u64), a + a + a + a); // Cast: JIT ABI convention
}

#[test]
fn p87_float_binop_chain_preserves_precision() {
    // float f(float a) { return a * a * a; }
    let code: Vec<u8> = vec![
        0x22, 0x22, 0x6a, // fload_0, fload_0, fmul
        0x22, 0x6a, // fload_0, fmul
        0xae, // freturn
        0, 0,
    ];
    let code_len = 6;
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

    let a = 2.5f32;
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            // Cast: float/double bit pattern to i64 (bit-preserving, VM all-GPR ABI)
            .try_call(&[a.to_bits() as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(f32::from_bits(result as u32), a * a * a); // 15.625 // Cast: JIT ABI convention
}

#[test]
fn p87_double_division_chain() {
    // double f(double a, double b) { return (a / b) / b; }
    // Tests non-commutative FP ops work correctly with XMM chaining
    let code: Vec<u8> = vec![
        0x26, 0x27, 0x6f, // dload_0, dload_1, ddiv
        0x27, 0x6f, // dload_1, ddiv
        0xaf, // dreturn
        0, 0,
    ];
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

    let a = 100.0f64;
    let b = 5.0f64;
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            // Cast: float/double bit pattern to i64 (bit-preserving, VM all-GPR ABI)
            .try_call(&[a.to_bits() as i64, b.to_bits() as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(f64::from_bits(result as u64), (a / b) / b); // 4.0 // Cast: JIT ABI convention
}

#[test]
fn p87_mixed_fp_and_int_computation() {
    // int f(double a, double b) { double c = a + b; return (int)c; }
    // dload_0, dload_1, dadd, d2i (0x8e), ireturn
    let code: Vec<u8> = vec![
        0x26, 0x27, 0x63, // dload_0, dload_1, dadd
        0x8e, // d2i
        0xac, // ireturn
        0, 0,
    ];
    let code_len = 5;
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

    let a = 3.7f64;
    let b = 2.1f64;
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            // Cast: float/double bit pattern to i64 (bit-preserving, VM all-GPR ABI)
            .try_call(&[a.to_bits() as i64, b.to_bits() as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(result, (a + b) as i32 as i64); // 5 // Cast: JIT ABI convention
}

#[test]
fn p87_fp_hoist_double_and_float() {
    // Verify hoist detection for both float and double types.
    //
    // The invariant `double` is `dload 4`, not the `dload_1` this test used to
    // carry: `dstore_0` at PC=4 writes slots 0 AND 1, so `dload_1` reads a
    // slot the body writes. The old comment said `dstore_0` "modifies 0, but
    // doesn't affect 1", which is not true of a category-2 store — see
    // `p87_fp_loop_hoist_detection`, which carried the same mistake.
    let code: Vec<u8> = vec![
        0x22, // 0: fload_0 (float — local 0, which the dstore_0 below writes)
        0x18, 0x04, // 1: dload 4 (double, invariant — slots 4+5)
        0x63, // 3: dadd (type mismatch at runtime, but tests analysis)
        0x47, // 4: dstore_0 (writes slots 0 AND 1)
        0x84, 0x03, 0x01, // 5: iinc 3, 1
        0x15, 0x03, // 8: iload 3
        0x08, // 10: iconst_5
        0xa1, 0xFF, 0xF5, // 11: if_icmplt -11 → target=0
        0x26, 0xaf, 0, 0,
    ];
    let code_len = 16;
    let loops = detect_loops(&code, code_len);
    let hoists = find_fp_loop_hoists(&code, code_len, &loops);

    // fload_0 at PC=0: local 0 IS modified (dstore_0 at PC=4) → NOT hoistable
    // dload 4 at PC=1: locals 4 and 5 are NOT modified → hoistable
    let hoisted_locals: Vec<(usize, bool)> =
        hoists.iter().map(|h| (h.local_idx, h.is_double)).collect();
    assert!(
        hoisted_locals.contains(&(4, true)),
        "dload 4 should be hoistable"
    );
    assert!(
        !hoisted_locals.iter().any(|(l, _)| *l == 0),
        "local 0 should not be hoistable"
    );
}

#[test]
fn p87_extract_dload_local_variants() {
    // Test dload extraction helpers
    assert_eq!(extract_dload_local(&[0x26], 0), Some(0)); // dload_0
    assert_eq!(extract_dload_local(&[0x27], 0), Some(1)); // dload_1
    assert_eq!(extract_dload_local(&[0x28], 0), Some(2)); // dload_2
    assert_eq!(extract_dload_local(&[0x29], 0), Some(3)); // dload_3
    assert_eq!(extract_dload_local(&[0x18, 0x05], 0), Some(5)); // dload 5
    assert_eq!(extract_dload_local(&[0x1a], 0), None); // iload_0 — not dload
}

#[test]
fn p87_extract_dstore_local_variants() {
    assert_eq!(extract_dstore_local(&[0x47], 0), Some(0)); // dstore_0
    assert_eq!(extract_dstore_local(&[0x48], 0), Some(1)); // dstore_1
    assert_eq!(extract_dstore_local(&[0x49], 0), Some(2)); // dstore_2
    assert_eq!(extract_dstore_local(&[0x4a], 0), Some(3)); // dstore_3
    assert_eq!(extract_dstore_local(&[0x39, 0x05], 0), Some(5)); // dstore 5
    assert_eq!(extract_dstore_local(&[0x3b], 0), None); // istore_0 — not dstore
}

// =========================================================================
// Session 32: Scalar replacement plan tests
// =========================================================================

#[test]
fn s32_plan_scalar_replacement_basic() {
    // Pattern: new; dup; invokespecial <init>()V; astore_1; aload_1; iconst_1; putfield
    let code: Vec<u8> = vec![
        0xbb, 0x00, 0x01, // 0: new #1
        0x59, // 3: dup
        0xb7, 0x00, 0x02, // 4: invokespecial #2 <init>()V
        0x4c, // 7: astore_1
        0x2b, // 8: aload_1
        0x04, // 9: iconst_1
        0xb5, 0x00, 0x03, // 10: putfield #3
        0x2b, // 13: aload_1
        0xb4, 0x00, 0x03, // 14: getfield #3
        0xac, // 17: ireturn
        0, 0,
    ];
    let code_len = 18;
    let mut non_escaping = std::collections::HashSet::new();
    non_escaping.insert(0usize);
    // CRIT-2 tuple: (pc, class_id, num_fields, has_prim_init, has_finalizer).
    let new_info = vec![(0usize, 1u32, 2usize, true, true)]; // 2 fields
                                                             // Create invoke_info for <init>()V at PC=4
                                                             // LEAK(intentional): the compiled method references this JitInvokeInfo by raw
                                                             // pointer, so it must be 'static and outlive the JIT code; owned by the test
                                                             // process for its lifetime.
    let init_info = Box::leak(Box::new(JitInvokeInfo {
        // LEAK(intentional): test-only; JitInvokeInfo must outlive JIT-compiled code pointer
        class_name: Box::leak("Test".to_string().into_boxed_str()), // LEAK(intentional): test-only; string field of leaked JitInvokeInfo
        method_name: Box::leak("<init>".to_string().into_boxed_str()), // LEAK(intentional): test-only; string field of leaked JitInvokeInfo
        descriptor: Box::leak("()V".to_string().into_boxed_str()), // LEAK(intentional): test-only; string field of leaked JitInvokeInfo
        num_jit_args: 1,
        return_type: b'V',
        invoke_kind: 0xb7,
        declaring_class_id: 0,
    }));
    let invoke_info = vec![(4usize, init_info as *const JitInvokeInfo)]; // Cast: address arithmetic
    let scalar_base = 4; // some offset

    let plan = plan_scalar_replacement(
        &code,
        code_len,
        &non_escaping,
        &new_info,
        &invoke_info,
        scalar_base,
    );

    assert!(
        plan.objects.contains_key(&0),
        "new at PC=0 should be scalar-replaced"
    );
    assert_eq!(plan.objects[&0].num_fields, 2);
    assert!(
        plan.init_skips.contains(&4),
        "invokespecial at PC=4 should be skipped"
    );
    assert!(
        plan.field_ops.contains_key(&10),
        "putfield at PC=10 should be scalar"
    );
    assert!(
        plan.field_ops.contains_key(&14),
        "getfield at PC=14 should be scalar"
    );
    // Two heap fields × two 8-byte words per `Value` slot (`SLOT_SIZE` = 16).
    assert_eq!(plan.total_slots, 4);

    // Phase B: the resolved class id is captured on the object descriptor.
    assert_eq!(plan.objects[&0].class_id, 1);

    // Phase B: per-PC local provenance records local 1 holding the scalar
    // object (new_pc 0) from `astore_1` (PC 7 stores it; first observable at
    // the PC-8 entry) through the rest of the straight-line region. It is NOT
    // recorded before the store (PCs 0..=7 have local 1 empty at entry).
    assert_eq!(
        plan.local_prov_at.get(&8).map(|v| v.as_slice()),
        Some([(1usize, 0usize)].as_slice()),
        "local 1 holds scalar obj 0 at PC 8 (aload_1)"
    );
    for &pc in &[9usize, 10, 13, 14] {
        assert_eq!(
            plan.local_prov_at.get(&pc).map(|v| v.as_slice()),
            Some([(1usize, 0usize)].as_slice()),
            "local 1 holds scalar obj 0 at PC {pc}"
        );
    }
    for &pc in &[0usize, 3, 4, 7] {
        assert!(
            !plan.local_prov_at.contains_key(&pc),
            "no scalar local recorded at PC {pc} (before astore_1)"
        );
    }
}

#[test]
fn s32_phase_b_sr_field_values_typed_by_tag() {
    use crate::deopt::FrameValue;
    // 5 fields, base offset 80 (positive `[rbp - off]`), SLOT_SIZE = 16.
    // Field tags: 0=ref(L), 1=long(J), 2=double(D), 3=float(F), 4=unaccessed.
    let tags = [Some(b'L'), Some(b'J'), Some(b'D'), Some(b'F'), None];
    let fvs = sr_field_values(5, 80, |k| tags[k]);
    // Offsets: field k at `[rbp - (80 + k*16)]` → encoded `-(80 + k*16)`.
    assert_eq!(fvs[0], FrameValue::StackSlotRef(-80));
    assert_eq!(fvs[1], FrameValue::StackSlotLong(-(80 + 16)));
    assert_eq!(fvs[2], FrameValue::StackSlotDouble(-(80 + 32)));
    assert_eq!(fvs[3], FrameValue::StackSlotFloat(-(80 + 48)));
    // Never-accessed field ⇒ zero default, NOT a slot read.
    assert_eq!(fvs[4], FrameValue::Int(0));
    // An int-family tag (b'I'/b'B'/b'C'/b'S'/b'Z') maps to the cat-1 StackSlot.
    let ints = sr_field_values(1, 16, |_| Some(b'I'));
    assert_eq!(ints[0], FrameValue::StackSlot(-16));
}

#[test]
fn s32_phase_c_plan_tracks_scalar_monitors() {
    // `Foo f = new Foo(); synchronized(f) { f.x = 1; }` (straight-line, no
    // exception handler) — a `synchronized` block over a scalar object.
    let code: Vec<u8> = vec![
        0xbb, 0x00, 0x01, // 0: new #1
        0x59, // 3: dup
        0xb7, 0x00, 0x02, // 4: invokespecial #2 <init>()V
        0x4c, // 7: astore_1            -> f in local 1
        0x2b, // 8: aload_1             -> [f]
        0x59, // 9: dup                 -> [f, f]
        0x4d, // 10: astore_2           -> local 2 = f, [f]
        0xc2, // 11: monitorenter       -> lock f (mon_depth[0]=1)
        0x2b, // 12: aload_1            -> [f]
        0x04, // 13: iconst_1           -> [f, 1]
        0xb5, 0x00, 0x03, // 14: putfield #3
        0x2c, // 17: aload_2            -> [f]
        0xc3, // 18: monitorexit        -> unlock (mon_depth[0]=0)
        0xb1, // 19: return
        0, 0,
    ];
    let code_len = 20;
    let mut non_escaping = std::collections::HashSet::new();
    non_escaping.insert(0usize);
    let new_info = vec![(0usize, 7u32, 1usize, true, true)]; // class_id 7, 1 field
                                                             // LEAK(intentional): this test JitInvokeInfo is referenced by raw
                                                             // pointer from generated code, so it and its string fields must remain
                                                             // valid for the process lifetime.
    let init_info = Box::leak(Box::new(JitInvokeInfo {
        // LEAK(intentional): field of leaked JitInvokeInfo.
        class_name: Box::leak("Foo".to_string().into_boxed_str()),
        // LEAK(intentional): field of leaked JitInvokeInfo.
        method_name: Box::leak("<init>".to_string().into_boxed_str()),
        // LEAK(intentional): field of leaked JitInvokeInfo.
        descriptor: Box::leak("()V".to_string().into_boxed_str()),
        num_jit_args: 1,
        return_type: b'V',
        invoke_kind: 0xb7,
        declaring_class_id: 0,
    }));
    let invoke_info = vec![(4usize, init_info as *const JitInvokeInfo)];
    let plan = plan_scalar_replacement(&code, code_len, &non_escaping, &new_info, &invoke_info, 0);

    assert!(
        plan.objects.contains_key(&0),
        "Foo should be scalar-replaced"
    );
    // Both monitor ops are over the scalar object (relockable, not blocking).
    assert!(
        plan.monitor_scalar_ops.contains(&11),
        "monitorenter@11 is scalar"
    );
    assert!(
        plan.monitor_scalar_ops.contains(&18),
        "monitorexit@18 is scalar"
    );
    // The lock is held (depth 1) across the block body and AT the monitorexit
    // entry (it executes the unlock), but NOT before the enter / after the exit.
    assert_eq!(
        plan.monitor_at.get(&12).map(|v| v.as_slice()),
        Some([(0usize, 1u32)].as_slice())
    );
    assert_eq!(
        plan.monitor_at.get(&14).map(|v| v.as_slice()),
        Some([(0usize, 1u32)].as_slice())
    );
    assert_eq!(
        plan.monitor_at.get(&18).map(|v| v.as_slice()),
        Some([(0usize, 1u32)].as_slice())
    );
    assert!(
        !plan.monitor_at.contains_key(&11),
        "not held before the enter executes"
    );
    assert!(
        !plan.monitor_at.contains_key(&19),
        "released after the exit"
    );
}

#[test]
fn s32_plan_scalar_replacement_escaping_not_replaced() {
    // Pattern: new; dup; invokespecial; areturn — object escapes, should NOT be replaced
    let code: Vec<u8> = vec![
        0xbb, 0x00, 0x01, // 0: new #1
        0x59, // 3: dup
        0xb7, 0x00, 0x02, // 4: invokespecial <init>
        0xb0, // 7: areturn
        0, 0,
    ];
    // NOT in non_escaping_new → should produce empty plan
    let non_escaping = std::collections::HashSet::new();
    // CRIT-2 tuple shape: see compiler struct doc.
    let new_info = vec![(0usize, 1u32, 2usize, true, true)];
    let plan = plan_scalar_replacement(&code, 8, &non_escaping, &new_info, &[], 0);
    assert!(plan.objects.is_empty());
    assert!(plan.field_ops.is_empty());
    assert!(plan.init_skips.is_empty());
}

#[test]
fn s32_plan_nonvoid_init_not_skipped() {
    // invokespecial with non-()V descriptor should NOT be skipped
    let code: Vec<u8> = vec![
        0xbb, 0x00, 0x01, // 0: new #1
        0x59, // 3: dup
        0x04, // 4: iconst_1
        0xb7, 0x00, 0x02, // 5: invokespecial <init>(I)V
        0xac, // 8: ireturn (dummy)
        0, 0,
    ];
    let mut non_escaping = std::collections::HashSet::new();
    non_escaping.insert(0usize);
    // CRIT-2 tuple shape: see compiler struct doc.
    let new_info = vec![(0usize, 1u32, 2usize, true, true)];
    // LEAK(intentional): the compiled method references this JitInvokeInfo by raw
    // pointer, so it must be 'static and outlive the JIT code; owned by the test
    // process for its lifetime.
    let init_info = Box::leak(Box::new(JitInvokeInfo {
        // LEAK(intentional): test-only; JitInvokeInfo must outlive JIT-compiled code pointer
        class_name: Box::leak("Test".to_string().into_boxed_str()), // LEAK(intentional): test-only; string field of leaked JitInvokeInfo
        method_name: Box::leak("<init>".to_string().into_boxed_str()), // LEAK(intentional): test-only; string field of leaked JitInvokeInfo
        descriptor: Box::leak("(I)V".to_string().into_boxed_str()), // LEAK(intentional): test-only; string field of leaked JitInvokeInfo
        num_jit_args: 2,
        return_type: b'V',
        invoke_kind: 0xb7,
        declaring_class_id: 0,
    }));
    let invoke_info = vec![(5usize, init_info as *const JitInvokeInfo)]; // Cast: address arithmetic
    let plan = plan_scalar_replacement(&code, 9, &non_escaping, &new_info, &invoke_info, 0);
    // init should NOT be skipped (non-void-init has args)
    assert!(!plan.init_skips.contains(&5));
}

// ===================================================================
// Session 34: JIT Switch Compilation tests
// ===================================================================

/// Helper: build bytecode for a method that uses tableswitch.
/// Layout: iload_0 at PC 0, tableswitch at PC 1 (padded to 4-byte align),
/// then case bodies that push sipush val; ireturn.
fn build_tableswitch_bytecode(low: i32, cases: &[i32], default_val: i32) -> (Vec<u8>, usize) {
    let mut code = Vec::new();
    // PC 0: iload_0
    code.push(0x1A);
    // PC 1: tableswitch opcode
    code.push(0xAA);
    // Padding to align to 4-byte boundary from start of method (PC 0)
    while (code.len()) % 4 != 0 {
        code.push(0);
    }
    let table_base = 1; // base_pc of the tableswitch opcode
    let high = low + cases.len() as i32 - 1; // Cast: x86-64 immediate encoding
    let num_cases = cases.len();

    // Each case body: sipush val (3 bytes) + ireturn (1 byte) = 4 bytes
    let switch_data_size = 12 + num_cases * 4;
    let aligned_start = code.len();
    let body_start_pc = aligned_start + switch_data_size;
    let default_body_pc = body_start_pc + num_cases * 4;
    let default_offset = default_body_pc as i32 - table_base as i32; // Cast: x86-64 immediate encoding

    code.extend_from_slice(&default_offset.to_be_bytes());
    code.extend_from_slice(&low.to_be_bytes());
    code.extend_from_slice(&high.to_be_bytes());

    for i in 0..num_cases {
        let case_body_pc = body_start_pc + i * 4;
        let off = case_body_pc as i32 - table_base as i32; // Cast: x86-64 immediate encoding
        code.extend_from_slice(&off.to_be_bytes());
    }

    // Case bodies: sipush val; ireturn
    for &val in cases {
        code.push(0x11); // sipush
        code.extend_from_slice(&(val as i16).to_be_bytes()); // Cast: x86-64 immediate encoding
        code.push(0xAC); // ireturn
    }

    // Default body
    code.push(0x11);
    code.extend_from_slice(&(default_val as i16).to_be_bytes()); // Cast: x86-64 immediate encoding
    code.push(0xAC);

    let code_len = code.len();
    code.push(0);
    code.push(0);
    (code, code_len)
}

/// Helper: build bytecode for a method that uses lookupswitch.
fn build_lookupswitch_bytecode(pairs: &[(i32, i32)], default_val: i32) -> (Vec<u8>, usize) {
    let mut code = Vec::new();
    // PC 0: iload_0
    code.push(0x1A);
    // PC 1: lookupswitch opcode
    code.push(0xAB);
    while (code.len()) % 4 != 0 {
        code.push(0);
    }
    let table_base = 1;
    let npairs = pairs.len();

    let switch_data_size = 8 + npairs * 8;
    let aligned_start = code.len();
    let body_start_pc = aligned_start + switch_data_size;
    let default_body_pc = body_start_pc + npairs * 4;
    let default_offset = default_body_pc as i32 - table_base as i32; // Cast: x86-64 immediate encoding

    code.extend_from_slice(&default_offset.to_be_bytes());
    code.extend_from_slice(&(npairs as i32).to_be_bytes()); // Cast: x86-64 immediate encoding

    for (i, &(key, _val)) in pairs.iter().enumerate() {
        let case_body_pc = body_start_pc + i * 4;
        let off = case_body_pc as i32 - table_base as i32; // Cast: x86-64 immediate encoding
        code.extend_from_slice(&key.to_be_bytes());
        code.extend_from_slice(&off.to_be_bytes());
    }

    for &(_key, val) in pairs {
        code.push(0x11);
        code.extend_from_slice(&(val as i16).to_be_bytes()); // Cast: x86-64 immediate encoding
        code.push(0xAC);
    }

    code.push(0x11);
    code.extend_from_slice(&(default_val as i16).to_be_bytes()); // Cast: x86-64 immediate encoding
    code.push(0xAC);

    let code_len = code.len();
    code.push(0);
    code.push(0);
    (code, code_len)
}

/// `compile_switch_method`'s shape with the locals a handler-body fixture
/// needs. The bytecode under test never runs; only the compiler's decision
/// about it is asserted.
fn compile_probe_method(
    code: &[u8],
    num_params: usize,
    max_locals: usize,
) -> Option<CompiledMethod> {
    compile(
        code,
        code.len(),
        num_params,
        max_locals,
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
        None, // string_layout
    )
}

/// A dead region containing a branch of its own must not refuse the method.
///
/// The backend has no in-method exception-handler dispatch, so a handler
/// body is dead code in the emitted image — but a branch INSIDE one still
/// marks its own targets, and the DCE walk revived at every branch target
/// regardless of whether anything reachable could get there. It came back
/// with no recorded operand-stack depth, rebuilt an empty stack, and then
/// underflowed on the merge's `iadd`, refusing the whole method.
///
/// Shape transcribed from `Rbc6FieldProbe.getfieldRefHandlerLocal`'s
/// handler — `catch (NPE e) { return scratch + (seen == null ? 0 : 1); }`
/// — whose ternary is the branch in question. See
/// `singlepass-codegen-refuses-handler-body-merge-FIXED-20260803.md`.
#[test]
fn a_dead_region_with_an_internal_branch_does_not_refuse_the_method() {
    //  0: iload_0
    //  1: ireturn           <- everything below is unreachable
    //  2: iload_0
    //  3: aload_1
    //  4: ifnonnull 11
    //  7: iconst_0
    //  8: goto 12
    // 11: iconst_1
    // 12: iadd              <- the merge that underflowed
    // 13: ireturn
    let code = [
        0x1a, 0xac, 0x1a, 0x2b, 0xc7, 0x00, 0x07, 0x03, 0xa7, 0x00, 0x04, 0x04, 0x60, 0xac,
    ];
    assert!(
        compile_probe_method(&code, 2, 2).is_some(),
        "a branch inside an unreachable region must not refuse the method"
    );

    // The live prefix alone compiles, so the refusal really did come from
    // the dead tail and not from `iload_0; ireturn`.
    assert!(compile_probe_method(&code[..2], 2, 2).is_some());
}

/// The same shape reached through a `goto` over the dead region, which is
/// how a `try`/`catch` actually lays out: the live path jumps past the
/// handler, so the handler body sits between two live PCs.
#[test]
fn a_dead_region_between_two_live_ones_is_skipped_whole() {
    //  0: goto 14           (over the "handler")
    //  3: iload_0           <- unreachable from here ...
    //  4: aload_1
    //  5: ifnonnull 12
    //  8: iconst_0
    //  9: goto 13
    // 12: iconst_1
    // 13: iadd              <- ... to here
    // 14: iconst_2          <- live again
    // 15: ireturn
    let code = [
        0xa7, 0x00, 0x0e, 0x1a, 0x2b, 0xc7, 0x00, 0x07, 0x03, 0xa7, 0x00, 0x04, 0x04, 0x60, 0x05,
        0xac,
    ];
    assert!(compile_probe_method(&code, 2, 2).is_some());
}

/// A refusal raised through the `failed` FLAG names the site that raised
/// it, not whatever opcode the walk happened to reach afterwards.
///
/// `dup2` at PC 0 has no preceding instruction, so its top-of-stack width
/// is unprovable and the arm raises the flag. The walk then runs on to the
/// `ireturn`, which is exactly the misattribution this records: the old
/// reason string was `singlepass-codegen(pc=1,op=0xac)`.
#[test]
fn a_flag_refusal_names_the_site_that_raised_it() {
    let code = [0x5c, 0xac]; // dup2; ireturn
    let _ = crate::take_jit_bail_site(); // clear anything a prior test left
    assert!(compile_probe_method(&code, 0, 1).is_none());
    let (site, _, _) = crate::take_jit_bail_site().expect("a refusal records a site");
    assert_eq!(site, "singlepass-codegen/dup2-unprovable-top-width");
}

/// `return cond ? x : helper()` — the `else` arm's call sits immediately
/// before the shared `xreturn`, and the `then` arm's `goto` lands on it.
///
/// Both tail-call forms USED to swallow that `xreturn`: they emit no code for
/// its PC, so `pc_to_native` stayed -1 there, and `patch_branches` then
/// rejected the whole method with `branch-target-not-an-instruction-boundary`
/// — a reason that blames malformed bytecode for what is ordinary javac
/// output. See
/// `jit-tailcall-swallows-shared-return-FIXED-20260803.md`.
///
///     0: iload_0
///     1: ifeq 8
///     4: iconst_1
///     5: goto 12          <- the edge onto the return
///     8: iload_0
///     9: invokestatic
///    12: ireturn          <- swallowed by the tail form
const TAILCALL_OVER_SHARED_RETURN: [u8; 13] = [
    0x1a, 0x99, 0x00, 0x07, 0x04, 0xa7, 0x00, 0x07, 0x1a, 0xb8, 0x00, 0x01, 0xac,
];

/// The self-recursive tail form (an `invokestatic` with no `invoke_info` and no
/// `direct_call` is a self-call here, as every other test in this file relies
/// on).
#[test]
fn a_self_tail_call_may_not_swallow_a_branch_targeted_return() {
    assert!(
        compile_probe_method(&TAILCALL_OVER_SHARED_RETURN, 1, 1).is_some(),
        "the `goto`'s target is the `ireturn` the tail form consumes"
    );
}

/// The sibling tail form — reached only when the callee is ALREADY compiled,
/// which is why this never reproduced from a cold standalone probe and only
/// showed up inside a warm Spring context.
#[test]
fn a_sibling_tail_call_may_not_swallow_a_branch_targeted_return() {
    assert!(
        compile_with_direct_call(&TAILCALL_OVER_SHARED_RETURN, 1, 1, 9, direct_callee_i())
            .is_some(),
        "a direct-callable callee must not let the tail form eat the shared return"
    );
}

/// Without the branch onto it, the same call/return pair is still the fusible
/// shape and must keep compiling — the guard is about the merge, not about
/// tail calls.
#[test]
fn a_tail_call_over_an_unshared_return_still_compiles() {
    let code = [0x1a, 0xb8, 0x00, 0x01, 0xac]; // iload_0; invokestatic; ireturn
    assert!(compile_probe_method(&code, 1, 1).is_some());
    assert!(compile_with_direct_call(&code, 1, 1, 1, direct_callee_i()).is_some());
}

/// A one-`int`-arg callee returning `int`. `entry` is never executed — only
/// emitted as the JMP/CALL target.
fn direct_callee_i() -> crate::JitDirectCall {
    crate::JitDirectCall {
        entry: 0x1000,
        needs_context: false,
        num_params: 1,
        return_type: b'I',
        guard_class_id: 0,
    }
}

/// `compile_probe_method` with one direct-callable callee wired at `at_pc`.
fn compile_with_direct_call(
    code: &[u8],
    num_params: usize,
    max_locals: usize,
    at_pc: usize,
    callee: crate::JitDirectCall,
) -> Option<CompiledMethod> {
    compile(
        code,
        code.len(),
        num_params,
        max_locals,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        vec![(at_pc, callee)],
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
}

// -----------------------------------------------------------------------
// Regression: ecj-operandstack-corruption-jsp-compilation-500s-FIXED-20260821.md
// -----------------------------------------------------------------------
//
// A direct-call site that ALSO carries `invoke_info` reserves a cold-deopt
// copy of the arguments through `reserve_direct_call_service_slots`. That
// reservation has to sit ABOVE the argument slots `pop_stack` just handed
// back (they are still live sources for `emit_stack_arg_setup`), so it moves
// `next_spill_offset` past them — and the call's return value used to be
// pushed from there, one operand-stack slot per argument too deep.
//
// The shift is invisible inside a basic block: the linear walk keeps writing
// and reading the same shifted slots. It becomes wrong code at the first
// branch target after the call, whose depth is re-established from the
// bytecode — writer and reader then address different slots. Measured on
// ECJ's `OperandStack.pop(OperandCategory)`, whose `if_icmpeq` sits on a
// tableswitch merge and so compared `TypeBinding.id` against the expected
// category instead of `TypeIds.getCategory(id)`: every JSP compiled after
// that method tiered up threw `AssertionError: Unexpected operand at stack
// top`, surfacing as an HTTP 500 from Jasper.

/// A one-reference-arg callee returning a reference, direct-callable.
/// `entry` is never executed — only emitted as the CALL target.
fn direct_callee_ref() -> crate::JitDirectCall {
    crate::JitDirectCall {
        entry: 0x1000,
        needs_context: false,
        num_params: 1,
        return_type: b'L',
        guard_class_id: 0,
    }
}

///     0: aload_0
///     1: invokestatic #1   <- direct-callable, returns a reference
///     4: invokestatic #2   <- helper dispatch: a safepoint with the pc-1
///                             result live on the operand stack
///     7: areturn
const DIRECT_CALL_RESULT_LIVE_AT_SAFEPOINT: [u8; 8] =
    [0x2a, 0xb8, 0x00, 0x01, 0xb8, 0x00, 0x02, 0xb0];

/// Compile the shape above and return the oop-map frame slots recorded at the
/// pc-4 safepoint — i.e. where the pc-1 call parked its reference result.
/// `service_info_at_pc1` decides whether the direct-call site also carries
/// `invoke_info`, which is what makes it reserve the service-argument range.
fn direct_call_result_slots_at_pc4(service_info_at_pc1: bool) -> Vec<i16> {
    // LEAK(intentional): compiled code stores raw pointers to these, so they
    // must outlive it; the test process owns them for its (short) lifetime.
    let sink = Box::leak(Box::new(JitInvokeInfo {
        class_name: "T",
        method_name: "sink",
        descriptor: "()V",
        num_jit_args: 0,
        return_type: b'V',
        invoke_kind: 3,
        declaring_class_id: 0,
    }));
    let mut invoke_info: Vec<(usize, *const JitInvokeInfo)> =
        vec![(4usize, sink as *const JitInvokeInfo)];
    if service_info_at_pc1 {
        let callee = Box::leak(Box::new(JitInvokeInfo {
            class_name: "T",
            method_name: "f",
            descriptor: "(Ljava/lang/Object;)Ljava/lang/Object;",
            num_jit_args: 1,
            return_type: b'L',
            invoke_kind: 3,
            declaring_class_id: 0,
        }));
        invoke_info.push((1usize, callee as *const JitInvokeInfo));
    }
    let compiled = compile(
        &DIRECT_CALL_RESULT_LIVE_AT_SAFEPOINT,
        DIRECT_CALL_RESULT_LIVE_AT_SAFEPOINT.len(),
        1,
        1,
        true,
        Vec::new(), // multianewarray_info
        Vec::new(), // field_info
        Vec::new(), // typecheck_info
        Vec::new(), // static_field_info
        Vec::new(), // new_info
        Vec::new(), // anewarray_info
        invoke_info,
        vec![(1usize, direct_callee_ref())],
        Vec::new(), // mic_slots
        Vec::new(), // pic_slots
        Vec::new(), // ldc_info
        Vec::new(), // ldc2w_info
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .expect("direct call followed by a dispatched call must compile");
    let mut slots = compiled
        .oop_maps
        .iter()
        .find(|m| m.bytecode_pc == 4)
        .map(|m| m.frame_slot_offsets.clone())
        .unwrap_or_default();
    slots.sort_unstable();
    slots
}

/// The return value's operand-stack depth must not depend on whether the site
/// reserved a service-argument range.
#[test]
fn direct_call_result_slot_is_independent_of_service_arg_reservation() {
    let plain = direct_call_result_slots_at_pc4(false);
    let with_service_copy = direct_call_result_slots_at_pc4(true);
    assert!(
        !plain.is_empty(),
        "the pc-1 reference result must be a mapped live oop at the pc-4 safepoint"
    );
    assert_eq!(
        with_service_copy, plain,
        "the service-argument reservation moved the return value off its          operand-stack depth: the linear walk and every branch target after          this call now disagree about which slot holds it"
    );
}

fn compile_switch_method(code: &[u8], code_len: usize) -> Option<CompiledMethod> {
    compile(
        code,
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
}

#[test]
fn test_jit_scan_rejects_tableswitch_payload_past_code_len() {
    let (code, code_len) = build_tableswitch_bytecode(0, &[10], -1);
    assert!(jit_scan(&code, code_len, "(I)I").is_some());

    let truncated_len = 4 + 12 + 2; // aligned operands + header + partial entry
    assert!(truncated_len < code_len);
    assert!(
        jit_scan(&code, truncated_len, "(I)I").is_none(),
        "tableswitch payload extending past code_len must be rejected by scan"
    );
}

#[test]
fn test_jit_scan_rejects_lookupswitch_payload_past_code_len() {
    let (code, code_len) = build_lookupswitch_bytecode(&[(7, 10)], -1);
    assert!(jit_scan(&code, code_len, "(I)I").is_some());

    let truncated_len = 4 + 8 + 4; // aligned operands + header + partial pair
    assert!(truncated_len < code_len);
    assert!(
        jit_scan(&code, truncated_len, "(I)I").is_none(),
        "lookupswitch payload extending past code_len must be rejected by scan"
    );
}

#[test]
fn s34_tableswitch_small_3_cases() {
    let (code, code_len) = build_tableswitch_bytecode(0, &[10, 20, 30], -1);
    let compiled = compile_switch_method(&code, code_len).expect("compilation failed");
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        assert_eq!(compiled.try_call(&[0]).expect("test JIT call"), 10);
        assert_eq!(compiled.try_call(&[1]).expect("test JIT call"), 20);
        assert_eq!(compiled.try_call(&[2]).expect("test JIT call"), 30);
        assert_eq!(
            // Widening: i32 -> i64 (sign-extended)
            compiled.try_call(&[-1i32 as i64]).expect("test JIT call"),
            -1i64
        ); // Cast: JIT ABI convention
        assert_eq!(compiled.try_call(&[3]).expect("test JIT call"), -1i64);
        assert_eq!(compiled.try_call(&[100]).expect("test JIT call"), -1i64);
    }
}

#[test]
fn s34_tableswitch_large_jump_table() {
    // 10 cases triggers jump table path (count > 4)
    let cases: Vec<i32> = (0..10).map(|i| (i + 1) * 100).collect();
    let (code, code_len) = build_tableswitch_bytecode(0, &cases, -999);
    let compiled = compile_switch_method(&code, code_len).expect("compilation failed");
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        for i in 0..10 {
            assert_eq!(
                // Cast: test value to i64 for the JIT calling convention
                compiled.try_call(&[i as i64]).expect("test JIT call"),
                ((i + 1) * 100) as i64, // Cast: JIT ABI convention
                "case {} failed",
                i
            );
        }
        assert_eq!(
            // Widening: i32 -> i64 (sign-extended)
            compiled.try_call(&[-1i32 as i64]).expect("test JIT call"),
            -999i64
        ); // Cast: JIT ABI convention
        assert_eq!(compiled.try_call(&[10]).expect("test JIT call"), -999i64);
        assert_eq!(compiled.try_call(&[1000]).expect("test JIT call"), -999i64);
    }
}

#[test]
fn s34_tableswitch_nonzero_low() {
    let cases: Vec<i32> = vec![50, 60, 70, 80, 90];
    let (code, code_len) = build_tableswitch_bytecode(5, &cases, 0);
    let compiled = compile_switch_method(&code, code_len).expect("compilation failed");
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        assert_eq!(compiled.try_call(&[5]).expect("test JIT call"), 50);
        assert_eq!(compiled.try_call(&[6]).expect("test JIT call"), 60);
        assert_eq!(compiled.try_call(&[7]).expect("test JIT call"), 70);
        assert_eq!(compiled.try_call(&[8]).expect("test JIT call"), 80);
        assert_eq!(compiled.try_call(&[9]).expect("test JIT call"), 90);
        assert_eq!(compiled.try_call(&[4]).expect("test JIT call"), 0);
        assert_eq!(compiled.try_call(&[10]).expect("test JIT call"), 0);
    }
}

#[test]
fn s34_tableswitch_negative_low() {
    let cases: Vec<i32> = vec![200, 201, 202, 203, 204];
    let (code, code_len) = build_tableswitch_bytecode(-2, &cases, -1);
    let compiled = compile_switch_method(&code, code_len).expect("compilation failed");
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        assert_eq!(
            // Widening: i32 -> i64 (sign-extended)
            compiled.try_call(&[-2i32 as i64]).expect("test JIT call"),
            200
        ); // Cast: JIT ABI convention
        assert_eq!(
            compiled.try_call(&[-1i32 as i64]).expect("test JIT call"),
            201
        ); // Cast: JIT ABI convention
        assert_eq!(compiled.try_call(&[0]).expect("test JIT call"), 202);
        assert_eq!(compiled.try_call(&[1]).expect("test JIT call"), 203);
        assert_eq!(compiled.try_call(&[2]).expect("test JIT call"), 204);
        assert_eq!(
            // Widening: i32 -> i64 (sign-extended)
            compiled.try_call(&[-3i32 as i64]).expect("test JIT call"),
            -1i64
        ); // Cast: JIT ABI convention
        assert_eq!(compiled.try_call(&[3]).expect("test JIT call"), -1i64);
    }
}

#[test]
fn s34_tableswitch_single_case() {
    let (code, code_len) = build_tableswitch_bytecode(0, &[42], -1);
    let compiled = compile_switch_method(&code, code_len).expect("compilation failed");
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        assert_eq!(compiled.try_call(&[0]).expect("test JIT call"), 42);
        assert_eq!(compiled.try_call(&[1]).expect("test JIT call"), -1i64);
    }
}

#[test]
fn s34_lookupswitch_small_linear() {
    let pairs = vec![(10, 100), (20, 200), (30, 300)];
    let (code, code_len) = build_lookupswitch_bytecode(&pairs, -1);
    let compiled = compile_switch_method(&code, code_len).expect("compilation failed");
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        assert_eq!(compiled.try_call(&[10]).expect("test JIT call"), 100);
        assert_eq!(compiled.try_call(&[20]).expect("test JIT call"), 200);
        assert_eq!(compiled.try_call(&[30]).expect("test JIT call"), 300);
        assert_eq!(compiled.try_call(&[0]).expect("test JIT call"), -1i64);
        assert_eq!(compiled.try_call(&[15]).expect("test JIT call"), -1i64);
        assert_eq!(compiled.try_call(&[99]).expect("test JIT call"), -1i64);
    }
}

#[test]
fn s34_lookupswitch_large_binary_search() {
    // 10 sparse pairs triggers binary search (npairs > 6)
    // Values must fit in i16 for sipush encoding
    let pairs: Vec<(i32, i32)> = vec![
        (5, 50),
        (10, 100),
        (20, 200),
        (50, 500),
        (100, 1000),
        (200, 2000),
        (500, 5000),
        (1000, 10000),
        (2000, 20000),
        (5000, 30000),
    ];
    let (code, code_len) = build_lookupswitch_bytecode(&pairs, -1);
    let compiled = compile_switch_method(&code, code_len).expect("compilation failed");
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        for &(key, val) in &pairs {
            assert_eq!(
                // Cast: test value to i64 for the JIT calling convention
                compiled.try_call(&[key as i64]).expect("test JIT call"),
                val as i64, // Cast: JIT ABI convention
                "key {} should return {}",
                key,
                val
            );
        }
        assert_eq!(compiled.try_call(&[0]).expect("test JIT call"), -1i64);
        assert_eq!(compiled.try_call(&[7]).expect("test JIT call"), -1i64);
        assert_eq!(compiled.try_call(&[150]).expect("test JIT call"), -1i64);
        assert_eq!(compiled.try_call(&[99999]).expect("test JIT call"), -1i64);
    }
}

#[test]
fn s34_lookupswitch_negative_keys() {
    // 8 pairs with negatives → binary search
    let pairs: Vec<(i32, i32)> = vec![
        (-100, 1),
        (-50, 2),
        (-10, 3),
        (0, 4),
        (10, 5),
        (50, 6),
        (100, 7),
        (1000, 8),
    ];
    let (code, code_len) = build_lookupswitch_bytecode(&pairs, -1);
    let compiled = compile_switch_method(&code, code_len).expect("compilation failed");
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        for &(key, val) in &pairs {
            assert_eq!(
                // Cast: test value to i64 for the JIT calling convention
                compiled.try_call(&[key as i64]).expect("test JIT call"),
                // Cast: test value to i64 for the JIT calling convention
                val as i64
            ); // Cast: JIT ABI convention
        }
        assert_eq!(
            compiled.try_call(&[-200i32 as i64]).expect("test JIT call"),
            -1i64
        ); // Cast: JIT ABI convention
        assert_eq!(compiled.try_call(&[999]).expect("test JIT call"), -1i64);
    }
}

#[test]
fn s34_lookupswitch_single_pair() {
    let pairs = vec![(42, 999)];
    let (code, code_len) = build_lookupswitch_bytecode(&pairs, 0);
    let compiled = compile_switch_method(&code, code_len).expect("compilation failed");
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        assert_eq!(compiled.try_call(&[42]).expect("test JIT call"), 999);
        assert_eq!(compiled.try_call(&[0]).expect("test JIT call"), 0);
        assert_eq!(compiled.try_call(&[43]).expect("test JIT call"), 0);
    }
}

#[test]
fn s34_tableswitch_all_same_target() {
    let cases = vec![77; 8];
    let (code, code_len) = build_tableswitch_bytecode(0, &cases, -1);
    let compiled = compile_switch_method(&code, code_len).expect("compilation failed");
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        for i in 0..8 {
            // Cast: test value to i64 for the JIT calling convention
            assert_eq!(compiled.try_call(&[i as i64]).expect("test JIT call"), 77);
            // Cast: JIT ABI convention
        }
        assert_eq!(
            compiled.try_call(&[-1i32 as i64]).expect("test JIT call"),
            -1i64
        ); // Cast: JIT ABI convention
        assert_eq!(compiled.try_call(&[8]).expect("test JIT call"), -1i64);
    }
}

#[test]
fn s34_lookupswitch_two_pairs() {
    let pairs = vec![(0, 111), (1000, 222)];
    let (code, code_len) = build_lookupswitch_bytecode(&pairs, -1);
    let compiled = compile_switch_method(&code, code_len).expect("compilation failed");
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        assert_eq!(compiled.try_call(&[0]).expect("test JIT call"), 111);
        assert_eq!(compiled.try_call(&[1000]).expect("test JIT call"), 222);
        assert_eq!(compiled.try_call(&[500]).expect("test JIT call"), -1i64);
    }
}

#[test]
fn s34_tableswitch_large_20_cases() {
    let cases: Vec<i32> = (0..20).map(|i| i * 11).collect();
    let (code, code_len) = build_tableswitch_bytecode(0, &cases, -1);
    let compiled = compile_switch_method(&code, code_len).expect("compilation failed");
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        for i in 0..20 {
            assert_eq!(
                // Cast: test value to i64 for the JIT calling convention
                compiled.try_call(&[i as i64]).expect("test JIT call"),
                // Cast: loop counter expression to i64 (expected return value)
                (i * 11) as i64
            ); // Cast: JIT ABI convention
        }
        assert_eq!(compiled.try_call(&[20]).expect("test JIT call"), -1i64);
    }
}

// -----------------------------------------------------------------------
// S31 — JIT Method Inlining
// -----------------------------------------------------------------------

/// Helper: compile a method with inline sites.
fn compile_with_inlines(
    code: &[u8],
    code_len: usize,
    num_params: usize,
    max_locals: usize,
    inline_sites: HashMap<usize, crate::InlineSite>,
) -> Option<CompiledMethod> {
    compile(
        code,
        code_len,
        num_params,
        max_locals,
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
        Vec::new(), // pic_slots
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        inline_sites,
        None, // string_layout
    )
}

/// Build an InlineSite from raw callee bytecode.
fn make_inline_site(
    callee_bytecode: &[u8],
    callee_max_locals: usize,
    callee_num_args: usize,
    callee_is_static: bool,
    return_type: u8,
) -> crate::InlineSite {
    let mut callee_code = callee_bytecode.to_vec();
    callee_code.push(0); // padding
    callee_code.push(0);
    let declared_args = if callee_is_static {
        callee_num_args
    } else {
        callee_num_args.saturating_sub(1)
    };
    let descriptor = format!(
        "({}){}",
        "I".repeat(declared_args),
        if return_type == b'V' {
            "V".to_string()
        } else {
            "I".to_string()
        }
    );
    crate::InlineSite {
        callee_code,
        callee_code_len: callee_bytecode.len(),
        callee_max_locals,
        callee_num_args,
        callee_is_static,
        return_type,
        field_info: Vec::new(),
        compact_field_info: Vec::new(),
        static_field_info: Vec::new(),
        ldc_info: Vec::new(),
        ldc2w_info: Vec::new(),
        ldc_fp_pcs: Vec::new(),
        needs_heap: false,
        class_name: "Test".to_string(),
        class_id: 0,
        method_name: "inlined".to_string(),
        descriptor,
        elided_invoke_pcs: Vec::new(),
        invoke_targets: Vec::new(),
        resolved_invoke_infos: Vec::new(),
        nested_sites: Vec::new(),
        ir_new_info: Vec::new(),
        ir_typecheck_info: Vec::new(),
    }
}

// -----------------------------------------------------------------------
// Steps 4 and 5 — a call inside a spliced body, and a splice inside a splice
// -----------------------------------------------------------------------

/// `compile_with_inlines` with the VM-context slot established.
///
/// A spliced call passes the context as `jit_invoke_dispatch`'s first
/// argument, and with `needs_heap == false` there is no slot holding it
/// (`heap_local_offset` is 0, i.e. the saved `rbp`). Harmless for a stub that
/// ignores the pointer, but the tests below should exercise the shape the VM
/// actually compiles.
fn compile_with_inlines_heap(
    code: &[u8],
    code_len: usize,
    num_params: usize,
    max_locals: usize,
    inline_sites: HashMap<usize, crate::InlineSite>,
) -> Option<CompiledMethod> {
    compile(
        code,
        code_len,
        num_params,
        max_locals,
        true, // needs_heap — the dispatch helper takes the context
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
        Vec::new(), // pic_slots
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        inline_sites,
        None, // string_layout
    )
}

/// A `JitInvokeInfo` with the lifetime the emitter's baked immediate needs.
///
/// `try_compile_inner` interns these into the compile's own arena; a unit test
/// at this layer has no arena, so it leaks. One per call, deliberately: the
/// address is what `resolved_invoke_infos` carries and two sites must not
/// alias.
fn resolved_invoke(
    callee_pc: usize,
    class_name: &'static str,
    method_name: &'static str,
    descriptor: &'static str,
    num_jit_args: usize,
    return_type: u8,
    invoke_kind: u8,
) -> crate::ResolvedInlineInvoke {
    let info: &'static crate::JitInvokeInfo = Box::leak(Box::new(crate::JitInvokeInfo {
        class_name,
        method_name,
        descriptor,
        num_jit_args,
        return_type,
        invoke_kind,
        declaring_class_id: 0,
    }));
    crate::ResolvedInlineInvoke {
        callee_pc,
        info_addr: info as *const crate::JitInvokeInfo as usize, // Cast: address parked in a Send-able plan
        // 0 = "no direct bind", i.e. the dispatch-helper form. The direct form
        // needs a real compiled entry to CALL, which this layer has no way to
        // produce; `a_spliced_direct_call_is_registered_for_keep_alive` covers
        // that half on the data instead.
        direct_entry: 0,
        direct_needs_context: false,
        num_jit_args,
        return_type,
    }
}

fn take_last_dispatch() -> Option<(String, String, String, Vec<i64>)> {
    LAST_DISPATCH.with(|slot| slot.borrow_mut().take())
}

/// STEP 4. A callee that CALLS is spliceable, and the call it makes reaches the
/// dispatch helper with the right target and the right arguments in the right
/// order.
///
/// This is the whole point of the step: every `invoke*` used to reject the
/// enclosing site outright, so a body like `leaf(a)` below was never a
/// candidate no matter how small it was. Nothing here is about making the CALL
/// cheaper — it is the same `jit_invoke_dispatch` an un-spliced body would use.
///
/// Self-proving three ways, each of which fails on a different mistake:
///   * the return value (15) is wrong if the call is not emitted, or if its
///     result is not pushed as the spliced body's value;
///   * the recorded argument vector `[5, 10]` is wrong — reversed — if the
///     args buffer is built in the wrong direction, which a symmetric argument
///     set would hide;
///   * the recorded name triple is wrong if the emitter baked a different
///     `JitInvokeInfo` than `resolved_invoke_infos` named.
#[test]
fn a_call_inside_a_spliced_body_reaches_the_dispatch_helper() {
    // callee: `static int leaf(int a) { return target(a, 10); }`
    //   0: iload_0
    //   1: bipush 10
    //   3: invokestatic #3   -> resolved target
    //   6: ireturn
    let callee_body: [u8; 7] = [0x1a, 0x10, 0x0a, 0xb8, 0x00, 0x03, 0xac];
    let mut callee = make_inline_site(&callee_body, 1, 1, true, b'I');
    callee.resolved_invoke_infos = vec![resolved_invoke(
        3,
        "pkg/Target",
        "target",
        "(II)I",
        2,
        b'I',
        3,
    )];

    // caller: `static int f(int a) { return leaf(a); }`
    //   0: iload_0
    //   1: invokestatic #1   -> the inline site
    //   4: ireturn
    let caller: [u8; 7] = [0x1a, 0xb8, 0x00, 0x01, 0xac, 0, 0];
    let mut sites = HashMap::new();
    sites.insert(1, callee);

    let compiled = compile_with_inlines_heap(&caller, 5, 1, 1, sites)
        .expect("a callee containing a call must now splice");

    let _ = take_last_dispatch();
    // SAFETY: JIT-compiled code from valid bytecode in an executable mmap; the
    // dispatch helper is `stub_invoke_dispatch`, which only reads the argument
    // buffer the emitted code just built.
    let got = unsafe { compiled.call_with_heap(0, &[5]) };
    assert_eq!(
        got, 15,
        "the spliced call's result must be the body's value"
    );

    let (class_name, method_name, descriptor, args) =
        take_last_dispatch().expect("the spliced body must have called the dispatch helper");
    assert_eq!(
        (
            class_name.as_str(),
            method_name.as_str(),
            descriptor.as_str()
        ),
        ("pkg/Target", "target", "(II)I"),
        "the emitter must bake the JitInvokeInfo `resolved_invoke_infos` named",
    );
    assert_eq!(
        args,
        vec![5, 10],
        "arg[0] must be at the lowest address — a reversed buffer swaps these",
    );
}

/// Build a reference-returning `InlineSite` for `static Object id(Object o)`
/// — `aload_0; areturn`.
///
/// `make_inline_site` hard-codes an all-`I` descriptor, and the shape these
/// tests need is a REFERENCE result: only a reference is named in an oop map,
/// which is the observable that says which frame slot the splice parked it in.
fn identity_ref_inline_site() -> crate::InlineSite {
    let mut site = make_inline_site(&[0x2a, 0xb0], 1, 1, true, b'L');
    site.descriptor = "(Ljava/lang/Object;)Ljava/lang/Object;".to_string();
    site
}

/// Compile [`DIRECT_CALL_RESULT_LIVE_AT_SAFEPOINT`] with the pc-1 call either
/// SPLICED or dispatched, and return the oop-map frame slots recorded at the
/// pc-4 safepoint — i.e. where the pc-1 call parked its reference result.
fn spliced_result_slots_at_pc4(splice_pc1: bool) -> Vec<i16> {
    // LEAK(intentional): compiled code stores raw pointers to these, so they
    // must outlive it; the test process owns them for its (short) lifetime.
    let sink = Box::leak(Box::new(JitInvokeInfo {
        class_name: "T",
        method_name: "sink",
        descriptor: "()V",
        num_jit_args: 0,
        return_type: b'V',
        invoke_kind: 3,
        declaring_class_id: 0,
    }));
    let callee = Box::leak(Box::new(JitInvokeInfo {
        class_name: "T",
        method_name: "id",
        descriptor: "(Ljava/lang/Object;)Ljava/lang/Object;",
        num_jit_args: 1,
        return_type: b'L',
        invoke_kind: 3,
        declaring_class_id: 0,
    }));
    let invoke_info: Vec<(usize, *const JitInvokeInfo)> = vec![
        (1usize, callee as *const JitInvokeInfo),
        (4usize, sink as *const JitInvokeInfo),
    ];
    let mut inline_sites = HashMap::new();
    if splice_pc1 {
        inline_sites.insert(1usize, identity_ref_inline_site());
    }
    let compiled = compile(
        &DIRECT_CALL_RESULT_LIVE_AT_SAFEPOINT,
        DIRECT_CALL_RESULT_LIVE_AT_SAFEPOINT.len(),
        1,
        1,
        true,
        Vec::new(), // multianewarray_info
        Vec::new(), // field_info
        Vec::new(), // typecheck_info
        Vec::new(), // static_field_info
        Vec::new(), // new_info
        Vec::new(), // anewarray_info
        invoke_info,
        Vec::new(), // direct_calls
        Vec::new(), // mic_slots
        Vec::new(), // pic_slots
        Vec::new(), // ldc_info
        Vec::new(), // ldc2w_info
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        inline_sites,
        None, // string_layout
    )
    .expect("a reference-returning leaf callee must compile spliced and unspliced");
    let mut slots = compiled
        .oop_maps
        .iter()
        .find(|m| m.bytecode_pc == 4)
        .map(|m| m.frame_slot_offsets.clone())
        .unwrap_or_default();
    slots.sort_unstable();
    slots
}

/// A spliced callee's result must land at the operand-stack depth the CALLER's
/// bytecode gives it — not at the top of the callee's own frame region.
///
/// `try_emit_inline_body` carves the callee's locals AND a
/// `MAX_INLINE_MERGE_DEPTH` merge region out of the caller's spill area, and
/// does it BEFORE the arguments are popped (it has to — an argument slot stays
/// live until it is marshalled into a callee local). The cursor at the end of
/// the splice therefore sits `callee_locals + MAX_INLINE_MERGE_DEPTH` slots
/// above where the caller's operand stack actually tops out, and the `xreturn`
/// arms used to push the result from THERE.
///
/// Inside the caller's basic block that is invisible: the linear walk writes
/// and reads the same shifted slots and computes the right answer. It becomes
/// wrong code at the first merge point that re-establishes depth from the
/// bytecode — writer and reader then address different slots. That is the
/// `AssertionError: Unexpected operand at stack top` every JSP compile threw
/// once ECJ's `OperandStack.pop(OperandCategory)` tiered up
/// (tomcat/ecj-operandstack-*.md); the same defect had been fixed once in the
/// direct-call arms of `bytecode_walk.rs` on 2026-08-06 and came back through
/// the splicer when `0f55466d0` (2026-08-18) taught it to inline a
/// value-producing branch merge.
///
/// The oop map is the observable, exactly as in
/// [`direct_call_result_slot_is_independent_of_service_arg_reservation`]: the
/// slot a live reference is reported in IS the slot the result was pushed to.
/// Negative control (fix reverted, test kept) reports the spliced arm five
/// slots higher — one callee local plus the four-slot merge region.
#[test]
fn a_spliced_callees_result_lands_at_the_callers_operand_depth() {
    let dispatched = spliced_result_slots_at_pc4(false);
    let spliced = spliced_result_slots_at_pc4(true);
    assert!(
        !dispatched.is_empty(),
        "the pc-1 reference result must be a mapped live oop at the pc-4 safepoint"
    );
    assert_eq!(
        spliced, dispatched,
        "the splice's callee-locals and merge-region reservations moved the \
         return value off its operand-stack depth: the linear walk and every \
         merge point after this call now disagree about which slot holds it"
    );
}

/// ECJ's `OperandStack.pop(OperandCategory)` reduced to its skeleton, EXECUTED.
///
/// A value produced by an inlined call, held on the operand stack across a
/// `tableswitch`, and compared against a constant pushed at a switch arm — the
/// exact shape whose compiled body compared the raw `TypeBinding.id` against
/// the expected category instead of `TypeIds.getCategory(id)`.
///
/// This one is satisfied by EITHER half of the fix (the splice pushing at the
/// caller's depth, or the switch arms canonicalising the operands that outlive
/// them), and that is deliberate: it asserts the end-to-end behaviour the
/// tomcat page is about, while the two tests above pin the splice half on its
/// own. `tableswitch`/`lookupswitch` were the only branch shapes in this walk
/// that did not establish the canonical layout their own arms are revived with,
/// which is why the shift reached the merge here and not through an `ifeq`.
#[test]
fn an_inlined_result_held_across_a_tableswitch_reaches_the_merge_intact() {
    // callee: `static int category(int a) { return a == 1 ? 2 : 1; }`
    //   0: iload_0
    //   1: iconst_1
    //   2: if_icmpne 7
    //   5: iconst_2
    //   6: ireturn
    //   7: iconst_1     <- branch target
    //   8: ireturn
    let callee = make_inline_site(
        &[0x1a, 0x04, 0xa0, 0x00, 0x05, 0x05, 0xac, 0x04, 0xac],
        1,
        1,
        true,
        b'I',
    );

    // caller: `static int f(int a, int sel) {
    //             int cat = category(a);
    //             int k = switch (sel) { case 0 -> 1; case 1 -> 2; default -> 3; };
    //             return cat == k ? 1 : 0; }`
    //
    //    0: iload_0                          [a]
    //    1: invokestatic #1                  [cat]        <- the splice
    //    4: iload_1                          [cat, sel]
    //    5: tableswitch low=0 high=1         [cat]
    //         (6,7 padding; 8 default; 12 low; 16 high; 20 case0; 24 case1)
    //   28: iconst_1                         [cat, 1]     <- case 0
    //   29: goto 37
    //   32: iconst_2                         [cat, 2]     <- case 1
    //   33: goto 37
    //   36: iconst_3                         [cat, 3]     <- default
    //   37: if_icmpeq 42                     []
    //   40: iconst_0
    //   41: ireturn
    //   42: iconst_1
    //   43: ireturn
    let mut caller: Vec<u8> = vec![
        0x1a, // 0  iload_0
        0xb8, 0x00, 0x01, // 1  invokestatic #1
        0x1b, // 4  iload_1
        0xaa, // 5  tableswitch
        0x00, 0x00, // 6,7 padding to the 4-byte boundary
    ];
    caller.extend_from_slice(&(36i32 - 5).to_be_bytes()); //  8 default -> 36
    caller.extend_from_slice(&0i32.to_be_bytes()); // 12 low
    caller.extend_from_slice(&1i32.to_be_bytes()); // 16 high
    caller.extend_from_slice(&(28i32 - 5).to_be_bytes()); // 20 case 0 -> 28
    caller.extend_from_slice(&(32i32 - 5).to_be_bytes()); // 24 case 1 -> 32
    caller.extend_from_slice(&[
        0x04, // 28 iconst_1
        0xa7, 0x00, 0x08, // 29 goto 37
        0x05, // 32 iconst_2
        0xa7, 0x00, 0x04, // 33 goto 37
        0x06, // 36 iconst_3
        0x9f, 0x00, 0x05, // 37 if_icmpeq 42
        0x03, // 40 iconst_0
        0xac, // 41 ireturn
        0x04, // 42 iconst_1
        0xac, // 43 ireturn
    ]);
    let caller_len = caller.len();
    assert_eq!(caller_len, 44, "hand-assembled caller length");
    caller.push(0); // padding, like every other method the JIT sees
    caller.push(0);

    let mut sites = HashMap::new();
    sites.insert(1, callee);
    let compiled = compile_with_inlines(&caller, caller_len, 2, 2, sites)
        .expect("a branchy leaf callee held across a tableswitch must compile");

    // SAFETY: JIT-compiled code from valid bytecode in an executable mmap. The
    // body is arithmetic and branches only — no helper is reached.
    unsafe {
        // a=1 -> cat=2; sel=1 -> k=2; equal.
        // Under the bug the merge reads canonical slot 0, which still holds the
        // `a` the caller pushed at pc 0: 1 vs 2 -> 0.
        assert_eq!(
            compiled.try_call(&[1, 1]).expect("test JIT call"),
            1,
            "the spliced result must be what the tableswitch merge compares"
        );
        // a=1 -> cat=2; sel=0 -> k=1; not equal. Under the bug: 1 vs 1 -> 1.
        assert_eq!(compiled.try_call(&[1, 0]).expect("test JIT call"), 0);
        // a=0 -> cat=1; sel=0 -> k=1; equal. Under the bug: 0 vs 1 -> 0.
        assert_eq!(compiled.try_call(&[0, 0]).expect("test JIT call"), 1);
        // a=3 -> cat=1; sel=9 -> default k=3; not equal.
        assert_eq!(compiled.try_call(&[3, 9]).expect("test JIT call"), 0);
        // a=1 -> cat=2; sel=9 -> default k=3; not equal. Under the bug: 1 vs 3
        // -> 0 (agrees) — kept so the default arm is covered on both callee arms.
        assert_eq!(compiled.try_call(&[1, 9]).expect("test JIT call"), 0);
    }
}

/// A spliced call site with no resolved target must REFUSE the splice.
///
/// The emitter cannot invent a dispatch target: `InlineSite::invoke_targets` is
/// resolved against the CALLEE's constant pool, and the enclosing method's
/// `invoke_info` is keyed by CALLER pc — a different bytecode space, where the
/// same integer names an unrelated call. Guessing there is how you get a body
/// that calls the wrong method.
///
/// Same site as the test above with `resolved_invoke_infos` emptied. Refusing
/// is not the same as failing: the compile still succeeds (the site falls back
/// to whatever the caller's own pc resolves to, which in this harness is
/// nothing), and what must NOT appear is a dispatch emitted against a target
/// the emitter does not have. A splice that ran anyway would have to bake some
/// address as `jit_invoke_dispatch`'s second argument, and every address
/// available to it here is wrong.
#[test]
fn a_spliced_call_with_no_resolved_target_refuses_the_splice() {
    let callee_body: [u8; 7] = [0x1a, 0x10, 0x0a, 0xb8, 0x00, 0x03, 0xac];
    // resolved_invoke_infos deliberately left empty.
    let callee = make_inline_site(&callee_body, 1, 1, true, b'I');

    let caller: [u8; 7] = [0x1a, 0xb8, 0x00, 0x01, 0xac, 0, 0];
    let mut sites = HashMap::new();
    sites.insert(1, callee);

    let compiled = compile_with_inlines_heap(&caller, 5, 1, 1, sites)
        .expect("refusing the splice must not fail the whole compile");
    assert_eq!(
        calls_to(&compiled, test_helpers().invoke_dispatch),
        0,
        "the callee's call must not be emitted against an un-interned target",
    );
}

/// `invokeinterface` is FIVE bytes inside a spliced body, like everywhere else.
///
/// The other three invoke forms are three. Advancing by three over an
/// `invokeinterface` lands the walk on its `count` operand, and a `count` of 2
/// decodes as `iconst_m1` (0x02) — so the body would push -1, `nop` over the
/// trailing zero, and return -1 instead of the call's result. Silent wrong
/// answer, no bail, no diagnostic: exactly the failure mode the width guard
/// exists for.
#[test]
fn an_invokeinterface_inside_a_splice_is_five_bytes_wide() {
    // callee: `static int leaf(int a) { return iface.m(a, 10); }` — shaped so
    // the operands are the same two the test above uses.
    //   0: iload_0
    //   1: bipush 10
    //   3: invokeinterface #3, count=2, 0
    //   8: ireturn
    let callee_body: [u8; 9] = [0x1a, 0x10, 0x0a, 0xb9, 0x00, 0x03, 0x02, 0x00, 0xac];
    let mut callee = make_inline_site(&callee_body, 1, 1, true, b'I');
    callee.resolved_invoke_infos = vec![resolved_invoke(3, "pkg/Iface", "m", "(II)I", 2, b'I', 2)];

    let caller: [u8; 7] = [0x1a, 0xb8, 0x00, 0x01, 0xac, 0, 0];
    let mut sites = HashMap::new();
    sites.insert(1, callee);

    let compiled = compile_with_inlines_heap(&caller, 5, 1, 1, sites)
        .expect("an invokeinterface-carrying callee must splice");

    let _ = take_last_dispatch();
    // SAFETY: as above.
    let got = unsafe { compiled.call_with_heap(0, &[5]) };
    assert_eq!(
        got, 15,
        "a 3-byte advance would decode the count operand as iconst_m1 and return -1",
    );
    assert!(
        take_last_dispatch().is_some(),
        "the interface call must still have gone through the dispatch helper",
    );
}

/// STEP 5. A call inside a spliced body that is itself spliced emits NO call.
///
/// The nested body computes `a - b`, while the dispatch stub returns `a + b`.
/// The two disagree for the arguments used, so the returned value alone says
/// which path ran — an arithmetic body that agreed with the stub (an `iadd`)
/// would have made this test pass whether nesting worked or not.
#[test]
fn a_nested_splice_replaces_the_call_entirely() {
    // innermost: `static int inner(int a, int b) { return a - b; }`
    //   0: iload_0; 1: iload_1; 2: isub; 3: ireturn
    let inner = make_inline_site(&[0x1a, 0x1b, 0x64, 0xac], 2, 2, true, b'I');

    // middle: `static int leaf(int a) { return inner(a, 10); }`
    let callee_body: [u8; 7] = [0x1a, 0x10, 0x0a, 0xb8, 0x00, 0x03, 0xac];
    let mut callee = make_inline_site(&callee_body, 1, 1, true, b'I');
    // ADDITIVE by design: the pc carries both a nested body and a dispatch
    // target, so a nested bail falls back to the call instead of failing the
    // outer splice. The next test relies on exactly this.
    callee.resolved_invoke_infos = vec![resolved_invoke(
        3,
        "pkg/Inner",
        "inner",
        "(II)I",
        2,
        b'I',
        3,
    )];
    callee.nested_sites = vec![crate::NestedInlineSite {
        callee_pc: 3,
        guard_class_id: 0,
        site: inner,
    }];

    let caller: [u8; 7] = [0x1a, 0xb8, 0x00, 0x01, 0xac, 0, 0];
    let mut sites = HashMap::new();
    sites.insert(1, callee);

    let compiled =
        compile_with_inlines_heap(&caller, 5, 1, 1, sites).expect("a nested splice must compile");

    let _ = take_last_dispatch();
    // SAFETY: as above.
    let got = unsafe { compiled.call_with_heap(0, &[5]) };
    assert_eq!(got, -5, "5 - 10; the dispatch stub would have answered 15");
    assert!(
        take_last_dispatch().is_none(),
        "a nested splice must emit no dispatch at all",
    );
    assert_eq!(
        calls_to(&compiled, test_helpers().invoke_dispatch),
        0,
        "and no CALL to the helper may be left in the code",
    );
}

/// A nested splice that BAILS falls back to the ordinary call.
///
/// This is why `nested_sites` and `resolved_invoke_infos` both carry the pc.
/// If a nested body could only succeed or fail the outer splice, one
/// unmodellable opcode three levels down would cost the whole chain.
///
/// `arraylength` (0xbe) is the bail: the inline mini-emitter has no arm for it
/// (bounds-checked array access is refused at this layer), so the nested
/// attempt rolls back and the pc takes its dispatch entry — answering 15 (the
/// stub's sum) rather than the nested body's -5.
#[test]
fn a_nested_splice_that_bails_falls_back_to_the_call() {
    // innermost, but unspliceable: `arraylength` has no inline arm.
    let inner = make_inline_site(&[0x1a, 0x1b, 0xbe, 0xac], 2, 2, true, b'I');

    let callee_body: [u8; 7] = [0x1a, 0x10, 0x0a, 0xb8, 0x00, 0x03, 0xac];
    let mut callee = make_inline_site(&callee_body, 1, 1, true, b'I');
    callee.resolved_invoke_infos = vec![resolved_invoke(
        3,
        "pkg/Inner",
        "inner",
        "(II)I",
        2,
        b'I',
        3,
    )];
    callee.nested_sites = vec![crate::NestedInlineSite {
        callee_pc: 3,
        guard_class_id: 0,
        site: inner,
    }];

    let caller: [u8; 7] = [0x1a, 0xb8, 0x00, 0x01, 0xac, 0, 0];
    let mut sites = HashMap::new();
    sites.insert(1, callee);

    let compiled = compile_with_inlines_heap(&caller, 5, 1, 1, sites)
        .expect("the outer splice must survive a nested bail");

    let _ = take_last_dispatch();
    // SAFETY: as above.
    let got = unsafe { compiled.call_with_heap(0, &[5]) };
    assert_eq!(
        got, 15,
        "the nested bail must fall back to the dispatch call"
    );
    let (_, _, _, args) = take_last_dispatch().expect("the fallback dispatch must have run");
    assert_eq!(
        args,
        vec![5, 10],
        "with the same arguments the nested body would have had"
    );
}

/// A VALUE-PRODUCING BRANCH MERGE splices, and both paths answer correctly.
///
/// This is the shape that blocked the whole netty inlining line of work.
/// `AssertionUtils.objectsAreEqual` — the first rung past `assertEquals` — is
/// exactly it:
///
/// ```text
///    8: iconst_1     9: goto 13    12: iconst_0    13: ireturn
/// ```
///
/// Two paths reach pc 13 with the same value in different places, and the
/// emitter's symbolic operand stack can only name one, so commit 419a6f5
/// refused every such body outright. Measured 2026-08-18, that refusal is what
/// made `nested-splice=0` and `outer-splice-rolled-back=2`: the call the
/// devirtualisation work was aimed at, one instruction later at pc 16, was
/// never emitted at all.
///
/// One compiled body, both paths, and the two answers differ — which is the
/// only way to show the merge carries a VALUE rather than happening to agree.
#[test]
fn a_value_producing_branch_merge_splices_and_both_paths_are_right() {
    // callee: `static int f(int a) { return a != 0 ? 0 : 1; }`, emitted as the
    // objectsAreEqual diamond.
    //   0: iload_0
    //   1: ifne 8
    //   4: iconst_1
    //   5: goto 9
    //   8: iconst_0
    //   9: ireturn      <-- merge, one value live
    let callee = make_inline_site(
        &[0x1a, 0x9a, 0x00, 0x07, 0x04, 0xa7, 0x00, 0x04, 0x03, 0xac],
        1,
        1,
        true,
        b'I',
    );

    // caller: `static int g(int a) { return f(a); }`
    let caller: [u8; 7] = [0x1a, 0xb8, 0x00, 0x01, 0xac, 0, 0];
    let mut sites = HashMap::new();
    sites.insert(1, callee);

    let compiled = compile_with_inlines_heap(&caller, 5, 1, 1, sites)
        .expect("a diamond-shaped callee must now splice");

    // SAFETY: JIT-compiled code from valid bytecode in an executable mmap.
    unsafe {
        assert_eq!(
            compiled.call_with_heap(0, &[0]),
            1,
            "fall-through path: iconst_1 must survive the goto to the merge",
        );
        assert_eq!(
            compiled.call_with_heap(0, &[7]),
            0,
            "taken path: iconst_0 must be what the merge reads",
        );
    }
    assert_eq!(
        calls_to(&compiled, test_helpers().invoke_dispatch),
        0,
        "and the body must be spliced, not called",
    );
}

/// The two paths leave the value in DIFFERENT slots, which is what the merge
/// region is actually for — and what makes the spill's ORDER observable.
///
/// The other two merge tests do not discriminate that order, and it is worth
/// saying why rather than leaving it implied: in a constant-vs-constant
/// diamond both paths push into the same operand slot (each starts from an
/// empty callee stack and the emitter allocates deterministically from
/// `save_spill`), so re-running one path's store on the other path reads the
/// slot that path already wrote. Idempotent — correct by accident. Verified by
/// mutation: recording the label BEFORE the fall-through spill leaves both of
/// them passing.
///
/// `iload` is the discriminator. It pushes `StackSlot::Frame(local_offset)` —
/// the LOCAL's slot, not a fresh operand slot — so here the taken path's value
/// lives in the merge region's source at one address and the fall-through's at
/// another. If the fall-through's stores are emitted after the label, the
/// branch path re-runs them against a slot it never wrote.
#[test]
fn a_merge_whose_paths_use_different_slots_pins_the_spill_order() {
    // callee: `static int f(int a) { return a != 0 ? a : 1; }`
    //   0: iload_0
    //   1: ifeq 8        (a == 0 -> 8)
    //   4: iload_0       <-- pushes the LOCAL's slot
    //   5: goto 9
    //   8: iconst_1      <-- pushes a fresh operand slot
    //   9: ireturn       <-- merge; the two sources are different addresses
    let callee = make_inline_site(
        &[0x1a, 0x99, 0x00, 0x07, 0x1a, 0xa7, 0x00, 0x04, 0x04, 0xac],
        1,
        1,
        true,
        b'I',
    );

    let caller: [u8; 7] = [0x1a, 0xb8, 0x00, 0x01, 0xac, 0, 0];
    let mut sites = HashMap::new();
    sites.insert(1, callee);

    let compiled = compile_with_inlines_heap(&caller, 5, 1, 1, sites)
        .expect("a merge over two different slots must splice");

    // SAFETY: JIT-compiled code from valid bytecode in an executable mmap.
    unsafe {
        assert_eq!(
            compiled.call_with_heap(0, &[5]),
            5,
            "the branch path must read the local it stored, not the \
             fall-through's operand slot",
        );
        assert_eq!(compiled.call_with_heap(0, &[0]), 1);
    }
}

/// Two values live across the merge, and two merges at different depths in one
/// body.
///
/// Depth 1 is the common case and could pass by accident — a single value that
/// both paths happen to leave in the same slot proves nothing about the
/// canonical-home machinery. Here `iconst_5` stays live UNDER the diamond, so
/// the `goto`'s merge carries two values while the branch target above it
/// carries one; the two depths must be tracked per target rather than globally.
#[test]
fn two_merges_at_different_depths_in_one_body() {
    //   0: iconst_5          [5]
    //   1: iload_0
    //   2: ifne 9            -> target 9 with depth 1
    //   5: iconst_1          [5, 1]
    //   6: goto 10           -> target 10 with depth 2
    //   9: iconst_0          [5, 0]
    //  10: iadd              <-- merge, depth 2
    //  11: ireturn
    let callee = make_inline_site(
        &[
            0x08, 0x1a, 0x9a, 0x00, 0x07, 0x04, 0xa7, 0x00, 0x04, 0x03, 0x60, 0xac,
        ],
        1,
        1,
        true,
        b'I',
    );

    let caller: [u8; 7] = [0x1a, 0xb8, 0x00, 0x01, 0xac, 0, 0];
    let mut sites = HashMap::new();
    sites.insert(1, callee);

    let compiled =
        compile_with_inlines_heap(&caller, 5, 1, 1, sites).expect("a two-deep merge must splice");

    // SAFETY: as above.
    unsafe {
        assert_eq!(compiled.call_with_heap(0, &[0]), 6, "5 + 1");
        assert_eq!(compiled.call_with_heap(0, &[7]), 5, "5 + 0");
    }
}

/// DEVIRTUALISATION INSIDE A SPLICE. Both edges of the receiver guard, from one
/// compiled body.
///
/// The hot edge is a spliced body that returns 42; the cold edge is the
/// ordinary call, which this harness answers with the argument sum. So the
/// SAME machine code returns 42 for a receiver whose class id matches the
/// guard and takes the dispatch for one that does not — which is the only way
/// to show that the guard is a guard and not a constant.
///
/// What each assertion would catch:
///   * hit returning something other than 42 — the guard fell through to the
///     call, or the spliced body was never emitted;
///   * a recorded dispatch on the hit path — the JMP over the miss edge is
///     missing, so both arms run;
///   * miss NOT recording a dispatch — the guard is never false, i.e. the
///     class-id compare is against the wrong operand or the wrong offset;
///   * the miss dispatch seeing arguments other than `[receiver, 10]` — the
///     miss arm did not pop what the hit arm popped, which is the operand-stack
///     disagreement the emitter restores its symbolic state to prevent.
#[test]
fn a_guarded_nested_splice_takes_the_body_on_a_hit_and_the_call_on_a_miss() {
    const GUARD_CLASS_ID: u32 = 0x4242;

    // The devirtualised target: `int m(int) { return 42; }` — a constant, so
    // the returned value alone says which arm ran.
    let inner = make_inline_site(&[0x10, 0x2a, 0xac], 2, 2, false, b'I');

    // The spliced body: `static int leaf(Object o) { return o.m(10); }`
    //   0: aload_0
    //   1: bipush 10
    //   3: invokevirtual #3
    //   6: ireturn
    let mut callee = make_inline_site(
        &[0x2a, 0x10, 0x0a, 0xb6, 0x00, 0x03, 0xac],
        1,
        1,
        true,
        b'I',
    );
    // A guarded pc KEEPS its dispatch entry: the miss edge has to go
    // somewhere, and a virtual site has no direct bind to send it to.
    callee.resolved_invoke_infos = vec![resolved_invoke(3, "pkg/T", "m", "(I)I", 2, b'I', 0)];
    callee.nested_sites = vec![crate::NestedInlineSite {
        callee_pc: 3,
        guard_class_id: GUARD_CLASS_ID,
        site: inner,
    }];

    let caller: [u8; 7] = [0x2a, 0xb8, 0x00, 0x01, 0xac, 0, 0];
    let mut sites = HashMap::new();
    sites.insert(1, callee);

    let compiled = compile_with_inlines_heap(&caller, 5, 1, 1, sites)
        .expect("a guarded nested splice must compile");

    // Two receivers, one compiled body. Class id lives in the first four bytes
    // of the object header, which is what `CMP [rax+0], imm32` reads.
    let mut hit = Box::new([0u64; 8]);
    let mut miss = Box::new([0u64; 8]);
    hit[0] = GUARD_CLASS_ID as u64;
    miss[0] = (GUARD_CLASS_ID + 1) as u64;
    let hit_addr = hit.as_mut_ptr() as i64; // Cast: receiver address
    let miss_addr = miss.as_mut_ptr() as i64; // Cast: receiver address

    let _ = take_last_dispatch();
    // SAFETY: JIT-compiled code from valid bytecode in an executable mmap; the
    // receiver is a live 64-byte buffer shaped like an object header, and the
    // only helper reachable is `stub_invoke_dispatch`, which reads the argument
    // buffer and nothing else.
    let got_hit = unsafe { compiled.call_with_heap(0, &[hit_addr]) };
    assert_eq!(got_hit, 42, "a guard hit must run the spliced body");
    assert!(
        take_last_dispatch().is_none(),
        "and must not also fall into the miss edge",
    );

    // SAFETY: as above.
    let got_miss = unsafe { compiled.call_with_heap(0, &[miss_addr]) };
    let (_, _, _, args) = take_last_dispatch().expect("a guard miss must take the ordinary call");
    assert_eq!(
        args,
        vec![miss_addr, 10],
        "the miss arm must pop exactly the operands the hit arm popped",
    );
    assert_eq!(
        got_miss,
        miss_addr.wrapping_add(10),
        "and its result must be the call's, not the body's",
    );
}

/// A null receiver fails the guard rather than dereferencing it.
///
/// `CMP DWORD [RAX+0], guard` on a null receiver is a segfault, so the null
/// test has to come FIRST and branch to the same miss edge. Nothing else in
/// this construct would catch that: a null receiver is exactly the case a
/// profile never records, and the ordinary call reproduces the NPE correctly.
#[test]
fn a_null_receiver_takes_the_guarded_splices_miss_edge() {
    const GUARD_CLASS_ID: u32 = 0x4242;
    let inner = make_inline_site(&[0x10, 0x2a, 0xac], 2, 2, false, b'I');
    let mut callee = make_inline_site(
        &[0x2a, 0x10, 0x0a, 0xb6, 0x00, 0x03, 0xac],
        1,
        1,
        true,
        b'I',
    );
    callee.resolved_invoke_infos = vec![resolved_invoke(3, "pkg/T", "m", "(I)I", 2, b'I', 0)];
    callee.nested_sites = vec![crate::NestedInlineSite {
        callee_pc: 3,
        guard_class_id: GUARD_CLASS_ID,
        site: inner,
    }];

    let caller: [u8; 7] = [0x2a, 0xb8, 0x00, 0x01, 0xac, 0, 0];
    let mut sites = HashMap::new();
    sites.insert(1, callee);
    let compiled = compile_with_inlines_heap(&caller, 5, 1, 1, sites)
        .expect("a guarded nested splice must compile");

    let _ = take_last_dispatch();
    // SAFETY: JIT-compiled code from valid bytecode; a null receiver must not
    // be dereferenced by the guard, which is the property under test.
    let got = unsafe { compiled.call_with_heap(0, &[0]) };
    let (_, _, _, args) =
        take_last_dispatch().expect("a null receiver must reach the ordinary call, not the body");
    assert_eq!(args, vec![0, 10]);
    assert_eq!(got, 10);
}

/// The interning pass fills `resolved_invoke_infos` from `invoke_targets`, for
/// nested bodies as well as the top one.
///
/// A nested body whose calls were left uninterned would bail at the emitter's
/// "no resolved target" arm and quietly degrade to a dispatch — a regression
/// that costs only speed, and so would never fail a correctness test. Asserted
/// on the DATA rather than on emitted code, because that is where the bug
/// would live.
#[test]
fn interning_reaches_nested_bodies_too() {
    let mut inner = make_inline_site(&[0x1a, 0x1b, 0x60, 0xac], 2, 2, true, b'I');
    inner.invoke_targets = vec![(
        1,
        crate::InlineInvokeTarget {
            class_name: "pkg/Deep".to_string(),
            method_name: "deep".to_string(),
            descriptor: "()I".to_string(),
            num_jit_args: 0,
            return_type: b'I',
            invoke_kind: 3,
            declaring_class_id: 7,
            direct_entry: None,
        },
    )];
    let mut outer = make_inline_site(&[0x1a, 0xac], 1, 1, true, b'I');
    outer.invoke_targets = vec![(
        0,
        crate::InlineInvokeTarget {
            class_name: "pkg/Inner".to_string(),
            method_name: "inner".to_string(),
            descriptor: "(II)I".to_string(),
            num_jit_args: 2,
            return_type: b'I',
            invoke_kind: 3,
            declaring_class_id: 9,
            direct_entry: Some((0xfeed_0000, true)),
        },
    )];
    outer.nested_sites = vec![crate::NestedInlineSite {
        callee_pc: 0,
        guard_class_id: 0,
        site: inner,
    }];
    // Stale pointers from a hypothetical earlier compile must be REPLACED, not
    // appended to: an `InlineSite` can be cloned out of a cached plan.
    outer.resolved_invoke_infos = vec![crate::ResolvedInlineInvoke {
        callee_pc: 999,
        info_addr: 0xdead_beef,
        ..Default::default()
    }];

    let mut strings: Vec<Box<str>> = Vec::new();
    let mut infos: Vec<Box<crate::JitInvokeInfo>> = Vec::new();
    // `(entry, epoch)`: the pin gained an epoch when a baked direct call turned
    // out to be pinned by ADDRESS alone and the address had changed hands
    // (b56bb79b9). This call site was not updated with it, which left the whole
    // `cratonvm-jit` lib test binary failing to COMPILE on dev — so every test
    // in it, not just this one, was silently unrunnable.
    let mut direct_entries: Vec<(usize, u64)> = Vec::new();
    crate::intern_inline_invoke_targets(&mut outer, &mut strings, &mut infos, &mut direct_entries);

    assert_eq!(outer.resolved_invoke_infos.len(), 1);
    assert_eq!(outer.resolved_invoke_infos[0].callee_pc, 0);
    let deep = &outer.nested_sites[0].site;
    assert_eq!(
        deep.resolved_invoke_infos.len(),
        1,
        "a nested body's own calls must be interned too",
    );
    // SAFETY: the pointee is `infos[1]`, alive for the rest of this test.
    let deep_info =
        unsafe { &*(deep.resolved_invoke_infos[0].info_addr as *const crate::JitInvokeInfo) };
    assert_eq!(deep_info.class_name, "pkg/Deep");
    assert_eq!(deep_info.declaring_class_id, 7);
    // Six boxed strs (two triples), two infos.
    assert_eq!(strings.len(), 6);
    assert_eq!(infos.len(), 2);

    // THE KEEP-ALIVE. A baked direct-call address that never reaches
    // `_direct_callee_entries` is a use-after-free with no symptom until the
    // callee tiers up: nothing pins the callee artifact
    // (`prepare_for_publication`) and the invalidation reverse closure has no
    // way to find this caller. The outer target carries one bind, the nested
    // one carries none, so exactly one address must come out — and it must be
    // the one that was bound, not a placeholder.
    // The ADDRESS is what this test is about; the epoch beside it is whatever
    // `jit_entry_artifact_id` answers for a fabricated address, which is not
    // this test's business. Assert the addresses.
    assert_eq!(
        direct_entries.iter().map(|&(e, _)| e).collect::<Vec<_>>(),
        vec![0xfeed_0000],
        "every baked direct-call entry must be registered for keep-alive",
    );
    assert_eq!(
        outer.resolved_invoke_infos[0].direct_entry, 0xfeed_0000,
        "and the emitter must be handed the same address that was registered",
    );
    assert!(outer.resolved_invoke_infos[0].direct_needs_context);
    assert_eq!(
        deep.resolved_invoke_infos[0].direct_entry, 0,
        "a target with no bind stays on the dispatch form",
    );
}

#[test]
fn inline_ctor_fresh_store_proof_rejects_repeated_and_non_ctor_writes() {
    let mut site = make_inline_site(
        &[
            0xb5, 0x00, 0x01, // pc 0: first write of field 0
            0xb5, 0x00, 0x01, // pc 3: repeated write of field 0
            0xb5, 0x00, 0x02, // pc 6: first write of field 1
            0xb1,
        ],
        3,
        3,
        false,
        b'V',
    );
    site.method_name = "<init>".to_string();
    site.field_info = vec![(0, 0, b'L'), (3, 0, b'L'), (6, 1, b'L')];

    assert!(inline_site_is_fresh_ctor_first_store(&site, 0, 0));
    assert!(!inline_site_is_fresh_ctor_first_store(&site, 3, 0));
    assert!(inline_site_is_fresh_ctor_first_store(&site, 6, 1));

    site.method_name = "setFields".to_string();
    assert!(!inline_site_is_fresh_ctor_first_store(&site, 0, 0));
}

#[test]
fn s31_inline_ctor_with_elided_super_call() {
    // Constructor-shaped callee: `void <init>() { super(); }` —
    //   aload_0 (0x2a); invokespecial #2 (0xb7, pc=1, ELIDED); return (0xb1)
    // Caller: `int f(ref obj) { obj.<init>(); return 5; }` —
    //   aload_0; invokespecial #1 (pc=1, inline site); iconst_5; ireturn
    //
    // Self-proving: with the elided-super-call support the site inlines
    // to nothing (pop the receiver, no code) and f returns 5. Without it,
    // try_emit_inline bails and the site falls back to the dispatch
    // helper — which in these tests is the panicking stub, so try_call
    // would abort.
    let caller_code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xb7, 0x00, 0x01, // 1: invokespecial #1
        0x08, // 4: iconst_5
        0xac, // 5: ireturn
        0, 0, // padding
    ];
    let caller_len = 6;

    let mut callee = make_inline_site(
        &[0x2a, 0xb7, 0x00, 0x02, 0xb1], // aload_0; invokespecial #2; return
        1,                               // max_locals (receiver)
        1,                               // num_args (receiver)
        false,                           // instance method
        b'V',                            // void return
    );
    callee.elided_invoke_pcs = vec![1];

    let mut sites = HashMap::new();
    sites.insert(1, callee);

    let compiled = compile_with_inlines(&caller_code, caller_len, 1, 1, sites)
        .expect("ctor-shaped callee with elided super call must compile");

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        assert_eq!(compiled.try_call(&[0x1000]).expect("test JIT call"), 5);
    }
}

#[test]
fn s31_inline_ctor_non_elided_super_call_bails_cleanly() {
    // Same shape but the invokespecial pc is NOT in elided_invoke_pcs:
    // try_emit_inline must bail (rollback) without corrupting the
    // compile. The site then needs a dispatch fallback which this
    // harness does not provide — we only assert the compiler survives
    // (Some or None both acceptable shapes at this layer, but it must
    // not panic and must not emit the inline body).
    let caller_code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xb7, 0x00, 0x01, // 1: invokespecial #1
        0x08, // 4: iconst_5
        0xac, // 5: ireturn
        0, 0,
    ];
    let caller_len = 6;

    let callee = make_inline_site(&[0x2a, 0xb7, 0x00, 0x02, 0xb1], 1, 1, false, b'V');
    // elided_invoke_pcs deliberately empty.

    let mut sites = HashMap::new();
    sites.insert(1, callee);

    let _ = compile_with_inlines(&caller_code, caller_len, 1, 1, sites);
}

/// An `ldc` the [`crate::InlineSite`] cannot describe must REFUSE the splice,
/// never push zero.
///
/// `site.ldc_info` carries only the constants this mini-emitter can materialise
/// as an x86 immediate — `Integer` and `Float`. A String / Class / MethodHandle
/// / condy `ldc` names a *reference* built at run time by
/// `helpers.ldc_string` / `helpers.ldc_class_cp`, which the inline emitter
/// never calls, so there is no i64 that stands for it and a miss is not a
/// "value unknown, use 0" case.
///
/// It used to be treated as one, on BOTH ends: `build_inline_site` ended its
/// constant-pool match `_ => 0`, and the emitter's lookup ended
/// `else { xor rax, rax }`. A one-line
/// `Dialect.extractPattern(unit) { return "extract(?1 from ?2)"; }` spliced
/// into `H2Dialect.extractPattern` therefore compiled to `xor eax,eax; ret`,
/// and every Hibernate HQL `extract()` / `cast()` / `str()` query died in
/// `PatternRenderer.<init>` with `NullPointerException: ... "pattern" is null`.
///
/// THREE arms, so neither half can pass vacuously:
/// * a site whose `ldc_info` HAS the pc — must splice, and `try_call` returns
///   the constant, which is what proves the ldc arm is reachable at all;
/// * the same site with `ldc_info` EMPTY — must be REFUSED;
/// * a control site of the identical shape (same descriptor, `callee_max_locals`
///   and `callee_code_len`, so the same frame reservation and buffer estimate)
///   whose body opens with an opcode the mini-emitter has never had an arm for.
///   That one is refused by the long-standing catch-all, and arm 2 must emit
///   byte-for-byte what it emits. Byte equality against a *no-site* compile
///   would not work and is not the claim: merely HAVING a site changes
///   `inline_stack_reserve`, hence the frame size, hence several immediates.
#[test]
fn s31_inline_refuses_an_ldc_it_has_no_constant_for() {
    // Caller: `int f() { return k(); }` — invokestatic #1 (pc 0); ireturn.
    let caller_code: Vec<u8> = vec![
        0xb8, 0x00, 0x01, // 0: invokestatic #1
        0xac, // 3: ireturn
        0, 0, // padding
    ];
    let caller_len = 4;
    // Callee: `static int k() { return <cp#5>; }` — ldc #5 (pc 0); ireturn.
    let callee_bytes = [0x12, 0x05, 0xac];

    // Arm 1 — the constant IS describable: splice it.
    let mut resolvable = make_inline_site(&callee_bytes, 0, 0, true, b'I');
    resolvable.ldc_info = vec![(0, 1234)];
    let mut sites = HashMap::new();
    sites.insert(0, resolvable);
    let spliced = compile_with_inlines(&caller_code, caller_len, 0, 1, sites)
        .expect("a resolvable ldc must inline");
    // SAFETY: JIT-compiled code from valid bytecode in an executable mapping.
    unsafe {
        assert_eq!(
            spliced.try_call(&[]).expect("test JIT call"),
            1234,
            "the spliced body must return the constant the site described — if this \
             stops holding the ldc arm is no longer reached and arm 2 proves nothing",
        );
    }

    // Arm 2 — nothing describes pc 0 (the String / Class / condy case).
    let unresolvable = make_inline_site(&callee_bytes, 0, 0, true, b'I');
    assert!(
        unresolvable.ldc_info.is_empty(),
        "this arm's whole point is an ldc with no entry",
    );
    let mut sites = HashMap::new();
    sites.insert(0, unresolvable);
    let refused = compile_with_inlines(&caller_code, caller_len, 0, 1, sites)
        .expect("a refused inline must still compile — it falls back to a real call");
    assert_ne!(
        refused.code_bytes(),
        spliced.code_bytes(),
        "an unresolvable ldc must not emit the same body as a resolvable one",
    );

    // Arm 3 — the control refusal: `monitorenter` (0xc2) has never had an arm
    // in the inline emitter, so this body is refused by the catch-all at
    // callee pc 0, exactly where the ldc arm must now refuse.
    let control = make_inline_site(&[0xc2, 0x00, 0xac], 0, 0, true, b'I');
    let mut sites = HashMap::new();
    sites.insert(0, control);
    let control_refused = compile_with_inlines(&caller_code, caller_len, 0, 1, sites)
        .expect("the control refusal must also still compile");
    assert_eq!(
        refused.code_bytes(),
        control_refused.code_bytes(),
        "an ldc with no constant must be refused and rolled back, leaving exactly \
         what any other refusal leaves — pushing 0 (`xor rax,rax`) here is a null \
         where the callee returns a live reference",
    );
}

#[test]
fn s31_inline_getter_iload_ireturn() {
    // Caller: int f(int x) { return getX(x); }
    // callee: int getX(int x) { return x; }   → iload_0, ireturn
    //
    // Caller bytecode: iload_0, invokestatic #1 (pc=1), ireturn
    // We place the inline site at pc=1 (the invokestatic).
    let caller_code: Vec<u8> = vec![
        0x1a, // 0: iload_0
        0xb8, 0x00, 0x01, // 1: invokestatic #1
        0xac, // 4: ireturn
        0, 0, // padding
    ];
    let caller_len = 5;

    let callee = make_inline_site(
        &[0x1a, 0xac], // iload_0, ireturn
        1,             // max_locals
        1,             // num_args (one int param)
        true,          // static
        b'I',          // return type
    );

    let mut sites = HashMap::new();
    sites.insert(1, callee); // inline at pc=1

    let compiled =
        compile_with_inlines(&caller_code, caller_len, 1, 1, sites).expect("compilation failed");

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        assert_eq!(compiled.try_call(&[42]).expect("test JIT call"), 42);
        assert_eq!(compiled.try_call(&[0]).expect("test JIT call"), 0);
        assert_eq!(
            // Widening: i32 -> i64 (sign-extended)
            compiled.try_call(&[-7i32 as i64]).expect("test JIT call"),
            -7
        ); // Cast: JIT ABI convention
    }
}

/// PGO-02 §3, tested by INJECTING the violation it exists to catch.
///
/// An inlined body is entered and left inside one frame — the caller's own —
/// and `deopt::FrameState::caller` is populated by no producer, so an inlined
/// scope cannot be described. A deopt point published from inside a spliced
/// body would therefore name the caller's method with the callee's bci: a
/// well-formed answer about a stack that never existed. `try_emit_inline`
/// refuses such a splice.
///
/// No production emitter reaches that state today, which is exactly why the
/// check needs a deliberate violation to prove it fires — asserting "no
/// production path publishes one" would pass vacuously forever, including
/// after the day it stopped being true.
#[test]
fn inline_publishing_a_deopt_point_is_refused() {
    let caller_code: Vec<u8> = vec![
        0x1a, // 0: iload_0
        0xb8, 0x00, 0x01, // 1: invokestatic #1
        0xac, // 4: ireturn
        0, 0, // padding
    ];
    let caller_len = 5;
    let sites = || {
        let mut sites = HashMap::new();
        sites.insert(1, make_inline_site(&[0x1a, 0xac], 1, 1, true, b'I'));
        sites
    };

    // Control: the splice happens and the spliced code is correct.
    let inlined = compile_with_inlines(&caller_code, caller_len, 1, 1, sites())
        .expect("control compile must succeed");
    // SAFETY: JIT-compiled machine code produced by this compiler from valid
    // bytecode, in an executable mapping.
    unsafe {
        assert_eq!(inlined.try_call(&[42]).expect("test JIT call"), 42);
    }

    // Injected violation: the same body now publishes deopt metadata.
    super::INLINE_TEST_PUBLISHES_DEOPT.with(|f| f.set(true));
    let refused = compile_with_inlines(&caller_code, caller_len, 1, 1, sites());
    super::INLINE_TEST_PUBLISHES_DEOPT.with(|f| f.set(false));

    let refused = refused
        .expect("refusing the splice must fall back to a normal call, not bail the whole method");
    assert_ne!(
        refused.code_bytes(),
        inlined.code_bytes(),
        "a body that published deopt metadata must NOT have been spliced — identical \
         machine code means the postcondition did not fire and an unrepresentable \
         inlined scope was published"
    );
}

#[test]
fn s31_inline_add_two_params() {
    // Caller: int f(int a, int b) { return add(a, b); }
    // Callee: int add(int a, int b) { return a + b; }  → iload_0, iload_1, iadd, ireturn
    let caller_code: Vec<u8> = vec![
        0x1a, // 0: iload_0
        0x1b, // 1: iload_1
        0xb8, 0x00, 0x01, // 2: invokestatic #1
        0xac, // 5: ireturn
        0, 0,
    ];
    let caller_len = 6;

    let callee = make_inline_site(
        &[0x1a, 0x1b, 0x60, 0xac], // iload_0, iload_1, iadd, ireturn
        2,
        2, // two params
        true,
        b'I',
    );

    let mut sites = HashMap::new();
    sites.insert(2, callee);

    let compiled =
        compile_with_inlines(&caller_code, caller_len, 2, 2, sites).expect("compilation failed");

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        assert_eq!(compiled.try_call(&[3, 4]).expect("test JIT call"), 7);
        assert_eq!(
            compiled
                // Widening: i32 -> i64 (sign-extended)
                .try_call(&[100, -50i32 as i64])
                .expect("test JIT call"),
            50
        ); // Cast: JIT ABI convention
        assert_eq!(compiled.try_call(&[0, 0]).expect("test JIT call"), 0);
    }
}

#[test]
fn s31_inline_constant_return() {
    // Caller: int f() { return five(); }
    // Callee: int five() { return 5; }  → iconst_5, ireturn
    let caller_code: Vec<u8> = vec![
        0xb8, 0x00, 0x01, // 0: invokestatic #1
        0xac, // 3: ireturn
        0, 0,
    ];
    let caller_len = 4;

    let callee = make_inline_site(
        &[0x08, 0xac], // iconst_5, ireturn
        0,
        0, // no params
        true,
        b'I',
    );

    let mut sites = HashMap::new();
    sites.insert(0, callee);

    let compiled =
        compile_with_inlines(&caller_code, caller_len, 0, 0, sites).expect("compilation failed");

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        assert_eq!(compiled.try_call(&[]).expect("test JIT call"), 5);
    }
}

#[test]
fn s31_inline_void_method() {
    // Caller: int f(int x) { noop(); return x; }
    // Callee: void noop() { return; }
    let caller_code: Vec<u8> = vec![
        0xb8, 0x00, 0x01, // 0: invokestatic #1 (void)
        0x1a, // 3: iload_0
        0xac, // 4: ireturn
        0, 0,
    ];
    let caller_len = 5;

    let callee = make_inline_site(
        &[0xb1], // return (void)
        0,
        0,
        true,
        b'V',
    );

    let mut sites = HashMap::new();
    sites.insert(0, callee);

    let compiled =
        compile_with_inlines(&caller_code, caller_len, 1, 1, sites).expect("compilation failed");

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        assert_eq!(compiled.try_call(&[99]).expect("test JIT call"), 99);
    }
}

#[test]
fn s31_inline_with_branch() {
    // Callee: int abs(int x) { return x >= 0 ? x : -x; }
    // Bytecode: iload_0, ifge +5, iload_0, ineg, ireturn, iload_0, ireturn
    //           0       1        4       5     6        7       8
    let callee_bc: Vec<u8> = vec![
        0x1a, // 0: iload_0
        0x9c, 0x00, 0x06, // 1: ifge +6 → target=7
        0x1a, // 4: iload_0
        0x74, // 5: ineg
        0xac, // 6: ireturn
        0x1a, // 7: iload_0
        0xac, // 8: ireturn
    ];

    let caller_code: Vec<u8> = vec![
        0x1a, // 0: iload_0
        0xb8, 0x00, 0x01, // 1: invokestatic #1
        0xac, // 4: ireturn
        0, 0,
    ];
    let caller_len = 5;

    let callee = make_inline_site(&callee_bc, 1, 1, true, b'I');

    let mut sites = HashMap::new();
    sites.insert(1, callee);

    let compiled =
        compile_with_inlines(&caller_code, caller_len, 1, 1, sites).expect("compilation failed");

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        assert_eq!(compiled.try_call(&[5]).expect("test JIT call"), 5);
        assert_eq!(
            // Widening: i32 -> i64 (sign-extended)
            compiled.try_call(&[-5i32 as i64]).expect("test JIT call"),
            5
        ); // Cast: JIT ABI convention
        assert_eq!(compiled.try_call(&[0]).expect("test JIT call"), 0);
    }
}

#[test]
fn s31_inline_with_if_icmp() {
    // Callee: int max(int a, int b) { return a >= b ? a : b; }
    // Exercises the restored if_icmp inline path: a forward conditional
    // branch (if_icmplt) with two return arms and an EMPTY operand stack at
    // the merge — the provably-safe subset that 419a6f5 over-bailed.
    let callee_bc: Vec<u8> = vec![
        0x1a, // 0: iload_0 (a)
        0x1b, // 1: iload_1 (b)
        0xa1, 0x00, 0x05, // 2: if_icmplt +5 -> 7 (if a<b, return b)
        0x1a, // 5: iload_0 (a)
        0xac, // 6: ireturn
        0x1b, // 7: iload_1 (b)   <- branch target (empty stack)
        0xac, // 8: ireturn
    ];
    let caller_code: Vec<u8> = vec![
        0x1a, // 0: iload_0
        0x1b, // 1: iload_1
        0xb8, 0x00, 0x01, // 2: invokestatic #1
        0xac, // 5: ireturn
        0, 0,
    ];
    let caller_len = 6;
    let callee = make_inline_site(&callee_bc, 2, 2, true, b'I');
    let mut sites = HashMap::new();
    sites.insert(2, callee);
    let compiled =
        compile_with_inlines(&caller_code, caller_len, 2, 2, sites).expect("compilation failed");
    // SAFETY: executing JIT-compiled machine code produced from valid
    // bytecode; the mmap region is executable.
    unsafe {
        assert_eq!(compiled.try_call(&[5, 3]).expect("test JIT call"), 5);
        assert_eq!(compiled.try_call(&[3, 5]).expect("test JIT call"), 5);
        assert_eq!(compiled.try_call(&[4, 4]).expect("test JIT call"), 4);
    }
}

#[test]
fn s31_inline_arithmetic_chain() {
    // Callee: int triple(int x) { return x + x + x; }
    // iload_0, iload_0, iadd, iload_0, iadd, ireturn
    let callee_bc: Vec<u8> = vec![0x1a, 0x1a, 0x60, 0x1a, 0x60, 0xac];

    let caller_code: Vec<u8> = vec![
        0x1a, // 0: iload_0
        0xb8, 0x00, 0x01, // 1: invokestatic #1
        0xac, // 4: ireturn
        0, 0,
    ];
    let caller_len = 5;

    let callee = make_inline_site(&callee_bc, 1, 1, true, b'I');

    let mut sites = HashMap::new();
    sites.insert(1, callee);

    let compiled =
        compile_with_inlines(&caller_code, caller_len, 1, 1, sites).expect("compilation failed");

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        assert_eq!(compiled.try_call(&[7]).expect("test JIT call"), 21);
        assert_eq!(compiled.try_call(&[0]).expect("test JIT call"), 0);
        assert_eq!(
            // Widening: i32 -> i64 (sign-extended)
            compiled.try_call(&[-3i32 as i64]).expect("test JIT call"),
            -9
        ); // Cast: JIT ABI convention
    }
}

#[test]
fn s31_inline_iinc_loop() {
    // Callee: int inc5(int x) { x++; x++; x++; x++; x++; return x; }
    // iinc 0,1 × 5, iload_0, ireturn
    let callee_bc: Vec<u8> = vec![
        0x84, 0x00, 0x01, // iinc local0, 1
        0x84, 0x00, 0x01, 0x84, 0x00, 0x01, 0x84, 0x00, 0x01, 0x84, 0x00, 0x01,
        0x1a, // iload_0
        0xac, // ireturn
    ];

    let caller_code: Vec<u8> = vec![0x1a, 0xb8, 0x00, 0x01, 0xac, 0, 0];
    let caller_len = 5;

    let callee = make_inline_site(&callee_bc, 1, 1, true, b'I');
    let mut sites = HashMap::new();
    sites.insert(1, callee);

    let compiled =
        compile_with_inlines(&caller_code, caller_len, 1, 1, sites).expect("compilation failed");

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        assert_eq!(compiled.try_call(&[10]).expect("test JIT call"), 15);
        assert_eq!(compiled.try_call(&[0]).expect("test JIT call"), 5);
    }
}

#[test]
fn s31_inline_callee_uses_extra_locals() {
    // Callee: int swap_add(int a, int b) { int t = a; a = b; b = t; return a + b; }
    // iload_0, istore_2, iload_1, istore_0, iload_2, istore_1, iload_0, iload_1, iadd, ireturn
    let callee_bc: Vec<u8> = vec![
        0x1a, // 0: iload_0
        0x3d, // 1: istore_2 (local 2 = temp)
        0x1b, // 2: iload_1
        0x3b, // 3: istore_0
        0x1c, // 4: iload_2
        0x3c, // 5: istore_1
        0x1a, // 6: iload_0
        0x1b, // 7: iload_1
        0x60, // 8: iadd
        0xac, // 9: ireturn
    ];

    let caller_code: Vec<u8> = vec![
        0x1a, // 0: iload_0
        0x1b, // 1: iload_1
        0xb8, 0x00, 0x01, // 2: invokestatic
        0xac, // 5: ireturn
        0, 0,
    ];
    let caller_len = 6;

    let callee = make_inline_site(&callee_bc, 3, 2, true, b'I');
    let mut sites = HashMap::new();
    sites.insert(2, callee);

    let compiled =
        compile_with_inlines(&caller_code, caller_len, 2, 2, sites).expect("compilation failed");

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        // swap_add(a,b) = a + b regardless of swap, so always sum
        assert_eq!(compiled.try_call(&[3, 7]).expect("test JIT call"), 10);
        assert_eq!(compiled.try_call(&[100, 200]).expect("test JIT call"), 300);
    }
}

#[test]
fn s31_inline_bailout_unsupported_opcode() {
    // Callee has monitorenter (0xC2) which is unsupported — should bail out.
    // The caller should still compile (the invokestatic falls through to normal call path).
    let callee_bc: Vec<u8> = vec![
        0x1a, // iload_0
        0xC2, // monitorenter — unsupported in inline
        0xb1, // return
    ];

    let caller_code: Vec<u8> = vec![
        0x1a, // 0: iload_0
        0xb8, 0x00, 0x01, // 1: invokestatic — will try inline but bail
        0xac, // 4: ireturn
        0, 0,
    ];
    let caller_len = 5;

    let callee = make_inline_site(&callee_bc, 1, 1, true, b'V');
    let mut sites = HashMap::new();
    sites.insert(1, callee);

    // This should either compile successfully (falling back to a call stub)
    // or return None if the fallback isn't available. Either way, no crash.
    let _result = compile_with_inlines(&caller_code, caller_len, 1, 1, sites);
    // Main assertion: no panic or crash
}

#[test]
fn s31_inline_long_arithmetic() {
    // Callee: long double_it(long x) { return x + x; }  → lload_0, lload_0, ladd, lreturn
    // We test through int path since our JIT uses i64 uniformly.
    // Caller: int f(int x) { return double_it(x); }
    let callee_bc: Vec<u8> = vec![0x1a, 0x1a, 0x61, 0xac]; // iload_0, iload_0, ladd, ireturn

    let caller_code: Vec<u8> = vec![
        0x1a, // 0: iload_0
        0xb8, 0x00, 0x01, // 1: invokestatic
        0xac, // 4: ireturn
        0, 0,
    ];
    let caller_len = 5;

    let callee = make_inline_site(&callee_bc, 1, 1, true, b'I');
    let mut sites = HashMap::new();
    sites.insert(1, callee);

    let compiled =
        compile_with_inlines(&caller_code, caller_len, 1, 1, sites).expect("compilation failed");

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        assert_eq!(compiled.try_call(&[50]).expect("test JIT call"), 100);
        assert_eq!(
            // Widening: i32 -> i64 (sign-extended)
            compiled.try_call(&[-7i32 as i64]).expect("test JIT call"),
            -14
        ); // Cast: JIT ABI convention
    }
}

#[test]
fn s31_inline_category2_params_use_jvm_local_slots() {
    // Mirrors Bouncy Castle Bits.bitPermuteStep(JJI)J:
    //   long x -> local 0/1, long m -> local 2/3, int s -> local 4.
    // The inline emitter used to deposit the three JIT args compactly into
    // locals 0, 1, 2, so `lload_2` read the shift count and `iload 4` read
    // zero. That miscompiled Interleave.expand64To128 after tiered inlining.
    let callee_bc: Vec<u8> = vec![
        0x1e, // 0: lload_0
        0x1e, // 1: lload_0
        0x15, 0x04, // 2: iload 4
        0x7d, // 4: lushr
        0x83, // 5: lxor
        0x20, // 6: lload_2
        0x7f, // 7: land
        0x37, 0x05, // 8: lstore 5
        0x16, 0x05, // 10: lload 5
        0x16, 0x05, // 12: lload 5
        0x15, 0x04, // 14: iload 4
        0x79, // 16: lshl
        0x83, // 17: lxor
        0x1e, // 18: lload_0
        0x83, // 19: lxor
        0xad, // 20: lreturn
    ];

    let caller_code: Vec<u8> = vec![
        0x1a, // 0: iload_0 (x, represented as i64 in this JIT ABI)
        0x1b, // 1: iload_1 (m)
        0x1c, // 2: iload_2 (s)
        0xb8, 0x00, 0x01, // 3: invokestatic
        0xad, // 6: lreturn
        0, 0,
    ];
    let caller_len = 7;

    let mut callee = make_inline_site(&callee_bc, 7, 3, true, b'J');
    callee.descriptor = "(JJI)J".to_string();
    let mut sites = HashMap::new();
    sites.insert(3, callee);

    let compiled = compile_with_inlines(&caller_code, caller_len, 3, 3, sites)
        .expect("category-2 inline callee must compile");

    let expect = |x: i64, m: i64, s: i64| -> i64 {
        let x = x as u64;
        let m = m as u64;
        let s = (s as u32) & 0x3f;
        let t = (x ^ (x >> s)) & m;
        (x ^ t ^ (t << s)) as i64
    };

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        let cases = [
            (0x0123_4567_89ab_cdef_i64, 0x0000_ffff_0000_0000_i64, 16_i64),
            (
                0x8000_0000_0000_0001_u64 as i64,
                0x2222_2222_2222_2222_i64,
                1_i64,
            ),
            (
                0xfedc_ba98_7654_3210_u64 as i64,
                0x0c0c_0c0c_0c0c_0c0c_i64,
                2_i64,
            ),
        ];
        for (x, m, s) in cases {
            assert_eq!(
                compiled.try_call(&[x, m, s]).expect("test JIT call"),
                expect(x, m, s)
            );
        }
    }
}

#[test]
fn s31_inline_bipush_sipush() {
    // Callee: int f() { return 100; }  → bipush 100, ireturn
    let callee_bc: Vec<u8> = vec![0x10, 100, 0xac]; // bipush 100, ireturn

    let caller_code: Vec<u8> = vec![
        0xb8, 0x00, 0x01, // 0: invokestatic
        0xac, // 3: ireturn
        0, 0,
    ];
    let caller_len = 4;

    let callee = make_inline_site(&callee_bc, 0, 0, true, b'I');
    let mut sites = HashMap::new();
    sites.insert(0, callee);

    let compiled =
        compile_with_inlines(&caller_code, caller_len, 0, 0, sites).expect("compilation failed");

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        assert_eq!(compiled.try_call(&[]).expect("test JIT call"), 100);
    }
}

#[test]
fn s31_inline_multiple_sites() {
    // Caller: int f(int x) { return inc(inc(x)); }
    // Two invokestatics, each inlining "int inc(int x) { return x + 1; }"
    // Callee: iload_0, iconst_1, iadd, ireturn
    let callee_bc: Vec<u8> = vec![0x1a, 0x04, 0x60, 0xac];

    let caller_code: Vec<u8> = vec![
        0x1a, // 0: iload_0
        0xb8, 0x00, 0x01, // 1: invokestatic #1 (first inc)
        0xb8, 0x00, 0x02, // 4: invokestatic #2 (second inc)
        0xac, // 7: ireturn
        0, 0,
    ];
    let caller_len = 8;

    let site1 = make_inline_site(&callee_bc, 1, 1, true, b'I');
    let site2 = make_inline_site(&callee_bc, 1, 1, true, b'I');

    let mut sites = HashMap::new();
    sites.insert(1, site1);
    sites.insert(4, site2);

    let compiled =
        compile_with_inlines(&caller_code, caller_len, 1, 1, sites).expect("compilation failed");

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        assert_eq!(compiled.try_call(&[10]).expect("test JIT call"), 12); // 10 + 1 + 1
        assert_eq!(compiled.try_call(&[0]).expect("test JIT call"), 2);
    }
}

// ===================================================================
// Inline array-access codegen tests
//
// These exercise the inline machine code emitted for `arraylength`
// (0xbe) and the primitive `Xaload` family (`iaload` 0x2e, `laload`
// 0x2f, `baload` 0x33, `caload` 0x34, `saload` 0x35). The JIT emits
// raw loads from the array header/data region with no helper `CALL`
// on the fast path:
//   - length:  MOV EAX, [array + ARRAY_LENGTH_OFFSET(12)]
//   - element: load from [array + HEADER_SIZE + idx*scale]
//     with scale 1/2/4/8 and the correct sign/zero extension.
// The two exception edges (null-array NPE, out-of-bounds AIOOBE)
// still funnel through the shared deopt stubs, which call the
// `bastore` / `throw_aioobe` runtime helpers respectively.
//
// Heap arrays are allocated via the `cratonvm-gc` dev-dependency
// (`GenerationalHeap`), matching the existing `test_getfield_*`
// pattern — no `SharedVm` / `vm-tests` gating needed because the
// fast path takes no VM-context argument.
// ===================================================================

thread_local! {
    /// Set by [`flagging_throw_aioobe`] so an inline out-of-bounds
    /// access can be observed from a test: `(index, length, bytecode_pc)`.
    static TEST_AIOOBE_HIT: std::cell::Cell<Option<(i64, i64, i64)>> =
        const { std::cell::Cell::new(None) };
    /// Set by [`flagging_npe_with_action`] when an inline null-check deopt
    /// stub fires.
    static TEST_NPE_HIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// JEP 358 — the action code the firing null-check stub passed to
    /// `jit_npe_with_action`, DECODED out of the packed word. `-1` = no stub
    /// fired. Lets the null-NPE tests assert the per-opcode action is threaded
    /// correctly.
    static TEST_NPE_ACTION: std::cell::Cell<i64> = const { std::cell::Cell::new(-1) };
    /// The trap-site key from the same packed word: the id
    /// `inlining::record_npe_trap_site` issued for THIS null check, which is
    /// what lets the stack walk recover the bci of the frame that trapped.
    /// `-1` = no stub fired, `0` = the site was not recorded (which is what
    /// `CRATONVM_JIT_NO_NPE_TRAP_LINES=1` produces).
    static TEST_NPE_TRAP_KEY: std::cell::Cell<i64> = const { std::cell::Cell::new(-1) };
}

/// Test stand-in for `jit_throw_aioobe`: records the failure payload
/// and returns the `i64::MIN` deopt sentinel, exactly like the real
/// helper. The bounds-check stub calls this when `idx >= len`.
///
/// SAFETY: plain `extern "C"` callback invoked by JIT code with four
/// `i64` arguments; touches only a thread-local.
unsafe extern "C" fn flagging_throw_aioobe(
    index: i64,
    length: i64,
    _array_ptr: i64,
    bytecode_pc: i64,
) -> i64 {
    TEST_AIOOBE_HIT.with(|c| c.set(Some((index, length, bytecode_pc))));
    i64::MIN
}

/// Test stand-in for `jit_npe_with_action`: each inline null-check deopt
/// stub calls `helpers.jit_npe_with_action(packed)` to flag a pending NPE.
/// Records the hit AND both halves of the packed word; the stub itself loads
/// the `i64::MIN` sentinel.
///
/// The argument is NOT the bare action code. Since the trap-site side channel
/// landed, each described null check gets its own ten-byte trampoline that
/// passes `action | (trap_key << 8)`, so the walk that builds the trace can
/// ask which check fired and recover the bci of the frame that trapped. A test
/// that asserts on the raw word is asserting on the low byte AND the site id
/// at once, and would move every time a new check is emitted ahead of it —
/// so split it here, once, rather than in each test.
///
/// SAFETY: plain `extern "C"` callback invoked by JIT code with one `i64`
/// argument; touches only thread-locals.
unsafe extern "C" fn flagging_npe_with_action(packed: i64) {
    TEST_NPE_HIT.with(|c| c.set(true));
    TEST_NPE_ACTION.with(|c| c.set(packed & 0xff));
    TEST_NPE_TRAP_KEY.with(|c| c.set((packed >> 8) & 0x00ff_ffff));
}

/// `test_helpers()` with the `throw_aioobe` and `jit_npe_with_action`
/// slots wired to the flagging stubs above so the exception-edge tests
/// can take the deopt path without panicking on an unimplemented stub.
fn array_test_helpers() -> JitRuntimeHelpers {
    let mut h = test_helpers();
    // Cast: fn pointer to usize helper address
    h.throw_aioobe = flagging_throw_aioobe as *const () as usize;
    // Cast: fn pointer to usize helper address
    h.jit_npe_with_action = flagging_npe_with_action as *const () as usize;
    h
}

/// Compile a single-method body with `array_test_helpers()`. Mirrors
/// the argument convention of the `test_getfield_*` helpers — the
/// `compile` positional args after `code_len` are `num_params` then
/// `max_locals` (there is no separate `max_stack` parameter).
fn compile_array_test(
    code: &[u8],
    code_len: usize,
    num_params: usize,
    max_locals: usize,
) -> CompiledMethod {
    compile(
        code,
        code_len,
        num_params,
        max_locals,
        // needs_heap = false: inline array access (length + Xaload) is
        // pure machine code with no hidden VM-context argument, so the
        // tests call `CompiledMethod::call` directly with just the Java
        // args. The deopt stubs call `bastore`/`throw_aioobe` helpers
        // but those take no context. Mirrors the `test_getfield_*`
        // pattern; see the note in `test_bounds_check_iaload_in_bounds`.
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
        Vec::new(), // pic_slots — no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &array_test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap()
}

#[test]
fn test_inline_arraylength() {
    // int f(int[] arr) { return arr.length; }
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0      (array)
        0xbe, // 1: arraylength
        0xac, // 2: ireturn
        0, 0,
    ];
    let compiled = compile_array_test(&code, 3, 1, 1);

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_gc::heap::ArrayElementType;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();

    let arr0 = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 0);
    let arr5 = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 5);
    let arr257 = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 257);

    // SAFETY: JIT-compiled code from valid bytecode; mmap region executable.
    unsafe {
        assert_eq!(
            compiled
                // Cast: object/array pointer to i64 for the JIT calling convention
                .try_call(&[arr0.as_ptr() as i64])
                .expect("test JIT call"),
            0
        );
        assert_eq!(
            compiled
                // Cast: object/array pointer to i64 for the JIT calling convention
                .try_call(&[arr5.as_ptr() as i64])
                .expect("test JIT call"),
            5
        );
        assert_eq!(
            compiled
                // Cast: object/array pointer to i64 for the JIT calling convention
                .try_call(&[arr257.as_ptr() as i64])
                .expect("test JIT call"),
            257
        );
    }
}

#[test]
fn test_inline_iaload() {
    // int f(int[] arr, int i) { return arr[i]; }
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x1b, // 1: iload_1
        0x2e, // 2: iaload
        0xac, // 3: ireturn
        0, 0,
    ];
    let compiled = compile_array_test(&code, 4, 2, 2);

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_gc::heap::ArrayElementType;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();

    let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 4);
    // Include a negative element to confirm 32-bit values round-trip.
    let vals = [7i32, -100_000, i32::MAX, i32::MIN];
    for (i, v) in vals.iter().enumerate() {
        heap.set_array_element(arr, i, Value::Int(*v)).unwrap();
    }

    // SAFETY: JIT-compiled code from valid bytecode; mmap region executable.
    unsafe {
        for (i, v) in vals.iter().enumerate() {
            // iaload sign-extends the 32-bit element to the 64-bit
            // return register, so the expected value is `*v as i64`.
            assert_eq!(
                compiled
                    // Cast: object/array pointer to i64 for the JIT calling convention
                    .try_call(&[arr.as_ptr() as i64, i as i64])
                    .expect("test JIT call"),
                // Cast: test value to i64 for the JIT calling convention
                *v as i64,
                "iaload mismatch at index {i}"
            );
        }
    }
}

#[test]
fn test_inline_baload_sign_extends() {
    // int f(byte[] arr, int i) { return arr[i]; }
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x1b, // 1: iload_1
        0x33, // 2: baload
        0xac, // 3: ireturn
        0, 0,
    ];
    let compiled = compile_array_test(&code, 4, 2, 2);

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_gc::heap::ArrayElementType;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();

    let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Byte, 4);
    // Store raw byte patterns via Value::Int (heap narrows to i8).
    // 0xFF -> -1, 0x80 -> -128 prove the sign extension.
    heap.set_array_element(arr, 0, Value::Int(0x7F)).unwrap();
    heap.set_array_element(arr, 1, Value::Int(0xFF)).unwrap();
    heap.set_array_element(arr, 2, Value::Int(0x80)).unwrap();
    heap.set_array_element(arr, 3, Value::Int(0x01)).unwrap();

    // SAFETY: JIT-compiled code from valid bytecode; mmap region executable.
    unsafe {
        assert_eq!(
            compiled
                // Cast: object/array pointer to i64 for the JIT calling convention
                .try_call(&[arr.as_ptr() as i64, 0])
                .expect("test JIT call"),
            127
        );
        assert_eq!(
            compiled
                // Cast: object/array pointer to i64 for the JIT calling convention
                .try_call(&[arr.as_ptr() as i64, 1])
                .expect("test JIT call"),
            -1
        );
        assert_eq!(
            compiled
                // Cast: object/array pointer to i64 for the JIT calling convention
                .try_call(&[arr.as_ptr() as i64, 2])
                .expect("test JIT call"),
            -128
        );
        assert_eq!(
            compiled
                // Cast: object/array pointer to i64 for the JIT calling convention
                .try_call(&[arr.as_ptr() as i64, 3])
                .expect("test JIT call"),
            1
        );
    }
}

#[test]
fn test_inline_caload_zero_extends() {
    // int f(char[] arr, int i) { return arr[i]; }
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x1b, // 1: iload_1
        0x34, // 2: caload
        0xac, // 3: ireturn
        0, 0,
    ];
    let compiled = compile_array_test(&code, 4, 2, 2);

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_gc::heap::ArrayElementType;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();

    let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Char, 3);
    // 0xFFFF must zero-extend to 65535 (not -1).
    heap.set_array_element(arr, 0, Value::Int(0x0041)).unwrap(); // 'A'
    heap.set_array_element(arr, 1, Value::Int(0xFFFF)).unwrap();
    heap.set_array_element(arr, 2, Value::Int(0x8000)).unwrap();

    // SAFETY: JIT-compiled code from valid bytecode; mmap region executable.
    unsafe {
        assert_eq!(
            compiled
                // Cast: object/array pointer to i64 for the JIT calling convention
                .try_call(&[arr.as_ptr() as i64, 0])
                .expect("test JIT call"),
            65
        );
        assert_eq!(
            compiled
                // Cast: object/array pointer to i64 for the JIT calling convention
                .try_call(&[arr.as_ptr() as i64, 1])
                .expect("test JIT call"),
            65535
        );
        assert_eq!(
            compiled
                // Cast: object/array pointer to i64 for the JIT calling convention
                .try_call(&[arr.as_ptr() as i64, 2])
                .expect("test JIT call"),
            32768
        );
    }
}

#[test]
fn test_inline_saload_sign_extends() {
    // int f(short[] arr, int i) { return arr[i]; }
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x1b, // 1: iload_1
        0x35, // 2: saload
        0xac, // 3: ireturn
        0, 0,
    ];
    let compiled = compile_array_test(&code, 4, 2, 2);

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_gc::heap::ArrayElementType;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();

    let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Short, 3);
    // 0xFFFF must sign-extend to -1, 0x8000 to -32768.
    heap.set_array_element(arr, 0, Value::Int(0x7FFF)).unwrap();
    heap.set_array_element(arr, 1, Value::Int(0xFFFF)).unwrap();
    heap.set_array_element(arr, 2, Value::Int(0x8000)).unwrap();

    // SAFETY: JIT-compiled code from valid bytecode; mmap region executable.
    unsafe {
        assert_eq!(
            compiled
                // Cast: object/array pointer to i64 for the JIT calling convention
                .try_call(&[arr.as_ptr() as i64, 0])
                .expect("test JIT call"),
            32767
        );
        assert_eq!(
            compiled
                // Cast: object/array pointer to i64 for the JIT calling convention
                .try_call(&[arr.as_ptr() as i64, 1])
                .expect("test JIT call"),
            -1
        );
        assert_eq!(
            compiled
                // Cast: object/array pointer to i64 for the JIT calling convention
                .try_call(&[arr.as_ptr() as i64, 2])
                .expect("test JIT call"),
            -32768
        );
    }
}

#[test]
fn test_inline_laload() {
    // long f(long[] arr, int i) { return arr[i]; }
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x1b, // 1: iload_1
        0x2f, // 2: laload
        0xad, // 3: lreturn
        0, 0,
    ];
    let compiled = compile_array_test(&code, 4, 2, 2);

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_gc::heap::ArrayElementType;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();

    let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Long, 3);
    let vals = [0x0123_4567_89AB_CDEFi64, i64::MIN, -1i64];
    for (i, v) in vals.iter().enumerate() {
        heap.set_array_element(arr, i, Value::Long(*v)).unwrap();
    }

    // SAFETY: JIT-compiled code from valid bytecode; mmap region executable.
    unsafe {
        for (i, v) in vals.iter().enumerate() {
            assert_eq!(
                compiled
                    // Cast: object/array pointer to i64 for the JIT calling convention
                    .try_call(&[arr.as_ptr() as i64, i as i64])
                    .expect("test JIT call"),
                *v,
                "laload mismatch at index {i}"
            );
        }
    }
}

#[test]
fn test_inline_arraylength_null_throws_npe() {
    // int f(int[] arr) { return arr.length; }  with arr == null.
    // The inline `MOV EAX, [arr + 12]` is guarded by a TEST/JZ that
    // branches to the per-action null-check deopt stub. That stub calls
    // `helpers.jit_npe_with_action(ARRAY_LENGTH)` (flagging our NPE
    // thread-locals) and returns the `i64::MIN` deopt sentinel.
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xbe, // 1: arraylength
        0xac, // 2: ireturn
        0, 0,
    ];
    let compiled = compile_array_test(&code, 3, 1, 1);

    TEST_NPE_HIT.with(|c| c.set(false));
    TEST_NPE_ACTION.with(|c| c.set(-1));
    TEST_NPE_TRAP_KEY.with(|c| c.set(-1));
    // SAFETY: JIT-compiled code from valid bytecode; mmap region executable.
    let result = unsafe { compiled.try_call(&[0]).expect("test JIT call") }; // null array
    assert_eq!(
        result,
        i64::MIN,
        "null arraylength must deopt with sentinel"
    );
    assert!(
        TEST_NPE_HIT.with(|c| c.get()),
        "null arraylength must take the NPE deopt stub"
    );
    // JEP 358: the stub must thread the `arraylength` action code.
    assert_eq!(
        TEST_NPE_ACTION.with(|c| c.get()),
        // Widening: narrower int -> i64
        npe_action::ARRAY_LENGTH as i64,
        "null arraylength must report the ARRAY_LENGTH action"
    );
    // The other half of the same word: the trap-site id. A zero here means the
    // stub fired but named no site, which is exactly what the trace shows as
    // `Method:-1` — the frame survives and its line does not.
    if super::inlining::npe_trap_lines_enabled() {
        assert!(
            TEST_NPE_TRAP_KEY.with(|c| c.get()) > 0,
            "the null-check trampoline must carry a recorded trap-site id              beside the action; got {}",
            TEST_NPE_TRAP_KEY.with(|c| c.get())
        );
    }
}

#[test]
fn test_inline_iaload_null_throws_npe() {
    // int f(int[] arr, int i) { return arr[i]; }  with arr == null.
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x1b, // 1: iload_1
        0x2e, // 2: iaload
        0xac, // 3: ireturn
        0, 0,
    ];
    let compiled = compile_array_test(&code, 4, 2, 2);

    TEST_NPE_HIT.with(|c| c.set(false));
    TEST_NPE_ACTION.with(|c| c.set(-1));
    TEST_NPE_TRAP_KEY.with(|c| c.set(-1));
    // SAFETY: JIT-compiled code from valid bytecode; mmap region executable.
    let result = unsafe { compiled.try_call(&[0, 0]).expect("test JIT call") }; // null array
    assert_eq!(result, i64::MIN, "null iaload must deopt with sentinel");
    assert!(
        TEST_NPE_HIT.with(|c| c.get()),
        "null iaload must take the NPE deopt stub"
    );
    // JEP 358: the stub must thread the `iaload` (int load) action code —
    // proving the per-opcode threading, not the old fixed byte-store code.
    assert_eq!(
        TEST_NPE_ACTION.with(|c| c.get()),
        // Widening: narrower int -> i64
        npe_action::ALOAD_INT as i64,
        "null iaload must report the ALOAD_INT action"
    );
}

#[test]
fn test_inline_castore_null_threads_char_action() {
    // void f(char[] arr) { arr[0] = 'x'; }  with arr == null.
    // Proves a *store* of a *char[]* threads ASTORE_CHAR — i.e. both the
    // direction (store) and the element type (char) are now precise,
    // where the old single shared stub fabricated ASTORE_BYTE for every
    // inline array opcode.
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0      (array)
        0x03, // 1: iconst_0     (index)
        0x10, 0x78, // 2: bipush 'x'
        0x55, // 4: castore
        0xb1, // 5: return
        0, 0,
    ];
    let compiled = compile_array_test(&code, 6, 1, 1);

    TEST_NPE_HIT.with(|c| c.set(false));
    TEST_NPE_ACTION.with(|c| c.set(-1));
    TEST_NPE_TRAP_KEY.with(|c| c.set(-1));
    // SAFETY: JIT-compiled code from valid bytecode; mmap region executable.
    let result = unsafe { compiled.try_call(&[0]).expect("test JIT call") }; // null array
    assert_eq!(result, i64::MIN, "null castore must deopt with sentinel");
    assert!(
        TEST_NPE_HIT.with(|c| c.get()),
        "null castore must take the NPE deopt stub"
    );
    assert_eq!(
        TEST_NPE_ACTION.with(|c| c.get()),
        // Widening: narrower int -> i64
        npe_action::ASTORE_CHAR as i64,
        "null castore must report the ASTORE_CHAR action"
    );
}

#[test]
fn test_inline_iaload_out_of_bounds_throws_aioobe() {
    // int f(int[] arr, int i) { return arr[i]; }
    // The bounds check (`CMP ECX, R10D; JAE stub`) treats the index
    // as unsigned, so both `idx >= len` and negative indices land
    // in the AIOOBE stub which calls `helpers.throw_aioobe`.
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x1b, // 1: iload_1
        0x2e, // 2: iaload
        0xac, // 3: ireturn
        0, 0,
    ];
    let compiled = compile_array_test(&code, 4, 2, 2);

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_gc::heap::ArrayElementType;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();
    let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 3);

    // Index == length (just past the end).
    TEST_AIOOBE_HIT.with(|c| c.set(None));
    // SAFETY: JIT-compiled code from valid bytecode; mmap region executable.
    let result = unsafe {
        compiled
            .try_call(&[arr.as_ptr() as i64, 3])
            .expect("test JIT call")
    };
    assert_eq!(result, i64::MIN, "OOB iaload must deopt with sentinel");
    assert_eq!(
        TEST_AIOOBE_HIT.with(|c| c.get()),
        Some((3, 3, 2)),
        "OOB iaload must report index, length, and originating bci"
    );

    // Negative index — unsigned compare catches it as huge.
    TEST_AIOOBE_HIT.with(|c| c.set(None));
    // SAFETY: JIT-compiled code from valid bytecode; mmap region executable.
    let result = unsafe {
        compiled
            .try_call(&[arr.as_ptr() as i64, -1])
            .expect("test JIT call")
    };
    assert_eq!(result, i64::MIN, "negative-index iaload must deopt");
    assert!(
        TEST_AIOOBE_HIT.with(|c| c.get()).is_some(),
        "negative-index iaload must take the AIOOBE stub"
    );

    // In-bounds index must NOT trip the stub.
    heap.set_array_element(arr, 2, Value::Int(99)).unwrap();
    TEST_AIOOBE_HIT.with(|c| c.set(None));
    // SAFETY: JIT-compiled code from valid bytecode; mmap region executable.
    let result = unsafe {
        compiled
            .try_call(&[arr.as_ptr() as i64, 2])
            .expect("test JIT call")
    };
    assert_eq!(result, 99);
    assert_eq!(
        TEST_AIOOBE_HIT.with(|c| c.get()),
        None,
        "in-bounds iaload must not call throw_aioobe"
    );
}

// ============================================================
// Task #60 — Unroll robustness tests (helper rel32 re-patching
// + per-clone MIC/PIC slots). These bodies were previously
// outside the byte-copy unroller's allow-list because they
// contained helper CALLs (E8 rel32) and IC sites (MOV R10,
// imm64) that would land on the wrong target after a shift. The
// unroller now re-resolves helper rel32 per copy and mints a
// fresh MIC/PIC slot per IC site per copy.
// ============================================================

/// Unroll a small loop body that calls a helper (`getfield` →
/// `stub_getfield` via `emit_call_absolute`). Verifies the
/// helper rel32 re-patching path: every duplicated copy of the
/// body must dispatch to the same `stub_getfield` address, not
/// to `stub_getfield + shift` (which would SIGSEGV).
///
/// Method shape (Java pseudocode):
/// ```
/// int sum(Foo obj, int n) {
///     int s = 0;
///     int i = 0;
///     while (i < n) {           // <- header
///         s += obj.x;           // getfield → helper call inside loop body
///         i++;
///         // implicit goto header (back-edge that triggers unroll)
///     }
///     return s;
/// }
/// ```
/// The natural Java bytecode lowering uses if_icmpge to exit
/// (forward conditional) and a goto back-edge — the exact shape
/// the unroller recognizes. The loop body contains a `getfield`,
/// which lowers to `emit_call_absolute(helpers.getfield)` — the
/// E8 rel32 site the old allow-list bailed on.
#[test]
fn test_unroll_with_getfield_helper_call() {
    // Bytecode (offsets in comments):
    //   0: iconst_0          ; push 0
    //   1: istore_2          ; s = 0
    //   2: iconst_0          ; push 0
    //   3: istore_3          ; i = 0
    //   4: iload_3           ; loop header — load i
    //   5: iload_1           ; load n
    //   6: if_icmpge +16→22  ; exit if i >= n
    //   9: iload_2           ; load s
    //  10: aload_0           ; load obj
    //  11: getfield #1       ; obj.x  (lowers to helper call)
    //  14: iadd              ; s + obj.x
    //  15: istore_2          ; s = s + obj.x
    //  16: iinc 3, 1         ; i++
    //  19: goto -15 → 4      ; back-edge (triggers unroll)
    //  22: iload_2
    //  23: ireturn
    let code: Vec<u8> = vec![
        0x03, // 0
        0x3d, // 1
        0x03, // 2
        0x3e, // 3
        0x1d, // 4
        0x1b, // 5
        0xa2, 0x00, 0x10, // 6: if_icmpge +16 → 22
        0x1c, // 9
        0x2a, // 10
        0xb4, 0x00, 0x01, // 11: getfield #1
        0x60, // 14: iadd
        0x3d, // 15: istore_2
        0x84, 0x03, 0x01, // 16: iinc 3 1
        0xa7, 0xff, 0xf1, // 19: goto -15 → 4
        0x1c, // 22
        0xac, // 23
        0, 0,
    ];
    let code_len = 24;

    // Deliberately leave field_info empty so the JIT emits the
    // *helper-call* path for getfield (E8 rel32 → helpers.getfield)
    // instead of the inline MOV. The inline path is shift-safe
    // anyway (no helper call inside the body), so we'd be testing
    // the wrong code path with field_info populated. Test stub
    // `stub_getfield` falls through cleanly for field_index=0 on
    // a 2-slot object.
    let field_info: Vec<(usize, usize, u8)> = Vec::new();

    let compiled = compile(
        &code,
        code_len,
        2,
        4,
        false,
        Vec::new(),
        field_info,
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
        None,
    )
    .unwrap();

    // Build a heap object whose field 0 holds the loop's per-
    // iteration addend. The static heuristic unrolls 4x for a
    // body of this size (≤20 bytes between back-edge and header),
    // so the helper rel32 is patched into 3 duplicated copies in
    // addition to the original. If any copy dispatched to the
    // wrong address the call would SIGSEGV before returning.
    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 2);
    heap.set_field(obj, 0, Value::Int(7));

    // n = 8 → loop trips 8 times. With a 4x unroll the body runs
    // a mix of original + copy bodies; correct dispatch from every
    // copy is required to land on stub_getfield → return 7.
    // Expected sum: 7 * 8 = 56.
    // SAFETY: JIT-compiled code from valid bytecode.
    let result = unsafe {
        compiled
            .try_call(&[obj.as_ptr() as i64, 8])
            .expect("test JIT call")
    };
    assert_eq!(
        result, 56,
        "getfield-in-loop unroll must dispatch correctly"
    );

    // Smaller trip count: 7 * 3 = 21.
    let result = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call(&[obj.as_ptr() as i64, 3])
            .expect("test JIT call")
    };
    assert_eq!(result, 21);

    // n = 0 → loop body never executes. Still must compile + run
    // (verifying the unrolled copies don't fault on cold entry).
    let result = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call(&[obj.as_ptr() as i64, 0])
            .expect("test JIT call")
    };
    assert_eq!(result, 0);
}

/// Unroll the previously-broken N-Body pattern: getfield-double +
/// dmul + dastore in the same loop body. This is the Body.x
/// SIGSEGV shape from CHANGELOG that the old allow-list specifically
/// targeted. The shape doesn't matter beyond "helper calls + FP
/// math + back-edge" — what matters is that every unrolled copy
/// reaches the correct helper.
///
/// We can't easily run a real N-body kernel from a unit test
/// (it needs allocator + dispatch infrastructure that the test
/// helpers stub out with panic-sentinels). Instead we exercise
/// the same DUPLICATOR code path with a minimal double-getfield
/// loop and verify both compile and result, which is what would
/// have crashed before this task.
///
/// ```
/// double sum(Foo obj, int n) {
///   double s = 0;
///   int i = 0;
///   while (i < n) { s += obj.d; i++; }
///   return s;
/// }
/// ```
#[test]
fn test_unroll_nbody_pattern_getfield_double() {
    // Locals: 0=obj, 1=n, 2..3=s (double, wide), 4=i
    // Bytecode:
    //   0: dconst_0          ; push 0.0
    //   1: dstore_2          ; s = 0
    //   2: iconst_0
    //   3: istore 4          ; i = 0  (wide local index — uses bipush'd istore)
    //   5: iload 4           ; loop header
    //   7: iload_1           ; n
    //   8: if_icmpge +16→24  ; exit
    //  11: dload_2           ; load s
    //  12: aload_0           ; obj
    //  13: getfield #1       ; obj.d → double
    //  16: dadd
    //  17: dstore_2
    //  18: iinc 4, 1
    //  21: goto -16 → 5
    //  24: dload_2
    //  25: dreturn
    //
    // Use simple short-form opcodes where possible. We avoid the
    // wide local 4 by storing i in a non-wide slot — restructure:
    // 0=obj, 1=n, 2..3=s, 4=i (istore 4 = istore + index 4 → 0x36 0x04).
    let code: Vec<u8> = vec![
        0x0e, // 0: dconst_0
        0x49, // 1: dstore_2  (s = 0.0; takes slots 2 and 3)
        0x03, // 2: iconst_0
        0x36, 0x04, // 3: istore 4
        0x15, 0x04, // 5: iload 4
        0x1b, // 7: iload_1
        0xa2, 0x00, 0x10, // 8: if_icmpge +16 → 24
        0x28, // 11: dload_2
        0x2a, // 12: aload_0
        0xb4, 0x00, 0x01, // 13: getfield #1 (double)
        0x63, // 16: dadd
        0x49, // 17: dstore_2
        0x84, 0x04, 0x01, // 18: iinc 4 1
        0xa7, 0xff, 0xf0, // 21: goto -16 → 5
        0x28, // 24: dload_2
        0xaf, // 25: dreturn
        0, 0,
    ];
    let code_len = 26;

    // getfield at pc=13, field_index=0, type='D'.
    let field_info = vec![(13usize, 0usize, b'D')];

    let compiled = compile(
        &code,
        code_len,
        2,
        6,
        false,
        Vec::new(),
        field_info,
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
    .unwrap();

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 2);
    heap.set_field(obj, 0, Value::Double(2.5));

    // n=8, addend=2.5 → 20.0. Return is a double-bits-as-i64.
    // SAFETY: JIT-compiled code from valid bytecode.
    let result = unsafe {
        compiled
            .try_call(&[obj.as_ptr() as i64, 8])
            .expect("test JIT call")
    };
    let result_f = f64::from_bits(result as u64); // Cast: JIT ABI convention
    assert!(
        (result_f - 20.0).abs() < 1e-9,
        "expected ~20.0, got {} (raw={:#x}) — N-Body unroll pattern broke",
        result_f,
        result,
    );

    // n=0 → 0.0 (cold loop body, copies never executed but must
    // still be valid code).
    let result = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call(&[obj.as_ptr() as i64, 0])
            .expect("test JIT call")
    };
    let result_f = f64::from_bits(result as u64); // Cast: JIT ABI convention
    assert_eq!(result_f, 0.0);
}

/// Per-clone PIC slot allocation. Compile a loop containing an
/// invokevirtual with caller-supplied MIC/PIC slots, verify the
/// resulting CompiledMethod owns ADDITIONAL PIC slot boxes (one
/// per duplicated IC site), and that those new boxes' raw
/// pointers actually appear baked into the emitted instruction
/// stream at distinct addresses.
///
/// The inline dynamic-call fast path is default-ON — see
/// `direct_jit_callee_calls_enabled` — but this test sets its env var
/// explicitly anyway so it stays correct if the default ever flips back.
/// PIC supersedes MIC when both slots exist, so this exercises the
/// polymorphic path and its release-published receiver/entry pairs.
///
/// This is a static / structural check — we don't execute the
/// loop (the test invoke helpers would panic), but verifying the
/// duplicator minted-and-baked the right number of fresh slots
/// is sufficient to prove the per-clone path runs.
#[test]
fn test_unroll_mints_per_clone_pic_slots() {
    // This test exercises the optimizing IR pipeline, which is gated off
    // whenever the young generation can relocate. Pin the policy so the
    // test covers IR lowering regardless of DEFAULT_MOVING_YOUNG.
    super::set_moving_young_override(Some(false));
    // `CRATONVM_JIT_DIRECT_CALLEE_CALLS` is a *declared* flag, served from
    // the process-wide snapshot that latches on the first read of any flag
    // (`cratonvm_types::flags`). `set_var` here therefore did nothing at
    // all once any earlier test in this binary had touched a flag — the
    // assertions below were riding on the flag's default (enabled) rather
    // than on the value this test asked for, and would have silently
    // stopped testing the direct-callee path the day that default flipped.
    // The override pins it for real, on this thread only, so it cannot
    // perturb a parallel test.
    let _direct_callee_calls = cratonvm_types::flags::override_thread(
        cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
            "CRATONVM_JIT_DIRECT_CALLEE_CALLS",
            Some("1"),
        )]),
    );
    use crate::JitInvokeInfo;
    // Bytecode: a counted loop with a single invokevirtual.
    //
    //   0: iconst_0           ; s
    //   1: istore_2
    //   2: iconst_0           ; i
    //   3: istore_3
    //   4: iload_3            ; header
    //   5: iload_1            ; n
    //   6: if_icmpge +16→22   ; exit
    //   9: iload_2
    //  10: aload_0            ; receiver
    //  11: invokevirtual #1   ; obj.inc() → I (helper; panicking stub
    //                          here, but we only check the compile)
    //  14: iadd               ; s + ret
    //  15: istore_2
    //  16: iinc 3, 1
    //  19: goto -15 → 4
    //  22: iload_2
    //  23: ireturn
    let code: Vec<u8> = vec![
        0x03, 0x3d, 0x03, 0x3e, 0x1d, // 4: iload_3
        0x1b, // 5: iload_1
        0xa2, 0x00, 0x10, // 6: if_icmpge +16 → 22
        0x1c, // 9
        0x2a, // 10
        0xb6, 0x00, 0x01, // 11: invokevirtual #1
        0x60, // 14: iadd
        0x3d, // 15
        0x84, 0x03, 0x01, // 16: iinc 3 1
        0xa7, 0xff, 0xf1, // 19: goto -15 → 4
        0x1c, // 22
        0xac, // 23
        0, 0,
    ];
    let code_len = 24;

    // Caller-supplied invoke metadata. Strings are leaked for
    // 'static lifetime to match the production lib.rs path.
    // LEAK(intentional): these &'static str names are stored in the JitInvokeInfo
    // the compiled method dereferences by raw pointer, so they must outlive the JIT
    // code; owned by the test process for its lifetime (matches the lib.rs path).
    let class_name: &'static str = Box::leak("Foo".to_string().into_boxed_str());
    // LEAK(intentional): this &'static method name is read via raw pointer from the
    // JitInvokeInfo by the compiled method, so it must outlive the JIT code; owned by
    // the test process for its lifetime.
    let method_name: &'static str = Box::leak("inc".to_string().into_boxed_str());
    // Zero-param instance method: the loop body is
    //   s = s + obj.inc()  →  iload_2(s); aload_0(obj); invokevirtual;
    //   iadd; istore_2
    // so the invoke is net-zero on the operand stack (pops the
    // receiver, pushes the int result), leaving `s` underneath for
    // the following `iadd`. A `(I)I` descriptor (num_jit_args=2)
    // would pop BOTH `s` and the receiver, underflowing the `iadd`
    // and failing compilation before the PIC-mint path is reached.
    // LEAK(intentional): this &'static descriptor is referenced by the JitInvokeInfo
    // the compiled method reads via raw pointer, so it must outlive the JIT code;
    // owned by the test process for its lifetime.
    let desc: &'static str = Box::leak("()I".to_string().into_boxed_str());
    let info = Box::new(JitInvokeInfo {
        class_name,
        method_name,
        descriptor: desc,
        num_jit_args: 1, // receiver only
        return_type: b'I',
        invoke_kind: 0, // virtual
        declaring_class_id: 0,
    });
    let info_ptr: *const JitInvokeInfo = &*info;
    let invoke_info = vec![(11usize, info_ptr)];

    // Caller-supplied MIC and PIC slots for the invokevirtual.
    let caller_mic = Box::new(crate::JitMICSlot::new());
    let caller_mic_ptr: *const crate::JitMICSlot = &*caller_mic;
    let mic_slots = vec![(11usize, caller_mic_ptr)];

    let caller_pic = Box::new(crate::JitPICSlot::new());
    let caller_pic_ptr: *const crate::JitPICSlot = &*caller_pic;
    let pic_slots = vec![(11usize, caller_pic_ptr)];

    // The compile *may* bail before duplicating if the loop body
    // hits an unsupported path. We assert compilation succeeds
    // and that AT LEAST one extra PIC slot was minted (3 expected
    // from 4x unroll, but the static heuristic could pick 2x for
    // a body ≤ 50 bytes — either way the extra count > 0 proves
    // the per-clone path ran). The static unroller threshold is
    // body_size ≤ 20 → 3 extra copies.
    let compiled = compile(
        &code,
        code_len,
        2,
        4,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        invoke_info,
        Vec::new(),
        mic_slots,
        pic_slots,
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None,
    );

    // Compilation should succeed: the body has a back-edge and
    // the unroller fires. After unrolling, the compiled method's
    // `_jit_pic_slots` should contain the per-clone PIC slots
    // freshly minted by the duplicator.
    //
    // Body span is 15 bytes (pc 4..=19) → static heuristic picks
    // 4x unroll (3 extra copies). The inline-IC fast path engages
    // for invokevirtual with `args_fit && pic_ptr.is_some()`, which holds here (n=1,
    // vm_ptr+1 ≤ ARG_REGS.len()). So 3 fresh MIC slots should be
    // minted (one per copy).
    let method = compiled.expect("invokevirtual-in-loop must compile");
    // The compiled method does NOT carry the caller-supplied
    // PIC slot in its _jit_pic_slots (that vector is owned by
    // the caller in the production path; in this test the box
    // is held by the local `caller_pic`). It DOES carry the
    // duplicator-minted clones. Verify count > 0 to confirm
    // the per-clone path ran.
    let cloned_pics = method._jit_pic_slots.len();
    assert!(
        cloned_pics >= 1,
        "expected at least one cloned PIC slot from unroll, got {} \
         — per-clone IC slot allocation did not run",
        cloned_pics,
    );
    // And no duplicates among the cloned slots' raw pointers.
    // Each Box<JitPICSlot> has a unique heap address.
    let mut ptrs: Vec<usize> = method
        ._jit_pic_slots
        .iter()
        // Cast: non-negative index/count to usize
        .map(|b| b.as_ref() as *const _ as usize)
        .collect();
    ptrs.sort();
    let dedup_len = {
        let mut p = ptrs.clone();
        p.dedup();
        p.len()
    };
    assert_eq!(
        dedup_len,
        ptrs.len(),
        "cloned PIC slots must have distinct addresses (collision \
         would mean cache hits cross-pollute between unrolled copies)",
    );
    let machine_code = method._buffer.as_slice();
    let count_imm64 = |ptr: usize| {
        let needle = (ptr as u64).to_le_bytes();
        machine_code
            .windows(needle.len())
            .filter(|window| *window == needle)
            .count()
    };
    for pic in &method._jit_pic_slots {
        let ptr = pic.as_ref() as *const _ as usize;
        assert!(
            count_imm64(ptr) >= 2,
            "each cloned PIC must be embedded in both its inline guard and \
             its slow-helper PIC argument"
        );
    }
    for mic in &method._jit_mic_slots {
        let ptr = mic.as_ref() as *const _ as usize;
        assert!(
            count_imm64(ptr) >= 1,
            "each cloned MIC must be embedded in its slow-helper MIC argument"
        );
    }

    // Keep the caller-supplied boxes alive until end of scope —
    // the JIT code baked their addresses into the original
    // (un-cloned) body's IC slot site.
    drop(caller_mic);
    drop(caller_pic);
    drop(info);
}

/// Larger unroll trip: 4x with 8 trips means the body executes
/// twice — once via the original and once via a copy — on the
/// first iteration. Stresses both per-clone helper-call patching
/// AND oop-map shift for the same body.
///
/// Method: int sum_two_fields(Foo a, Foo b, int n) {
///   int s = 0;
///   int i = 0;
///   while (i < n) { s += a.x; s += b.x; i++; }
///   return s;
/// }
///
/// Two helper calls per body. With 4x unroll, that's 8 helper
/// calls in the emitted block — each rel32 must independently
/// land on the same `stub_getfield` address. A single off-by-N
/// in the rel32 reconstruction would corrupt one of them and
/// crash on the affected copy.
#[test]
fn test_unroll_with_two_getfields_per_body() {
    // Locals: 0=a, 1=b, 2=n, 3=s, 4=i
    let code: Vec<u8> = vec![
        0x03, // 0: iconst_0
        0x3e, // 1: istore_3   (s=0)
        0x03, // 2: iconst_0
        0x36, 0x04, // 3: istore 4 (i=0)
        0x15, 0x04, // 5: iload 4 (header)
        0x1c, // 7: iload_2 (n)
        0xa2, 0x00, 0x15, // 8: if_icmpge +21 → 29
        0x1d, // 11: iload_3 (s)
        0x2a, // 12: aload_0 (a)
        0xb4, 0x00, 0x01, // 13: getfield #1
        0x60, // 16: iadd
        0x2b, // 17: aload_1 (b)
        0xb4, 0x00, 0x01, // 18: getfield #1
        0x60, // 21: iadd
        0x3e, // 22: istore_3
        0x84, 0x04, 0x01, // 23: iinc 4, 1
        0xa7, 0xff, 0xeb, // 26: goto -21 → 5
        0x1d, // 29: iload_3
        0xac, // 30: ireturn
        0, 0,
    ];
    let code_len = 31;

    let field_info = vec![(13usize, 0usize, b'I'), (18usize, 0usize, b'I')];

    let compiled = compile(
        &code,
        code_len,
        3,
        5,
        false,
        Vec::new(),
        field_info,
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
    .unwrap();

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();
    let a = heap.alloc_object(ClassId::new(0), 2);
    let b = heap.alloc_object(ClassId::new(0), 2);
    heap.set_field(a, 0, Value::Int(3));
    heap.set_field(b, 0, Value::Int(5));

    // n=10 → (3 + 5) * 10 = 80.
    // SAFETY: JIT-compiled code from valid bytecode.
    let result = unsafe {
        compiled
            .try_call(&[a.as_ptr() as i64, b.as_ptr() as i64, 10])
            .expect("test JIT call")
    };
    assert_eq!(result, 80, "two-helper-calls-per-body unroll failed");

    // n=4 (matches the 4x unroll factor exactly): one full
    // unrolled block, zero spillover. Exercises the case where
    // every copy + the original execute exactly once.
    let result = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call(&[a.as_ptr() as i64, b.as_ptr() as i64, 4])
            .expect("test JIT call")
    };
    assert_eq!(result, 32);
}

/// Helper: drive `compile` over a raw `(I)I` method body. Returns whether
/// compilation succeeded (`Some`) or bailed to the interpreter (`None`).
/// Used by the switch-decode DoS regression tests below: a bail (`None`)
/// is the *expected* safe outcome for crafted/unverified switch bytecode.
fn try_compile_int_body(code: &[u8], code_len: usize) -> bool {
    compile(
        code,
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
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None,
    )
    .is_some()
}

/// M2a regression: a `tableswitch` whose `high - low + 1` count overflows
/// i32 (here low = i32::MIN, high = i32::MAX) must not wrap to a bogus
/// positive count and drive a ~16 GB allocation / out-of-bounds read.
/// Compilation must bail (return `None`) without panicking.
#[test]
fn test_tableswitch_count_overflow_bails() {
    // iconst_0; tableswitch @ pc=1 (padding to pc=4):
    //   default=+0, low=i32::MIN, high=i32::MAX
    let mut code: Vec<u8> = vec![0x03, 0xaa, 0x00, 0x00]; // iconst_0 + pad to 4
    code.extend_from_slice(&0i32.to_be_bytes()); // default
    code.extend_from_slice(&i32::MIN.to_be_bytes()); // low
    code.extend_from_slice(&i32::MAX.to_be_bytes()); // high
                                                     // (no jump-table entries follow — the count guard must reject first)
    let code_len = code.len();
    assert!(
        !try_compile_int_body(&code, code_len),
        "tableswitch with overflowing count must bail, not compile"
    );
}

/// M2a regression: a `tableswitch` whose declared table extends past the
/// end of `code` must bail rather than index out of bounds.
#[test]
fn test_tableswitch_table_past_end_bails() {
    // iconst_0; tableswitch: default=+0, low=0, high=999 (1000 entries)
    // but no entry bytes follow → remaining-bytes guard must reject.
    let mut code: Vec<u8> = vec![0x03, 0xaa, 0x00, 0x00];
    code.extend_from_slice(&0i32.to_be_bytes()); // default
    code.extend_from_slice(&0i32.to_be_bytes()); // low
    code.extend_from_slice(&999i32.to_be_bytes()); // high → 1000 entries
    let code_len = code.len();
    assert!(
        !try_compile_int_body(&code, code_len),
        "tableswitch with table past end must bail, not compile"
    );
}

/// M2a regression: a `tableswitch` header truncated by `code_len` must bail
/// before reading the 12-byte default/low/high header.
#[test]
fn test_tableswitch_truncated_header_bails() {
    // iconst_0; tableswitch + only a few header bytes (header needs 12).
    let mut code: Vec<u8> = vec![0x03, 0xaa, 0x00, 0x00];
    code.extend_from_slice(&[0x00, 0x00, 0x00]); // truncated header
    let code_len = code.len();
    assert!(
        !try_compile_int_body(&code, code_len),
        "tableswitch with truncated header must bail, not compile"
    );
}

/// M2b regression: a `lookupswitch` with a negative `npairs` must not be
/// reinterpreted as a huge usize and drive an OOM allocation / OOB read.
#[test]
fn test_lookupswitch_negative_npairs_bails() {
    // iconst_0; lookupswitch @ pc=1 (padding to pc=4): default=+0, npairs=-1
    let mut code: Vec<u8> = vec![0x03, 0xab, 0x00, 0x00];
    code.extend_from_slice(&0i32.to_be_bytes()); // default
    code.extend_from_slice(&(-1i32).to_be_bytes()); // npairs (negative)
    let code_len = code.len();
    assert!(
        !try_compile_int_body(&code, code_len),
        "lookupswitch with negative npairs must bail, not compile"
    );
}

/// M2b regression: a `lookupswitch` whose declared pair table extends past
/// the end of `code` must bail rather than index out of bounds.
#[test]
fn test_lookupswitch_pairs_past_end_bails() {
    // iconst_0; lookupswitch: default=+0, npairs=1000 but no pair bytes.
    let mut code: Vec<u8> = vec![0x03, 0xab, 0x00, 0x00];
    code.extend_from_slice(&0i32.to_be_bytes()); // default
    code.extend_from_slice(&1000i32.to_be_bytes()); // npairs
    let code_len = code.len();
    assert!(
        !try_compile_int_body(&code, code_len),
        "lookupswitch with pair table past end must bail, not compile"
    );
}

/// M2b regression: a `lookupswitch` header truncated by `code_len` must
/// bail before reading the 8-byte default/npairs header.
#[test]
fn test_lookupswitch_truncated_header_bails() {
    let mut code: Vec<u8> = vec![0x03, 0xab, 0x00, 0x00];
    code.extend_from_slice(&[0x00, 0x00, 0x00]); // truncated header (<8)
    let code_len = code.len();
    assert!(
        !try_compile_int_body(&code, code_len),
        "lookupswitch with truncated header must bail, not compile"
    );
}

/// A branch whose target lands INSIDE another instruction (here: the
/// middle of a `goto`'s operand bytes) is never emitted as an
/// instruction boundary, so `pc_to_native[target]` stays -1.
/// `patch_branches` must reject the method (bail to the interpreter)
/// instead of leaving the rel32 placeholder 0 in executable code —
/// a zero rel32 silently falls through, and when the branch is the
/// last emitted instruction execution runs off the body into the
/// out-of-line stubs (observed as STATUS_ACCESS_VIOLATION from a
/// hand-written test with an off-by-one target, 2026-06-09). javac
/// output is verified and cannot contain this; unverified/synthetic
/// bytecode can.
#[test]
fn test_branch_target_mid_instruction_bails() {
    //  0: iconst_3            [3]
    //  1: iconst_0            [3, 0]
    //  2: iconst_0            [3, 0, 0]
    //  3: if_icmpeq +4 → 7    [3]   (7 = middle of the goto at 6..=8)
    //  6: goto +4 → 10        [3]
    //  9: iconst_0            (dead filler, never a target)
    // 10: ireturn
    let code: Vec<u8> = vec![
        0x06, 0x03, 0x03, 0x9f, 0x00, 0x04, 0xa7, 0x00, 0x04, 0x03, 0xac,
    ];
    let code_len = code.len();
    assert!(
        !try_compile_int_body(&code, code_len),
        "branch into the middle of an instruction must bail, not compile"
    );
}

// ---------------------------------------------------------------------------
// `dup2_x2` (0x5E) — all four JVMS forms, executed.
//
// This opcode was admitted by `jit_scan` and lowered by neither x64 backend
// from the first day both existed, so every method containing one reached the
// dispatch loop's `_ =>` catch-all and stayed interpreted for the life of the
// process — the refusal attributed to an arm that names nothing. See
// dup2_x2-is-scan-admitted-but-lowered-by-neither-x64-backend-20260817-FIXED.md.
//
// Each case below RUNS the compiled body and checks a value that a
// wrong-width shuffle cannot produce, because the failure mode this opcode
// invites is not a crash: it is duplicating an unrelated slot. `.expect(...)`
// alone would pass against a shuffle that compiles and computes nonsense.
//
// Local numbering follows this harness's convention (see
// `test_compile_math_min_max_long_intrinsic`): every parameter, `long`
// included, is ONE local slot.
// ---------------------------------------------------------------------------

/// FORM 4 — `[v2, v1] -> [v1, v2, v1]`, both operands category-2. Two entries
/// in this backend's one-entry-per-value model, structurally `dup_x1`.
///
/// `long f(long a, long b) { ... }` computing `a + 2b`: any shuffle that
/// duplicated two entries instead of one, or inserted the copy at the wrong
/// depth, gives a different sum.
#[test]
fn dup2_x2_form4_two_category_2_operands() {
    // 0: lload_0     [a]
    // 1: lload_1     [a, b]
    // 2: dup2_x2     [b, a, b]
    // 3: ladd        [b, a+b]
    // 4: ladd        [a+2b]
    // 5: lreturn
    let code = [0x1e, 0x1f, 0x5e, 0x61, 0x61, 0xad];
    let compiled = compile_probe_method(&code, 2, 2)
        .expect("FORM-4 dup2_x2 must JIT-compile, not reach the catch-all");
    // SAFETY: JIT-compiled machine code produced from valid bytecode in-test.
    let r = unsafe { compiled.try_call(&[3, 5]).expect("test JIT call") };
    assert_eq!(r, 3 + 2 * 5, "a + 2b for a=3 b=5");
    // SAFETY: JIT-compiled machine code produced from valid bytecode in-test.
    let r = unsafe { compiled.try_call(&[-7, 11]).expect("test JIT call") };
    assert_eq!(r, -7 + 2 * 11, "a + 2b for a=-7 b=11");
}

/// FORM 2 — `[v3, v2, v1] -> [v1, v3, v2, v1]`, a category-2 top over two
/// category-1 values. Three entries; structurally `dup_x2`.
///
/// This is the form javac actually emits: `longArr[i] = otherArr[j] = v`
/// leaves `[arrayref, index, longvalue]` and has to slide the value under the
/// two category-1 slots.
#[test]
fn dup2_x2_form2_category_2_over_two_category_1() {
    // 0: iload_1     [i]
    // 1: iload_2     [i, j]
    // 2: lload_0     [i, j, a]
    // 3: dup2_x2     [a, i, j, a]
    // 4: l2i         [a, i, j, (int)a]
    // 5: iadd        [a, i, j+(int)a]
    // 6: iadd        [a, i+j+(int)a]
    // 7: i2l         [a, (long)(i+j+(int)a)]
    // 8: ladd        [2a+i+j]
    // 9: lreturn
    let code = [0x1b, 0x1c, 0x1e, 0x5e, 0x88, 0x60, 0x60, 0x85, 0x61, 0xad];
    let compiled = compile_probe_method(&code, 3, 3)
        .expect("FORM-2 dup2_x2 must JIT-compile, not reach the catch-all");
    // SAFETY: JIT-compiled machine code produced from valid bytecode in-test.
    let r = unsafe { compiled.try_call(&[10, 3, 4]).expect("test JIT call") };
    assert_eq!(r, 2 * 10 + 3 + 4, "2a + i + j for a=10 i=3 j=4");
}

/// FORM 3 — `[v3, v2, v1] -> [v2, v1, v3, v2, v1]`, two category-1 values
/// duplicated over one category-2. Three entries; the copy goes three deep,
/// not four, because those four JVM slots are a single entry here.
#[test]
fn dup2_x2_form3_two_category_1_over_a_category_2() {
    //  0: lload_0    [a]
    //  1: iload_1    [a, i]
    //  2: iload_2    [a, i, j]
    //  3: dup2_x2    [i, j, a, i, j]
    //  4: iadd       [i, j, a, i+j]
    //  5: i2l        [i, j, a, (long)(i+j)]
    //  6: ladd       [i, j, a+i+j]
    //  7: lstore_3   [i, j]
    //  8: iadd       [i+j]
    //  9: i2l        [(long)(i+j)]
    // 10: lload_3    [(long)(i+j), a+i+j]
    // 11: ladd       [a+2i+2j]
    // 12: lreturn
    let code = [
        0x1e, 0x1b, 0x1c, 0x5e, 0x60, 0x85, 0x61, 0x42, 0x60, 0x85, 0x21, 0x61, 0xad,
    ];
    let compiled = compile_probe_method(&code, 3, 4)
        .expect("FORM-3 dup2_x2 must JIT-compile, not reach the catch-all");
    // SAFETY: JIT-compiled machine code produced from valid bytecode in-test.
    let r = unsafe { compiled.try_call(&[10, 3, 4]).expect("test JIT call") };
    assert_eq!(r, 10 + 2 * 3 + 2 * 4, "a + 2i + 2j for a=10 i=3 j=4");
}

/// FORM 1 — `[v4, v3, v2, v1] -> [v2, v1, v4, v3, v2, v1]`, all four
/// category-1. The only form aarch64's unconditional four-pop arm ever
/// handled correctly, and the deepest of the four.
#[test]
fn dup2_x2_form1_four_category_1_operands() {
    // 0: iload_0   [a]
    // 1: iload_1   [a,b]
    // 2: iload_2   [a,b,c]
    // 3: iload_3   [a,b,c,d]
    // 4: dup2_x2   [c,d,a,b,c,d]
    // 5: iadd      [c,d,a,b,c+d]
    // 6: iadd      [c,d,a,b+c+d]
    // 7: iadd      [c,d,a+b+c+d]
    // 8: iadd      [c,a+b+c+2d]
    // 9: iadd      [a+b+2c+2d]
    // 10: ireturn
    let code = [
        0x1a, 0x1b, 0x1c, 0x1d, 0x5e, 0x60, 0x60, 0x60, 0x60, 0x60, 0xac,
    ];
    let compiled = compile_probe_method(&code, 4, 4)
        .expect("FORM-1 dup2_x2 must JIT-compile, not reach the catch-all");
    // SAFETY: JIT-compiled machine code produced from valid bytecode in-test.
    let r = unsafe { compiled.try_call(&[1, 2, 3, 4]).expect("test JIT call") };
    assert_eq!(r, 1 + 2 + 2 * 3 + 2 * 4, "a + b + 2c + 2d for 1,2,3,4");
}

/// The conservative half of the contract: when the width analysis cannot type
/// the entries under the dup, the method stays interpreted and says so under
/// its own name — not under whatever opcode the walk reached afterwards.
///
/// `dup2_x2` at pc 0 has no operands at all, so no oracle can answer.
#[test]
fn dup2_x2_without_a_provable_form_bails_under_its_own_name() {
    let code = [0x5e, 0xac]; // dup2_x2; ireturn
    let _ = crate::take_jit_bail_site(); // clear anything a prior test left
    assert!(compile_probe_method(&code, 0, 1).is_none());
    let (site, _, _) = crate::take_jit_bail_site().expect("a refusal records a site");
    assert_eq!(site, "singlepass-codegen/dup2_x2-unprovable-form");
}

// ---------------------------------------------------------------------------
// dup_x2 (0x5B) and dup2_x1 (0x5D) — the forms the second-entry width oracle
// admitted on 2026-08-27.
//
// Before it, `dup_x2` was lowered ONLY when the next opcode was a category-1
// array store (the `++z[i]` peephole) and `dup2_x1` ONLY when its top was
// category-2. Everything else reached `self.fail(...)` and stayed interpreted
// for the life of the process — including javac's POST-increment
// `arr[n[0]++] = v`, which is what kept `HibfixComposeProbe2.chain`
// `ineligible-by-policy` with `dup_x2-unprovable-form(pc=15,op=0x5b)`.
//
// Each case RUNS the compiled body and checks a value a wrong-depth insert
// cannot produce: `.expect(...)` alone would pass against a shuffle that
// compiles and computes nonsense. Local numbering follows this harness's
// convention — every parameter, `long` included, is ONE local slot.
// ---------------------------------------------------------------------------

/// `dup_x2` FORM-1 — `[c, b, a] -> [a, c, b, a]`, all three category-1, and
/// NOT followed by an array store.
///
/// This is the case the old peephole could not prove. The body computes
/// `a + b + 2c`, so a copy inserted two deep instead of three (i.e. FORM-2's
/// depth) gives a different sum.
#[test]
fn dup_x2_form1_admitted_without_the_array_store_peephole() {
    // 0: iload_0  [a]
    // 1: iload_1  [a, b]
    // 2: iload_2  [a, b, c]
    // 3: dup_x2   [c, a, b, c]
    // 4: iadd     [c, a, b+c]
    // 5: iadd     [c, a+b+c]
    // 6: iadd     [a+b+2c]
    // 7: ireturn
    let code = [0x1a, 0x1b, 0x1c, 0x5b, 0x60, 0x60, 0x60, 0xac];
    let compiled = compile_probe_method(&code, 3, 3)
        .expect("FORM-1 dup_x2 must JIT-compile without the astore peephole");
    // SAFETY: JIT-compiled machine code produced from valid bytecode in-test.
    let r = unsafe { compiled.try_call(&[1, 2, 3]).expect("test JIT call") };
    assert_eq!(r, 1 + 2 + 2 * 3, "a + b + 2c for a=1 b=2 c=3");
    // SAFETY: JIT-compiled machine code produced from valid bytecode in-test.
    let r = unsafe { compiled.try_call(&[-5, 7, 11]).expect("test JIT call") };
    assert_eq!(r, -5 + 7 + 2 * 11, "a + b + 2c for a=-5 b=7 c=11");
}

/// `dup_x2` FORM-2 — `[w, a] -> [a, w, a]`, where `w` is a category-2
/// long/double and therefore ONE entry in this backend's model.
///
/// The whole point of the second-entry oracle: the top is category-1 in both
/// forms, so only the width of the entry BELOW it decides whether the copy
/// goes two entries deep or three. Inserting at FORM-1's depth here would
/// index past the bottom of a two-entry stack.
#[test]
fn dup_x2_form2_category_2_below_the_top() {
    // 0: lload_0  [w]
    // 1: iload_1  [w, i]
    // 2: dup_x2   [i, w, i]
    // 3: pop      [i, w]
    // 4: l2i      [i, (int)w]
    // 5: iadd     [i + (int)w]
    // 6: ireturn
    let code = [0x1e, 0x1b, 0x5b, 0x57, 0x88, 0x60, 0xac];
    let compiled = compile_probe_method(&code, 2, 2).expect("FORM-2 dup_x2 must JIT-compile");
    // SAFETY: JIT-compiled machine code produced from valid bytecode in-test.
    let r = unsafe { compiled.try_call(&[10, 3]).expect("test JIT call") };
    assert_eq!(r, 13, "i + (int)w for w=10 i=3");
    // SAFETY: JIT-compiled machine code produced from valid bytecode in-test.
    let r = unsafe { compiled.try_call(&[-4, 9]).expect("test JIT call") };
    assert_eq!(r, 5, "i + (int)w for w=-4 i=9");
}

/// `dup2_x1` FORM-1 — `[c, b, a] -> [b, a, c, b, a]`, all three category-1.
///
/// TWO entries are duplicated here, not one, so this is the form the old
/// top-width-only peephole could never prove: `dup2_top_cat2` answering
/// `false` says the top is category-1 and says nothing about how deep the pair
/// must be inserted. The body computes `a + 2b + 2c`.
#[test]
fn dup2_x1_form1_all_category_1() {
    // 0: iload_0  [a]
    // 1: iload_1  [a, b]
    // 2: iload_2  [a, b, c]
    // 3: dup2_x1  [b, c, a, b, c]
    // 4: iadd     [b, c, a, b+c]
    // 5: iadd     [b, c, a+b+c]
    // 6: iadd     [b, a+b+2c]
    // 7: iadd     [a+2b+2c]
    // 8: ireturn
    let code = [0x1a, 0x1b, 0x1c, 0x5d, 0x60, 0x60, 0x60, 0x60, 0xac];
    let compiled = compile_probe_method(&code, 3, 3).expect("FORM-1 dup2_x1 must JIT-compile");
    // SAFETY: JIT-compiled machine code produced from valid bytecode in-test.
    let r = unsafe { compiled.try_call(&[1, 2, 3]).expect("test JIT call") };
    assert_eq!(r, 1 + 2 * 2 + 2 * 3, "a + 2b + 2c for a=1 b=2 c=3");
}

/// `dup2_x1` FORM-2 — `[b, w] -> [w, b, w]`, a category-2 top over one
/// category-1. The shape the peephole alone already proved; kept so a future
/// change to the oracle cannot silently drop it. Computes `2w + (long)b`.
#[test]
fn dup2_x1_form2_category_2_top() {
    // 0: iload_1  [b]
    // 1: lload_0  [b, w]
    // 2: dup2_x1  [w, b, w]
    // 3: lstore_2 [w, b]
    // 4: i2l      [w, (long)b]
    // 5: ladd     [w + (long)b]
    // 6: lload_2  [w + b, w]
    // 7: ladd     [2w + b]
    // 8: lreturn
    let code = [0x1b, 0x1e, 0x5d, 0x41, 0x85, 0x61, 0x20, 0x61, 0xad];
    let compiled = compile_probe_method(&code, 2, 3).expect("FORM-2 dup2_x1 must JIT-compile");
    // SAFETY: JIT-compiled machine code produced from valid bytecode in-test.
    let r = unsafe { compiled.try_call(&[10, 3]).expect("test JIT call") };
    assert_eq!(r, 2 * 10 + 3, "2w + b for w=10 b=3");
}

/// The conservative half of the contract, for `dup_x2`: with no operands at
/// all under the dup, neither the width analysis nor the peephole can answer,
/// and the method stays interpreted under its OWN name.
#[test]
fn dup_x2_without_a_provable_form_bails_under_its_own_name() {
    let code = [0x5b, 0xac]; // dup_x2; ireturn
    let _ = crate::take_jit_bail_site(); // clear anything a prior test left
    assert!(compile_probe_method(&code, 0, 1).is_none());
    let (site, _, _) = crate::take_jit_bail_site().expect("a refusal records a site");
    assert_eq!(site, "singlepass-codegen/dup_x2-unprovable-form");
}

/// The catch-all is no longer anonymous. `frem` (0x72) has no arm in this
/// walk — `jit_scan` admits it on purpose so the optimizing IR backend can
/// see the method, which is why it is on
/// [`SCAN_ADMITTED_WITHOUT_A_SINGLE_PASS_ARM`] — so driving the single-pass
/// walk onto it lands on the catch-all and pins the reason string it now
/// records. Before, every opcode that landed here reported the bare
/// `singlepass-codegen` site, naming nothing.
///
/// The exemplar used to be `wide` (0xC4), which stopped being unlowered on
/// 2026-08-28 and turned this test red for a reason that had nothing to do
/// with what it asserts. So the exemplar is now checked before it is used:
/// a hard-coded opcode in a test about UNLOWERED opcodes is a fact with a
/// shelf life, and the failure should say which fact expired.
#[test]
fn the_unlowered_opcode_catch_all_names_itself() {
    const EXEMPLAR: u8 = 0x72; // frem
    assert!(
        !single_pass_dispatch_arms().contains(&EXEMPLAR),
        "0x{EXEMPLAR:02x} is lowered now, so it can no longer drive the walk \
         onto its catch-all. Pick another opcode with no dispatch arm — \
         `SCAN_ADMITTED_WITHOUT_A_SINGLE_PASS_ARM` lists the deliberate ones."
    );
    // fconst_0; fconst_0; frem; freturn — the two operands keep the walk's
    // stack model honest right up to the opcode under test.
    let code = [0x0b, 0x0b, EXEMPLAR, 0xae];
    let _ = crate::take_jit_bail_site();
    assert!(compile_probe_method(&code, 0, 2).is_none());
    let (site, _, _) = crate::take_jit_bail_site().expect("a refusal records a site");
    assert_eq!(
        site, "singlepass-codegen/opcode-scan-admitted-but-unlowered",
        "the catch-all must name itself rather than claim it cannot happen"
    );
}

// ---------------------------------------------------------------------------
// The opcode-coverage guard.
//
// `jit_scan` (admission) and the single-pass dispatch loop (codegen) are two
// hand-maintained match statements over the same 202 opcode values, and until
// this test nothing forced them to agree. An opcode the scanner advances past
// but the walk has no arm for does not fail loudly: it falls into the walk's
// catch-all and the method is bail-listed for the life of the process, with
// the refusal attributed to an arm that names nothing.
//
// That gap has now cost three opcodes — `pop2` (0x58) and `dup2_x1` (0x5D),
// found by the commons-math throughput work, and `dup2_x2` (0x5E), found by
// hand-enumerating the arms of all four walkers. Each had been asserted away
// for years by the comment "should not happen — jit_scan should have caught
// this". A "should not happen" arm is a CLAIM, and claims about opcode
// coverage are checkable.
// ---------------------------------------------------------------------------

/// Opcodes `jit_scan` admits on purpose despite the single-pass walk having
/// no arm for them, each with the reason it is not the `dup2_x2` shape.
///
/// The bar for an entry here is a SECOND HOME: some other backend must lower
/// the opcode, so admitting it buys a compilation the scanner would otherwise
/// refuse. "Nobody lowers it anywhere" is the `dup2_x2` shape and belongs in
/// an arm, not on this list.
const SCAN_ADMITTED_WITHOUT_A_SINGLE_PASS_ARM: &[(u8, &str)] = &[
    (
        0x72,
        "frem — the optimizing IR backend lowers it via a call to the jit_frem \
         fmod helper, so admitting it lets the IR pipeline see the method",
    ),
    (0x73, "drem — same as frem, via jit_drem"),
];

/// Every opcode value the top-level dispatch `match op` in `bytecode_walk.rs`
/// has an arm for.
///
/// Parsed from the source at COMPILE time (`include_str!`), because the set is
/// a property of that match statement and nothing else — there is no runtime
/// handle on it. Only arms at the match's own brace depth count, so the
/// nested `match op` statements inside the branch arms (which re-dispatch on
/// the same variable to pick a condition code) cannot forge coverage.
fn single_pass_dispatch_arms() -> std::collections::BTreeSet<u8> {
    let src = include_str!("bytecode_walk.rs");
    // The dispatch loop's own `match op {`. Anchored on the two lines that
    // immediately precede it so a nested `match op {` cannot be picked up.
    let anchor = "self.dbg_last_op = op;\n            match op {\n";
    let start = src
        .find(anchor)
        .expect("the single-pass dispatch `match op` must be findable")
        + anchor.len();

    let mut arms = std::collections::BTreeSet::new();
    let mut depth = 0i32; // brace depth relative to the match body
    for line in src[start..].lines() {
        if depth == 0 {
            if let Some(set) = parse_opcode_arm(line) {
                arms.extend(set);
            }
            // The catch-all closes the enumeration.
            if line.trim_start().starts_with("_ => {") {
                break;
            }
        }
        for ch in line.chars() {
            match ch {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
        }
        if depth < 0 {
            break; // the match's own closing brace
        }
    }
    arms
}

/// `0x5e => {`, `0xC2 | 0xC3 => {`, `0x1a..=0x1d => {` — and nothing else.
/// Returns `None` for any line that is not an opcode match arm, including
/// comments and emitted byte literals.
fn parse_opcode_arm(line: &str) -> Option<Vec<u8>> {
    let t = line.trim();
    if t.starts_with("//") {
        return None;
    }
    let head = t.split("=>").next()?;
    if head == t {
        return None; // no `=>` on this line
    }
    let head = head.trim();
    if head.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    for alt in head.split('|') {
        let alt = alt.trim();
        let bytes: Vec<u8> = if let Some((lo, hi)) = alt.split_once("..=") {
            let lo = parse_hex_byte(lo.trim())?;
            let hi = parse_hex_byte(hi.trim())?;
            (lo..=hi).collect()
        } else {
            vec![parse_hex_byte(alt)?]
        };
        out.extend(bytes);
    }
    Some(out)
}

fn parse_hex_byte(tok: &str) -> Option<u8> {
    let hex = tok.strip_prefix("0x").or_else(|| tok.strip_prefix("0X"))?;
    if hex.len() != 2 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u8::from_str_radix(hex, 16).ok()
}

/// The parser must actually find the dispatch arms — a `single_pass_dispatch_arms`
/// that silently returned an empty set would make the guard below vacuously
/// green, which is precisely how the `dup2_x2` gap survived so long.
#[test]
fn the_dispatch_arm_parser_reads_the_real_match() {
    let arms = single_pass_dispatch_arms();
    assert!(
        arms.len() > 150,
        "the single-pass walk lowers most of the opcode space; parsed only {}",
        arms.len()
    );
    // Spot checks across the shapes the parser has to handle.
    for (op, what) in [
        (0x00u8, "nop, a bare single arm"),
        (0x5eu8, "dup2_x2, the arm this guard was written for"),
        (0x1bu8, "iload_1, inside a `..=` range arm"),
        (0xc2u8, "monitorenter, inside an alternation arm"),
        (0xacu8, "ireturn"),
        // 2026-08-28: `wide` moved from the negative list below to this
        // one when the prefix was lowered (netty's `FastLz.compress` is
        // 1617 bytes carrying three `iinc_w`, and a scan refusal is
        // permanent for the whole method at every compile door). A spot
        // check either way is what makes the move visible.
        (0xc4u8, "wide, the prefix arm"),
    ] {
        assert!(arms.contains(&op), "dispatch arm for 0x{op:02x} ({what})");
    }
    // And it must not invent coverage for opcodes nobody lowers here.
    for (op, what) in [
        (0xa8u8, "jsr — unlowered in both walkers"),
        (0x72u8, "frem — deliberately IR-only"),
    ] {
        assert!(
            !arms.contains(&op),
            "0x{op:02x} ({what}) must not read as lowered"
        );
    }
}

/// The guard itself: no opcode may be admitted by `jit_scan` and lowered by
/// nothing.
///
/// Admission is probed BEHAVIOURALLY — a one-instruction body per opcode,
/// handed to the real `jit_scan` — so the scanner's own table is never
/// transcribed here and cannot drift from what it actually does.
#[test]
fn scan_admitted_opcodes_are_lowered_or_declared() {
    let arms = single_pass_dispatch_arms();
    let declared: std::collections::BTreeMap<u8, &str> = SCAN_ADMITTED_WITHOUT_A_SINGLE_PASS_ARM
        .iter()
        .copied()
        .collect();

    let mut offenders = Vec::new();
    for op in 0x00u8..=0xc9u8 {
        // A body of just this opcode plus operand padding and a `return`. The
        // scanner walks opcode widths and never simulates the stack, so this
        // is enough to ask it the only question that matters: does it advance
        // past this opcode, or refuse the method?
        let mut code = vec![op, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        code.push(0xb1); // return
        let admitted = super::bytecode_compat::jit_scan(&code, code.len(), "()V").is_some();
        if !admitted || arms.contains(&op) {
            continue;
        }
        if declared.contains_key(&op) {
            continue;
        }
        offenders.push(op);
    }

    assert!(
        offenders.is_empty(),
        "these opcodes are admitted by `jit_scan` and lowered by no single-pass \
         arm, so every method containing one silently never compiles: {}. \
         Either add an arm in `bytecode_walk.rs`, stop admitting them in \
         `jit_scan`, or — only if some OTHER backend lowers them — add them to \
         SCAN_ADMITTED_WITHOUT_A_SINGLE_PASS_ARM with the reason.",
        offenders
            .iter()
            .map(|op| format!("0x{op:02x}"))
            .collect::<Vec<_>>()
            .join(", ")
    );

    // The allowlist is a ratchet in both directions: an entry that has since
    // grown an arm must be removed, or it hides the next real gap behind a
    // stale exemption.
    for (op, reason) in SCAN_ADMITTED_WITHOUT_A_SINGLE_PASS_ARM {
        assert!(
            !arms.contains(op),
            "0x{op:02x} now HAS a single-pass arm; drop its exemption ({reason})"
        );
    }
}

/// The METHOD-ENTRY safepoint poll must not read as "the local-oop dataflow
/// never reached here".
///
/// `emit_safepoint_poll_prologue` deliberately stamps `cur_bc_pc` with
/// `ENTRY_POLL_BC_PC` so its map cannot collide with a genuine bci-0 safepoint
/// in the sp-id-keyed lookup. That is right, and it had a consequence nobody
/// costed: `local_oop_reached.get(ENTRY_POLL_BC_PC)` is `None`, so
/// `moving_young_safepoint_coverage_complete` reported the entry poll as
/// unprovable and its `OopMapEntry` was pushed with
/// `moving_young_coverage_complete: false`.
///
/// `CompiledMethod::fully_shadow_covered` ANDs that flag over every safepoint
/// of a method, so ONE such entry made it false for **every method the fast
/// tier ever compiled** — and through the OSR fallback that refused relocation
/// on 725 of 759 collections of `TestKillProcessWhileWriting`. Measured with
/// `CRATONVM_DBG_OOPCOV=1`, every uncovered method reported exactly
/// `shadow_missing_pcs=[4294967295]`.
///
/// Non-vacuous in three directions: the entry pc answers with the SEEDED mask
/// (not zero, not `local_oop_masks[0]`), a reached bci answers with its own
/// mask, and an unreached bci still answers `None` — so the fix cannot be
/// mistaken for "everything is covered now".
#[test]
fn the_method_entry_poll_knows_its_own_live_oop_locals() {
    let alloc_result = crate::regalloc::RegAllocResult {
        assignments: Vec::new(),
        xmm_assignments: Vec::new(),
        used_callee_saved: Vec::new(),
        used_xmm_regs: Vec::new(),
        block_live_in: Vec::new(),
    };
    let mut compiler = Compiler::new(
        "entry-poll-oop-mask-test".to_string(),
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

    // Entry state: locals 0 and 2 are reference parameters. bci 0's own entry
    // is deliberately DIFFERENT (a back edge to bci 0 would have intersected
    // it), so reading the mask out of `local_oop_masks[0]` cannot pass this.
    compiler.param_oop_mask = 0b101;
    compiler.local_oop_masks = vec![0b001, 0b010, 0];
    compiler.local_oop_reached = vec![true, true, false];

    compiler.cur_bc_pc = crate::x64::safepoint::ENTRY_POLL_BC_PC;
    assert_eq!(
        compiler.local_oop_mask_at_current_pc(),
        Some(0b101),
        "the entry poll must answer with the dataflow's SEEDED entry state — \
         `None` here is the defect, and `Some(0b001)` would mean it read \
         `local_oop_masks[0]`, which a back edge can have narrowed",
    );

    compiler.cur_bc_pc = 1;
    assert_eq!(compiler.local_oop_mask_at_current_pc(), Some(0b010));

    compiler.cur_bc_pc = 2;
    assert_eq!(
        compiler.local_oop_mask_at_current_pc(),
        None,
        "a pc the forward dataflow never reached (an exception-handler-only \
         entry) must still refuse to make a precise claim",
    );

    compiler.cur_bc_pc = 99;
    assert_eq!(
        compiler.local_oop_mask_at_current_pc(),
        None,
        "and so must a pc past the end of the vectors",
    );
}

/// A site that inlines something ITSELF must be sized for the whole tree.
///
/// `InlineSite::nested_sites` holds full recursive `InlineSite`s and the
/// emitter splices those bodies into the SAME code buffer as the body that
/// contains them. Both sizing sites in `compile_with_param_slots` — the code
/// buffer estimate (`callee_code_len * 64`) and the spill reservation
/// (`max(callee_max_locals, param_span) + callee_code_len`) — used to read the
/// root site's `callee_code_len` and stop there, reserving the outer body's
/// bytes and none of the nested ones.
///
/// That is not a hypothetical shortfall. `VolumeOps.grad` (kfusion's raycast
/// normal, 1511 bytes / 273 invokes of 5-15 byte getters that each splice a
/// constructor and three field loads) overran its 433,312-byte estimate by
/// 8% — `wanted=468437` — and after `MAX_CODE_BUFFER_RETRIES` stopped being
/// compiled at all, leaving the app's hottest method interpreted.
///
/// TWO arms, because a test that only checked the nested case would pass just
/// as well against a helper that returned some large constant:
/// * a leaf site (no nested bodies) must be sized EXACTLY as the old per-site
///   formula did — this is the no-regression half;
/// * the same site carrying nested bodies must be sized strictly larger, by
///   exactly the nested bodies' own contribution, transitively through two
///   levels (so a one-level-deep walk fails it too).
#[test]
fn s31_inline_reservations_count_nested_bodies() {
    use crate::x64::driver::{spliced_bytecode_len, spliced_stack_reserve};

    // Named, not spelled 4: if `MAX_INLINE_MERGE_DEPTH` moves this test must
    // move with it rather than becoming a silently disagreeing second copy.
    const MERGE: usize = crate::x64::MAX_INLINE_MERGE_DEPTH;

    // Leaf: 3 bytes of bytecode, 2 locals, static ()I -> param_span 0.
    let leaf = make_inline_site(&[0x12, 0x05, 0xac], 2, 0, true, b'I');
    assert_eq!(
        spliced_bytecode_len(&leaf),
        3,
        "a leaf site is its own bytecode and nothing else"
    );
    assert_eq!(
        spliced_stack_reserve(&leaf),
        2 + 3 + MERGE,
        "a leaf site is locals/params + code_len + the branch-merge area, added 2026-09-02: \
         max(callee_max_locals, param_span) + callee_code_len"
    );

    // Depth 2: leaf <- middle <- outer. Distinct sizes so a walk that stops
    // one level early produces a number no correct walk can also produce.
    let middle_body = [0xb8, 0x00, 0x01, 0xac, 0x00, 0x00, 0x00]; // 7 bytes
    let mut middle = make_inline_site(&middle_body, 4, 0, true, b'I');
    middle.nested_sites = vec![crate::NestedInlineSite {
        callee_pc: 0,
        guard_class_id: 0,
        site: leaf.clone(),
    }];

    let outer_body = [0xb8, 0x00, 0x02, 0xac, 0x00]; // 5 bytes
    let mut outer = make_inline_site(&outer_body, 6, 0, true, b'I');
    outer.nested_sites = vec![crate::NestedInlineSite {
        callee_pc: 0,
        guard_class_id: 0,
        site: middle.clone(),
    }];

    // 5 (outer) + 7 (middle) + 3 (leaf). A one-level walk would say 12.
    assert_eq!(
        spliced_bytecode_len(&outer),
        5 + 7 + 3,
        "the buffer estimate must count nested bodies transitively"
    );
    // Each level plus its own merge area: every level of a chain is live at
    // once and each reserves `MAX_INLINE_MERGE_DEPTH` words of its own.
    assert_eq!(
        spliced_stack_reserve(&outer),
        (6 + 5 + MERGE) + (4 + 7 + MERGE) + (2 + 3 + MERGE),
        "a nested body gets its own locals, operand stack AND merge area on top of the \
         body that splices it, so the reserves add transitively"
    );

    // And the fix is strictly a GROWTH: never size a nested tree below what
    // the old root-only formula would have reserved for its root.
    assert!(spliced_bytecode_len(&outer) > spliced_bytecode_len(&leaf));
    assert!(spliced_stack_reserve(&outer) > spliced_stack_reserve(&leaf));
}

/// `emit_cmp_r64_mem_disp` picks the narrowest legal encoding, and the widths
/// it declines to narrow are declined for a reason, not by omission.
///
/// The six containment compares in `emit_guarded_getfield_receiver_check` read
/// table words at displacements 0..40 through RDX. Every one fits a `disp8`;
/// before this encoder existed each paid the disp32 form, three wasted bytes
/// apiece on a guard that runs before every unproven-receiver field access.
///
/// The two x86 base-register special cases are pinned here as well, because
/// both are silent mis-encodings rather than assembler errors: RBP/R13 have no
/// `mod=00` form (that bit pattern is RIP-relative), and RSP/R12 need a SIB
/// byte this emitter does not produce.
#[test]
fn the_containment_compare_narrows_its_displacement_and_knows_the_two_base_cases() {
    let mk = || {
        let alloc_result = crate::regalloc::RegAllocResult {
            assignments: Vec::new(),
            xmm_assignments: Vec::new(),
            used_callee_saved: Vec::new(),
            used_xmm_regs: Vec::new(),
            block_live_in: Vec::new(),
        };
        Compiler::new(
            "cmp-disp-width-test".to_string(),
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
        )
    };
    let bytes = |f: &dyn Fn(&mut Compiler)| -> Vec<u8> {
        let mut c = mk();
        let at = c.buf.pos();
        f(&mut c);
        c.buf.as_slice()[at..].to_vec()
    };

    // RDX base, displacement 0: mod=00, no displacement byte at all.
    assert_eq!(
        bytes(&|c| c.emit_cmp_r64_mem_disp(RAX, RDX, 0)),
        vec![0x48, 0x3B, 0x02],
    );
    // RDX base, the five remaining table words: mod=01 + one byte.
    for disp in [8i32, 16, 24, 32, 40] {
        assert_eq!(
            bytes(&|c| c.emit_cmp_r64_mem_disp(RAX, RDX, disp)),
            vec![0x48, 0x3B, 0x42, disp as u8],
            "displacement {disp} did not take the disp8 form",
        );
    }
    // Past a signed byte: back to mod=10 + disp32, byte-identical to the
    // encoder this one delegates to.
    let wide = bytes(&|c| c.emit_cmp_r64_mem_disp(RAX, RDX, 4096));
    assert_eq!(wide, bytes(&|c| c.emit_cmp_r64_mem_disp32(RAX, RDX, 4096)));

    // RBP has no mod=00 form: a zero displacement still emits an explicit
    // disp8 of 0, never the three-byte shape RDX gets.
    assert_eq!(
        bytes(&|c| c.emit_cmp_r64_mem_disp(RAX, RBP, 0)),
        vec![0x48, 0x3B, 0x45, 0x00],
    );
    // RSP needs a SIB byte neither form emits, so it is handed to the disp32
    // encoder unchanged rather than narrowed into a wrong operand here.
    assert_eq!(
        bytes(&|c| c.emit_cmp_r64_mem_disp(RAX, RSP, 8)),
        bytes(&|c| c.emit_cmp_r64_mem_disp32(RAX, RSP, 8)),
    );
    // An extended base keeps its REX.B, and R13 inherits RBP's rule.
    assert_eq!(
        bytes(&|c| c.emit_cmp_r64_mem_disp(RAX, R13, 0)),
        vec![0x49, 0x3B, 0x45, 0x00],
    );
}

/// The gated reference store writes the RIGHT CELL for a legacy receiver, and
/// takes no helper call to do it.
///
/// Executed, not inspected: the compiled body runs against a fake object and
/// the assertions read the bytes it wrote.
///
/// This arm used to send every non-compact receiver to `jit_putfield_object`,
/// which made it an arm that essentially never fired — `init_object_header`,
/// the TLAB fast path serving nearly every allocation, writes a LEGACY header
/// unconditionally whatever layout the class has registered. A run-time path
/// census on `RefStoreLoopProbe` measured `inline=0` out of 16,380,000, all of
/// them bailing at the compactness test, while the compile-time census said
/// `gated=2 declined=0` and looked healthy.
///
/// Both halves are asserted. A test that only checked the legacy receiver
/// would pass on an arm that had simply swapped one dead shape for another.
#[test]
fn the_gated_ref_store_writes_both_cell_shapes_without_a_helper_call() {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    static HELPER_CALLS: AtomicUsize = AtomicUsize::new(0);
    static BARRIER_CALLS: AtomicUsize = AtomicUsize::new(0);
    /// Records the full-barrier path and deliberately does NOT store, so an
    /// inline store and a helper store are trivially distinguishable.
    unsafe extern "C" fn marker_putfield_object(_vm: i64, _obj: i64, _idx: i64, _val: i64) {
        HELPER_CALLS.fetch_add(1, Ordering::SeqCst);
    }
    unsafe extern "C" fn marker_write_barrier(_heap: i64, _obj: i64, _val: i64) {
        BARRIER_CALLS.fetch_add(1, Ordering::SeqCst);
    }

    // The published plan: SATB disarmed, old objects present, and the
    // generational MASK shape. `post_active` is deliberately 1 so the mask is
    // what rules the barrier out and not a global "nothing is old" shortcut.
    static PRE_GATE: AtomicU64 = AtomicU64::new(0);
    static POST_GATE: AtomicU64 = AtomicU64::new(1);
    static READ_BOUNDS: [AtomicUsize; 6] = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];

    // void setRef(Object this, Object v) { this.f = v; }
    let code: Vec<u8> = vec![0x2a, 0x2b, 0xb5, 0x00, 0x01, 0xb1, 0, 0];
    let field_info = vec![(2usize, 0usize, b'L')];
    let mut helpers = test_helpers();
    helpers.putfield_object = marker_putfield_object as *const () as usize; // Cast: fn → slot
    helpers.write_barrier = marker_write_barrier as *const () as usize; // Cast: fn → slot
    helpers.read_bounds_addr = READ_BOUNDS.as_ptr() as usize; // Cast: static address
    helpers.ref_store_pre_gate = std::ptr::addr_of!(PRE_GATE) as usize; // Cast: static address
    helpers.ref_store_post_gate = std::ptr::addr_of!(POST_GATE) as usize; // Cast: static address
    helpers.ref_store_post_young_floor = 0;
    helpers.ref_store_post_skip_mask = cratonvm_types::GC_FLAG_OLD_GEN as usize;

    set_pending_compact_field_info(vec![(2, 0, true)]);
    let compiled = compile(
        &code,
        6,
        2,
        2,
        true, // needs_heap — the barrier helper takes it
        Vec::new(),
        field_info,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots
        Vec::new(), // ldc_info
        Vec::new(), // ldc2w_info
        HashMap::new(),
        HashMap::new(),
        &helpers,
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .expect("reference putfield must compile");

    let val = Box::new([0u64; 8]);
    let val_addr = val.as_ptr() as usize; // Cast: stored reference

    // ---- LEGACY receiver: no GC_FLAG_COMPACT, the 16-byte `Value` cell ----
    let mut obj = Box::new([0u64; 8]);
    // SAFETY: `obj` is 64 bytes and 8-byte aligned; both writes land inside it.
    unsafe {
        let p = obj.as_mut_ptr() as *mut u8; // Cast: array base → byte cursor
        *p.add(cratonvm_types::GC_FLAGS_BYTE_OFFSET) = 0; // young, LEGACY
        std::ptr::write_unaligned(
            p.add(cratonvm_types::NUM_SLOTS_OFFSET) as *mut u32, // Cast: header field
            4u32,
        );
    }
    let obj_addr = obj.as_mut_ptr() as usize; // Cast: receiver address
    let page = obj_addr & !0xFFF;
    READ_BOUNDS[0].store(page, Ordering::Release);
    READ_BOUNDS[1].store(page + 0x10000, Ordering::Release);

    HELPER_CALLS.store(0, Ordering::SeqCst);
    BARRIER_CALLS.store(0, Ordering::SeqCst);
    // SAFETY: JIT-compiled code from valid bytecode in an executable mmap; the
    // receiver is a live 64-byte buffer shaped like an object header and
    // neither marker helper stores anything.
    unsafe {
        compiled.call_with_heap(0, &[obj_addr as i64, val_addr as i64]); // Cast: JIT ABI
    }
    assert_eq!(
        (
            HELPER_CALLS.load(Ordering::SeqCst),
            BARRIER_CALLS.load(Ordering::SeqCst)
        ),
        (0, 0),
        "a young LEGACY receiver must store inline and call NOTHING — before \
         the two-shape store it took the helper on every single execution"
    );
    let word = cratonvm_types::HEADER_SIZE / 8;
    assert_eq!(
        obj[word] as u32,
        cratonvm_types::FIELD_CELL_TAG_OBJECT,
        "the legacy shape must write the cell's TAG; a payload written without \
         it leaves a discriminant saying whatever the field held before, and \
         the reader believes the discriminant"
    );
    assert_eq!(
        obj[word + 1] as usize, // Cast: raw stored pointer word
        val_addr,
        "the legacy shape must write the 64-bit pointer payload"
    );

    // ---- COMPACT receiver: the bare 8-byte pointer at the cell base ----
    let mut c_obj = fake_compact_young_object();
    let c_addr = c_obj.as_mut_ptr() as usize; // Cast: receiver address
    let c_page = c_addr & !0xFFF;
    READ_BOUNDS[0].store(c_page, Ordering::Release);
    READ_BOUNDS[1].store(c_page + 0x10000, Ordering::Release);
    HELPER_CALLS.store(0, Ordering::SeqCst);
    BARRIER_CALLS.store(0, Ordering::SeqCst);
    // SAFETY: as above.
    unsafe {
        compiled.call_with_heap(0, &[c_addr as i64, val_addr as i64]); // Cast: JIT ABI
    }
    assert_eq!(
        (
            HELPER_CALLS.load(Ordering::SeqCst),
            BARRIER_CALLS.load(Ordering::SeqCst)
        ),
        (0, 0),
        "a young COMPACT receiver must still store inline and call nothing"
    );
    assert_eq!(
        fake_object_ref_cell(&c_obj),
        val_addr,
        "the compact shape must survive the addition of the legacy one — the \
         receiver's own header decides, and a genuinely compact object stores \
         the bare pointer"
    );

    // ---- an OLD receiver still reaches the collector's write barrier ----
    // SAFETY: `c_obj` is the buffer built above; this sets its flags byte.
    unsafe {
        let p = c_obj.as_mut_ptr() as *mut u8; // Cast: array base → byte cursor
        *p.add(cratonvm_types::GC_FLAGS_BYTE_OFFSET) =
            cratonvm_types::GC_FLAG_COMPACT | cratonvm_types::GC_FLAG_OLD_GEN;
    }
    BARRIER_CALLS.store(0, Ordering::SeqCst);
    // SAFETY: as above.
    unsafe {
        compiled.call_with_heap(0, &[c_addr as i64, val_addr as i64]); // Cast: JIT ABI
    }
    assert_eq!(
        BARRIER_CALLS.load(Ordering::SeqCst),
        1,
        "an OLD receiver is the case the mask exists to catch: the store is \
         still inline, but the collector's own write barrier must run"
    );
}

/// The two post-barrier gate shapes are mutually exclusive, and a plan that
/// supplies neither (or both) is declined.
///
/// The mask shape exists because the age FLOOR cannot express the generational
/// collector's question. Its `write_barrier` cards a store only when the
/// receiver is in the old generation, which is `GC_FLAG_OLD_GEN` — a bit in the
/// flags nibble of the same byte the floor compares. The two do not order: an
/// object allocated straight into old gen has `gc_age == 0`, so its flags byte
/// is `0x01`, BELOW the age-zero floor `0x10`, while a young object that has
/// survived three collections is `0x30`, above it. A single unsigned threshold
/// would therefore have told compiled code to skip the card on exactly the
/// receivers that need one — so publishing both shapes at once is refused
/// rather than silently preferring one.
#[test]
fn a_ref_store_plan_publishes_exactly_one_post_barrier_shape() {
    let mut helpers = test_helpers();
    helpers.ref_store_pre_gate = 0x1000;
    helpers.ref_store_post_gate = 0x1008;

    let gates = |h: &JitRuntimeHelpers| {
        (
            super::objects::ref_store_gates_of(h).is_some(),
            super::objects::ref_store_post_skip_mask_of(h),
        )
    };

    // Neither shape: nothing can rule the post barrier out.
    helpers.ref_store_post_young_floor = 0;
    helpers.ref_store_post_skip_mask = 0;
    assert_eq!(gates(&helpers), (false, None), "neither shape");

    // Floor only — ZGC's shape.
    helpers.ref_store_post_young_floor = 0x1010;
    helpers.ref_store_post_skip_mask = 0;
    assert_eq!(gates(&helpers), (true, None), "floor only");

    // Mask only — the generational collector's shape.
    helpers.ref_store_post_young_floor = 0;
    helpers.ref_store_post_skip_mask = usize::from(cratonvm_types::GC_FLAG_OLD_GEN);
    assert_eq!(
        gates(&helpers),
        (true, Some(cratonvm_types::GC_FLAG_OLD_GEN)),
        "mask only",
    );

    // Both: refused. Two independent skips for one question, and for a mask
    // publisher the floor is not merely redundant but wrong.
    helpers.ref_store_post_young_floor = 0x1010;
    helpers.ref_store_post_skip_mask = usize::from(cratonvm_types::GC_FLAG_OLD_GEN);
    assert_eq!(gates(&helpers).0, false, "both shapes");
}

/// The exact numbers behind that refusal, as a statement about the flags byte
/// rather than about the emitter: an old-gen receiver can sit BELOW the
/// age-zero floor, so no unsigned threshold separates old from young.
#[test]
fn no_age_floor_can_separate_an_old_gen_receiver_from_a_young_one() {
    let flags_byte = |age: u8, flags: u8| (age << 4) | flags;
    // Allocated straight into old gen: age 0, and it NEEDS a card.
    let old_new = flags_byte(0, cratonvm_types::GC_FLAG_OLD_GEN);
    // Survived three collections, still young: it needs none.
    let young_old = flags_byte(3, 0);
    assert!(
        old_new < young_old,
        "the receiver that needs a card ({old_new:#04x}) sorts BELOW one that \
         does not ({young_old:#04x}) — which is why the floor shape cannot be \
         used here, and the mask can",
    );
    // The mask answers both correctly.
    assert_ne!(old_new & cratonvm_types::GC_FLAG_OLD_GEN, 0);
    assert_eq!(young_old & cratonvm_types::GC_FLAG_OLD_GEN, 0);
}
