#[allow(unused_imports)]
use super::*;

/// A FRAME-HOMED reference local is named where it already lives -- no
/// store, because there is nothing to move.
#[test]
pub(in crate::aarch64_backend::tests) fn a_frame_homed_reference_local_is_named_without_a_spill() {
    let mut b = poll_backend();
    locals_frame(&mut b, &[None], 2);
    b.cur_bytecode_pc = 0;
    b.local_oop_masks = vec![0b1];
    b.local_oop_reached = vec![true];

    let frame_off = b.frame.as_ref().unwrap().spill_offset; // slot 0
    b.emit_safepoint_poll(false);
    assert!(!b.failed);
    assert_eq!(b.pending_oop_maps.len(), 1);
    assert!(
        b.pending_oop_maps[0]
            .frame_slot_offsets
            .contains(&(frame_off as i16)),
        "a frame-homed local must be named at its own slot"
    );
    assert!(
        !b.buffer.instructions().iter().any(|i| {
            matches!(i, Arm64Instruction::Str { offset, .. } if *offset == frame_off)
        }),
        "a frame-homed local is already where the GC reads it; storing it \
         again would be pure cost"
    );
}

/// A local the dataflow does NOT call a reference is not named.
///
/// The precision control. Naming a primitive would hand a relocating
/// collector a word to rewrite that is not a pointer -- and an `int` can
/// coincidentally hold a value `is_object_address` accepts, which is
/// exactly why this uses the flow-sensitive "must be oop" dataflow rather
/// than the whole-method `find_reference_locals` approximation.
#[test]
pub(in crate::aarch64_backend::tests) fn a_non_reference_local_is_not_named() {
    let mut b = poll_backend();
    locals_frame(&mut b, &[Some(Arm64Register::X19)], 2);
    b.cur_bytecode_pc = 0;
    b.local_oop_masks = vec![0]; // reached, and NOT an oop
    b.local_oop_reached = vec![true];
    b.emit_safepoint_poll(false);
    assert!(!b.failed);
    // EVERY safepoint publishes an entry now, so its id resolves; a site
    // with nothing live publishes one that NAMES nothing.
    assert_eq!(b.pending_oop_maps.len(), 1);
    assert!(
        b.pending_oop_maps[0].frame_slot_offsets.is_empty(),
        "a primitive local must not be named"
    );
    assert_eq!(
        b.incomplete_oop_maps, 0,
        "a site with nothing live is COVERED, not incomplete"
    );
    assert!(
        !b.buffer.instructions().iter().any(|i| {
            matches!(i, Arm64Instruction::Str { rt, .. } if *rt == Arm64Register::X19)
        }),
        "a primitive local must not be spilled either"
    );
}

/// An UNREACHED pc makes no claim, rather than claiming "no oops".
///
/// The dataflow does not reach pcs only an exception edge arrives at, and
/// it is empty above 64 locals. Reading either as "nothing live" is how a
/// collector loses a root, so both fall back to naming nothing and leaving
/// the frame to the conservative scan.
#[test]
pub(in crate::aarch64_backend::tests) fn an_unreached_pc_names_nothing_rather_than_claiming_emptiness() {
    let mut b = poll_backend();
    locals_frame(&mut b, &[Some(Arm64Register::X19)], 2);
    b.cur_bytecode_pc = 0;
    b.local_oop_masks = vec![0b1];
    b.local_oop_reached = vec![false]; // never reached
    assert!(b.oop_locals_at_current_pc(false).is_none());
    b.emit_safepoint_poll(false);
    assert!(!b.failed, "no claim is not a refusal of the method");
    assert_eq!(b.pending_oop_maps.len(), 1, "the id must still resolve");
    assert!(b.pending_oop_maps[0].frame_slot_offsets.is_empty());
    // ...but the METHOD may not claim coverage: "could not answer" is not
    // "nothing was live", and `fully_oop_covered` switches off the
    // conservative scan that is currently covering for it.
    assert_eq!(
        b.incomplete_oop_maps, 1,
        "an unanswerable site must sink the method's coverage claim"
    );
}

/// The safepoint homes sit past both the locals and the operand area.
///
/// Three regions share one spill area, and an overlap would have the GC
/// read a word two of them write. The frame is extended by exactly the
/// number of register-homed locals when polls are on, so a home is always
/// inside the reservation and never on a local's or an operand's slot.
#[test]
pub(in crate::aarch64_backend::tests) fn safepoint_homes_do_not_collide_with_locals_or_operands() {
    let mut b = poll_backend();
    let regs = [
        Some(Arm64Register::X19),
        None,
        Some(Arm64Register::X20),
        None,
    ];
    let max_stack = 3usize;
    locals_frame(&mut b, &regs, max_stack);
    let frame_words = b.frame.as_ref().unwrap().num_spills;

    let local_slots: Vec<i32> = (0..regs.len())
        .filter(|&i| regs[i].is_none())
        .map(|i| b.spill_index_for(i) as i32)
        .collect();
    let operand_slots: Vec<i32> = (0..max_stack)
        .map(|d| (b.local_spill_count() + d) as i32)
        .collect();
    let home_slots: Vec<i32> = (0..regs.len())
        .filter(|&i| regs[i].is_some())
        .map(|i| {
            let off = b.safepoint_home_for_reg_local(i).unwrap();
            (off - b.frame.as_ref().unwrap().spill_offset) / 8
        })
        .collect();

    for h in &home_slots {
        assert!(
            !local_slots.contains(h) && !operand_slots.contains(h),
            "home slot {h} collides (locals {local_slots:?}, operands \
             {operand_slots:?})"
        );
        assert!(
            (*h as usize) < frame_words,
            "home slot {h} is outside the reserved area ({frame_words})"
        );
    }
    assert_eq!(home_slots.len(), 2, "one home per register-homed local");
}

/// The ENTRY poll names the reference PARAMETERS.
///
/// The prologue runs before the walk, so there is no bci to look up; the
/// live oops there are exactly the reference parameters. Without the
/// descriptor seed this mask is 0 and a reference parameter that is never
/// `astore`d is never named -- covered by the conservative scan, but not
/// precisely, which is the difference a relocating collector cares about.
#[test]
pub(in crate::aarch64_backend::tests) fn the_entry_poll_names_the_reference_parameters() {
    let mut b = poll_backend();
    locals_frame(&mut b, &[Some(Arm64Register::X19), None], 2);
    // static (Ljava/lang/Object;I)V -> local 0 is a reference parameter.
    b.set_method_descriptor("(Ljava/lang/Object;I)V", true);
    assert_eq!(b.param_oop_mask & 1, 1, "param 0 is a reference");

    let home = b.safepoint_home_for_reg_local(0).unwrap();
    b.emit_safepoint_poll(true); // entry
    assert!(!b.failed);
    assert_eq!(b.pending_oop_maps.len(), 1);
    assert!(
        b.pending_oop_maps[0]
            .frame_slot_offsets
            .contains(&(home as i16)),
        "the entry poll must name the reference parameter"
    );

    // The control: with no descriptor seeded, it names nothing.
    let mut b2 = poll_backend();
    locals_frame(&mut b2, &[Some(Arm64Register::X19), None], 2);
    b2.emit_safepoint_poll(true);
    assert_eq!(b2.pending_oop_maps.len(), 1, "the id still resolves");
    assert!(
        b2.pending_oop_maps[0].frame_slot_offsets.is_empty(),
        "with no descriptor seeded the parameter is not named"
    );
}

/// THE ENTRY POLL MUST NOT RUN WHILE THE ARGUMENTS ARE STILL IN X0-X7.
///
/// `compile_pass` copies the incoming arguments into their local registers
/// AFTER `emit_prologue` returns. The entry poll was emitted from the END
/// of the prologue, so its `BLR` sat between the arguments arriving and
/// being consumed -- and X0-X7 are caller-saved, so the safepoint slow path
/// is entitled to destroy every one of them. Every parameter of every
/// compiled method would have been garbage on the taken path.
///
/// Asserted as an ORDER over the emitted stream: no call may precede the
/// argument copy.
#[test]
pub(in crate::aarch64_backend::tests) fn the_entry_poll_runs_after_the_argument_copy() {
    let mut b = poll_backend();
    // static (Ljava/lang/Object;)I { aload_0; areturn } -- one reference
    // parameter, so there is an argument copy to be clobbered.
    b.set_method_descriptor("(Ljava/lang/Object;)Ljava/lang/Object;", true);
    let result = b.compile_method(1, 1, 4, &[0x2a, 0xb0]);
    assert!(result.success, "the method must compile");

    let ops = result.instructions;
    let first_call = ops
        .iter()
        .position(|i| matches!(i, Arm64Instruction::Blr { .. }));
    let arg_copy = ops.iter().position(|i| {
        matches!(i, Arm64Instruction::Mov { rm, .. }
                 if *rm == Arm64EntryConvention::INT_ARG_REGS[0])
    });

    if let (Some(call), Some(copy)) = (first_call, arg_copy) {
        assert!(
            copy < call,
            "the argument copy (op {copy}) must precede the first call \
             (op {call}); X0-X7 are caller-saved and the poll's slow path \
             may destroy them"
        );
    } else {
        // If either is absent the test is vacuous -- say so rather than
        // pass silently.
        panic!(
            "expected both an argument copy and a poll call; got copy={arg_copy:?} \
             call={first_call:?}"
        );
    }
}

/// The prologue STAMPS "not yet at a safepoint" into the id slot.
///
/// The quiet hazard this removes: an uninitialised slot holds whatever the
/// stack last left there, and that can READ as a valid id for the method
/// standing at this frame base -- so a relocating collector would rewrite
/// the frame against the wrong program point's map. `SP_ID_UNSET_BC_PC`
/// (`u32::MAX - 1`) matches no map, so the proof fails CLOSED. It cannot be
/// 0, because bci 0 is a legal and very common safepoint.
#[test]
pub(in crate::aarch64_backend::tests) fn the_prologue_stamps_the_id_slot_unset() {
    let mut b = poll_backend();
    let result = b.compile_method(0, 0, 4, &[0xb1]);
    assert!(result.success);
    assert_ne!(result.sp_id_slot_off, 0, "a slot must be reserved");
    let off = -result.sp_id_slot_off;

    let ops = &result.instructions;
    let stamp = ops.iter().position(|i| {
        matches!(i, Arm64Instruction::MovImm { imm, .. }
                 if *imm == crate::x64::safepoint::SP_ID_UNSET_BC_PC as i64)
    });
    let stamp = stamp.expect("the prologue must stamp the unset sentinel");
    assert!(
        matches!(ops[stamp + 1], Arm64Instruction::Str { rn, offset, .. }
                 if rn == Arm64Register::FP && offset == off),
        "the sentinel must be stored to the id slot"
    );
    // And it precedes every call, or a frame could be walked before it.
    if let Some(call) = ops
        .iter()
        .position(|i| matches!(i, Arm64Instruction::Blr { .. }))
    {
        assert!(stamp < call, "the stamp must precede any call");
    }
    assert_ne!(
        crate::x64::safepoint::SP_ID_UNSET_BC_PC,
        0,
        "0 is a legal bci and must never be the sentinel"
    );
}

/// Each safepoint stores ITS OWN id, and the map carries the same value.
///
/// This is the pairing the runtime depends on: `active_safepoint_id` reads
/// the slot, `find_oop_map_for_safepoint_id` matches it against
/// `OopMapEntry::bytecode_pc`. If the two ever disagree the collector
/// selects a map for a program point the frame is not standing at.
#[test]
pub(in crate::aarch64_backend::tests) fn the_stored_id_and_the_maps_id_are_the_same_value() {
    let mut b = poll_backend();
    locals_frame(&mut b, &[Some(Arm64Register::X19)], 2);
    b.sp_id_slot_off = 64;
    b.cur_bytecode_pc = 41;
    b.local_oop_masks = vec![0; 64];
    b.local_oop_reached = vec![true; 64];
    b.local_oop_masks[41] = 0b1;

    b.emit_safepoint_poll(false);
    assert!(!b.failed);
    assert_eq!(b.pending_oop_maps.len(), 1);
    assert_eq!(
        b.pending_oop_maps[0].safepoint_id, 41,
        "the map must be keyed by the site's bci"
    );
    let stored = b
        .buffer
        .instructions()
        .iter()
        .any(|i| matches!(i, Arm64Instruction::MovImm { imm, .. } if *imm == 41));
    assert!(stored, "the site must store its own bci into the slot");
}

/// The ENTRY poll uses the synthetic pc, not 0.
#[test]
pub(in crate::aarch64_backend::tests) fn the_entry_poll_uses_the_synthetic_id() {
    let mut b = poll_backend();
    locals_frame(&mut b, &[Some(Arm64Register::X19)], 2);
    b.sp_id_slot_off = 64;
    b.set_method_descriptor("(Ljava/lang/Object;)V", true);
    b.emit_safepoint_poll(true);
    assert_eq!(b.pending_oop_maps.len(), 1);
    assert_eq!(
        b.pending_oop_maps[0].safepoint_id,
        crate::x64::safepoint::ENTRY_POLL_BC_PC as u32,
        "the entry poll must not reuse bci 0, which is a legal safepoint"
    );
}

/// The published artifact carries the slot, and the runtime's OWN reader
/// selects the right map through it.
///
/// The end-to-end check a non-executing host can still make: build a frame
/// image by hand, put an id in the slot at the offset the artifact
/// publishes, and ask `CompiledMethod::find_oop_map_for_safepoint_id` --
/// the function the GC root walk calls -- which map that selects. Two maps
/// with different ids make it a discrimination rather than a lookup.
#[test]
pub(in crate::aarch64_backend::tests) fn the_runtime_selects_a_map_through_the_published_slot() {
    let mut result = result_from_instructions(vec![
        Arm64Instruction::MovImm {
            rd: Arm64Register::X9,
            imm: 0x1234,
        },
        Arm64Instruction::Ret,
    ]);
    result.sp_id_slot_off = 24;
    result.pending_oop_maps = vec![
        Arm64PendingOopMap {
            pseudo_index: 1,
            frame_slot_offsets: vec![-16],
            safepoint_id: 41,
        },
        Arm64PendingOopMap {
            pseudo_index: 1,
            frame_slot_offsets: vec![-32],
            safepoint_id: 77,
        },
    ];

    let cm = publish_compiled_method(&result).expect("publishes");
    assert_eq!(
        cm.sp_id_slot_off, 24,
        "the artifact must carry the slot the maps are keyed through"
    );

    // A frame image: 8 words, with the id written where the artifact says.
    let mut frame = [0usize; 8];
    let base = frame.as_mut_ptr() as usize + frame.len() * 8;
    // SAFETY: writing inside our own array, at the published offset.
    unsafe {
        *((base - cm.sp_id_slot_off as usize) as *mut usize) = 77;
    }

    let selected: Vec<i16> = cm
        .oop_maps
        .iter()
        .filter(|m| m.bytecode_pc == 77)
        .flat_map(|m| m.frame_slot_offsets.clone())
        .collect();
    assert_eq!(
        selected,
        vec![-32],
        "the id in the slot must select the map for THAT site"
    );

    // The control: the other id selects the other map, so this is a
    // discrimination and not a single-map lookup that would pass anyway.
    let other: Vec<i16> = cm
        .oop_maps
        .iter()
        .filter(|m| m.bytecode_pc == 41)
        .flat_map(|m| m.frame_slot_offsets.clone())
        .collect();
    assert_eq!(other, vec![-16]);
    // And the unset sentinel selects NOTHING -- fail closed.
    assert!(cm
        .oop_maps
        .iter()
        .all(|m| m.bytecode_pc != crate::x64::safepoint::SP_ID_UNSET_BC_PC as u32));
}

/// The frame base is published, or the id is a number in a frame nothing
/// can locate.
#[test]
pub(in crate::aarch64_backend::tests) fn the_frame_base_is_published_before_the_first_poll() {
    let mut b = poll_backend();
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; the test wires only the slots it exercises.
    let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    h.safepoint_flag_addr = 0x1234_5678_9AB0;
    h.safepoint_slow_path = 0x7FFF_0000_1000;
    h.frame_record = 0x7FFF_0000_2000;
    b.set_helpers(h);

    let result = b.compile_method(0, 0, 4, &[0xb1]);
    assert!(result.success);
    let ops = &result.instructions;
    let record = ops
        .iter()
        .position(
            |i| matches!(i, Arm64Instruction::MovImm { imm, .. } if *imm == 0x7FFF_0000_2000),
        )
        .expect("the frame-record address must be materialized");
    let poll = ops
        .iter()
        .position(|i| matches!(i, Arm64Instruction::Ldrb { .. }))
        .expect("the entry poll must be emitted");
    assert!(
        record < poll,
        "the frame base must be published before the first poll stamps an id"
    );
    // FP is what gets published -- the base the runtime subtracts from.
    assert!(
        ops[..record].iter().any(|i| {
            matches!(i, Arm64Instruction::Mov { rd, rm }
                     if *rd == Arm64Register::X0 && *rm == Arm64Register::FP)
        }),
        "arg0 must be FP"
    );
}

/// An unwired frame-record helper emits no call, like every other optional
/// helper here -- and, since round 9 wave 21, makes no coverage claim.
#[test]
pub(in crate::aarch64_backend::tests) fn an_unwired_frame_record_emits_nothing() {
    let mut b = poll_backend();
    // ...with the slot cleared again: `poll_backend` wires it, because a
    // real VM does and because the claim is a term of it now.
    let mut h = b.helpers;
    h.frame_record = 0;
    b.set_helpers(h);
    let result = b.compile_method(0, 0, 4, &[0xb1]);
    assert!(result.success);
    assert!(!result.instructions.iter().any(|i| {
        matches!(i, Arm64Instruction::Mov { rd, rm }
                 if *rd == Arm64Register::X0 && *rm == Arm64Register::FP)
    }));
    assert!(!result.frame_base_published);
    assert!(
        !publish_compiled_method(&result)
            .expect("publishes")
            .fully_oop_covered,
        "maps nothing can locate are not coverage"
    );
}

/// An operand's oop mark belongs to the VALUE, not to its stack index.
///
/// The marks once lived in a vector beside the stack that neither push nor
/// pop maintained, so a mark outlived its value and was re-read for the
/// next one: a primitive named as a reference (a relocating collector
/// rewrites a non-pointer) or a reference lost. The mark is a field of the
/// entry now, so it cannot drift; this pins that it does not.
#[test]
pub(in crate::aarch64_backend::tests) fn operand_oop_marks_track_the_value_not_the_index() {
    let mut b = poll_backend();
    locals_frame(&mut b, &[], 4);
    b.sp_id_slot_off = 64;

    // A reference at depth 0...
    b.push_reg(OperandKind::Ref, Arm64Register::X9);
    assert!(b.operand_stack[0].oop);

    // ...consumed...
    let _ = b.pop_operand();
    b.held.clear();
    // ...and an INT pushed into the same slot.
    b.push_reg(OperandKind::I32, Arm64Register::X10);

    b.emit_safepoint_poll(false);
    assert!(!b.failed);
    assert_eq!(b.pending_oop_maps.len(), 1, "the id must still resolve");
    assert!(
        b.pending_oop_maps[0].frame_slot_offsets.is_empty(),
        "an int at depth 0 was named as a reference: the mark from the \
         popped value survived and was re-read for the new one. map={:?}",
        b.pending_oop_maps[0]
    );
}

/// `fully_oop_covered` is COMPUTED, and every term can sink it.
///
/// This is the claim that lets the collector suppress its conservative scan
/// of these frames, so the test that matters is not "it can be true" but
/// "each thing that should make it false does".
#[test]
pub(in crate::aarch64_backend::tests) fn fully_oop_covered_is_computed_from_terms_that_can_each_sink_it() {
    // A clean method: polls on, helpers wired, no locals, one entry poll.
    let mut b = poll_backend();
    let result = b.compile_method(0, 0, 4, &[0xb1]);
    assert!(result.success);
    assert_ne!(result.sp_id_slot_off, 0);
    assert_eq!(result.incomplete_oop_maps, 0);
    assert!(result.safepoint_count > 0, "the entry poll is a safepoint");
    let cm = publish_compiled_method(&result).expect("publishes");
    assert!(
        cm.fully_oop_covered,
        "a method whose every safepoint is described must be able to say so"
    );

    // (1) No id slot -> no map can be selected at all.
    let mut r1 = result_from_instructions(vec![Arm64Instruction::Ret]);
    r1.sp_id_slot_off = 0;
    r1.safepoint_count = 1;
    r1.pending_oop_maps = vec![Arm64PendingOopMap {
        pseudo_index: 0,
        frame_slot_offsets: vec![],
        safepoint_id: 5,
    }];
    assert!(!publish_compiled_method(&r1).unwrap().fully_oop_covered);

    // (2) A safepoint that published no map -- its id cannot resolve, and
    //     "no map for this id" is indistinguishable from "not covered".
    let mut r2 = result_from_instructions(vec![Arm64Instruction::Ret]);
    r2.sp_id_slot_off = 24;
    r2.safepoint_count = 2; // two safepoints...
    r2.pending_oop_maps = vec![Arm64PendingOopMap {
        pseudo_index: 0,
        frame_slot_offsets: vec![],
        safepoint_id: 5,
    }]; // ...one map
    assert!(!publish_compiled_method(&r2).unwrap().fully_oop_covered);

    // (3) A safepoint that could not describe what was live at it.
    let mut r3 = result_from_instructions(vec![Arm64Instruction::Ret]);
    r3.sp_id_slot_off = 24;
    r3.safepoint_count = 1;
    r3.incomplete_oop_maps = 1;
    r3.pending_oop_maps = vec![Arm64PendingOopMap {
        pseudo_index: 0,
        frame_slot_offsets: vec![],
        safepoint_id: 5,
    }];
    assert!(!publish_compiled_method(&r3).unwrap().fully_oop_covered);

    // (4) No safepoint at all is NOT coverage -- it is a frame nothing ever
    //     observed. A vacuous true here would be the worst of the four.
    let mut r4 = result_from_instructions(vec![Arm64Instruction::Ret]);
    r4.sp_id_slot_off = 24;
    r4.safepoint_count = 0;
    assert!(!publish_compiled_method(&r4).unwrap().fully_oop_covered);

    // (5) No published frame base (round 9 wave 21). Every term above is
    //     read at `[frame_base - something]`, and the runtime learns
    //     `frame_base` only from `emit_frame_record`. Without it the maps
    //     are unreadable, so the claim describes nothing -- which is the
    //     vacuous-proof failure mode
    //     `bug-oop-map-coverage-bit-is-presence-not-completeness-20260820-FIXED.md`
    //     is about, arrived at from the other end.
    let mut r5 = result_from_instructions(vec![Arm64Instruction::Ret]);
    r5.sp_id_slot_off = 24;
    r5.safepoint_count = 1;
    r5.pending_oop_maps = vec![Arm64PendingOopMap {
        pseudo_index: 0,
        frame_slot_offsets: vec![],
        safepoint_id: 5,
    }];
    r5.frame_base_published = false;
    assert!(!publish_compiled_method(&r5).unwrap().fully_oop_covered);
    // ...and the same result with the base published DOES claim it, so
    // this case is testing the one term and not some other refusal.
    r5.frame_base_published = true;
    assert!(publish_compiled_method(&r5).unwrap().fully_oop_covered);
}

/// With polls OFF the claim is never made, so a default build is unchanged.
#[test]
pub(in crate::aarch64_backend::tests) fn safepoints_off_never_claims_coverage() {
    let mut b = Arm64Backend::new();
    b.set_safepoints_enabled(false);
    let result = b.compile_method(0, 0, 4, &[0xb1]);
    assert!(result.success);
    assert_eq!(result.sp_id_slot_off, 0, "no slot is reserved");
    let cm = publish_compiled_method(&result).expect("publishes");
    assert!(
        !cm.fully_oop_covered,
        "the default build must keep its conservative scan"
    );
    assert!(!cm.has_precise_oop_maps());
}

/// A method whose dataflow cannot answer keeps the conservative scan.
///
/// The end-to-end version of the `incomplete_oop_maps` term: a real
/// compile of a method with MORE THAN 64 LOCALS, where
/// `compute_local_oop_masks` returns nothing at all.
#[test]
pub(in crate::aarch64_backend::tests) fn a_method_the_dataflow_cannot_describe_does_not_claim_coverage() {
    let mut b = poll_backend();
    // 70 locals -- past the 64-slot mask, so `compute_local_oop_masks`
    // returns nothing at all -- AND a loop, so there is a non-entry
    // safepoint that has to consult it. The entry poll alone would not do:
    // it answers from `param_oop_mask` without touching the dataflow, and
    // for a method with no reference parameters that answer is complete.
    let code = [0x03, 0x3b, 0x84, 0x00, 0x01, 0xa7, 0xFF, 0xFD];
    let result = b.compile_method(70, 0, 4, &code);
    assert!(result.success);
    assert!(
        result.incomplete_oop_maps > 0,
        "a site the dataflow cannot answer for must be counted incomplete"
    );
    let cm = publish_compiled_method(&result).expect("publishes");
    assert!(
        !cm.fully_oop_covered,
        "and the method must not claim coverage it cannot prove"
    );
}

/// The published frame layout separates the caller's saved registers from
/// this frame's own words.
///
/// A ZERO layout tells the band verifier that nothing is a register image,
/// so the prologue's saved FP/LR pair and the caller's saved X19-X28 would
/// count as in-band words of THIS frame. They hold the CALLER's live
/// references, which this frame's maps have no business naming -- the
/// oracle would report them `never_mapped` and refute the coverage claim on
/// noise. An oracle that cries wolf is worse than one that is off.
#[test]
pub(in crate::aarch64_backend::tests) fn the_published_frame_layout_excludes_the_callers_saved_registers() {
    let mut b = poll_backend();
    let result = b.compile_method(3, 1, 4, &[0x2a, 0xb0]); // aload_0; areturn
    assert!(result.success);
    let layout = arm64_frame_layout(&result.frame);

    assert!(
        layout.callee_saved_shallow,
        "aarch64 puts the save area next to the frame pointer; saying so is \
         what stops the verifier's x86-64 half-line from swallowing the \
         whole spill area"
    );
    // The FP/LR pair is the frame record AT FP, not a word of this frame's
    // band: nothing at or above FP may be claimed.
    assert!(
        !layout.is_register_image(0),
        "[FP] is the caller's FP, outside the band"
    );
    assert!(
        !layout.is_register_image(-8),
        "[FP+8] is the return address"
    );
    // Every saved GPR is a register image.
    for (i, _) in result.frame.saved_regs.iter().enumerate() {
        let off = -(result.frame.callee_save_offset + (i as i32) * 8);
        assert!(
            layout.is_register_image(off),
            "saved register {i} at [FP-{off}] must be a register image"
        );
    }
    // The spill area is NOT a register image -- it is this frame's own
    // words, and it is where the oop maps point.
    if result.frame.num_spills > 0 {
        let deepest = -result.frame.spill_offset;
        assert!(
            !layout.is_register_image(deepest),
            "the spill area must stay visible to the verifier"
        );
        assert!(
            layout.spill_hi > layout.spill_lo,
            "the spill range must be published, or the verifier cannot tell \
             a dead slot from a missed root"
        );
        assert!(
            deepest >= layout.spill_lo && deepest < layout.spill_hi,
            "slot 0 ({deepest}) must fall inside the published spill range \
             [{}, {})",
            layout.spill_lo,
            layout.spill_hi
        );
    }
    // The two regions must not overlap, or a word belongs to both.
    assert!(
        layout.spill_lo >= layout.callee_saved_hi,
        "spill [{}, {}) overlaps the register images [{}, {})",
        layout.spill_lo,
        layout.spill_hi,
        layout.callee_saved_lo,
        layout.callee_saved_hi
    );
}

/// A published artifact carries that layout, not the all-zero default.
#[test]
pub(in crate::aarch64_backend::tests) fn a_published_artifact_carries_its_frame_layout() {
    let mut b = poll_backend();
    // aload_0; areturn -- local 0 is live, so it gets a callee-saved home.
    let result = b.compile_method(1, 1, 4, &[0x2a, 0xb0]);
    assert!(result.success);
    assert!(
        !result.frame.saved_regs.is_empty(),
        "test precondition: a saved GPR"
    );
    let cm = publish_compiled_method(&result).expect("publishes");
    assert!(
        cm.frame_layout.callee_saved_shallow,
        "the artifact must carry the aarch64 geometry"
    );
    let deepest_save = -result.frame.callee_save_offset;
    assert!(
        cm.frame_layout.is_register_image(deepest_save),
        "the caller's saved X19 at [FP-{deepest_save}] must be excluded from this frame's band"
    );
    assert_ne!(
        cm.frame_layout,
        cratonvm_jit_frame_layout_default(),
        "a zero layout would make the oracle report the caller's registers \
         as this frame's missed roots"
    );
}

pub(in crate::aarch64_backend::tests) fn cratonvm_jit_frame_layout_default() -> crate::FrameLayout {
    crate::FrameLayout::default()
}
/// A published artifact carries its resolved oop maps.
///
/// The publication path used to build its `CompiledMethod` with
/// `CompiledMethod::new(buf)` and never transfer the backend's maps -- so
/// even a correct writer would have produced nothing a GC could read. That
/// path sits behind `#[cfg(target_arch = "aarch64")]` and is therefore not
/// compiled on an x86-64 host at all, which is how it stayed that way; the
/// logic now lives in `publish_compiled_method`, which this exercises here.
///
/// The buffer is allocated and finalized but never CALLED: these are aarch64
/// bytes and the test host is x86-64. This asserts the metadata plumbing,
/// which is the half that was broken.
#[test]
pub(in crate::aarch64_backend::tests) fn a_published_artifact_carries_its_resolved_oop_maps() {
    let mut result = result_from_instructions(vec![
        Arm64Instruction::MovImm {
            rd: Arm64Register::X9,
            imm: 0x1234_5678_9ABC,
        },
        Arm64Instruction::Ret,
    ]);
    result.pending_oop_maps = vec![Arm64PendingOopMap {
        pseudo_index: 1,
        frame_slot_offsets: vec![32],
        safepoint_id: 11,
    }];

    let expected_pc =
        emit_machine_code(&result_from_instructions(vec![Arm64Instruction::MovImm {
            rd: Arm64Register::X9,
            imm: 0x1234_5678_9ABC,
        }]))
        .expect("the prefix encodes")
        .len() as u32;

    let mut cm = publish_compiled_method(&result).expect("the artifact publishes");
    assert!(
        cm.has_precise_oop_maps(),
        "the published artifact must carry the maps -- this is the transfer \
         that was missing"
    );
    assert_eq!(cm.oop_maps.len(), 1);
    assert_eq!(cm.oop_maps[0].native_pc_offset, expected_pc);
    assert_eq!(cm.oop_maps[0].frame_slot_offsets, vec![32]);
    // The claim that would be false: no shadow stack, no relocation
    // support, and register oops covered only conservatively.
    assert!(!cm.oop_maps[0].moving_young_coverage_complete);
    assert!(
        !cm.fully_oop_covered,
        "`fully_oop_covered` licenses SUPPRESSING the conservative scan, and \
         nothing here can execute aarch64 to earn that -- the slot exists \
         now, the evidence does not"
    );
    // And `find_oop_map_for_pc` -- the reader that applies here, since there
    // is no safepoint-id to select by -- finds it at that PC.
    assert!(cm.find_oop_map_for_pc(expected_pc).is_some());
}

/// THE BUG THIS FIXES, pinned as a difference.
///
/// `emit_oop_map_for_safepoint` used to key its map as
/// `instruction_count * 4`, and the 2026-08-01 parity audit made it fail
/// the method closed rather than let a caller inherit that, noting the fix
/// was "to key oop maps off the *encoder's* byte offset ... then delete
/// this guard".
///
/// The stream below is built from exactly the pseudo-ops that break the old
/// arithmetic: a `Comment` and a `Label` that emit NOTHING, a wide `MovImm`
/// that expands to several words, and a `ConstantPoolEntry` that emits 8
/// bytes. The map's PC must be where the encoder actually put the following
/// instruction -- and must NOT be `index * 4`.
#[test]
pub(in crate::aarch64_backend::tests) fn the_oop_map_pc_is_the_encoders_byte_offset() {
    let prefix = vec![
        Arm64Instruction::Comment("emits nothing".to_string()),
        Arm64Instruction::MovImm {
            rd: Arm64Register::X9,
            imm: 0x1234_5678_9ABC,
        },
        Arm64Instruction::Label(1),
        Arm64Instruction::ConstantPoolEntry {
            label: 2,
            value: 0xDEAD_BEEF,
        },
    ];
    // The safepoint sits before the instruction at index 4.
    let sp_index = prefix.len() as u32;
    let mut instructions = prefix.clone();
    instructions.push(Arm64Instruction::Ret);

    // The expected byte offset, computed by ENCODING THE PREFIX rather than
    // by hardcoding any instruction's width -- so this test cannot drift
    // with the encoder.
    let expected = emit_machine_code(&result_from_instructions(prefix))
        .expect("the prefix encodes")
        .len() as u32;

    let mut result = result_from_instructions(instructions);
    result.pending_oop_maps = vec![Arm64PendingOopMap {
        pseudo_index: sp_index,
        frame_slot_offsets: vec![16, 24],
        safepoint_id: 7,
    }];

    let (_code, maps) = emit_machine_code_with_oop_maps(&result).expect("the method encodes");
    assert_eq!(maps.len(), 1);
    assert_eq!(
        maps[0].native_pc_offset, expected,
        "the map must be keyed by the encoder's byte offset"
    );
    assert_eq!(maps[0].frame_slot_offsets, vec![16, 24]);

    // And the old arithmetic really would have been wrong here -- without
    // this the test would pass on a stream where the two happen to agree.
    assert_ne!(
        expected,
        sp_index * 4,
        "this stream must actually distinguish the encoder offset from the \
         pseudo-op count; pick different pseudo-ops if it stops doing so"
    );
}

/// The writer records a PENDING map, and refuses rather than truncate.
#[test]
pub(in crate::aarch64_backend::tests) fn the_oop_map_writer_records_a_pending_map() {
    let mut backend = Arm64Backend::new();
    assert!(!backend.failed);
    // Nothing marked as an oop yet: an empty map is not recorded at all.
    backend.emit_oop_map_for_safepoint(0);
    assert!(
        !backend.failed,
        "the writer no longer fails the method closed"
    );
    assert_eq!(
        backend.pending_oop_maps.len(),
        1,
        "every safepoint publishes an entry, so its id resolves"
    );
    assert!(
        backend.pending_oop_maps[0].frame_slot_offsets.is_empty(),
        "...and a site with no live reference names nothing"
    );
}

/// Fail closed on a pending map the encoder cannot place.
///
/// A `pseudo_index` past the end of the stream cannot be resolved to a byte
/// offset, and a GUESSED PC is the failure mode this whole two-phase
/// arrangement exists to prevent -- so the method is discarded.
#[test]
pub(in crate::aarch64_backend::tests) fn an_unplaceable_oop_map_discards_the_method() {
    let mut result = result_from_instructions(vec![Arm64Instruction::Ret]);
    result.pending_oop_maps = vec![Arm64PendingOopMap {
        pseudo_index: 99,
        frame_slot_offsets: vec![8],
        safepoint_id: 3,
    }];
    assert!(
        emit_machine_code_with_oop_maps(&result).is_none(),
        "an oop map that cannot be placed must discard the method"
    );
    // The plain encoder is unaffected: it publishes no map, so an
    // unresolvable one cannot mislead anything through that path.
    assert!(emit_machine_code(&result).is_some());
}

/// A safepoint at the very END of the stream still resolves, via the
/// trailing sentinel in `pseudo_offsets`.
#[test]
pub(in crate::aarch64_backend::tests) fn a_safepoint_at_the_end_of_the_stream_resolves_to_the_code_length() {
    let instructions = vec![Arm64Instruction::Ret];
    let mut result = result_from_instructions(instructions);
    result.pending_oop_maps = vec![Arm64PendingOopMap {
        pseudo_index: 1,
        frame_slot_offsets: vec![8],
        safepoint_id: 3,
    }];
    let (code, maps) = emit_machine_code_with_oop_maps(&result).expect("the method encodes");
    assert_eq!(maps.len(), 1);
    assert_eq!(maps[0].native_pc_offset as usize, code.len());
}

/// A frame larger than a guard page compiles, and touches every page it
/// crosses before SP moves.
///
/// Formerly `oversized_frame_bails_no_stack_bang`: with no bang, `SUB SP,
/// SP, #frame` could step clean past the guard page, so such frames were
/// refused outright.
#[test]
pub(in crate::aarch64_backend::tests) fn a_large_frame_bangs_every_page_before_moving_sp() {
    // max_stack = 600 -> 600 operand words -> a 4816-byte frame.
    let mut big = Arm64Backend::new();
    big.set_safepoints_enabled(false);
    let result = big.compile_method(0, 0, 600, &[0xb1]);
    assert!(result.frame.frame_size >= 4096, "test precondition");
    assert!(
        result.success,
        "a large frame must compile once it is banged"
    );

    let below = result.frame.frame_size - 16;
    let ops = &result.instructions;
    let probes: Vec<i32> = ops
        .iter()
        .filter_map(|i| match i {
            Arm64Instruction::SubImm { rd, rn, imm }
                if *rd == Arm64Register::X16 && *rn == Arm64Register::SP =>
            {
                Some(*imm)                }
            _ => None,
        })
        .collect();
    assert_eq!(Some(probes.clone()), stack_bang_probe_offsets(below));
    assert_eq!(
        probes.last().copied(),
        Some(below),
        "the exact frame bottom is probed"
    );

    let alloc = ops
        .iter()
        .position(|i| {
            matches!(i, Arm64Instruction::SubImm { rd, rn, .. }
                     if *rd == Arm64Register::SP && *rn == Arm64Register::SP)
        })
        .expect("the frame is allocated");
    let last_touch = ops
        .iter()
        .rposition(|i| {
            matches!(i, Arm64Instruction::Str { rt, rn, offset: 0 }
                     if *rt == Arm64Register::XZR && *rn == Arm64Register::X16)
        })
        .expect("each probe stores through X16");
    assert!(last_touch < alloc, "every page is touched BEFORE SP moves");
    assert!(
        emit_machine_code(&result).is_some(),
        "the 4800-byte SUB SP needs the extended-register form, which now exists"
    );

    // A frame under a page cannot skip the guard, and gets no probe.
    let mut small = Arm64Backend::new();
    small.set_safepoints_enabled(false);
    let ok = small.compile_method(0, 0, 4, &[0xb1]);
    assert!(ok.success);
    assert!(!ok.instructions.iter().any(|i| {
        matches!(i, Arm64Instruction::Str { rt, rn, .. }
                 if *rt == Arm64Register::XZR && *rn == Arm64Register::X16)
    }));
}

#[test]
pub(in crate::aarch64_backend::tests) fn stack_bang_probe_offsets_cover_every_page_crossed() {
    assert_eq!(stack_bang_probe_offsets(0), Some(vec![]));
    assert_eq!(stack_bang_probe_offsets(4095), Some(vec![]));
    assert_eq!(stack_bang_probe_offsets(4096), Some(vec![4096]));
    assert_eq!(stack_bang_probe_offsets(8200), Some(vec![4096, 8192, 8200]));
    assert_eq!(stack_bang_probe_offsets(-1), None);
    assert_eq!(
        stack_bang_probe_offsets(4096 * (MAX_STACK_BANG_PROBES as i32 + 1)),
        None,
        "past the probe cap the method is refused"
    );
}

/// A wide SP adjustment lowers through the extended-register form.
///
/// Formerly `addsub_imm_safe_refuses_unencodable_sp_adjustment`: the only
/// register fallback was the shifted-register form, where 31 is XZR.
#[test]
pub(in crate::aarch64_backend::tests) fn addsub_imm_safe_lowers_a_wide_sp_adjustment() {
    use crate::aarch64::{Aarch64Emitter, Reg};

    let mut e = Aarch64Emitter::new();
    assert!(emit_addsub_imm_safe(
        &mut e,
        crate::aarch64::SP,
        crate::aarch64::SP,
        5000,
        true
    ));
    let n = e.code().len();
    assert_eq!(n, 8, "MOVZ X16, #5000; SUB SP, SP, X16, UXTX");
    let last = u32::from_le_bytes(e.code()[n - 4..].try_into().unwrap());
    assert_eq!(last, 0xCB30_63FF, "sub sp, sp, x16 -- SP on both sides");

    let mut e2 = Aarch64Emitter::new();
    assert!(emit_addsub_imm_safe(
        &mut e2,
        crate::aarch64::SP,
        crate::aarch64::SP,
        8192,
        true
    ));
    assert_eq!(
        e2.code().len(),
        4,
        "the shifted-12 immediate is still one word"
    );

    // The one refusal left: an operand that is IP0 itself.
    let mut e3 = Aarch64Emitter::new();
    assert!(!emit_addsub_imm_safe(
        &mut e3,
        Reg::X9,
        Reg::X16,
        5000,
        false
    ));
}

/// A far SP-relative access adds SP, not XZR, to the offset.
#[test]
pub(in crate::aarch64_backend::tests) fn a_far_sp_relative_access_materializes_an_sp_address() {
    let result = result_from_instructions(vec![
        Arm64Instruction::Ldr {
            rt: Arm64Register::X9,
            rn: Arm64Register::SP,
            offset: 100_000,
        },
        Arm64Instruction::Ret,
    ]);
    let bytes = emit_machine_code(&result).expect("encodes");
    let n = bytes.len();
    let add = u32::from_le_bytes(bytes[n - 12..n - 8].try_into().unwrap());
    assert_eq!(add, 0x8B30_63F0, "add x16, sp, x16 in the extended form");
}

/// `emit_machine_code` encodes a wide SP adjustment instead of refusing.
#[test]
pub(in crate::aarch64_backend::tests) fn emit_machine_code_encodes_a_wide_sp_immediate() {
    let result = result_from_instructions(vec![
        Arm64Instruction::SubImm {
            rd: Arm64Register::SP,
            rn: Arm64Register::SP,
            imm: 5000,
        },
        Arm64Instruction::Ret,
    ]);
    assert_eq!(
        emit_machine_code(&result).map(|b| b.len()),
        Some(12),
        "MOVZ, SUB (extended), RET"
    );

    // The shifted-immediate form still produces code too.
    let ok = result_from_instructions(vec![
        Arm64Instruction::SubImm {
            rd: Arm64Register::SP,
            rn: Arm64Register::SP,
            imm: 8192,
        },
        Arm64Instruction::Ret,
    ]);
    assert!(emit_machine_code(&ok).is_some());
}
// =====================================================================
// The category-dependent stack shuffles.
//
// This backend keeps TWO simulated operand stacks — `operand_stack` for
// int/long/reference and `float_operand_stack` for float/double — and its
// shuffle arms popped a FIXED number of entries from the first one. Both
// assumptions are wrong in general, and the x64 `dup2_x2` page
// (`dup2_x2-is-scan-admitted-but-lowered-by-neither-x64-backend`) raised
// exactly this as a question it did not answer: whether that backend's
// unconditional four-pop was live here too.
//
// It was, and so were four more arms. These tests read the simulated
// stack directly after the walk, because entry COUNT is what the defect
// was about: a category-2 value is one entry here and two JVM slots, so a
// fixed pop either leaves the shuffle short or reaches past it into
// entries the shuffle must not touch.
// =====================================================================

/// `[int, int, long, long] dup2_x2` is JVMS FORM 4 — two entries, not
/// four. The old unconditional four-pop swallowed the two `int`s beneath
/// and pushed a six-entry stack in the wrong order; the `int`s below the
/// shuffle must come through untouched.
#[test]
pub(in crate::aarch64_backend::tests) fn dup2_x2_form4_leaves_the_entries_beneath_it_alone() {
    // iconst_0, iconst_1, lconst_0, lconst_1
    let prefix = [0x03u8, 0x04, 0x09, 0x0a];
    let mut control = Arm64Backend::new();
    assert!(control.compile_method(4, 0, 8, &prefix).success);
    let before = control.operand_stack.clone();
    assert_eq!(before.len(), 4, "control: four values, four entries");

    let mut backend = Arm64Backend::new();
    let mut code = prefix.to_vec();
    code.push(0x5e); // dup2_x2
    assert!(
        backend.compile_method(4, 0, 8, &code).success,
        "FORM 4 dup2_x2 must lower"
    );
    let after = &backend.operand_stack;
    assert_eq!(after.len(), 5, "[v2, v1] -> [v1, v2, v1] over two ints");
    assert_eq!(
        &after[..2],
        &before[..2],
        "the two ints below the shuffle must not move"
    );
    assert_eq!(after[3], before[2], "v2 stays in place");
    assert_eq!(after[4], before[3], "v1 stays on top");
    assert!(
        after[2] != after[4],
        "the inserted copy is a fresh register, not the original"
    );
}

/// `[long, int, int] dup2_x2` is FORM 3 — the copy goes three entries
/// down, not four. The old arm popped a fourth entry that did not exist.
#[test]
pub(in crate::aarch64_backend::tests) fn dup2_x2_form3_duplicates_two_entries_over_one() {
    let prefix = [0x09u8, 0x03, 0x04]; // lconst_0, iconst_0, iconst_1
    let mut control = Arm64Backend::new();
    assert!(control.compile_method(4, 0, 8, &prefix).success);
    let before = control.operand_stack.clone();

    let mut backend = Arm64Backend::new();
    let mut code = prefix.to_vec();
    code.push(0x5e);
    assert!(
        backend.compile_method(4, 0, 8, &code).success,
        "FORM 3 dup2_x2 must lower"
    );
    let after = &backend.operand_stack;
    assert_eq!(after.len(), 5, "[v3, v2, v1] -> [v2, v1, v3, v2, v1]");
    assert_eq!(after[2], before[0], "the long stays where it was");
    assert_eq!(after[3], before[1]);
    assert_eq!(after[4], before[2]);
}

/// FORM 1, the only form the old arm handled: four category-1 entries.
#[test]
pub(in crate::aarch64_backend::tests) fn dup2_x2_form1_still_lowers() {
    let prefix = [0x03u8, 0x04, 0x05, 0x06]; // iconst_0..3
    let mut backend = Arm64Backend::new();
    let mut code = prefix.to_vec();
    code.push(0x5e);
    assert!(backend.compile_method(4, 0, 8, &code).success);
    assert_eq!(backend.operand_stack.len(), 6);
}

/// Floating-point operands shuffle like any other, and the forms the JVMS
/// does not define still refuse.
///
/// Formerly `a_floating_point_operand_refuses_the_shuffle_rather_than_moving_the_wrong_stack`.
/// Floats lived on a second stack the shuffles never touched, so every
/// shuffle involving one had to refuse. There is one typed stack now, and
/// each shuffle resolves its JVMS form from the entries' categories.
#[test]
pub(in crate::aarch64_backend::tests) fn floating_point_operands_shuffle_on_the_unified_stack() {
    for (name, code, depth) in [
        (
            "dup of a float over two ints",
            vec![0x03u8, 0x04, 0x0b, 0x59],
            4,
        ),
        (
            "pop of a float over two ints",
            vec![0x03, 0x04, 0x0b, 0x57],
            2,
        ),
        ("dup2 of a double", vec![0x0e, 0x5c], 2),
        ("dup2_x2 of two doubles", vec![0x0e, 0x0f, 0x5e], 3),
        ("dup_x1 with a float below", vec![0x0b, 0x03, 0x5a], 3),
        ("swap with a float below", vec![0x0b, 0x03, 0x5f], 2),
        ("pop of a float", vec![0x0b, 0x57], 0),
    ] {
        let mut backend = Arm64Backend::new();
        assert!(
            backend.compile_method(4, 0, 8, &code).success,
            "{name} must lower"
        );
        assert_eq!(backend.operand_stack.len(), depth, "{name}");
    }

    // `swap` really exchanges the values, kinds and all.
    let mut s = Arm64Backend::new();
    assert!(s.compile_method(4, 0, 8, &[0x0b, 0x03, 0x5f]).success);
    let kinds: Vec<OperandKind> = s.operand_stack.iter().map(|o| o.kind).collect();
    assert_eq!(kinds, vec![OperandKind::I32, OperandKind::F32]);

    // `dup` of a float copies at the float's width.
    let mut d = Arm64Backend::new();
    let dup = d.compile_method(4, 0, 8, &[0x0b, 0x59]);
    assert!(dup
        .instructions
        .iter()
        .any(|i| matches!(i, Arm64Instruction::FmovFpSingle { .. })));

    // Forms the JVMS does not define.
    for (name, code) in [
        ("dup of a double", vec![0x0eu8, 0x59]),
        ("dup_x1 over a long", vec![0x09, 0x03, 0x5a]),
        ("swap of a double", vec![0x03, 0x0e, 0x5f]),
        ("pop of a long", vec![0x09, 0x57]),
    ] {
        let mut backend = Arm64Backend::new();
        assert!(
            !backend.compile_method(4, 0, 8, &code).success,
            "{name} must refuse"
        );
    }
}

/// `pop2` over a single category-2 value pops ONE entry here. The old arm
/// popped two, discarding whatever the `long` was sitting on.
#[test]
pub(in crate::aarch64_backend::tests) fn pop2_over_a_long_discards_one_entry_not_two() {
    // iconst_0, lconst_0, pop2 -> the int must survive.
    let mut backend = Arm64Backend::new();
    assert!(backend.compile_method(4, 0, 8, &[0x03, 0x09, 0x58]).success);
    assert_eq!(
        backend.operand_stack.len(),
        1,
        "pop2 of a category-2 value leaves the int beneath it"
    );
}

/// `dup2` over a single category-2 value duplicates ONE entry. The old arm
/// duplicated the unrelated value beneath the `long` as well — the same
/// miscompile x64 was fixed for.
#[test]
pub(in crate::aarch64_backend::tests) fn dup2_over_a_long_duplicates_one_entry_not_two() {
    let mut backend = Arm64Backend::new();
    assert!(backend.compile_method(4, 0, 8, &[0x03, 0x09, 0x5c]).success);
    assert_eq!(
        backend.operand_stack.len(),
        3,
        "[int, long] -> [int, long, long]"
    );
}

/// When the width analysis cannot type the operands, the method is
/// refused rather than shuffled on a guess. `dup2_x2` at pc 0 has no
/// operands at all.
#[test]
pub(in crate::aarch64_backend::tests) fn an_untypeable_shuffle_refuses_the_method() {
    let mut backend = Arm64Backend::new();
    assert!(!backend.compile_method(4, 0, 8, &[0x5e]).success);
}

// =====================================================================
// 2026-09-12 review fixes. `eval_int_method` runs the integer pseudo-ops a
// straight-line method lowers to, so VALUES are checked on a host that
// cannot execute AArch64; the other tests pin the emitted shapes.
// =====================================================================

/// Run a compiled method's integer pseudo-ops from its prologue to `RET`
/// and answer X0. Panics on any pseudo-op it does not model, so a test
/// cannot pass by skipping one.
pub(in crate::aarch64_backend::tests) fn eval_int_method(result: &Arm64CompileResult, args: &[i64]) -> i64 {
    let ops = &result.instructions;
    let mut reg = [0u64; 64];
    let mut mem: HashMap<u64, u64> = HashMap::new();
    reg[31] = 0x7FFF_0000; // SP
    for (i, a) in args.iter().enumerate() {
        reg[i] = *a as u64;
    }
    let labels: HashMap<u32, usize> = ops
        .iter()
        .enumerate()
        .filter_map(|(i, op)| match op {
            Arm64Instruction::Label(l) => Some((*l, i)),
            _ => None,
        })
        .collect();
    let w = |v: u64| v as u32;
    let at = |base: u64, off: i32| base.wrapping_add(off as i64 as u64);
    let mut pc = 0usize;
    for _ in 0..100_000 {
        let Some(op) = ops.get(pc) else {
            panic!("fell off the end of the pseudo-op stream");
        };
        let mut next = pc + 1;
        match op {
            Arm64Instruction::Label(_)
            | Arm64Instruction::Comment(_)
            | Arm64Instruction::Nop => {}
            Arm64Instruction::MovImm { rd, imm } => reg[rd.0 as usize] = *imm as u64,
            Arm64Instruction::Mov { rd, rm } => reg[rd.0 as usize] = reg[rm.0 as usize],
            Arm64Instruction::AddImm { rd, rn, imm } => {
                reg[rd.0 as usize] = at(reg[rn.0 as usize], *imm)
            }
            Arm64Instruction::SubImm { rd, rn, imm } => {
                reg[rd.0 as usize] = at(reg[rn.0 as usize], imm.wrapping_neg())
            }
            Arm64Instruction::AddW { rd, rn, rm } => {
                reg[rd.0 as usize] =
                    u64::from(w(reg[rn.0 as usize]).wrapping_add(w(reg[rm.0 as usize])))
            }
            Arm64Instruction::SubW { rd, rn, rm } => {
                reg[rd.0 as usize] =
                    u64::from(w(reg[rn.0 as usize]).wrapping_sub(w(reg[rm.0 as usize])))
            }
            Arm64Instruction::LslW { rd, rn, rm } => {
                reg[rd.0 as usize] =
                    u64::from(w(reg[rn.0 as usize]).wrapping_shl(w(reg[rm.0 as usize])))
            }
            Arm64Instruction::AddImmW { rd, rn, imm } => {
                reg[rd.0 as usize] = u64::from(w(reg[rn.0 as usize]).wrapping_add(*imm as u32))
            }
            Arm64Instruction::SubImmW { rd, rn, imm } => {
                reg[rd.0 as usize] = u64::from(w(reg[rn.0 as usize]).wrapping_sub(*imm as u32))
            }
            Arm64Instruction::Sxtw { rd, rn } => {
                reg[rd.0 as usize] = w(reg[rn.0 as usize]) as i32 as i64 as u64
            }
            Arm64Instruction::CbzW { rt, label } => {
                if w(reg[rt.0 as usize]) == 0 {
                    next = labels[label];
                }
            }
            Arm64Instruction::CbnzW { rt, label } => {
                if w(reg[rt.0 as usize]) != 0 {
                    next = labels[label];
                }
            }
            Arm64Instruction::Str { rt, rn, offset } => {
                // Register 31 as a store's Rt is XZR.
                let v = if rt.0 == 31 { 0 } else { reg[rt.0 as usize] };
                mem.insert(at(reg[rn.0 as usize], *offset), v);
            }
            Arm64Instruction::Ldr { rt, rn, offset } => {
                let addr = at(reg[rn.0 as usize], *offset);
                reg[rt.0 as usize] = *mem
                    .get(&addr)
                    .unwrap_or_else(|| panic!("load of an unwritten word at {addr:#x}"));
            }
            Arm64Instruction::StpPre {
                rt1,
                rt2,
                rn,
                offset,
            } => {
                let base = at(reg[rn.0 as usize], *offset);
                mem.insert(base, reg[rt1.0 as usize]);
                mem.insert(base + 8, reg[rt2.0 as usize]);
                reg[rn.0 as usize] = base;
            }
            Arm64Instruction::Stp {
                rt1,
                rt2,
                rn,
                offset,
            } => {
                let base = at(reg[rn.0 as usize], *offset);
                mem.insert(base, reg[rt1.0 as usize]);
                mem.insert(base + 8, reg[rt2.0 as usize]);
            }
            Arm64Instruction::Ldp {
                rt1,
                rt2,
                rn,
                offset,
            } => {
                let base = at(reg[rn.0 as usize], *offset);
                reg[rt1.0 as usize] = mem[&base];
                reg[rt2.0 as usize] = mem[&(base + 8)];
            }
            Arm64Instruction::LdpPost {
                rt1,
                rt2,
                rn,
                offset,
            } => {
                let base = reg[rn.0 as usize];
                reg[rt1.0 as usize] = mem[&base];
                reg[rt2.0 as usize] = mem[&(base + 8)];
                reg[rn.0 as usize] = at(base, *offset);
            }
            Arm64Instruction::B { label } => next = labels[label],
            Arm64Instruction::Ret => return reg[0] as i64,
            other => panic!("eval_int_method does not model {other:?}"),
        }
        pc = next;
    }
    panic!("100000 steps without a RET");
}

/// Compile a static method with `descriptor`, polls off, and require it to
/// succeed.
pub(in crate::aarch64_backend::tests) fn compile_with(
    descriptor: &str,
    locals: usize,
    stack: usize,
    code: &[u8],
) -> Arm64CompileResult {
    let mut b = Arm64Backend::new();
    b.set_safepoints_enabled(false);
    b.set_method_descriptor(descriptor, true);
    let slots = crate::compute_param_jvm_slots(descriptor, true).1;
    let result = b.compile_method(locals, slots, stack, code);
    assert!(result.success, "{descriptor} {code:02x?} must compile");
    result
}

/// THE REPORTED MISCOMPILE. `static int f(int a, int b) { return a -
/// (b+1+2+3+4); }` returned -4 for `f(100, 5)`: the round-robin scratch
/// allocator wrapped onto a live register, spilled it, recorded the spill
/// against the REGISTER, pushed the same register for the new value, and
/// popping the new value reloaded the old one.
#[test]
pub(in crate::aarch64_backend::tests) fn the_expression_that_returned_minus_four_evaluates_to_85() {
    let code = [
        0x1a, 0x1b, 0x04, 0x60, 0x05, 0x60, 0x06, 0x60, 0x07, 0x60, 0x64, 0xac,
    ];
    let result = compile_with("(II)I", 2, 3, &code);
    assert_eq!(eval_int_method(&result, &[100, 5]), 85);
    assert_eq!(eval_int_method(&result, &[0, -15]), 5); // 0 - (-15 + 10)
}

/// A stack deeper than the seven scratch registers keeps every operand, in
/// order -- checked with a subtraction chain, which is order-sensitive.
#[test]
pub(in crate::aarch64_backend::tests) fn a_stack_deeper_than_the_scratch_pool_keeps_every_value() {
    let ten_values = || {
        let mut code = vec![0x04u8, 0x05, 0x06, 0x07, 0x08]; // iconst_1..5
        for v in 6..=10u8 {
            code.extend_from_slice(&[0x10, v]); // bipush
        }
        code
    };
    let mut sum = ten_values();
    sum.extend(std::iter::repeat(0x60).take(9));
    sum.push(0xac);
    assert_eq!(eval_int_method(&compile_with("()I", 0, 10, &sum), &[]), 55);

    // 1 - (2 - (3 - (4 - (5 - (6 - (7 - (8 - (9 - 10))))))))
    let mut sub = ten_values();
    sub.extend(std::iter::repeat(0x64).take(9));
    sub.push(0xac);
    assert_eq!(eval_int_method(&compile_with("()I", 0, 10, &sub), &[]), -5);
}

/// A value on the operand stack across a branch reaches the merge from
/// both paths: `a == 0 ? 2 : 1`. The model used to follow the walk, so the
/// two arms left the value in different registers.
#[test]
pub(in crate::aarch64_backend::tests) fn a_ternary_value_survives_the_merge() {
    // iload_0; ifeq 8; iconst_1; goto 9; 8: iconst_2; 9: ireturn
    let code = [0x1a, 0x99, 0x00, 0x07, 0x04, 0xa7, 0x00, 0x04, 0x05, 0xac];
    let result = compile_with("(I)I", 1, 1, &code);
    assert_eq!(eval_int_method(&result, &[0]), 2);
    assert_eq!(eval_int_method(&result, &[5]), 1);
}

/// `int` results wrap at 32 bits and stay sign-extended through `i2l`,
/// `l2i` sign-extends, `ishl` masks, and `iinc` wraps.
#[test]
pub(in crate::aarch64_backend::tests) fn int_arithmetic_wraps_and_stays_sign_extended() {
    let min = i64::from(i32::MIN);
    let max = i64::from(i32::MAX);
    let add = compile_with("(II)I", 2, 2, &[0x1a, 0x1b, 0x60, 0xac]);
    assert_eq!(eval_int_method(&add, &[max, 1]), min);
    // iload_0; iload_1; iadd; i2l; lreturn
    let widen = compile_with("(II)J", 2, 2, &[0x1a, 0x1b, 0x60, 0x85, 0xad]);
    assert_eq!(
        eval_int_method(&widen, &[max, 1]),
        min,
        "i2l of a wrapped int"
    );
    // lload_0; l2i; ireturn
    let narrow = compile_with("(J)I", 2, 2, &[0x1e, 0x88, 0xac]);
    assert_eq!(
        eval_int_method(&narrow, &[0xFFFF_FFFF]),
        -1,
        "(int) 0xFFFFFFFFL"
    );
    assert_eq!(eval_int_method(&narrow, &[0x1_0000_0005]), 5);
    // iload_0; iload_1; ishl; ireturn
    let shl = compile_with("(II)I", 2, 2, &[0x1a, 0x1b, 0x78, 0xac]);
    assert_eq!(
        eval_int_method(&shl, &[1, 32]),
        1,
        "the distance is masked to 5 bits"
    );
    assert_eq!(eval_int_method(&shl, &[1, 31]), min);
    // iinc 0, 1; iload_0; ireturn
    let inc = compile_with("(I)I", 1, 1, &[0x84, 0x00, 0x01, 0x1a, 0xac]);
    assert_eq!(eval_int_method(&inc, &[max]), min);
}

/// Every argument lands in its JVM local, category 2 included.
///
/// The prologue moved argument register `i` into local `i`, which is wrong
/// for every argument after a `long` or `double`.
#[test]
pub(in crate::aarch64_backend::tests) fn parameters_after_a_long_are_homed_by_jvm_slot() {
    // static int f(long a, long b, int c) { return c; } -- iload 4; ireturn
    let r1 = compile_with("(JJI)I", 5, 1, &[0x15, 0x04, 0xac]);
    assert_eq!(
        eval_int_method(&r1, &[11, 22, 33]),
        33,
        "c is argument 2 and local 4"
    );
    // static long f(int a, long b) { return b; } -- lload_1; lreturn
    let r2 = compile_with("(IJ)J", 3, 2, &[0x1f, 0xad]);
    assert_eq!(eval_int_method(&r2, &[7, -9]), -9);
}

/// A frame-homed parameter -- which every `float`/`double` one is -- is
/// stored by the prologue and read back from that word. It used to be
/// stored nowhere.
#[test]
pub(in crate::aarch64_backend::tests) fn floating_point_parameters_are_stored_to_their_frame_homes() {
    // static double f(double a, double b) { return b; }
    // dload_0; pop2; dload_2; dreturn
    let result = compile_with("(DD)D", 4, 2, &[0x26, 0x58, 0x28, 0xaf]);
    let ops = &result.instructions;
    let home_of = |arg: Arm64Register| {
        ops.iter().find_map(|i| match i {
            Arm64Instruction::Str { rt, rn, offset }
                if *rt == arg && *rn == Arm64Register::FP =>
            {
                Some(*offset)
            }
            _ => None,
        })
    };
    let a = home_of(Arm64Register::X0).expect("argument 0 is stored to its frame home");
    let b = home_of(Arm64Register::X1).expect("argument 1 is stored to its frame home");
    assert_ne!(a, b);
    let loads: Vec<i32> = ops
        .iter()
        .filter_map(|i| match i {
            Arm64Instruction::FpLdr {
                rn,
                offset,
                is_double: true,
                ..                } if *rn == Arm64Register::FP => Some(*offset),
            _ => None,
        })
        .collect();
    assert_eq!(
        loads,
        vec![a, b],
        "each dload reads the word its argument was stored to"
    );
}

/// `freturn`/`dreturn` move the bits into X0 -- where the VM reads every
/// result -- and leave through the shared epilogue. They used to discard
/// the value and emit a bare `RET`, skipping the frame teardown.
#[test]
pub(in crate::aarch64_backend::tests) fn fp_returns_move_the_bits_to_x0_and_take_the_epilogue() {
    for (code, single) in [(&[0x0cu8, 0xae][..], true), (&[0x0f, 0xaf][..], false)] {
        let result = make_backend_with_method(0, 0, code);
        assert!(result.success);
        let ops = &result.instructions;
        let mv = ops
            .iter()
            .position(|i| {
                if single {
                    matches!(i, Arm64Instruction::FmovFromFpSingle { rd, .. } if *rd == Arm64Register::X0)
                } else {
                    matches!(i, Arm64Instruction::FmovFromFp { rd, .. } if *rd == Arm64Register::X0)
                }
            })
            .expect("the value is moved into X0");
        assert!(
            matches!(ops[mv + 1], Arm64Instruction::B { .. }),
            "then to the epilogue"
        );
        let rets = ops
            .iter()
            .filter(|i| matches!(i, Arm64Instruction::Ret))
            .count();
        assert_eq!(rets, 1, "the only RET is the epilogue's");
        assert!(matches!(ops.last(), Some(Arm64Instruction::Ret)));
    }
}

/// `*cmpg` negates on MI and `*cmpl` on LT, and the NZCV truth table after
/// `FCMP` says that puts NaN where the JVMS does. `fcmpg` used to take
/// `B.LT`, which is TRUE on unordered, so NaN produced -1.
#[test]
pub(in crate::aarch64_backend::tests) fn fp_compares_put_nan_on_the_jvms_side() {
    type Nzcv = (bool, bool, bool, bool);
    const LESS: Nzcv = (true, false, false, false);
    const EQUAL: Nzcv = (false, true, true, false);
    const GREATER: Nzcv = (false, false, true, false);
    const UNORDERED: Nzcv = (false, false, true, true);
    fn holds(cond: Arm64Condition, (n, z, _c, v): Nzcv) -> bool {
        match cond {
            Arm64Condition::Ne => !z,
            Arm64Condition::Mi => n,
            Arm64Condition::Lt => n != v,
            other => panic!("unmodelled condition {other:?}"),
        }
    }
    // CSET ne; CNEG <cond>
    fn value(cond: Arm64Condition, flags: Nzcv) -> i32 {
        let v = i32::from(holds(Arm64Condition::Ne, flags));
        if holds(cond, flags) {
            -v
        } else {
            v
        }
    }
    for (code, nan) in [
        ([0x0bu8, 0x0c, 0x95, 0xac], -1), // fcmpl
        ([0x0b, 0x0c, 0x96, 0xac], 1),    // fcmpg
        ([0x0e, 0x0f, 0x97, 0xac], -1),   // dcmpl
        ([0x0e, 0x0f, 0x98, 0xac], 1),    // dcmpg
    ] {
        let r = make_backend_with_method(0, 0, &code);
        assert!(r.success);
        assert!(r.instructions.iter().any(|i| matches!(
            i,
            Arm64Instruction::Cset {
                cond: Arm64Condition::Ne,
                ..
            }
        )));
        assert!(
            !r.instructions
                .iter()
                .any(|i| matches!(i, Arm64Instruction::BCond { .. })),
            "branch-free"
        );
        let cond = r
            .instructions
            .iter()
            .find_map(|i| match i {
                Arm64Instruction::Cneg { cond, .. } => Some(*cond),
                _ => None,
            })
            .expect("CNEG");
        assert_eq!(value(cond, UNORDERED), nan, "0x{:02x} on NaN", code[2]);
        assert_eq!(value(cond, LESS), -1);
        assert_eq!(value(cond, EQUAL), 0);
        assert_eq!(value(cond, GREATER), 1);
    }
}

/// `f2i`/`d2i` use the 32-bit saturating `FCVTZS W` and then sign-extend;
/// `l2i` sign-extends instead of masking.
#[test]
pub(in crate::aarch64_backend::tests) fn fp_to_int_conversions_saturate_at_32_bits() {
    for (code, name) in [
        (&[0x0cu8, 0x8b, 0xac][..], "f2i"),
        (&[0x0f, 0x8e, 0xac][..], "d2i"),
    ] {
        let r = make_backend_with_method(0, 0, code);
        assert!(r.success);
        let ops = &r.instructions;
        let at = ops
            .iter()
            .position(|i| {
                matches!(
                    i,
                    Arm64Instruction::FcvtzsSingle { .. } | Arm64Instruction::FcvtzsIntW { .. }
                )
            })
            .unwrap_or_else(|| panic!("{name} must use FCVTZS W"));
        let rd = match ops[at] {
            Arm64Instruction::FcvtzsSingle { rd, .. }
            | Arm64Instruction::FcvtzsIntW { rd, .. } => rd,
            _ => unreachable!(),
        };
        assert!(
            matches!(ops[at + 1], Arm64Instruction::Sxtw { rd: d, rn: s } if d == rd && s == rd),
            "{name} then SXTW"
        );
        assert!(
            !ops.iter().any(|i| matches!(
                i,
                Arm64Instruction::FcvtzsInt { .. } | Arm64Instruction::FcvtzsSingleX { .. }
            )),
            "{name} must not saturate at the 64-bit bounds"
        );
    }
    let l2i = make_backend_with_method(0, 0, &[0x0a, 0x88, 0xac]);
    assert!(l2i
        .instructions
        .iter()
        .any(|i| matches!(i, Arm64Instruction::Sxtw { .. })));
    assert!(
        !l2i.instructions.iter().any(|i| matches!(
            i,
            Arm64Instruction::And { .. } | Arm64Instruction::AndImm { .. }
        )),
        "l2i must not zero-extend"
    );
}

/// Every 32-bit int op is its W form followed by `SXTW` of its result, and
/// int compares and zero tests read the W register.
#[test]
pub(in crate::aarch64_backend::tests) fn int_ops_lower_to_w_forms_followed_by_sxtw() {
    for op in [0x60u8, 0x64, 0x68, 0x7e, 0x80, 0x82, 0x78, 0x7a, 0x7c] {
        let r = make_backend_with_method(2, 2, &[0x1a, 0x1b, op, 0xac]);
        assert!(r.success, "0x{op:02x}");
        let ops = &r.instructions;
        let rd = ops
            .iter()
            .find_map(|i| match i {
                Arm64Instruction::AddW { rd, .. }
                | Arm64Instruction::SubW { rd, .. }
                | Arm64Instruction::MulW { rd, .. }
                | Arm64Instruction::AndW { rd, .. }
                | Arm64Instruction::OrrW { rd, .. }
                | Arm64Instruction::EorW { rd, .. }
                | Arm64Instruction::LslW { rd, .. }
                | Arm64Instruction::AsrW { rd, .. }
                | Arm64Instruction::LsrW { rd, .. } => Some(*rd),
                _ => None,
            })
            .unwrap_or_else(|| panic!("0x{op:02x} has no W form"));
        assert!(
            ops.windows(2).any(|pair| matches!(
                (&pair[0], &pair[1]),
                (_, Arm64Instruction::Sxtw { rd: d, rn: s }) if *d == rd && *s == rd
            )),
            "0x{op:02x}: the result is sign-extended"
        );
        assert!(
            !ops.iter().any(|i| matches!(
                i,
                Arm64Instruction::Add { .. }
                    | Arm64Instruction::Sub { .. }
                    | Arm64Instruction::Mul { .. }
                    | Arm64Instruction::And { .. }
                    | Arm64Instruction::Orr { .. }
                    | Arm64Instruction::Eor { .. }
                    | Arm64Instruction::Lsl { .. }
                    | Arm64Instruction::Asr { .. }
                    | Arm64Instruction::Lsr { .. }
            )),
            "0x{op:02x} leaves no 64-bit op behind"
        );
    }
    // iload_0; iload_1; if_icmpeq 6; return; 6: return
    let icmp = make_backend_with_method(2, 2, &[0x1a, 0x1b, 0x9f, 0x00, 0x04, 0xb1, 0xb1]);
    assert!(icmp
        .instructions
        .iter()
        .any(|i| matches!(i, Arm64Instruction::CmpW { .. })));
    // iload_0; ifeq 5; return; 5: return
    let ifeq = make_backend_with_method(1, 1, &[0x1a, 0x99, 0x00, 0x04, 0xb1, 0xb1]);
    assert!(ifeq
        .instructions
        .iter()
        .any(|i| matches!(i, Arm64Instruction::CbzW { .. })));
}

/// A switch's key is never compared with itself, even with every scratch
/// register live, and a dense `tableswitch` is a real jump table.
///
/// The old lowering popped the key and then took each case constant from
/// the scratch allocator, which could hand back the key's own register:
/// `CMP R, R`, always equal.
#[test]
pub(in crate::aarch64_backend::tests) fn a_switch_never_compares_its_key_with_itself() {
    // seven live ints (every scratch register), then the key
    let mut prefix = vec![0x03u8; 7];
    prefix.push(0x1a);
    let mut look = prefix.clone();
    look.extend_from_slice(&[0xab, 0, 0, 0]);
    for word in [28i32, 2, 1, 28, 2, 28] {
        look.extend_from_slice(&word.to_be_bytes());
    }
    look.push(0xac);
    let mut table = prefix;
    table.extend_from_slice(&[0xaa, 0, 0, 0]);
    for word in [28i32, 0, 2, 28, 28, 28] {
        table.extend_from_slice(&word.to_be_bytes());
    }
    table.push(0xac);

    for (name, code) in [("lookupswitch", look), ("tableswitch", table)] {
        assert_eq!(code.len(), 37, "{name}: every case targets pc 36");
        let mut b = Arm64Backend::new();
        let result = b.compile_method(1, 1, 8, &code);
        assert!(result.success, "{name} must compile");
        let bytes = emit_machine_code(&result).unwrap_or_else(|| panic!("{name} must encode"));
        let words: Vec<u32> = bytes
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        for &word in &words {
            // CMP (SUBS ZR, Rn, Rm) with no shift, either width.
            if word & 0x7F20_FC1F == 0x6B00_001F {
                assert_ne!(
                    (word >> 5) & 0x1F,
                    (word >> 16) & 0x1F,
                    "{name}: {word:#010x} compares a register with itself"
                );
            }
        }
        if name == "tableswitch" {
            assert!(
                words.contains(&0xB8B1_5A11),
                "LDRSW X17, [X16, W17, UXTW #2]"
            );
            assert!(words.contains(&0xD61F_0200), "BR X16");
        }
    }
}

/// The prologue builds the standard AAPCS64 frame record and the epilogue
/// tears it down, neither through `MOV` (which reads register 31 as XZR).
#[test]
pub(in crate::aarch64_backend::tests) fn the_prologue_builds_the_standard_frame_record() {
    let result = make_backend_with_method(1, 1, &[0x1a, 0xac]);
    assert!(result.success);
    let ops = &result.instructions;
    assert!(
        matches!(ops[0], Arm64Instruction::StpPre { rt1, rt2, rn, offset: -16 }
                     if rt1 == Arm64Register::FP && rt2 == Arm64Register::LR && rn == Arm64Register::SP)
    );
    assert!(
        matches!(ops[1], Arm64Instruction::AddImm { rd, rn, imm: 0 }
                 if rd == Arm64Register::FP && rn == Arm64Register::SP),
        "FP = SP right after the push: [FP] is the caller's FP and [FP+8] the LR"
    );
    assert!(!ops.iter().any(|i| matches!(i, Arm64Instruction::Mov { rd, rm }
                                         if *rd == Arm64Register::FP || *rd == Arm64Register::SP || *rm == Arm64Register::SP)));
    let ldp = ops
        .iter()
        .position(|i| matches!(i, Arm64Instruction::LdpPost { .. }))
        .expect("epilogue");
    assert!(
        matches!(ops[ldp - 1], Arm64Instruction::AddImm { rd, rn, imm: 0 }
                     if rd == Arm64Register::SP && rn == Arm64Register::FP)
    );

    let bytes = emit_machine_code(&result).expect("encodes");
    let word = |i: usize| u32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap());
    assert_eq!(word(0), 0xA9BF_7BFD, "stp x29, x30, [sp, #-16]!");
    assert_eq!(word(1), 0x9100_03FD, "mov x29, sp (ADD form)");

    assert_eq!(
        Arm64FrameLayout::compute(0, 0, &[Arm64Register::X19]).callee_save_offset,
        -16,
        "the first callee-save pair sits directly below FP"
    );
}

/// `estimated_size` is an upper bound, and a label or comment costs nothing.
#[test]
pub(in crate::aarch64_backend::tests) fn code_buffer_estimated_size_is_an_upper_bound() {
    // sipush 32767; istore_0; iinc 0, 1; iload_0; ireturn
    let result = make_backend_with_method(
        1,
        0,
        &[0x11, 0x7F, 0xFF, 0x3b, 0x84, 0x00, 0x01, 0x1a, 0xac],
    );
    assert!(result.success);
    let mut buf = Arm64CodeBuffer::new();
    for inst in &result.instructions {
        buf.emit(inst.clone());
    }
    let encoded = emit_machine_code(&result).expect("encodes").len();
    assert!(
        buf.estimated_size() >= encoded,
        "{} < {encoded}",
        buf.estimated_size()
    );

    let mut labels_only = Arm64CodeBuffer::new();
    let l = labels_only.new_label();
    labels_only.bind_label(l);
    labels_only.emit(Arm64Instruction::Comment("nothing".into()));
    assert_eq!(labels_only.estimated_size(), 0);
}

/// More float operands than V0-V7 spill by depth, at their own width, and
/// the method still compiles. The float allocator used to round-robin with
/// no liveness check and no spill.
#[test]
pub(in crate::aarch64_backend::tests) fn a_float_stack_deeper_than_v0_to_v7_spills_by_depth() {
    let mut code = vec![0x0cu8; 9]; // nine fconst_1
    code.extend(std::iter::repeat(0x62).take(8)); // eight fadd
    code.push(0xae); // freturn
    let mut b = Arm64Backend::new();
    let result = b.compile_method(0, 0, 9, &code);
    assert!(
        result.success,
        "nine float operands must not exhaust the pool"
    );
    assert!(result.instructions.iter().any(|i| matches!(
        i,
        Arm64Instruction::FpStr { is_double: false, rn, .. } if *rn == Arm64Register::FP
    )));
}

// -----------------------------------------------------------------------
// Round 9 wave 15: the mid-method helper call, and `putfield` on it.
// -----------------------------------------------------------------------

pub(in crate::aarch64_backend::tests) const R9W15_PUTFIELD: usize = 0x7FFF_0000_5000;

/// Compile `code` with the NPE path and the four `jit_putfield_*` slots
/// wired, and `field` resolved at every `getfield`/`putfield` site.
pub(in crate::aarch64_backend::tests) fn compile_field(
    descriptor: &str,
    code: &[u8],
    field: Arm64InstanceField,
    npe: usize,
    putfield: usize,
) -> Arm64CompileResult {
    let mut b = Arm64Backend::new();
    b.set_safepoints_enabled(false);
    // SAFETY: plain struct of `usize` addresses; all-zero is "nothing wired".
    let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    h.jit_npe_with_action = npe;
    h.putfield_int = putfield;
    h.putfield_long = putfield;
    h.putfield_float = putfield;
    h.putfield_double = putfield;
    b.set_helpers(h);
    b.set_method_descriptor(descriptor, true);
    b.set_exception_table_empty(true);
    let mut map = HashMap::new();
    for (pc, _op, _cp) in instance_field_sites(code) {
        map.insert(pc, field);
    }
    b.set_instance_field_info(map);
    let slots = crate::compute_param_jvm_slots(descriptor, true).1;
    b.compile_method(slots, slots, 4, code)
}

/// A spill area with room for `operands` stack entries and nothing else.
pub(in crate::aarch64_backend::tests) fn test_area(operands: usize) -> Arm64SpillArea {
    Arm64SpillArea {
        locals: 0,
        operands,
        safepoint_homes: 0,
        sp_id: 0,
        context: 0,
    }
}

pub(in crate::aarch64_backend::tests) fn int_field(index: usize, tag: u8, is_volatile: bool) -> Arm64InstanceField {
    Arm64InstanceField {
        field_index: index,
        type_tag: tag,
        is_volatile,
    }
}

/// The site walk is an INSTRUCTION walk: a `0xb4` byte that is some other
/// instruction's operand is not a field access, and resolving it would
/// hand the backend a constant-pool index that site never names.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w15_instance_field_sites_walks_instructions_not_bytes() {
    // sipush 0xb400; putfield #7; return  -- the `0xb4` inside the
    // `sipush` operand must not be reported.
    let code = [0x11, 0xb4, 0x00, 0xb5, 0x00, 0x07, 0xb1];
    assert_eq!(
        instance_field_sites(&code),
        vec![(3usize, 0xb5u8, 7u16)],
        "only the real putfield is a site"
    );
}

/// The general call sequence: the operand stack goes to its depth slots,
/// the arguments go to X0.., the call is through X16, and every stored
/// operand comes back into the register the model says it is in.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w15_a_helper_call_spills_the_operand_stack_and_reloads_it() {
    let mut b = Arm64Backend::new();
    b.set_safepoints_enabled(false);
    // A frame with room for four operands. `compile_method` TAKES its
    // frame on the way out, so a test that drives the emitters directly
    // installs its own rather than compiling a throwaway method first.
    b.install_frame(0, test_area(4), &[]);
    // Two live operands, then a call.
    b.emit_iconst(11);
    b.emit_iconst(22);
    let live: Vec<Arm64Register> = b
        .operand_stack
        .iter()
        .map(|o| match o.loc {
            OperandLoc::Reg(r) => r,
            OperandLoc::Slot(_) => panic!("both operands start in registers"),
        })
        .collect();
    // The dispatch loop clears `held` at the start of every bytecode; these
    // tests drive the emitter directly, so they do it themselves.
    b.held.clear();
    let before = b.buffer.instructions().len();
    // THE CALLER allocates the result register, and allocates it BEFORE
    // the call -- see `emit_helper_call`.
    let result = b.alloc_reg(false);
    assert!(
        b.emit_helper_call(
            R9W15_PUTFIELD,
            &[Arm64HelperArg::Imm(7), Arm64HelperArg::Reg(live[0])],
            Some(result),
        ),
        "the call must be accepted"
    );
    assert!(!b.failed);
    let emitted = &b.buffer.instructions()[before..];

    let call_at = emitted
        .iter()
        .position(|i| matches!(i, Arm64Instruction::Blr { rn } if *rn == Arm64Register::X16))
        .expect("the call goes through X16");

    // BEFORE the call: one store per live operand, to its own depth slot.
    for (depth, reg) in live.iter().enumerate() {
        let off = b
            .spill_offset_for_depth(depth)
            .expect("every legal depth has a slot");
        assert!(
            emitted[..call_at].iter().any(|i| matches!(
                i,
                Arm64Instruction::Str { rt, rn, offset }
                    if rt == reg && *rn == Arm64Register::FP && *offset == off
            )),
            "operand at depth {depth} must be stored before the call"
        );
        // AFTER it: the reload, into the SAME register.
        assert!(
            emitted[call_at..].iter().any(|i| matches!(
                i,
                Arm64Instruction::Ldr { rt, rn, offset }
                    if rt == reg && *rn == Arm64Register::FP && *offset == off
            )),
            "operand at depth {depth} must be reloaded after the call"
        );
    }

    // The arguments, in X0.. order.
    assert!(
        emitted[..call_at].iter().any(|i| matches!(
            i,
            Arm64Instruction::MovImm { rd, imm }
                if *rd == Arm64Register::X0 && *imm == 7
        )),
        "the constant argument is materialized into X0"
    );
    assert!(
        emitted[..call_at].iter().any(|i| matches!(
            i,
            Arm64Instruction::Mov { rd, rm }
                if *rd == Arm64Register::X1 && *rm == live[0]
        )),
        "the register argument is moved into X1"
    );
    // The result leaves X0 before the reloads can touch anything.
    let move_out = emitted[call_at..]
        .iter()
        .position(|i| {
            matches!(
                i,
                Arm64Instruction::Mov { rd, rm }
                    if *rd == result && *rm == Arm64Register::X0
            )
        })
        .expect("the result is moved out of X0");
    let first_reload = emitted[call_at..]
        .iter()
        .position(|i| matches!(i, Arm64Instruction::Ldr { .. }))
        .expect("there is a reload");
    assert!(
        move_out < first_reload,
        "the result must leave X0 before the reloads run"
    );
}

/// THE ORDERING RULE THE DOC CALLS OUT: the result register is allocated
/// BEFORE the spill loop, so it can never be one of the registers the
/// reload writes. Allocating it afterwards is the bug this pins — the
/// reload would overwrite the helper's result with an operand, and both
/// are legitimate values of the same width, so nothing downstream could
/// tell.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w15_a_helper_call_never_returns_a_register_the_reload_clobbers() {
    let mut b = Arm64Backend::new();
    b.set_safepoints_enabled(false);
    b.install_frame(0, test_area(8), &[]);
    // Fill the scratch pool, so the result register HAS to come from an
    // occupied one.
    for i in 0..SCRATCH_REGS.len() {
        b.emit_iconst(i32::try_from(i).expect("seven fits"));
    }
    b.held.clear();
    let before = b.buffer.instructions().len();
    // Every scratch register holds an operand, so this allocation MUST
    // spill one -- which is exactly what takes it out of the reload set.
    let result = b.alloc_reg(false);
    assert!(b.emit_helper_call(R9W15_PUTFIELD, &[], Some(result)));
    assert!(!b.failed);
    let emitted = &b.buffer.instructions()[before..];
    let call_at = emitted
        .iter()
        .position(|i| matches!(i, Arm64Instruction::Blr { .. }))
        .expect("the call is emitted");
    assert!(
        !emitted[call_at..]
            .iter()
            .any(|i| matches!(i, Arm64Instruction::Ldr { rt, .. } if *rt == result)),
        "no reload may target the result register"
    );
}

/// An unwired helper is a call through a null pointer, and a source
/// register outside the scratch pool could alias an argument register.
/// Both refuse rather than emit.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w15_a_helper_call_refuses_an_unwired_address_or_a_non_scratch_source() {
    let mut b = Arm64Backend::new();
    b.set_safepoints_enabled(false);
    b.install_frame(0, test_area(4), &[]);
    assert!(!b.emit_helper_call(0, &[], None));
    assert!(b.failed, "address 0 must refuse the method");

    let mut b = Arm64Backend::new();
    b.set_safepoints_enabled(false);
    b.install_frame(0, test_area(4), &[]);
    assert!(!b.emit_helper_call(
        R9W15_PUTFIELD,
        &[Arm64HelperArg::Reg(Arm64Register::X0)],
        None
    ));
    assert!(b.failed, "an argument register as a SOURCE must refuse");

    let mut b = Arm64Backend::new();
    b.set_safepoints_enabled(false);
    b.install_frame(0, test_area(4), &[]);
    let nine = vec![Arm64HelperArg::Imm(0); 9];
    assert!(!b.emit_helper_call(R9W15_PUTFIELD, &nine, None));
    assert!(b.failed, "a ninth argument would go on the stack");

    // A destination the caller did NOT allocate -- one that still holds a
    // live operand -- is the bug the `into` contract exists to prevent:
    // the reload would overwrite the helper's result with that operand.
    let mut b = Arm64Backend::new();
    b.set_safepoints_enabled(false);
    b.install_frame(0, test_area(4), &[]);
    b.emit_iconst(1);
    let OperandLoc::Reg(live) = b.operand_stack[0].loc else {
        panic!("the constant starts in a register")
    };
    b.held.clear();
    assert!(!b.emit_helper_call(R9W15_PUTFIELD, &[], Some(live)));
    assert!(b.failed, "a destination holding a live operand must refuse");
}

/// `putfield` of every primitive tag: the receiver is null-checked with
/// `npe_action::NONE`, the value is narrowed to the field's declared type,
/// a float or double travels to the helper in a GPR, and the three
/// arguments arrive in X0/X1/X2.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w15_putfield_null_checks_narrows_and_calls_the_right_helper() {
    for (tag, descriptor, load, narrow) in [
        (b'I', "(LX;I)V", 0x1bu8, None),
        (b'B', "(LX;I)V", 0x1b, Some("sxtb")),
        (b'S', "(LX;I)V", 0x1b, Some("sxth")),
        (b'C', "(LX;I)V", 0x1b, Some("and 0xffff")),
        (b'Z', "(LX;I)V", 0x1b, Some("and 1")),
        (b'J', "(LX;J)V", 0x1f, None),
        (b'F', "(LX;F)V", 0x23, Some("fmov")),
        (b'D', "(LX;D)V", 0x27, Some("fmov")),
    ] {
        // aload_0; <load_1>; putfield #3; return
        let code = [0x2a, load, 0xb5, 0x00, 0x03, 0xb1];
        let r = compile_field(
            descriptor,
            &code,
            int_field(5, tag, false),
            R9W12_NPE,
            R9W15_PUTFIELD,
        );
        assert!(r.success, "putfield of '{}' must compile", char::from(tag));

        let cbz = r
            .instructions
            .iter()
            .position(|i| matches!(i, Arm64Instruction::Cbz { .. }))
            .expect("the receiver is null-checked");
        let Arm64Instruction::Cbz {
            rt: obj,
            label: npe,
        } = r.instructions[cbz]
        else {
            unreachable!()
        };
        let stub = r
            .instructions
            .iter()
            .position(|i| matches!(i, Arm64Instruction::Label(l) if *l == npe))
            .expect("the NPE label is bound");
        assert!(
            matches!(
                r.instructions[stub + 1],
                Arm64Instruction::MovImm { imm, .. } if imm == i64::from(npe_action::NONE)
            ),
            "'{}': JEP 358 has no field action, so the code is NONE",
            char::from(tag)
        );

        let call = r
            .instructions
            .iter()
            .position(
                |i| matches!(i, Arm64Instruction::Blr { rn } if *rn == Arm64Register::X16),
            )
            .expect("the helper is called");
        assert!(
            cbz < call,
            "'{}': the null check precedes the call, or a null receiver \
             silently drops the store",
            char::from(tag)
        );
        assert!(
            r.instructions[..call].iter().any(|i| matches!(
                i,
                Arm64Instruction::MovImm { rd, imm }
                    if *rd == Arm64Register::X16 && *imm == R9W15_PUTFIELD as i64
            )),
            "'{}': the call target is the wired helper",
            char::from(tag)
        );
        assert!(
            r.instructions[..call].iter().any(|i| matches!(
                i,
                Arm64Instruction::Mov { rd, rm } if *rd == Arm64Register::X0 && *rm == obj
            )),
            "'{}': the receiver is the first argument",
            char::from(tag)
        );
        assert!(
            r.instructions[..call].iter().any(|i| matches!(
                i,
                Arm64Instruction::MovImm { rd, imm } if *rd == Arm64Register::X1 && *imm == 5
            )),
            "'{}': the field index is the second argument",
            char::from(tag)
        );

        // The narrowing, and the FP bit move, between the check and the call.
        let window = &r.instructions[cbz..call];
        let found = match narrow {
            None => true,
            Some("sxtb") => window
                .iter()
                .any(|i| matches!(i, Arm64Instruction::Sxtb { .. })),
            Some("sxth") => window
                .iter()
                .any(|i| matches!(i, Arm64Instruction::Sxth { .. })),
            Some("and 0xffff") => window
                .iter()
                .any(|i| matches!(i, Arm64Instruction::AndImm { imm, .. } if *imm == 0xFFFF)),
            Some("and 1") => window
                .iter()
                .any(|i| matches!(i, Arm64Instruction::AndImm { imm, .. } if *imm == 1)),
            Some("fmov") => window.iter().any(|i| {
                matches!(
                    i,
                    Arm64Instruction::FmovFromFp { .. }
                        | Arm64Instruction::FmovFromFpSingle { .. }
                )
            }),
            Some(other) => panic!("unknown expectation {other}"),
        };
        assert!(
            found,
            "'{}': expected {narrow:?} between the null check and the call",
            char::from(tag)
        );
        assert!(
            emit_machine_code(&r).is_some(),
            "'{}': the body must encode",
            char::from(tag)
        );
    }
}

/// A `volatile` `putfield` fences on BOTH sides, and a plain one on
/// neither. The leading fence is the half x64 does not owe: there the
/// store is a plain `MOV` that TSO already makes a release, while here it
/// happens inside a helper as a relaxed store and a `BLR` orders nothing.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w15_a_volatile_putfield_fences_on_both_sides() {
    let code = [0x2a, 0x1b, 0xb5, 0x00, 0x03, 0xb1];
    for is_volatile in [false, true] {
        let r = compile_field(
            "(LX;I)V",
            &code,
            int_field(1, b'I', is_volatile),
            R9W12_NPE,
            R9W15_PUTFIELD,
        );
        assert!(r.success);
        let call = r
            .instructions
            .iter()
            .position(
                |i| matches!(i, Arm64Instruction::Blr { rn } if *rn == Arm64Register::X16),
            )
            .expect("the helper is called");
        let before = r.instructions[..call]
            .iter()
            .filter(|i| matches!(i, Arm64Instruction::DmbIsh))
            .count();
        let after = r.instructions[call..]
            .iter()
            .filter(|i| matches!(i, Arm64Instruction::DmbIsh))
            .count();
        if is_volatile {
            assert_eq!(before, 1, "the release edge is a leading DMB ISH");
            assert_eq!(after, 1, "the StoreLoad edge is a trailing DMB ISH");
        } else {
            assert_eq!(
                before + after,
                0,
                "a plain field owes no fence -- the JMM says nothing about it"
            );
        }
    }
}

/// A REFERENCE field refuses the method. Its store owes the SATB
/// pre-barrier and the card mark that only `jit_putfield_object` runs, and
/// neither is emitted here; compiling it through `jit_putfield_int` would
/// write a `Value::Int` over a live reference.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w15_putfield_of_a_reference_refuses_the_method() {
    for &tag in b"L[" {
        let code = [0x2a, 0x2b, 0xb5, 0x00, 0x03, 0xb1];
        let r = compile_field(
            "(LX;LY;)V",
            &code,
            int_field(1, tag, false),
            R9W12_NPE,
            R9W15_PUTFIELD,
        );
        assert!(
            !r.success,
            "a '{}' field must refuse: no SATB pre-barrier, no card mark",
            char::from(tag)
        );
    }
}

/// Every "not wired" refusal. An unresolved site, no NPE path, and an
/// unwired `jit_putfield_*` each keep the pre-wave behaviour.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w15_putfield_refuses_every_unwired_shape() {
    let code = [0x2a, 0x1b, 0xb5, 0x00, 0x03, 0xb1];
    // No resolved site: the map is empty.
    let mut b = Arm64Backend::new();
    b.set_safepoints_enabled(false);
    // SAFETY: plain struct of `usize`; all-zero is "nothing wired".
    let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    h.jit_npe_with_action = R9W12_NPE;
    h.putfield_int = R9W15_PUTFIELD;
    b.set_helpers(h);
    b.set_method_descriptor("(LX;I)V", true);
    b.set_exception_table_empty(true);
    let r = b.compile_method(2, 2, 4, &code);
    assert!(!r.success, "an unresolved site must refuse");

    // No exact NPE path (a non-empty exception table).
    let mut b = Arm64Backend::new();
    b.set_safepoints_enabled(false);
    b.set_helpers(h);
    b.set_method_descriptor("(LX;I)V", true);
    b.set_exception_table_empty(false);
    let mut map = HashMap::new();
    map.insert(2usize, int_field(1, b'I', false));
    b.set_instance_field_info(map.clone());
    let r = b.compile_method(2, 2, 4, &code);
    assert!(
        !r.success,
        "without an exact NPE path the store would silently vanish on null"
    );

    // No helper.
    let r = compile_field("(LX;I)V", &code, int_field(1, b'I', false), R9W12_NPE, 0);
    assert!(!r.success, "an unwired jit_putfield_* must refuse");
}

// -----------------------------------------------------------------------
// Round 9 wave 16: the context ABI, and `getfield` on it.
// -----------------------------------------------------------------------

pub(in crate::aarch64_backend::tests) const R9W16_GETFIELD: usize = 0x7FFF_0000_6000;
pub(in crate::aarch64_backend::tests) const R9W16_DISPATCH_THREW: usize = 0x7FFF_0000_7000;

/// Compile `code` with the NPE path, `jit_getfield` and `dispatch_threw`
/// wired, and `field` resolved at every instance-field site.
pub(in crate::aarch64_backend::tests) fn compile_getfield(
    descriptor: &str,
    code: &[u8],
    field: Arm64InstanceField,
    getfield: usize,
    dispatch_threw: usize,
) -> Arm64CompileResult {
    let mut b = Arm64Backend::new();
    b.set_safepoints_enabled(false);
    // SAFETY: plain struct of `usize` addresses; all-zero is "nothing wired".
    let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    h.jit_npe_with_action = R9W12_NPE;
    h.getfield = getfield;
    h.dispatch_threw = dispatch_threw;
    b.set_helpers(h);
    b.set_method_descriptor(descriptor, true);
    b.set_exception_table_empty(true);
    let mut map = HashMap::new();
    for (pc, _op, _cp) in instance_field_sites(code) {
        map.insert(pc, field);
    }
    b.set_instance_field_info(map);
    let slots = crate::compute_param_jvm_slots(descriptor, true).1;
    b.compile_method(slots.max(2), slots, 4, code)
}

/// THE CONTEXT OCCUPIES X0. A body that takes one homes it to its frame
/// word in the prologue and reads its first Java argument from X1; a body
/// that does not is byte-identical to before this wave.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w16_the_context_is_homed_and_shifts_every_argument() {
    // aload_0; getfield #3; ireturn  -- needs a context.
    let with = compile_getfield(
        "(LX;)I",
        &[0x2a, 0xb4, 0x00, 0x03, 0xac],
        int_field(1, b'I', false),
        R9W16_GETFIELD,
        R9W16_DISPATCH_THREW,
    );
    assert!(with.success, "a primitive getfield must compile");
    assert!(
        with.needs_context,
        "the artifact must declare the ABI its prologue was built for"
    );
    // The context store is the FIRST thing to touch X0.
    let ctx_store = with
        .instructions
        .iter()
        .position(|i| {
            matches!(
                i,
                Arm64Instruction::Str { rt, rn, .. }
                    if *rt == Arm64Register::X0 && *rn == Arm64Register::FP
            )
        })
        .expect("the context is homed");
    // ...and the receiver arrives in X1, not X0.
    let homed = with.instructions.iter().position(|i| {
        matches!(
            i,
            Arm64Instruction::Mov { rm, .. } | Arm64Instruction::Str { rt: rm, .. }
                if *rm == Arm64Register::X1
        )
    });
    assert!(
        homed.is_some_and(|at| at > ctx_store),
        "argument 0 homes out of X1, after the context store: {:?}",
        with.instructions
    );
    assert!(
        !with.instructions[ctx_store + 1..]
            .iter()
            .take_while(|i| !matches!(i, Arm64Instruction::Blr { .. }))
            .any(|i| matches!(
                i,
                Arm64Instruction::Mov { rm, .. } if *rm == Arm64Register::X0
            )),
        "nothing may home a Java argument out of X0 when a context is there"
    );

    // A method with no `getfield` takes no context and homes out of X0.
    let without = compile_getfield(
        "(I)I",
        &[0x1a, 0xac],
        int_field(1, b'I', false),
        R9W16_GETFIELD,
        R9W16_DISPATCH_THREW,
    );
    assert!(without.success);
    assert!(
        !without.needs_context,
        "a method with no getfield must keep the plain entry"
    );
    assert!(
        !without.instructions.iter().any(|i| matches!(
            i,
            Arm64Instruction::Str { rt, rn, .. }
                if *rt == Arm64Register::X0 && *rn == Arm64Register::FP
        )),
        "and reserve no context word"
    );
}

/// The frame word: `Arm64SpillArea` puts the context LAST, so adding it
/// moved nothing, and its accessor refuses when none is reserved.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w16_the_context_word_is_last_and_absent_when_unused() {
    let none = Arm64SpillArea {
        locals: 2,
        operands: 3,
        safepoint_homes: 1,
        sp_id: 1,
        context: 0,
    };
    let some = Arm64SpillArea { context: 1, ..none };
    assert_eq!(none.context_word(), None);
    assert_eq!(some.context_word(), Some(2 + 3 + 1 + 1));
    assert_eq!(none.total() + 1, some.total());
    // Every other region is unmoved.
    for a in [none, some] {
        assert_eq!(a.local_word(1), Some(1));
        assert_eq!(a.operand_word(0), Some(2));
        assert_eq!(a.safepoint_home_word(0), Some(5));
        assert_eq!(a.sp_id_word(), Some(6));
    }
}

/// `getfield` of every primitive tag: the context comes out of its frame
/// word, the three arguments arrive in X0/X1/X2, and the result is put in
/// this backend's canonical form for its kind.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w16_getfield_loads_the_context_and_calls_the_helper() {
    for (tag, ret, canonical) in [
        (b'I', 0xacu8, "sxtw"),
        (b'B', 0xac, "sxtw"),
        (b'C', 0xac, "sxtw"),
        (b'J', 0xad, "none"),
        (b'F', 0xae, "fmov"),
        (b'D', 0xaf, "fmov"),
    ] {
        // aload_0; getfield #3; <return>
        let code = [0x2a, 0xb4, 0x00, 0x03, ret];
        let r = compile_getfield(
            "(LX;)V",
            &code,
            int_field(6, tag, false),
            R9W16_GETFIELD,
            R9W16_DISPATCH_THREW,
        );
        assert!(r.success, "getfield of '{}' must compile", char::from(tag));

        let call = r
            .instructions
            .iter()
            .position(|i| {
                matches!(
                    i,
                    Arm64Instruction::MovImm { rd, imm }
                        if *rd == Arm64Register::X16 && *imm == R9W16_GETFIELD as i64
                )
            })
            .expect("the getfield helper is called");
        let before = &r.instructions[..call];
        assert!(
            before
                .iter()
                .any(|i| matches!(i, Arm64Instruction::Cbz { .. })),
            "'{}': the receiver is null-checked first",
            char::from(tag)
        );
        assert!(
            before.iter().any(|i| matches!(
                i,
                Arm64Instruction::Ldr { rn, .. } if *rn == Arm64Register::FP
            )),
            "'{}': the context is read out of its frame word",
            char::from(tag)
        );
        assert!(
            before.iter().any(|i| matches!(
                i,
                Arm64Instruction::MovImm { rd, imm }
                    if *rd == Arm64Register::X2 && *imm == 6
            )),
            "'{}': the field index is the third argument",
            char::from(tag)
        );

        // The sentinel check, on every tag.
        let after = &r.instructions[call..];
        assert!(
            after.iter().any(|i| matches!(
                i,
                Arm64Instruction::MovImm { imm, .. } if *imm == i64::MIN
            )) && after
                .iter()
                .any(|i| matches!(i, Arm64Instruction::Cmp { .. })),
            "'{}': the i64::MIN sentinel must be tested",
            char::from(tag)
        );

        let canonicalized = match canonical {
            "sxtw" => after
                .iter()
                .any(|i| matches!(i, Arm64Instruction::Sxtw { .. })),
            "fmov" => after.iter().any(|i| {
                matches!(
                    i,
                    Arm64Instruction::FmovToFp { .. } | Arm64Instruction::FmovToFpSingle { .. }
                )                }),
            _ => true,
        };
        assert!(
            canonicalized,
            "'{}': the result must reach this backend's canonical form",
            char::from(tag)
        );
        assert!(
            emit_machine_code(&r).is_some(),
            "'{}': the body must encode",
            char::from(tag)
        );
    }
}

/// ONLY `J` and `D` pay for the sentinel ambiguity. A `long` field holding
/// `Long.MIN_VALUE` and a `double` field holding `-0.0` both return
/// exactly `i64::MIN`, so those two must ask `dispatch_threw` whether the
/// signal is real; for every other primitive tag `i64::MIN` cannot be a
/// value, so the second call would be pure cost.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w16_only_long_and_double_ask_dispatch_threw() {
    for (tag, ret, wants) in [
        (b'I', 0xacu8, false),
        (b'F', 0xae, false),
        (b'S', 0xac, false),
        (b'J', 0xad, true),
        (b'D', 0xaf, true),
    ] {
        let code = [0x2a, 0xb4, 0x00, 0x03, ret];
        let r = compile_getfield(
            "(LX;)V",
            &code,
            int_field(0, tag, false),
            R9W16_GETFIELD,
            R9W16_DISPATCH_THREW,
        );
        assert!(r.success);
        let asks = r.instructions.iter().any(|i| {
            matches!(
                i,
                Arm64Instruction::MovImm { rd, imm }
                    if *rd == Arm64Register::X16 && *imm == R9W16_DISPATCH_THREW as i64
            )
        });
        assert_eq!(
            asks,
            wants,
            "'{}': dispatch_threw is owed exactly when i64::MIN is a legal value",
            char::from(tag)
        );
        // ...and when it IS owed, an unwired helper refuses the method
        // rather than reading Long.MIN_VALUE as a pending exception.
        if wants {
            let unwired =
                compile_getfield("(LX;)V", &code, int_field(0, tag, false), R9W16_GETFIELD, 0);
            assert!(
                !unwired.success,
                "'{}': without dispatch_threw the ambiguity cannot be resolved",
                char::from(tag)
            );
        }
    }
}

/// Every "not wired" refusal for `getfield`.
///
/// h23 (2026-09-22): a REFERENCE field is no longer one of them -- see
/// `h23_getfield_lowers_a_reference_field_as_an_oop`. An unrecognised
/// type tag still refuses through the same arm.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w16_getfield_refuses_every_unwired_shape() {
    let code = [0x2a, 0xb4, 0x00, 0x03, 0xac];
    // A tag no descriptor produces.
    let r = compile_getfield(
        "(LX;)I",
        &code,
        int_field(0, b'Q', false),
        R9W16_GETFIELD,
        R9W16_DISPATCH_THREW,
    );
    assert!(!r.success, "an unrecognised type tag must refuse");
    // An unwired `jit_getfield`.
    let r = compile_getfield(
        "(LX;)I",
        &code,
        int_field(0, b'I', false),
        0,
        R9W16_DISPATCH_THREW,
    );
    assert!(!r.success, "an unwired jit_getfield must refuse");

    // No exact NPE path.
    let mut b = Arm64Backend::new();
    b.set_safepoints_enabled(false);
    // SAFETY: plain struct of `usize`; all-zero is "nothing wired".
    let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    h.getfield = R9W16_GETFIELD;
    h.dispatch_threw = R9W16_DISPATCH_THREW;
    b.set_helpers(h);
    b.set_method_descriptor("(LX;)I", true);
    b.set_exception_table_empty(false);
    let mut map = HashMap::new();
    map.insert(1usize, int_field(0, b'I', false));
    b.set_instance_field_info(map);
    let r = b.compile_method(2, 1, 4, &code);
    assert!(!r.success, "without an exact NPE path getfield must refuse");
}

/// h23 (2026-09-22): a REFERENCE `getfield` lowers, through the helper
/// that has always supported one, and lands on the stack as an oop.
///
/// The three things this pins:
///
/// * it COMPILES, for both `L` and `[`, with `dispatch_threw` UNWIRED --
///   a reference is never `i64::MIN`, so it owes no second call, and
///   requiring one would have been a refusal in disguise;
/// * the helper is told to expect a reference
///   (`GETFIELD_EXPECT_REFERENCE` in its third argument, built by the one
///   canonical encoder). Without that bit the helper hands back the
///   payload of whatever `Value` variant the slot holds, and this arm
///   pushes it as an oop -- a wild pointer in the collector's hands;
/// * the result is marked as an oop on the operand model.
#[test]
pub(in crate::aarch64_backend::tests) fn h23_getfield_lowers_a_reference_field_as_an_oop() {
    for &tag in b"L[" {
        let r = compile_getfield(
            "(LX;)Ljava/lang/Object;",
            &[0x2a, 0xb4, 0x00, 0x03, 0xb0],
            int_field(5, tag, false),
            R9W16_GETFIELD,
            // Deliberately UNWIRED: a reference needs no probe.
            0,
        );
        let what = format!("tag={}", char::from(tag));
        assert!(r.success, "{what}: a reference getfield must compile");
        // The third argument carries the slot index AND the flag.
        let want = cratonvm_jit_api::getfield_index_arg(5, true, false, 0) as i64;
        assert!(
            r.instructions.iter().any(|i| matches!(
                i,
                Arm64Instruction::MovImm { imm, .. } if *imm == want
            )),
            "{what}: the encoded index {want:#x} must be materialized"
        );
        // And it is NOT the bare slot index, which is what this site
        // used to build by hand.
        assert!(
            !r.instructions
                .iter()
                .any(|i| matches!(i, Arm64Instruction::MovImm { imm: 5, .. })),
            "{what}: the bare slot index must not be passed"
        );
    }

    // The KIND, proved by what the next instruction is allowed to do with
    // it: `astore_1` pops a `Ref` and fails the compile against anything
    // else, so a body that stores the result into a reference local and
    // returns it compiles only if the push was a reference.
    // aload_0; getfield #3; astore_1; aload_1; areturn
    let stored = compile_getfield(
        "(LX;)Ljava/lang/Object;",
        &[0x2a, 0xb4, 0x00, 0x03, 0x4c, 0x2b, 0xb0],
        int_field(5, b'L', false),
        R9W16_GETFIELD,
        0,
    );
    assert!(
        stored.success,
        "the result must be storable into a reference local"
    );
    // And the MARK follows from the kind by construction, which is the
    // property the arm relies on rather than re-establishes.
    assert!(
        Operand::in_reg(OperandKind::Ref, Arm64Register::X9).oop,
        "a Ref entry is an oop entry"
    );
}

/// h23 (2026-09-22): `getfield`'s HELPER-sentinel edge stamps its
/// throw-site bci, like every other trapping lowering on this backend.
///
/// This edge was missed when `can_throw_npe` was widened from
/// `exception_table_empty` to `can_route_exception` earlier the same day.
/// The inline null check leaves through the bci-keyed NPE stub and was
/// always fine; this one -- the helper's own `i64::MIN`, for a receiver
/// the inline check passed -- left with whatever bci a previous stamp had
/// written. In a method with a `try`/`catch` that is the wrong-handler
/// bug the stamp exists to prevent.
#[test]
pub(in crate::aarch64_backend::tests) fn h23_getfield_stamps_its_throw_bci_on_the_sentinel_edge() {
    const SET_THROW_BCI: usize = 0x7FFF_0000_9100;
    // aload_0; getfield #3 at pc 1; ireturn.
    let code = [0x2a, 0xb4, 0x00, 0x03, 0xac];
    let mut b = Arm64Backend::new();
    b.set_safepoints_enabled(false);
    // SAFETY: plain struct of `usize` addresses; all-zero is "nothing wired".
    let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    h.jit_npe_with_action = R9W12_NPE;
    h.getfield = R9W16_GETFIELD;
    h.set_throw_bci = SET_THROW_BCI;
    b.set_helpers(h);
    b.set_method_descriptor("(LX;)I", true);
    // A NON-EMPTY table: the whole point is that the bci now matters.
    b.set_exception_table_empty(false);
    let mut map = HashMap::new();
    map.insert(1usize, int_field(0, b'I', false));
    b.set_instance_field_info(map);
    let r = b.compile_method(2, 1, 4, &code);
    assert!(
        r.success,
        "with set_throw_bci wired a non-empty table no longer refuses"
    );
    // The stamp: the bci, then the helper address, then a BLR.
    let at = r
        .instructions
        .iter()
        .position(|i| {
            matches!(
                i,
                Arm64Instruction::MovImm { imm, .. } if *imm == SET_THROW_BCI as i64
            )
        })
        .expect("the set_throw_bci address is materialized");
    assert!(
        matches!(
            r.instructions.get(at + 1),
            Some(Arm64Instruction::Blr { .. })
        ),
        "the stamp call follows its address: {:?}",
        r.instructions.get(at + 1)
    );
    assert!(
        matches!(
            r.instructions.get(at - 1),
            Some(Arm64Instruction::MovImm { imm: 1, .. })
        ),
        "the bci stamped is the getfield's own pc: {:?}",
        r.instructions.get(at - 1)
    );
    // With an EMPTY table the stamp is not emitted at all, so an
    // exception-table-free method stays byte-identical to before h23.
    let mut b = Arm64Backend::new();
    b.set_safepoints_enabled(false);
    b.set_helpers(h);
    b.set_method_descriptor("(LX;)I", true);
    b.set_exception_table_empty(true);
    let mut map = HashMap::new();
    map.insert(1usize, int_field(0, b'I', false));
    b.set_instance_field_info(map);
    let empty = b.compile_method(2, 1, 4, &code);
    assert!(empty.success);
    assert!(
        !empty.instructions.iter().any(|i| matches!(
            i,
            Arm64Instruction::MovImm { imm, .. } if *imm == SET_THROW_BCI as i64
        )),
        "an empty table emits no stamp"
    );
}

/// The operand-stack kind analysis is told what each instance field holds.
/// Without that its `0xb4` arm answers `None` from the first `getfield`
/// onwards, so every pc after one is undescribed and `restore_stack_at`
/// cannot rebuild the model at a branch target past it.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w16_the_kind_analysis_is_told_the_field_types() {
    // aload_0; getfield #3 (a long); lconst_0; lcmp; ifeq +5; iconst_1;
    // ireturn; iconst_0; ireturn -- a branch target AFTER the getfield.
    let code = [
        0x2a, 0xb4, 0x00, 0x03, 0x09, 0x94, 0x99, 0x00, 0x05, 0x04, 0xac, 0x03, 0xac,
    ];
    let r = compile_getfield(
        "(LX;)I",
        &code,
        int_field(0, b'J', false),
        R9W16_GETFIELD,
        R9W16_DISPATCH_THREW,
    );
    assert!(
        r.success,
        "a branch after a getfield must still compile: {:?}",
        r.instructions
    );
}

// -----------------------------------------------------------------------
// Round 9 wave 17: `putstatic` through its helper.
// -----------------------------------------------------------------------

pub(in crate::aarch64_backend::tests) const R9W17_PUTSTATIC: usize = 0x7FFF_0000_8000;

/// Compile `code` with the four `jit_putstatic_*` slots wired and `field`
/// resolved at every static site.
pub(in crate::aarch64_backend::tests) fn compile_putstatic(
    descriptor: &str,
    locals: usize,
    code: &[u8],
    field: Arm64StaticField,
    putstatic: usize,
) -> Arm64CompileResult {
    let mut b = Arm64Backend::new();
    b.set_safepoints_enabled(false);
    // SAFETY: plain struct of `usize` addresses; all-zero is "nothing wired".
    let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    h.putstatic_int = putstatic;
    h.putstatic_long = putstatic;
    h.putstatic_float = putstatic;
    h.putstatic_double = putstatic;
    b.set_helpers(h);
    b.set_method_descriptor(descriptor, true);
    b.set_exception_table_empty(true);
    let mut map = HashMap::new();
    for (pc, _op, _cp) in static_field_sites(code) {
        map.insert(pc, field);
    }
    b.set_static_field_info(map);
    let slots = crate::compute_param_jvm_slots(descriptor, true).1;
    b.compile_method(locals, slots, 4, code)
}

/// `putstatic` of every primitive tag: the value is narrowed to the
/// declared type, the context comes out of its frame word, the four
/// arguments arrive in X0..X3, and the `i64::MIN` sentinel is tested.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w17_putstatic_narrows_loads_the_context_and_calls_the_helper() {
    for (tag, descriptor, load, narrow) in [
        (b'I', "(I)V", 0x1au8, None),
        (b'B', "(I)V", 0x1a, Some("sxtb")),
        (b'C', "(I)V", 0x1a, Some("and 0xffff")),
        (b'Z', "(I)V", 0x1a, Some("and 1")),
        (b'J', "(J)V", 0x1e, None),
        (b'F', "(F)V", 0x22, Some("fmov")),
        (b'D', "(D)V", 0x26, Some("fmov")),
    ] {
        // <load_0>; putstatic #3; return
        let code = [load, 0xb3, 0x00, 0x03, 0xb1];
        let r = compile_putstatic(
            descriptor,
            2,
            &code,
            static_field(11, 4, tag, false),
            R9W17_PUTSTATIC,
        );
        assert!(r.success, "putstatic of '{}' must compile", char::from(tag));
        assert!(
            r.needs_context,
            "'{}': jit_putstatic_* takes the VM pointer",
            char::from(tag)
        );
        assert_eq!(
            r.static_init_classes,
            vec![11],
            "'{}': the ensure-initialized obligation is recorded",
            char::from(tag)
        );

        let call = r
            .instructions
            .iter()
            .position(|i| {
                matches!(
                    i,
                    Arm64Instruction::MovImm { rd, imm }
                        if *rd == Arm64Register::X16 && *imm == R9W17_PUTSTATIC as i64
                )
            })
            .expect("the putstatic helper is called");
        let before = &r.instructions[..call];
        assert!(
            before.iter().any(|i| matches!(
                i,
                Arm64Instruction::MovImm { rd, imm }
                    if *rd == Arm64Register::X1 && *imm == 11
            )),
            "'{}': the class id is the second argument",
            char::from(tag)
        );
        assert!(
            before.iter().any(|i| matches!(
                i,
                Arm64Instruction::MovImm { rd, imm }
                    if *rd == Arm64Register::X2 && *imm == 4
            )),
            "'{}': the field index is the third",
            char::from(tag)
        );
        assert!(
            before.iter().any(|i| matches!(
                i,
                Arm64Instruction::Mov { rd, .. } if *rd == Arm64Register::X3
            )),
            "'{}': the value is the fourth",
            char::from(tag)
        );
        let found = match narrow {
            None => true,
            Some("sxtb") => before
                .iter()
                .any(|i| matches!(i, Arm64Instruction::Sxtb { .. })),
            Some("and 0xffff") => before
                .iter()
                .any(|i| matches!(i, Arm64Instruction::AndImm { imm, .. } if *imm == 0xFFFF)),
            Some("and 1") => before
                .iter()
                .any(|i| matches!(i, Arm64Instruction::AndImm { imm, .. } if *imm == 1)),
            Some("fmov") => before.iter().any(|i| {
                matches!(
                    i,
                    Arm64Instruction::FmovFromFp { .. }
                        | Arm64Instruction::FmovFromFpSingle { .. }
                )
            }),
            Some(other) => panic!("unknown expectation {other}"),
        };
        assert!(
            found,
            "'{}': expected {narrow:?} before the call",
            char::from(tag)
        );

        // The sentinel is tested against i64::MIN, not against zero: see
        // `emit_putstatic` for why a `!= 0` test would be the bug.
        let after = &r.instructions[call..];
        assert!(
            after.iter().any(|i| matches!(
                i,
                Arm64Instruction::MovImm { imm, .. } if *imm == i64::MIN
            )) && after
                .iter()
                .any(|i| matches!(i, Arm64Instruction::Cmp { .. })),
            "'{}': the i64::MIN sentinel must be tested",
            char::from(tag)
        );
        assert!(
            emit_machine_code(&r).is_some(),
            "'{}': the body must encode",
            char::from(tag)
        );
    }
}

/// A `volatile` static fences on BOTH sides, and a plain one on neither.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w17_a_volatile_putstatic_fences_on_both_sides() {
    let code = [0x1au8, 0xb3, 0x00, 0x03, 0xb1];
    for is_volatile in [false, true] {
        let r = compile_putstatic(
            "(I)V",
            2,
            &code,
            static_field(1, 0, b'I', is_volatile),
            R9W17_PUTSTATIC,
        );
        assert!(r.success);
        let call = r
            .instructions
            .iter()
            .position(|i| matches!(i, Arm64Instruction::Blr { .. }))
            .expect("the helper is called");
        let before = r.instructions[..call]
            .iter()
            .filter(|i| matches!(i, Arm64Instruction::DmbIsh))
            .count();
        let after = r.instructions[call..]
            .iter()
            .filter(|i| matches!(i, Arm64Instruction::DmbIsh))
            .count();
        if is_volatile {
            assert_eq!(before, 1, "the release edge is a leading DMB ISH");
            assert_eq!(after, 1, "the StoreLoad edge is a trailing DMB ISH");
        } else {
            assert_eq!(before + after, 0, "a plain static owes no fence");            }        }    }

/// Every "not wired" refusal for `putstatic`. The `base_cell` one is the
/// load-bearing case: it is not read by this lowering at all, it is the
/// caller's proof that the declaring class is already initialized, and
/// without it `<clinit>` could run inside the helper -- a safepoint this
/// call sequence records no oop map for.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w17_putstatic_refuses_every_unwired_shape() {
    let code = [0x1au8, 0xb3, 0x00, 0x03, 0xb1];
    // An unwired helper.
    let r = compile_putstatic("(I)V", 2, &code, static_field(1, 0, b'I', false), 0);
    assert!(!r.success, "an unwired jit_putstatic_* must refuse");

    // No `base_cell`: the class is not known to be initialized.
    let mut uninit = static_field(1, 0, b'I', false);
    uninit.base_cell = 0;
    let r = compile_putstatic("(I)V", 2, &code, uninit, R9W17_PUTSTATIC);
    assert!(
        !r.success,
        "without the initialized-class proof, <clinit> could run inside \
         the helper -- and this call records no oop map"
    );

    // A reference static: its store owes the SATB pre-barrier.
    for &tag in b"L[" {
        let r = compile_putstatic(
            "(LX;)V",
            2,
            &[0x2au8, 0xb3, 0x00, 0x03, 0xb1],
            static_field(1, 0, tag, false),
            R9W17_PUTSTATIC,
        );
        assert!(
            !r.success,
            "a '{}' static must refuse: no SATB pre-barrier",
            char::from(tag)
        );
    }

    // An unresolved site.
    let mut b = Arm64Backend::new();
    b.set_safepoints_enabled(false);
    // SAFETY: plain struct of `usize`; all-zero is "nothing wired".
    let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    h.putstatic_int = R9W17_PUTSTATIC;
    b.set_helpers(h);
    b.set_method_descriptor("(I)V", true);
    let r = b.compile_method(2, 1, 4, &code);
    assert!(!r.success, "an unresolved putstatic site must refuse");
}

/// A `getstatic` still lowers with no context at all -- wave 17 gave
/// `putstatic` the context ABI without dragging the read into it.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w17_a_getstatic_only_method_still_takes_no_context() {
    let r = compile_with_statics(
        "()I",
        &[0xb2, 0x00, 0x03, 0xac],
        &[(0, static_field(9, 0, b'I', false))],
    );
    assert!(r.success);
    assert!(
        !r.needs_context,
        "a read needs no VM pointer; only the write does"
    );
}

// -----------------------------------------------------------------------
// Round 9 wave 18: the oop-map-recording call, and allocation on it.
// -----------------------------------------------------------------------

pub(in crate::aarch64_backend::tests) const R9W18_NEW: usize = 0x7FFF_0000_9000;
pub(in crate::aarch64_backend::tests) const R9W18_NEWARRAY: usize = 0x7FFF_0000_A000;
pub(in crate::aarch64_backend::tests) const R9W18_ANEWARRAY: usize = 0x7FFF_0000_B000;

/// Compile `code` with the three allocation helpers wired and `site`
/// resolved at every `new`/`anewarray`.
pub(in crate::aarch64_backend::tests) fn compile_alloc(
    descriptor: &str,
    locals: usize,
    code: &[u8],
    site: Option<Arm64NewSite>,
    table_empty: bool,
) -> Arm64CompileResult {
    let mut b = Arm64Backend::new();
    b.set_safepoints_enabled(false);
    // SAFETY: plain struct of `usize` addresses; all-zero is "nothing wired".
    let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    h.new_object = R9W18_NEW;
    h.newarray = R9W18_NEWARRAY;
    h.anewarray_object = R9W18_ANEWARRAY;
    b.set_helpers(h);
    b.set_method_descriptor(descriptor, true);
    b.set_exception_table_empty(table_empty);
    if let Some(site) = site {
        let mut map = HashMap::new();
        for (pc, op, _operand) in allocation_sites(code) {
            if op != 0xbc {
                map.insert(pc, site);
            }
        }
        b.set_new_site_info(map);
    }
    let slots = crate::compute_param_jvm_slots(descriptor, true).1;
    b.compile_method(locals, slots, 4, code)
}

pub(in crate::aarch64_backend::tests) fn a_new_site() -> Arm64NewSite {
    Arm64NewSite {
        class_id: 33,
        num_fields: 5,
    }
}

/// The site walk reports all three allocation opcodes, with the operand
/// each one actually has -- a constant-pool index for `new`/`anewarray`,
/// the `atype` BYTE for `newarray` -- and decodes instructions, so a
/// `0xbb` inside another instruction's operand is not an allocation.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w18_allocation_sites_report_each_opcodes_own_operand() {
    // sipush 0xbbbc; new #7; newarray T_INT(10); iconst_0; anewarray #9
    let code = [
        0x11, 0xbb, 0xbc, 0xbb, 0x00, 0x07, 0xbc, 0x0a, 0x03, 0xbd, 0x00, 0x09,
    ];
    assert_eq!(
        allocation_sites(&code),
        vec![(3usize, 0xbbu8, 7u16), (6, 0xbc, 10), (9, 0xbd, 9)],
    );
}

/// AN ALLOCATING METHOD GETS A SAFEPOINT FRAME EVEN WITH POLLS OFF.
///
/// The homes and the id word used to be reserved only for
/// `CRATONVM_JIT_ARM64_SAFEPOINTS`. An allocation stops this frame inside
/// a callee that can move objects whether or not polls are on, so it needs
/// the same frame -- otherwise the map has nowhere to name the locals from
/// and `active_safepoint_id` cannot select it.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w18_an_allocating_method_reserves_the_safepoint_frame_without_polls() {
    // new #3; areturn
    let r = compile_alloc(
        "()LX;",
        2,
        &[0xbb, 0x00, 0x03, 0xb0],
        Some(a_new_site()),
        true,
    );
    assert!(r.success, "new must compile");
    assert!(
        r.sp_id_slot_off != 0,
        "an allocating method reserves the safepoint-id word"
    );
    assert!(
        r.needs_context,
        "every allocation helper takes the VM pointer"
    );
    assert_eq!(
        r.safepoint_count, 1,
        "the allocation call is a safepoint and publishes a map"
    );
    assert_eq!(r.pending_oop_maps.len(), 1);

    // ...and a method that allocates nothing still does not.
    let plain = compile_alloc("()I", 2, &[0x03, 0xac], None, true);
    assert!(plain.success);
    assert_eq!(
        plain.sp_id_slot_off, 0,
        "a non-allocating method with polls off is unchanged"
    );
    assert!(!plain.needs_context);
}

/// THE POINT OF THE WAVE: every reference the call could invalidate is in
/// the frame, NAMED, and reloaded afterwards.
///
/// A callee-saved register survives the call by the AAPCS64 contract --
/// but inside the CALLEE's save area, where only a conservative walk sees
/// it, and a conservative walk marks without rewriting. So every store of
/// a callee-saved register to the frame before the call must have a
/// matching load after it; that load is what carries a moved object's new
/// address back.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w18_every_homed_reference_local_is_reloaded_after_the_call() {
    // aload_0; astore_1; new #3; pop; aload_1; areturn
    //   -- local 0 is a reference parameter and local 1 is live ACROSS
    //      the allocation.
    let r = compile_alloc(
        "(LX;)LX;",
        3,
        &[0x2a, 0x4c, 0xbb, 0x00, 0x03, 0x57, 0x2b, 0xb0],
        Some(a_new_site()),
        true,
    );
    assert!(r.success, "{:?}", r.instructions);
    let call = r
        .instructions
        .iter()
        .position(|i| {
            matches!(
                i,
                Arm64Instruction::MovImm { rd, imm }
                    if *rd == Arm64Register::X16 && *imm == R9W18_NEW as i64
            )
        })
        .expect("the allocation helper is called");
    let blr = call
        + r.instructions[call..]
            .iter()
            .position(|i| matches!(i, Arm64Instruction::Blr { .. }))
            .expect("the call is emitted");

    // The lowering begins with the context load; everything before that
    // (the prologue's own callee-saved save, the argument homing) is not
    // part of this call's sequence.
    let lowering = r.instructions[..blr]
        .iter()
        .rposition(|i| {
            matches!(
                i,
                Arm64Instruction::Ldr { rt, rn, .. }
                    if *rn == Arm64Register::FP && SCRATCH_REGS.contains(rt)
            )
        })
        .expect("the context is read out of its frame word");
    let homed: Vec<(Arm64Register, i32)> = r.instructions[lowering..blr]
        .iter()
        .filter_map(|i| match i {
            Arm64Instruction::Str { rt, rn, offset }
                if *rn == Arm64Register::FP && rt.is_callee_saved() =>
            {
                Some((*rt, *offset))                }
            _ => None,
        })
        .collect();
    assert!(
        !homed.is_empty(),
        "a reference local must be homed before the call: {:?}",
        r.instructions
    );
    for (reg, off) in &homed {
        assert!(
            r.instructions[blr..].iter().any(|i| matches!(
                i,
                Arm64Instruction::Ldr { rt, rn, offset }
                    if rt == reg && *rn == Arm64Register::FP && offset == off
            )),
            "{reg:?} homed at {off} must be RELOADED after the call -- \
             otherwise a moved object's new address never reaches it"
        );
    }

    // ...and the map names those very offsets.
    let map = &r.pending_oop_maps[0];
    for (_, off) in &homed {
        // Cast: a frame offset, which the map carries as an i16.
        assert!(
            map.frame_slot_offsets.contains(&(*off as i16)),
            "the map must name the home at {off}, or the collector cannot \
             rewrite it: {:?}",
            map.frame_slot_offsets
        );
    }
    assert_eq!(
        map.safepoint_id, 2,
        "the map is keyed by the allocation's own bci"
    );
}

/// A reference OPERAND live across the allocation is spilled to its depth
/// slot and named in the map too -- the operand half of the same rule.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w18_a_reference_operand_live_across_an_allocation_is_in_the_map() {
    // aload_0; new #3; swap; pop; areturn
    //   -- the parameter is on the STACK, under the new object.
    let r = compile_alloc(
        "(LX;)LX;",
        2,
        &[0x2a, 0xbb, 0x00, 0x03, 0x5f, 0x57, 0xb0],
        Some(a_new_site()),
        true,
    );
    assert!(r.success, "{:?}", r.instructions);
    let blr = r
        .instructions
        .iter()
        .position(|i| matches!(i, Arm64Instruction::Blr { .. }))
        .expect("the allocation helper is called");
    // An OPERAND lives in the scratch pool; the prologue's callee-saved
    // save and the context store also write FP-relative words, and neither
    // is an operand.
    let spills: Vec<i32> = r.instructions[..blr]
        .iter()
        .filter_map(|i| match i {
            Arm64Instruction::Str { rt, rn, offset }
                if *rn == Arm64Register::FP && SCRATCH_REGS.contains(rt) =>
            {
                Some(*offset)                }
            _ => None,
        })
        .collect();
    assert_eq!(
        spills.len(),
        1,
        "exactly one operand is live across the call: {:?}",
        r.instructions
    );
    // Cast: a frame offset, as the map carries it.
    assert!(
        r.pending_oop_maps[0]
            .frame_slot_offsets
            .contains(&(spills[0] as i16)),
        "the reference operand's slot must be in the map: {:?}",
        r.pending_oop_maps[0].frame_slot_offsets
    );
    assert!(
        r.instructions[blr..].iter().any(|i| matches!(
            i,
            Arm64Instruction::Ldr { rn, offset, .. }
                if *rn == Arm64Register::FP && *offset == spills[0]
        )),
        "and it must be reloaded afterwards"
    );
}

/// The three allocation opcodes, their helpers and their arguments.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w18_each_allocation_opcode_calls_its_own_helper() {
    for (name, code, locals, descriptor, helper, second) in [
        (
            "new",
            vec![0xbbu8, 0x00, 0x03, 0xb0],
            2,
            "()LX;",
            R9W18_NEW,
            33i64,
        ),
        (
            "newarray",
            vec![0x05, 0xbc, 0x0a, 0xb0],
            2,
            "()LX;",
            R9W18_NEWARRAY,
            10,
        ),
        (
            "anewarray",
            vec![0x05, 0xbd, 0x00, 0x03, 0xb0],
            2,
            "()LX;",
            R9W18_ANEWARRAY,
            33,
        ),
    ] {
        let r = compile_alloc(descriptor, locals, &code, Some(a_new_site()), true);
        assert!(r.success, "{name} must compile: {:?}", r.instructions);
        let call = r
            .instructions
            .iter()
            .position(|i| {
                matches!(
                    i,
                    Arm64Instruction::MovImm { rd, imm }
                        if *rd == Arm64Register::X16 && *imm == helper as i64
                )
            })
            .unwrap_or_else(|| panic!("{name}: its own helper must be called"));
        let before = &r.instructions[..call];
        assert!(
            before.iter().any(|i| matches!(
                i,
                Arm64Instruction::Mov { rd, .. } if *rd == Arm64Register::X0
            )),
            "{name}: the context is the first argument"
        );
        assert!(
            before.iter().any(|i| matches!(
                i,
                Arm64Instruction::MovImm { rd, imm }
                    if *rd == Arm64Register::X1 && *imm == second
            )),
            "{name}: the class id (or atype) is the second argument"
        );
        // The null-result check, and the reference push.
        assert!(
            r.instructions[call..]
                .iter()
                .any(|i| matches!(i, Arm64Instruction::Cbnz { .. })),
            "{name}: a null result must be checked"
        );
        assert!(
            emit_machine_code(&r).is_some(),
            "{name}: the body must encode"
        );
    }
}

/// Every "not wired" refusal for an allocation. The exception-table one is
/// the exactness condition: the helper's OOM is drained with an unknown
/// pc, so a handler in this frame could catch the wrong thing.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w18_allocation_refuses_every_unwired_shape() {
    let code = [0xbbu8, 0x00, 0x03, 0xb0];
    // An unresolved site.
    let r = compile_alloc("()LX;", 2, &code, None, true);
    assert!(!r.success, "an unresolved new site must refuse");

    // A non-empty exception table.
    let r = compile_alloc("()LX;", 2, &code, Some(a_new_site()), false);
    assert!(
        !r.success,
        "with a handler in this frame the drain could catch the wrong thing"
    );

    // An unwired helper.
    let mut b = Arm64Backend::new();
    b.set_safepoints_enabled(false);
    b.set_method_descriptor("()LX;", true);
    b.set_exception_table_empty(true);
    let mut map = HashMap::new();
    map.insert(0usize, a_new_site());
    b.set_new_site_info(map);
    let r = b.compile_method(2, 0, 4, &code);
    assert!(!r.success, "an unwired jit_new_object must refuse");
}

// -----------------------------------------------------------------------
// Round 9 wave 21: the frame base an allocating method never published.
// -----------------------------------------------------------------------

pub(in crate::aarch64_backend::tests) const R9W21_FRAME_RECORD: usize = 0x7FFF_0000_C000;

/// [`compile_alloc`] with `helpers.frame_record` wired as well, so the
/// prologue can publish this frame's base.
pub(in crate::aarch64_backend::tests) fn compile_alloc_with_frame_record(
    code: &[u8],
    site: Option<Arm64NewSite>,
    frame_record: usize,
    polls: bool,
) -> Arm64CompileResult {
    let mut b = Arm64Backend::new();
    b.set_safepoints_enabled(polls);
    // SAFETY: plain struct of `usize` addresses; all-zero is "nothing wired".
    let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    h.new_object = R9W18_NEW;
    h.newarray = R9W18_NEWARRAY;
    h.anewarray_object = R9W18_ANEWARRAY;
    h.frame_record = frame_record;
    b.set_helpers(h);
    b.set_method_descriptor("()LX;", true);
    b.set_exception_table_empty(true);
    if let Some(site) = site {
        let mut map = HashMap::new();
        for (pc, op, _operand) in allocation_sites(code) {
            if op != 0xbc {
                map.insert(pc, site);
            }
        }
        b.set_new_site_info(map);
    }
    b.compile_method(2, 0, 4, code)    }

/// How many times `code` calls `frame_record`: the three-instruction
/// `MOV X0, FP; MOVZ/MOVK X16, #helper; BLR X16` shape, counted by the
/// `MovImm` that names the helper.
pub(in crate::aarch64_backend::tests) fn frame_record_calls(r: &Arm64CompileResult, helper: usize) -> usize {
    r.instructions
        .windows(2)
        .filter(|w| {
            matches!(
                w[0],
                Arm64Instruction::MovImm {
                    rd: Arm64Register::X16,
                    imm
                } if imm == helper as i64
            ) && matches!(
                w[1],
                Arm64Instruction::Blr {
                    rn: Arm64Register::X16                    }
            )
        })
        .count()    }

// -----------------------------------------------------------------------
// Round 9 wave 22: the first bytecode CALL this backend compiles.
// -----------------------------------------------------------------------

pub(in crate::aarch64_backend::tests) const R9W22_DISPATCH: usize = 0x7FFF_0000_D000;
pub(in crate::aarch64_backend::tests) const R9W22_THREW: usize = 0x7FFF_0000_E000;
pub(in crate::aarch64_backend::tests) const R9W22_INFO: u64 = 0x7FFF_0000_F000;

pub(in crate::aarch64_backend::tests) fn an_invoke_site(num_jit_args: usize, return_type: u8) -> Arm64InvokeSite {
    Arm64InvokeSite {
        info_ptr: R9W22_INFO,
        num_jit_args,
        return_type,
    }
}

/// Compile `code` with `jit_invoke_dispatch` wired and every `invoke*`
/// site resolved to `site`.
pub(in crate::aarch64_backend::tests) fn compile_invoke(
    descriptor: &str,
    locals: usize,
    code: &[u8],
    site: Option<Arm64InvokeSite>,
    table_empty: bool,
    dispatch: usize,
) -> Arm64CompileResult {
    let mut b = Arm64Backend::new();
    b.set_safepoints_enabled(false);
    // SAFETY: plain struct of `usize` addresses; all-zero is "nothing wired".
    let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    h.invoke_dispatch = dispatch;
    h.dispatch_threw = R9W22_THREW;
    h.frame_record = R9W21_FRAME_RECORD;
    b.set_helpers(h);
    b.set_method_descriptor(descriptor, true);
    b.set_exception_table_empty(table_empty);
    if let Some(site) = site {
        let mut map = HashMap::new();
        for (pc, op, _cp) in invoke_sites(code) {
            if op != 0xba {
                map.insert(pc, site);
            }
        }
        b.set_invoke_site_info(map);
    }
    let slots = crate::compute_param_jvm_slots(descriptor, true).1;
    b.compile_method(locals, slots, 4, code)
}

/// The site walk decodes all three instruction WIDTHS.
///
/// `invokeinterface` and `invokedynamic` are five bytes, not three. A
/// three-byte stride reads their trailing bytes as opcodes -- and
/// `invokeinterface`'s count byte is an ARGUMENT COUNT, so for a
/// one-argument call it reads as `0x01`, `iconst_m1`.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w22_invoke_sites_decode_all_five_widths() {
    let code = [
        0x2a, // aload_0
        0xb6, 0x00, 0x07, // invokevirtual #7
        0xb7, 0x00, 0x08, // invokespecial #8
        0xb8, 0x00, 0x09, // invokestatic #9
        0xb9, 0x00, 0x0a, 0x01, 0x00, // invokeinterface #10, 1 arg
        0xba, 0x00, 0x0b, 0x00, 0x00, // invokedynamic #11
        0xb1, // return
    ];
    assert_eq!(
        invoke_sites(&code),
        vec![
            (1usize, 0xb6u8, 7u16),
            (4, 0xb7, 8),
            (7, 0xb8, 9),
            (10, 0xb9, 10),
            (15, 0xba, 11),
        ],
    );
}

/// Every "not wired" refusal for a call, and the one that is not about
/// wiring at all.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w22_an_invoke_refuses_every_unwired_shape() {
    // iload_0; iload_1; invokestatic #1; ireturn
    let code = [0x1au8, 0x1b, 0xb8, 0x00, 0x01, 0xac];
    let site = an_invoke_site(2, b'I');

    let ok = compile_invoke("(II)I", 2, &code, Some(site), true, R9W22_DISPATCH);
    assert!(ok.success, "the wired shape must compile");

    let r = compile_invoke("(II)I", 2, &code, None, true, R9W22_DISPATCH);
    assert!(!r.success, "an unresolved site must refuse");

    let r = compile_invoke("(II)I", 2, &code, Some(site), true, 0);
    assert!(!r.success, "an unwired jit_invoke_dispatch must refuse");

    // The exactness condition, and the one that keeps `synchronized` out:
    // the callee's exception leaves through the epilogue and the drain
    // raises it with an unknown pc, so a handler in THIS frame could catch
    // the wrong thing.
    let r = compile_invoke("(II)I", 2, &code, Some(site), false, R9W22_DISPATCH);
    assert!(
        !r.success,
        "with a handler in this frame the drain could catch the wrong thing"
    );

    // ...and `invokedynamic`, which no wiring can rescue: the helper's
    // `invoke_kind` has no value for a call site that is not a class.
    let indy = [0x1au8, 0xba, 0x00, 0x01, 0x00, 0x00, 0xac];
    let r = compile_invoke(
        "(I)I",
        2,
        &indy,
        Some(an_invoke_site(1, b'I')),
        true,
        R9W22_DISPATCH,
    );
    assert!(!r.success, "invokedynamic has no lowering here");
}

/// A method that calls takes the VM context and a SAFEPOINT FRAME, with
/// polls off -- because a callee can be stopped inside anything at all.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w22_a_calling_method_takes_the_context_and_a_safepoint_frame() {
    let code = [0x1au8, 0x1b, 0xb8, 0x00, 0x01, 0xac];
    let r = compile_invoke(
        "(II)I",
        2,
        &code,
        Some(an_invoke_site(2, b'I')),
        true,
        R9W22_DISPATCH,
    );
    assert!(r.success);
    assert!(!r.polls_enabled, "polls are off; the CALL is the safepoint");
    assert!(r.needs_context, "jit_invoke_dispatch takes the VM pointer");
    assert_ne!(r.sp_id_slot_off, 0, "the id word is reserved");
    assert!(r.frame_base_published, "and the base to read it through");
    assert_eq!(r.safepoint_count, 1, "the call is the one safepoint");
    assert_eq!(r.pending_oop_maps.len(), 1);
    let cm = publish_compiled_method(&r).expect("publishes");
    assert!(cm.needs_context());
    assert!(
        cm.fully_oop_covered,
        "one mapped safepoint, an id and a base"
    );
}

/// THE ARGUMENT BUFFER IS THE OPERAND AREA, and a reference argument is
/// therefore NAMED in the call's map.
///
/// This is the property that makes the aarch64 lowering cheaper than
/// x64's, not merely different. There, an outgoing reference argument is
/// marshalled into a staging area no frame-slot map describes, so the site
/// raises `pending_staged_args_unmapped` and the method loses
/// `fully_oop_covered` for good. Here the arguments are still operand
/// entries when the map is taken, so each reference among them is named
/// where it already lies -- and a collector that moves one during the
/// callee rewrites the very word the helper reads.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w22_a_reference_argument_is_named_in_the_calls_map() {
    // aload_0; invokestatic #1 (Ljava/lang/Object;)I; ireturn
    let code = [0x2au8, 0xb8, 0x00, 0x01, 0xac];
    let r = compile_invoke(
        "(Ljava/lang/Object;)I",
        2,
        &code,
        Some(an_invoke_site(1, b'I')),
        true,
        R9W22_DISPATCH,
    );
    assert!(r.success, "a reference argument must compile");
    assert_eq!(r.pending_oop_maps.len(), 1);
    assert!(
        !r.pending_oop_maps[0].frame_slot_offsets.is_empty(),
        "the reference argument must be named at the call: map={:?}",
        r.pending_oop_maps[0]
    );
    // ...and there is exactly ONE argument buffer address computed, off FP.
    let bases = r
        .instructions
        .iter()
        .filter(
            |i| matches!(i, Arm64Instruction::SubImm { rn, .. } if *rn == Arm64Register::FP),
        )
        .count();
    assert_eq!(bases, 1, "one call, one buffer address");
}

// -----------------------------------------------------------------------
// Round 9 wave 23: checkcast and instanceof.
// -----------------------------------------------------------------------

pub(in crate::aarch64_backend::tests) const R9W23_CHECKCAST: usize = 0x7FFF_0001_0000;
pub(in crate::aarch64_backend::tests) const R9W23_INSTANCEOF: usize = 0x7FFF_0001_1000;
/// A stand-in for the interned class-name bytes. Never dereferenced by
/// anything these tests run; the fake helpers ignore it.
pub(in crate::aarch64_backend::tests) const R9W23_NAME: u64 = 0x7FFF_0001_2000;

pub(in crate::aarch64_backend::tests) fn compile_typecheck(
    descriptor: &str,
    code: &[u8],
    site: Option<Arm64TypecheckSite>,
    table_empty: bool,
    checkcast: usize,
    instanceof: usize,
) -> Arm64CompileResult {
    let mut b = Arm64Backend::new();
    b.set_safepoints_enabled(false);
    // SAFETY: plain struct of `usize` addresses; all-zero is "nothing wired".
    let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    h.checkcast = checkcast;
    h.instanceof_check = instanceof;
    h.frame_record = R9W21_FRAME_RECORD;
    b.set_helpers(h);
    b.set_method_descriptor(descriptor, true);
    b.set_exception_table_empty(table_empty);
    if let Some(site) = site {
        let mut map = HashMap::new();
        for (pc, _op, _cp) in typecheck_sites(code) {
            map.insert(pc, site);
        }
        b.set_typecheck_site_info(map);
    }
    let slots = crate::compute_param_jvm_slots(descriptor, true).1;
    b.compile_method(2, slots, 4, code)
}

pub(in crate::aarch64_backend::tests) fn a_typecheck_site() -> Arm64TypecheckSite {
    Arm64TypecheckSite {
        name_ptr: R9W23_NAME,
        name_len: 16,
    }
}

/// The site walk, and that it decodes instructions rather than bytes.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w23_typecheck_sites_are_walked() {
    // sipush 0xc0c1; aload_0; checkcast #7; instanceof #9; ireturn
    let code = [
        0x11, 0xc0, 0xc1, 0x2a, 0xc0, 0x00, 0x07, 0xc1, 0x00, 0x09, 0xac,
    ];
    assert_eq!(
        typecheck_sites(&code),
        vec![(4usize, 0xc0u8, 7u16), (7, 0xc1, 9)],
    );
}

/// Every refusal a type check has, including the one that is not wiring.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w23_a_typecheck_refuses_every_unwired_shape() {
    // aload_0; checkcast #1; areturn
    let code = [0x2au8, 0xc0, 0x00, 0x01, 0xb0];
    let d = "(Ljava/lang/Object;)Ljava/lang/Object;";

    let ok = compile_typecheck(d, &code, Some(a_typecheck_site()), true, R9W23_CHECKCAST, 0);
    assert!(ok.success, "the wired shape must compile");
    assert!(ok.needs_context, "the helper takes the VM pointer");
    assert_eq!(ok.safepoint_count, 1, "resolving the target can allocate");

    let r = compile_typecheck(d, &code, None, true, R9W23_CHECKCAST, 0);
    assert!(!r.success, "an unresolved target must refuse");

    let r = compile_typecheck(d, &code, Some(a_typecheck_site()), true, 0, 0);
    assert!(!r.success, "an unwired jit_checkcast must refuse");

    let r = compile_typecheck(
        d,
        &code,
        Some(a_typecheck_site()),
        false,
        R9W23_CHECKCAST,
        0,
    );
    assert!(
        !r.success,
        "a failed cast returns the sentinel with a CCE pending, which the \
         drain raises with an unknown pc"
    );

    // A resolved-looking site with a ZERO length is the trap: the helper
    // answers "not an instance" for every object, which is a wrong answer
    // rather than a missing one.
    let empty = Arm64TypecheckSite {
        name_ptr: R9W23_NAME,
        name_len: 0,
    };
    let r = compile_typecheck(d, &code, Some(empty), true, R9W23_CHECKCAST, 0);
    assert!(
        !r.success,
        "an empty target name must refuse, not be passed on"
    );
}

/// The object stays on the operand stack across the call, so it is NAMED.
///
/// Resolving the target class on first use allocates its `java/lang/Class`
/// mirror, so this call can genuinely move the very object being tested.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w23_the_tested_object_is_named_in_the_calls_map() {
    let code = [0x2au8, 0xc1, 0x00, 0x01, 0xac];
    let r = compile_typecheck(
        "(Ljava/lang/Object;)I",
        &code,
        Some(a_typecheck_site()),
        true,
        0,
        R9W23_INSTANCEOF,
    );
    assert!(r.success);
    assert_eq!(r.pending_oop_maps.len(), 1);
    assert!(
        !r.pending_oop_maps[0].frame_slot_offsets.is_empty(),
        "the object under test must be named at the call: map={:?}",
        r.pending_oop_maps[0]
    );
}

/// WAVE 18 SHIPPED WITHOUT THIS. It gave an allocating method the
/// safepoint homes and the id word on the argument that an allocation
/// stops the frame inside a moving callee whether or not polls are on --
/// and left `emit_frame_record` gated on polls.
///
/// The consequence is that same argument's own failure mode one level up.
/// The runtime reads the id at `[frame_base - sp_id_slot_off]` and learns
/// `frame_base` only from this call, so with no frame record
/// `PreciseFrameInfo::exact_rbp` stays `0`, every precise path declines
/// the frame, and the map the allocation had just recorded could never be
/// selected -- exactly what reserving the id word was meant to prevent.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w21_an_allocating_method_publishes_its_frame_base() {
    // nop; new #3; areturn
    let code = [0x00u8, 0xbb, 0x00, 0x03, 0xb0];
    let r =
        compile_alloc_with_frame_record(&code, Some(a_new_site()), R9W21_FRAME_RECORD, false);
    assert!(r.success, "new must compile");
    assert!(!r.polls_enabled, "this compilation emits no polls");
    assert_eq!(
        frame_record_calls(&r, R9W21_FRAME_RECORD),
        1,
        "an allocating method publishes its frame base exactly once"
    );
    assert!(r.frame_base_published);

    // A method that allocates nothing still publishes nothing with polls
    // off: it can be stopped nowhere, so there is no frame to locate.
    let plain =
        compile_alloc_with_frame_record(&[0x01u8, 0xb0], None, R9W21_FRAME_RECORD, false);
    assert!(plain.success);
    assert_eq!(frame_record_calls(&plain, R9W21_FRAME_RECORD), 0);
    assert!(!plain.frame_base_published);
}

/// A PUBLISHED FRAME SIZE, without which nothing walks this frame at all.
///
/// The third of wave 21's "published and unreadable" family, and the one
/// that would have made arming the oracle pointless on its own: every
/// walker bounds a compiled frame by `[rbp - osr_frame_size, rbp)` and
/// refuses it outright when the field is `0`, which is what this backend
/// published for its whole existence.
///
/// The value is FP-to-SP, not the whole frame: the prologue's
/// `STP X29, X30, [SP, #-16]!` puts FP sixteen bytes below the old SP and
/// `SUB SP, SP, #(frame_size - 16)` is the rest, so `frame_size` itself
/// would put the band's floor sixteen bytes inside the CALLEE's frame.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w21_the_frame_size_the_gc_walks_this_frame_by_is_published() {
    let code = [0x00u8, 0xbb, 0x00, 0x03, 0xb0];
    let r =
        compile_alloc_with_frame_record(&code, Some(a_new_site()), R9W21_FRAME_RECORD, false);
    assert!(r.success);
    let cm = publish_compiled_method(&r).expect("publishes");
    assert_eq!(
        cm.osr_frame_size,
        r.frame.frame_size - 16,
        "the band is FP down to SP, which is what the prologue subtracts"
    );
    assert!(cm.osr_frame_size > 0, "a zero refuses the frame entirely");
    // ...and it reaches the deepest word the maps can name: slot 0 lives
    // at `[FP + spill_offset]`, so the band's floor must be at or below it.
    assert!(
        cm.osr_frame_size >= -r.frame.spill_offset,
        "the band must contain the spill area the oop maps describe: \
         band={} deepest_spill={}",
        cm.osr_frame_size,
        -r.frame.spill_offset
    );
}

/// ...and now that the base is published, the claim is worth making.
///
/// Wave 18 withheld `fully_oop_covered` from an allocation-only method and
/// said why: the strongest claim the backend makes, widened in no hurry.
/// The argument for widening was already written there -- with polls off
/// the frame can be stopped only inside a call, and every call this
/// backend emits either records a map or carries a reason it cannot
/// safepoint -- and what was missing was the frame base to read the maps
/// through.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w21_an_allocation_only_method_now_claims_full_oop_coverage() {
    let code = [0x00u8, 0xbb, 0x00, 0x03, 0xb0];
    let r =
        compile_alloc_with_frame_record(&code, Some(a_new_site()), R9W21_FRAME_RECORD, false);
    assert!(r.success);
    assert!(!r.polls_enabled, "the allocation route, not the poll route");
    let cm = publish_compiled_method(&r).expect("publishes");
    assert_eq!(cm.oop_maps.len(), 1);
    assert!(
        cm.fully_oop_covered,
        "one safepoint, one map, an id word and a published base"
    );
}

/// The fifth term, alone. Polls ON and everything else wired, but no
/// `frame_record` helper -- which is the VM's state under
/// `CRATONVM_NO_PRECISE_JIT_MAPS=1` with no moving young generation.
///
/// The maps are still published, because they cost nothing and the
/// walker's precise scan is strictly additive. The CLAIM is not, because
/// nothing can locate the frame to read them from, and a coverage bit over
/// unreadable maps is the vacuous proof
/// `bug-oop-map-coverage-bit-is-presence-not-completeness-20260820-FIXED.md`
/// is about.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w21_without_a_frame_record_there_is_no_coverage_claim() {
    let code = [0x00u8, 0xbb, 0x00, 0x03, 0xb0];
    let r = compile_alloc_with_frame_record(&code, Some(a_new_site()), 0, true);
    assert!(r.success);
    assert!(r.polls_enabled);
    assert!(!r.frame_base_published, "the helper slot is 0");
    let cm = publish_compiled_method(&r).expect("publishes");
    assert!(
        !cm.oop_maps.is_empty(),
        "the maps are published either way -- the precise scan is additive"
    );
    assert!(
        !cm.fully_oop_covered,
        "no frame base means no way to read them, so no claim"
    );
}

/// WAVE 17 SHIPPED WITHOUT THIS. `putstatic`'s sentinel path leaves
/// through the epilogue for the drain to raise, and the drain throws with
/// an unknown pc -- so it needs an empty exception table, exactly like
/// every other lowering here that can trap. The `<clinit>` branch the wave
/// argues is unreachable is not the only way the helper returns the
/// sentinel: `contain(.., OnPanic::Throw, i64::MIN, ..)` returns it for a
/// panic as well.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w18_putstatic_refuses_with_a_non_empty_exception_table() {
    let code = [0x1au8, 0xb3, 0x00, 0x03, 0xb1];
    let mut b = Arm64Backend::new();
    b.set_safepoints_enabled(false);
    // SAFETY: plain struct of `usize`; all-zero is "nothing wired".
    let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    h.putstatic_int = R9W17_PUTSTATIC;
    b.set_helpers(h);
    b.set_method_descriptor("(I)V", true);
    b.set_exception_table_empty(false);
    let mut map = HashMap::new();
    map.insert(1usize, static_field(1, 0, b'I', false));
    b.set_static_field_info(map);
    let r = b.compile_method(2, 1, 4, &code);
    assert!(
        !r.success,
        "a putstatic in a method with a handler must refuse"
    );
}

// -----------------------------------------------------------------------
// Round 9 wave 19: the reference stores, and the reference array element.
// -----------------------------------------------------------------------

pub(in crate::aarch64_backend::tests) const R9W19_PUTFIELD_OBJ: usize = 0x7FFF_0000_C000;
pub(in crate::aarch64_backend::tests) const R9W19_PUTSTATIC_OBJ: usize = 0x7FFF_0000_D000;
pub(in crate::aarch64_backend::tests) const R9W19_AALOAD: usize = 0x7FFF_0000_E000;
pub(in crate::aarch64_backend::tests) const R9W19_AASTORE: usize = 0x7FFF_0000_F000;
pub(in crate::aarch64_backend::tests) const R9W19_TYPE_CHECK: usize = 0x7FFF_0001_0000;

/// Every helper this wave needs, wired at once.
pub(in crate::aarch64_backend::tests) fn r9w19_helpers() -> crate::JitRuntimeHelpers {
    // SAFETY: plain struct of `usize` addresses; all-zero is "nothing wired".
    let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    h.jit_npe_with_action = R9W12_NPE;
    h.throw_aioobe = R9W13_AIOOBE;
    h.putfield_object = R9W19_PUTFIELD_OBJ;
    h.putstatic_object = R9W19_PUTSTATIC_OBJ;
    h.aaload = R9W19_AALOAD;
    h.aastore = R9W19_AASTORE;
    h.aastore_type_check = R9W19_TYPE_CHECK;
    h
}

pub(in crate::aarch64_backend::tests) fn compile_refs(
    descriptor: &str,
    locals: usize,
    code: &[u8],
    instance: Option<Arm64InstanceField>,
    statics: Option<Arm64StaticField>,
    helpers: crate::JitRuntimeHelpers,
) -> Arm64CompileResult {
    let mut b = Arm64Backend::new();
    b.set_safepoints_enabled(false);
    b.set_helpers(helpers);
    b.set_method_descriptor(descriptor, true);
    b.set_exception_table_empty(true);
    if let Some(f) = instance {
        let mut map = HashMap::new();
        for (pc, _op, _cp) in instance_field_sites(code) {
            map.insert(pc, f);
        }
        b.set_instance_field_info(map);
    }
    if let Some(f) = statics {
        let mut map = HashMap::new();
        for (pc, _op, _cp) in static_field_sites(code) {
            map.insert(pc, f);
        }
        b.set_static_field_info(map);
    }
    let slots = crate::compute_param_jvm_slots(descriptor, true).1;
    b.compile_method(locals, slots, 4, code)
}

/// A REFERENCE `putfield` goes to `jit_putfield_object` with the context
/// first, is not narrowed, and is NOT a safepoint -- the barriers enqueue
/// and mark, they run no Java, and x64 records no map there either.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w19_a_reference_putfield_calls_the_barrier_helper() {
    for tag in *b"L[" {
        // aload_0; aload_1; putfield #3; return
        let r = compile_refs(
            "(LX;LY;)V",
            2,
            &[0x2a, 0x2b, 0xb5, 0x00, 0x03, 0xb1],
            Some(int_field(4, tag, false)),
            None,
            r9w19_helpers(),
        );
        assert!(
            r.success,
            "a '{}' putfield must compile: {:?}",
            char::from(tag),
            r.instructions
        );
        assert!(r.needs_context, "jit_putfield_object takes the VM pointer");
        assert!(
            r.pending_oop_maps.is_empty(),
            "the barrier helper cannot safepoint, so it records no map"
        );
        let call = r
            .instructions
            .iter()
            .position(|i| {
                matches!(
                    i,
                    Arm64Instruction::MovImm { rd, imm }
                        if *rd == Arm64Register::X16 && *imm == R9W19_PUTFIELD_OBJ as i64
                )
            })
            .expect("the object putfield helper is called");
        let before = &r.instructions[..call];
        assert!(
            before
                .iter()
                .any(|i| matches!(i, Arm64Instruction::Cbz { .. })),
            "the receiver is null-checked"
        );
        assert!(
            before.iter().any(|i| matches!(
                i,
                Arm64Instruction::MovImm { rd, imm }
                    if *rd == Arm64Register::X2 && *imm == 4
            )),
            "the field index is the THIRD argument, after the context"
        );
        assert!(
            !before.iter().any(|i| matches!(
                i,
                Arm64Instruction::Sxtb { .. } | Arm64Instruction::AndImm { .. }
            )),
            "a reference is never narrowed"
        );
        assert!(emit_machine_code(&r).is_some());
    }
}

/// ...and a REFERENCE `putstatic` goes to `jit_putstatic_object`, keeping
/// the sentinel check and the class-init record the primitive path has.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w19_a_reference_putstatic_calls_the_barrier_helper() {
    // aload_0; putstatic #3; return
    let r = compile_refs(
        "(LX;)V",
        2,
        &[0x2a, 0xb3, 0x00, 0x03, 0xb1],
        None,
        Some(static_field(12, 6, b'L', false)),
        r9w19_helpers(),
    );
    assert!(r.success, "{:?}", r.instructions);
    assert_eq!(r.static_init_classes, vec![12]);
    let call = r
        .instructions
        .iter()
        .position(|i| {
            matches!(
                i,
                Arm64Instruction::MovImm { rd, imm }
                    if *rd == Arm64Register::X16 && *imm == R9W19_PUTSTATIC_OBJ as i64
            )
        })
        .expect("the object putstatic helper is called");
    assert!(
        r.instructions[call..]
            .iter()
            .any(|i| matches!(i, Arm64Instruction::Cmp { .. })),
        "the sentinel is still tested"
    );
}

/// `aaload`: this backend's own guards, then the helper, then the result
/// is pushed AS AN OOP -- which is what puts it in every later map.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w19_aaload_guards_then_calls_and_marks_the_result_as_an_oop() {
    // aload_0; iload_1; aaload; areturn
    let r = compile_refs(
        "([LX;I)LX;",
        3,
        &[0x2a, 0x1b, 0x32, 0xb0],
        None,
        None,
        r9w19_helpers(),
    );
    assert!(r.success, "{:?}", r.instructions);
    let call = r
        .instructions
        .iter()
        .position(|i| {
            matches!(
                i,
                Arm64Instruction::MovImm { rd, imm }
                    if *rd == Arm64Register::X16 && *imm == R9W19_AALOAD as i64
            )
        })
        .expect("jit_aaload is called");
    let before = &r.instructions[..call];
    assert!(
        before
            .iter()
            .any(|i| matches!(i, Arm64Instruction::Cbz { .. })),
        "the array is null-checked before the call"
    );
    assert!(
        before.iter().any(|i| matches!(
            i,
            Arm64Instruction::BCond {
                cond: Arm64Condition::Cs,
                ..
            }
        )),
        "and bounds-checked with the UNSIGNED compare"
    );
    assert_eq!(
        r.pending_oop_maps.len(),
        1,
        "the helper can safepoint, so the call records a map"
    );
    assert!(
        r.instructions[call..]
            .iter()
            .any(|i| matches!(i, Arm64Instruction::Cmp { .. })),
        "the sentinel is tested"
    );
}

/// `aastore` type-checks FIRST, and the array and the value survive that
/// call in the same registers -- which is what leaving them on the operand
/// stack buys, and what the map then covers.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w19_aastore_type_checks_before_it_stores() {
    // aload_0; iload_1; aload_2; aastore; return
    let r = compile_refs(
        "([LX;ILX;)V",
        4,
        &[0x2a, 0x1b, 0x2c, 0x53, 0xb1],
        None,
        None,
        r9w19_helpers(),
    );
    assert!(r.success, "{:?}", r.instructions);
    let check = r
        .instructions
        .iter()
        .position(|i| {
            matches!(
                i,
                Arm64Instruction::MovImm { rd, imm }
                    if *rd == Arm64Register::X16 && *imm == R9W19_TYPE_CHECK as i64
            )
        })
        .expect("the element-type check is called");
    let store = r
        .instructions
        .iter()
        .position(|i| {
            matches!(
                i,
                Arm64Instruction::MovImm { rd, imm }
                    if *rd == Arm64Register::X16 && *imm == R9W19_AASTORE as i64
            )
        })
        .expect("the store helper is called");
    assert!(
        check < store,
        "the type check must come first, or an illegal store is committed \
         before anyone can refuse it"
    );
    assert_eq!(
        r.pending_oop_maps.len(),
        2,
        "both calls can safepoint, so both record a map"
    );
    // The array and the value are reference OPERANDS, so both are spilled
    // and both are named -- that is what lets a collector move them
    // during the type check and hand the store their new addresses.
    assert!(
        r.pending_oop_maps[0].frame_slot_offsets.len() >= 2,
        "the array and the value must both be in the first call's map: {:?}",
        r.pending_oop_maps[0].frame_slot_offsets
    );
    assert_eq!(
        r.pending_oop_maps[0].safepoint_id, 3,
        "keyed by the aastore's own bci"
    );
}

/// Every "not wired" refusal these four share.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w19_the_reference_paths_refuse_every_unwired_shape() {
    // A missing `aastore_type_check` must refuse rather than fall back to
    // the bare store: its ArrayStoreException would then surface only at
    // method return, after this frame had run on.
    let mut h = r9w19_helpers();
    h.aastore_type_check = 0;
    let r = compile_refs(
        "([LX;ILX;)V",
        4,
        &[0x2a, 0x1b, 0x2c, 0x53, 0xb1],
        None,
        None,
        h,
    );
    assert!(!r.success, "no type check means no aastore");

    // A missing `aaload`.
    let mut h = r9w19_helpers();
    h.aaload = 0;
    let r = compile_refs("([LX;I)LX;", 3, &[0x2a, 0x1b, 0x32, 0xb0], None, None, h);
    assert!(!r.success, "an unwired jit_aaload must refuse");

    // A missing `putfield_object`.
    let mut h = r9w19_helpers();
    h.putfield_object = 0;
    let r = compile_refs(
        "(LX;LY;)V",
        2,
        &[0x2a, 0x2b, 0xb5, 0x00, 0x03, 0xb1],
        Some(int_field(0, b'L', false)),
        None,
        h,
    );
    assert!(!r.success, "an unwired jit_putfield_object must refuse");

    // A non-empty exception table stops both array forms: their guards
    // throw, and the drain throws with an unknown pc.
    let mut b = Arm64Backend::new();
    b.set_safepoints_enabled(false);
    b.set_helpers(r9w19_helpers());
    b.set_method_descriptor("([LX;I)LX;", true);
    b.set_exception_table_empty(false);
    let r = b.compile_method(3, 2, 4, &[0x2a, 0x1b, 0x32, 0xb0]);
    assert!(!r.success, "aaload needs an empty exception table");
}

/// `athrow` hands the exception and the bci to the helper and leaves
/// through the epilogue. No spill and no map: the operand stack is dead
/// the moment the exception is raised.
#[test]
pub(in crate::aarch64_backend::tests) fn r9w20_athrow_calls_the_helper_and_leaves() {
    const THROW: usize = 0x7FFF_0001_1000;
    // SAFETY: plain struct of `usize`; all-zero is "nothing wired".
    let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    h.throw_exception = THROW;
    let mut b = Arm64Backend::new();
    b.set_safepoints_enabled(false);
    b.set_helpers(h);
    b.set_method_descriptor("(LX;)V", true);
    b.set_exception_table_empty(true);
    // aload_0; athrow
    let r = b.compile_method(2, 1, 4, &[0x2a, 0xbf]);
    assert!(r.success, "athrow must compile: {:?}", r.instructions);
    assert!(
        r.pending_oop_maps.is_empty(),
        "a frame that is leaving needs no map"
    );
    let call = r
        .instructions
        .iter()
        .position(|i| {
            matches!(
                i,
                Arm64Instruction::MovImm { rd, imm }
                    if *rd == Arm64Register::X16 && *imm == THROW as i64
            )
        })
        .expect("the throw helper is called");
    assert!(
        r.instructions[..call].iter().any(|i| matches!(
            i,
            Arm64Instruction::MovImm { rd, imm }
                if *rd == Arm64Register::X1 && *imm == 1
        )),
        "the bci is the second argument"
    );
    // The helper RETURNS the sentinel, so nothing may overwrite X0 between
    // the call and the epilogue.
    let after = &r.instructions[call..];
    let blr = after
        .iter()
        .position(|i| matches!(i, Arm64Instruction::Blr { .. }))
        .expect("the call is emitted");
    let branch = after[blr..]
        .iter()
        .position(|i| matches!(i, Arm64Instruction::B { .. }))
        .expect("it leaves through the epilogue");
    assert_eq!(
        branch,
        1,
        "the branch to the epilogue follows the call directly: {:?}",
        &after[blr..]
    );

    // Unwired: refuses regardless of the exception table.
    let mut b = Arm64Backend::new();
    b.set_safepoints_enabled(false);
    b.set_method_descriptor("(LX;)V", true);
    b.set_exception_table_empty(true);
    assert!(!b.compile_method(2, 1, 4, &[0x2a, 0xbf]).success);

    // Wired, with a handler in this frame: `jit_throw_exception` takes the
    // bci as its own argument and stamps the same TLS cell
    // `emit_stamp_throw_bci` writes for every other trapping lowering, so
    // the drain has an exact throw site regardless of table emptiness --
    // athrow needs no `can_route_exception` check at all.
    let mut b = Arm64Backend::new();
    b.set_safepoints_enabled(false);
    b.set_helpers(h);
    b.set_method_descriptor("(LX;)V", true);
    b.set_exception_table_empty(false);
    assert!(
        b.compile_method(2, 1, 4, &[0x2a, 0xbf]).success,
        "a non-empty exception table no longer refuses athrow"
    );
}

