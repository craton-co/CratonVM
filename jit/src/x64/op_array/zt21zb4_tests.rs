//! zt t21/zb4 (stage 2d): the inline colored-slot decode after a compiled
//! `aaload`, driven directly with an explicit base -- no process flag is
//! flipped and no base is published, so the other tests of this binary
//! keep their unarmed emission.

use super::*;

const BASE: u64 = 0x0000_7F00_0000_0000;

/// A fresh compiler whose operand stack holds an array and an index in
/// frame slots (no helpers wired: the colored arm calls none).
fn compiler_with_aaload_operands() -> Compiler {
    let mut c = crate::x64::emit::tests::test_compiler();
    for off in [16, 24] {
        c.stack.push(StackSlot::Frame(off));
        c.stack_oop_marks.push(false);
    }
    c
}

/// The emitted sequence, byte for byte (the report quotes it).
#[test]
fn zt21zb4_the_decode_bytes_are_the_documented_sequence() {
    let bytes = zgc_colored_slot_decode_bytes(BASE);
    let mut want = vec![0x48, 0x85, 0xC0, 0x79, 0x1A, 0x49, 0xBB];
    want.extend_from_slice(&((1u64 << 42) - 1).to_le_bytes());
    want.extend_from_slice(&[0x4C, 0x21, 0xD8, 0x49, 0xBB]);
    want.extend_from_slice(&BASE.to_le_bytes());
    want.extend_from_slice(&[0x4C, 0x01, 0xD8]);
    assert_eq!(bytes.to_vec(), want);
    assert_eq!(bytes.len(), ZGC_COLORED_SLOT_DECODE_LEN);
    // The JNS skips exactly the four instructions after it.
    assert_eq!(usize::from(bytes[4]), ZGC_COLORED_SLOT_DECODE_LEN - 5);
}

/// The colored arm is the unarmed inline arm with the decode spliced in
/// right after the element load, and calls nothing; the unarmed arm itself
/// carries no decode (its bytes are what the default path emits).
#[test]
fn zt21zb4_the_colored_aaload_is_the_inline_arm_plus_the_decode() {
    // zt t22/x18: the answer read below must hold through the walk.
    let _flip = crate::x64::licm::zgc_read_gate_flip_test_lock();
    if zgc_read_barrier_blocks_inline_fields() {
        // Another test of this binary armed the process flag; the unarmed
        // twin below would take the routed arm. Nothing to compare.
        return;
    }
    let mut plain = compiler_with_aaload_operands();
    let p0 = plain.buf.pos();
    let step = plain.walk_array(&[0x32], 1, 0x32, 0, &mut false, &[false], &[true]);
    assert!(matches!(step, WalkStep::Next(1)));
    let plain_code = plain.buf.as_slice()[p0..].to_vec();

    let mut colored = compiler_with_aaload_operands();
    let c0 = colored.buf.pos();
    let calls_before = colored.helper_call_patches.len();
    let step = colored.walk_aaload_colored_inline(&[0x32], 0, &[true], BASE);
    assert!(
        matches!(step, WalkStep::Next(1)),
        "the walk continues after the aaload"
    );
    assert_eq!(colored.stack.len(), 1, "the element replaces both operands");
    assert_eq!(colored.stack_oop_marks.last(), Some(&true), "and is an oop");
    assert_eq!(
        colored.helper_call_patches.len(),
        calls_before,
        "no helper call"
    );
    let colored_code = colored.buf.as_slice()[c0..].to_vec();

    // MOV RAX, [RAX + RCX*8 + disp8], the inline arm's load.
    let disp = u8::try_from(ARRAY_DATA_OFFSET).expect("a disp8 element base");
    let load = [0x48u8, 0x8B, 0x44, 0xC8, disp];
    let at = plain_code
        .windows(load.len())
        .position(|w| w == &load[..])
        .expect("the unarmed arm loads the element inline")
        + load.len();
    let decode = zgc_colored_slot_decode_bytes(BASE);
    assert!(
        !plain_code.windows(decode.len()).any(|w| w == &decode[..]),
        "the unarmed arm emits no decode"
    );
    let mut want = plain_code[..at].to_vec();
    want.extend_from_slice(&decode);
    want.extend_from_slice(&plain_code[at..]);
    assert_eq!(
        colored_code, want,
        "colored = inline arm + decode after the load"
    );
}

/// Unarmed (every default run), the predicate answers `None` and no
/// element load calls the helper: the default path is the inline arm.
#[test]
fn zt21zb4_an_unarmed_process_keeps_the_inline_arm() {
    // zt t22/x18: as above.
    let _flip = crate::x64::licm::zgc_read_gate_flip_test_lock();
    if zgc_read_barrier_blocks_inline_fields() {
        return;
    }
    assert_eq!(zgc_colored_inline_load_barrier_base(), None);
    assert!(!zgc_ref_element_load_calls_helper());
}

/// Both optimizing-tier halves ask the ONE predicate (the allocator's
/// call set and the lowering's helper arm), and the element emitter
/// appends the shared decode. Needles are assembled so this file does not
/// match them.
#[test]
fn zt21zb4_the_optimizing_tier_asks_one_predicate() {
    let pred = concat!("crate::x64::zgc_ref_element_load_calls_", "helper()");
    let lower = concat!(include_str!("../../ir_lower/mod.rs"), "\n", include_str!("../../ir_lower/virtual_object_info.rs"), "\n", include_str!("../../ir_lower/mir_plan_and_init.rs"), "\n", include_str!("../../ir_lower/vector_preloop.rs"), "\n", include_str!("../../ir_lower/value_phi_copies.rs"), "\n", include_str!("../../ir_lower/field_proof_entry_is_rechecked_osr.rs"), "\n", include_str!("../../ir_lower/int_expr_bounds.rs"), "\n", include_str!("../../ir_lower/character_to_chars_prefix.rs"), "\n", include_str!("../../ir_lower/getfield_legacy_read.rs"), "\n", include_str!("../../ir_lower/post_call_exit_site_for.rs"), "\n", include_str!("../../ir_lower/entry_fold_node.rs"), "\n", include_str!("../../ir_lower/call_loaded_ic_entry_releasing.rs"), "\n", include_str!("../../ir_lower/fallthrough_poll_spills.rs"), "\n", include_str!("../../ir_lower/ir_ic_cascade_admits_kind.rs"), "\n", include_str!("../../ir_lower/implicit_kind_matches_catch.rs"), "\n", include_str!("../../ir_lower/scoped_raw_tail.rs"), "\n", include_str!("../../ir_lower/frame_needs.rs"), "\n", include_str!("../../ir_lower/back_edge_dead_ref_homes.rs"), "\n", include_str!("../../ir_lower/block_lazy_clear_survivors.rs"), "\n", include_str!("../../ir_lower/promoted_pending_mark.rs"), "\n", include_str!("../../ir_lower/site_value_live_at.rs"), "\n", include_str!("../../ir_lower/plan_register_residency_with.rs"), "\n", include_str!("../../ir_lower/inner_attempt.rs"), "\n", include_str!("../../ir_lower/g1_aastore_screen.rs"), "\n", include_str!("../../ir_lower/scoped_session_confined_rows.rs"));
    let alloc = concat!(include_str!("../../regalloc/mod.rs"), "\n", include_str!("../../regalloc/local_gpr_pool.rs"), "\n", include_str!("../../regalloc/build_live_model_legacy_impl.rs"), "\n", include_str!("../../regalloc/verify_allocation.rs"));
    assert!(lower.contains(pred), "ir_lower's ArrayLoad(Ref) arm");
    assert!(alloc.contains(pred), "regalloc::ir_op_is_call");
    let emit = concat!("emit_zgc_colored_slot_", "decode(&mut self.buf, heap_base)");
    assert!(
        lower.contains(emit),
        "the optimizing reference load's colored arm"
    );
}

/// The decode EXECUTED on a word buffer: `extern "C" fn(word) -> word'`.
#[cfg(target_arch = "x86_64")]
#[test]
fn zt21zb4_the_decode_runs_and_agrees_with_the_gc_readers_decode() {
    use cratonvm_types::narrow_oop::{decode_colored_slot_word_with, COLORED_SLOT_TAG};
    let mut buf = crate::ExecutableBuffer::new(256).expect("an executable buffer");
    // MOV RAX, ARG0 (RCX on Win64, RDI on SysV; both below R8).
    let arg0 = ARG_REGS[0];
    buf.emit(&[0x48, 0x89, 0xC0 | ((arg0 & 7) << 3)]);
    emit_zgc_colored_slot_decode(&mut buf, BASE);
    buf.emit(&[0xC3]); // RET
    buf.finalize();
    // SAFETY: a finalized `extern "C" fn(u64) -> u64` that reads its
    // argument and writes RAX, R11 and the flags (all caller-saved).
    let f =
        unsafe { std::mem::transmute::<*const u8, extern "C" fn(u64) -> u64>(buf.as_ptr()) };
    let remapped = 1u64 << 44;
    let marked0 = 1u64 << 42;
    let max_off = (1u64 << 42) - 1;
    let words: [(u64, u64); 7] = [
        // null stays null
        (0, 0),
        // a plain machine pointer passes through
        (0x0000_7FFF_1234_5678, 0x0000_7FFF_1234_5678),
        (0x0000_0000_0040_0010, 0x0000_0000_0040_0010),
        // good colored words decode to base + offset
        (COLORED_SLOT_TAG | remapped | 0x1238, BASE + 0x1238),
        // an object at arena offset 0 is `base`, never null
        (COLORED_SLOT_TAG | remapped, BASE),
        (COLORED_SLOT_TAG | remapped | max_off, BASE + max_off),
        // the pinned decode ignores the color: a Marked0 word decodes the
        // same -- why the color flip must withdraw this sequence
        (COLORED_SLOT_TAG | marked0 | 0x80, BASE + 0x80),
    ];
    for (word, want) in words {
        assert_eq!(f(word), want, "word {word:#018x}");
        assert_eq!(
            f(word),
            decode_colored_slot_word_with(BASE, word),
            "the GC readers' decode agrees on {word:#018x}"
        );
    }
}
