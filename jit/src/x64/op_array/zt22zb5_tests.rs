//! zt t22/zb5 (stage 2e): the color flip's compiled element load, driven
//! directly with an explicit cell address and base -- no process flag is
//! flipped and no cell is published, so the other tests of this binary
//! keep their emission.

use super::*;

const BASE: u64 = 0x0000_7F00_0000_0000;
const CELL: u64 = 0x0000_7F11_2233_4440;
const HELPER: usize = 0x7FFF_0000_1000;

/// A fresh compiler whose operand stack holds an array and an index in
/// frame slots, with a reserved VM-context slot and `aaload` wired.
fn compiler_with_aaload_operands(aaload_helper: usize) -> Compiler {
    let mut c = crate::x64::emit::tests::test_compiler();
    c.helpers.aaload = aaload_helper;
    c.heap_local_offset = 8;
    for off in [16, 24] {
        c.stack.push(StackSlot::Frame(off));
        c.stack_oop_marks.push(false);
    }
    c
}

fn le32(bytes: &[u8], at: usize) -> i32 {
    i32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// The head, byte for byte (the report quotes it), its two patch slots,
/// and the JNZ that skips exactly the decode and the closing JMP.
#[test]
fn zt22zb5_the_flip_head_bytes_are_the_documented_sequence() {
    let bytes = zgc_colored_flip_head_bytes(CELL, BASE);
    let mut want = vec![0x48, 0x85, 0xC0, 0x0F, 0x89, 0, 0, 0, 0, 0x49, 0xBB];
    want.extend_from_slice(&CELL.to_le_bytes());
    want.extend_from_slice(&[0x49, 0x85, 0x03, 0x75, 0x1F, 0x49, 0xBB]);
    want.extend_from_slice(&((1u64 << 42) - 1).to_le_bytes());
    want.extend_from_slice(&[0x4C, 0x21, 0xD8, 0x49, 0xBB]);
    want.extend_from_slice(&BASE.to_le_bytes());
    want.extend_from_slice(&[0x4C, 0x01, 0xD8, 0xE9, 0, 0, 0, 0]);
    assert_eq!(bytes.to_vec(), want);
    assert_eq!(bytes.len(), ZGC_COLORED_FLIP_HEAD_LEN);
    // The patch slots are the two rel32 placeholders.
    assert_eq!(
        &bytes[ZGC_COLORED_FLIP_JNS_PATCH - 2..ZGC_COLORED_FLIP_JNS_PATCH],
        &[0x0F, 0x89]
    );
    assert_eq!(bytes[ZGC_COLORED_FLIP_JMP_PATCH - 1], 0xE9);
    assert_eq!(ZGC_COLORED_FLIP_JMP_PATCH + 4, ZGC_COLORED_FLIP_HEAD_LEN);
    // JNZ rel8 at 22..24 lands on .slow, the first byte after the head.
    assert_eq!(24 + usize::from(bytes[23]), ZGC_COLORED_FLIP_HEAD_LEN);
    // The good-word arm is stage 2d's decode minus its sign test.
    let decode = zgc_colored_slot_decode_bytes(BASE);
    assert_eq!(&bytes[24..50], &decode[5..]);
}

/// The flip arm: the unarmed arm's bytes up to the element load, then the
/// head, then a call of `jit_aaload`, with both rel32 exits patched to
/// the end of the site (`.done`); the element is one oop on the stack.
#[test]
fn zt22zb5_the_flip_aaload_is_the_inline_load_the_head_and_a_helper_call() {
    let _flip = crate::x64::licm::zgc_read_gate_flip_test_lock();
    if zgc_read_barrier_blocks_inline_fields() {
        // Another test of this binary armed the process flag; the unarmed
        // twin below would take the routed arm. Nothing to compare.
        return;
    }
    let mut plain = compiler_with_aaload_operands(HELPER);
    let p0 = plain.buf.pos();
    let step = plain.walk_array(&[0x32], 1, 0x32, 0, &mut false, &[false], &[true]);
    assert!(matches!(step, WalkStep::Next(1)));
    let plain_code = plain.buf.as_slice()[p0..].to_vec();

    let mut flip = compiler_with_aaload_operands(HELPER);
    let f0 = flip.buf.pos();
    let calls_before = flip.helper_call_patches.len();
    let step = flip.walk_aaload_colored_flip(&[0x32], 0, &[true], BASE, CELL);
    assert!(
        matches!(step, WalkStep::Next(1)),
        "the walk continues after the aaload"
    );
    assert_eq!(flip.stack.len(), 1, "the element replaces both operands");
    assert_eq!(flip.stack_oop_marks.last(), Some(&true), "and is an oop");
    let code = flip.buf.as_slice()[f0..].to_vec();
    let addr = (HELPER as u64).to_le_bytes();
    let called = flip.helper_call_patches.len() > calls_before
        || code.windows(addr.len()).any(|w| w == &addr[..]);
    assert!(called, "the slow branch calls the helper: {code:02x?}");

    // MOV RAX, [RAX + RCX*8 + disp8], the inline arm's load.
    let disp = u8::try_from(ARRAY_DATA_OFFSET).expect("a disp8 element base");
    let load = [0x48u8, 0x8B, 0x44, 0xC8, disp];
    let at = code
        .windows(load.len())
        .position(|w| w == &load[..])
        .expect("the flip arm loads the element inline")
        + load.len();
    // The pinned 2d decode is not emitted.
    let decode = zgc_colored_slot_decode_bytes(BASE);
    assert!(
        !code.windows(decode.len()).any(|w| w == &decode[..]),
        "no pinned decode under the flip"
    );
    // The head right after the load; both exits patched to the end.
    let head = zgc_colored_flip_head_bytes(CELL, BASE);
    assert_eq!(&code[at..at + 5], &head[..5], "TEST + JNS opcode");
    assert_eq!(
        &code[at + 9..at + 51],
        &head[9..51],
        "cell, bad test, decode"
    );
    let end = code.len();
    let jns = at + ZGC_COLORED_FLIP_JNS_PATCH;
    let jmp = at + ZGC_COLORED_FLIP_JMP_PATCH;
    // `.done` is where the element is pushed: depending on the operand
    // stack's register state, that push is nothing (a register) or one
    // frame-slot store (zt t24: 4 bytes after `.done` on the host run).
    let done = jns + 4 + le32(&code, jns) as usize;
    assert_eq!(jmp + 4 + le32(&code, jmp) as usize, done, "JMP -> .done");
    assert!(
        done <= end && end - done <= 8,
        "JNS -> .done, followed by the element's push only: done={done} end={end}"
    );
    // Up to the load, the flip arm is the unarmed arm (both operands sat
    // in frame slots, so the flush emitted nothing).
    let plain_at = plain_code
        .windows(load.len())
        .position(|w| w == &load[..])
        .expect("the unarmed arm loads the element inline");
    assert_eq!(&code[..at - load.len()], &plain_code[..plain_at]);
}

/// An unwired helper table refuses the flip site (never a bad word
/// decoded without its slow path), and nothing is popped.
#[test]
fn zt22zb5_an_unwired_aaload_helper_refuses_the_flip_site() {
    let mut c = compiler_with_aaload_operands(0);
    let step = c.walk_aaload_colored_flip(&[0x32], 0, &[true], BASE, CELL);
    assert!(matches!(step, WalkStep::Return(false)));
    assert_eq!(c.stack.len(), 2, "nothing was popped");
}

/// Unarmed (every default run) no cell is offered and no element load
/// calls the helper.
#[test]
fn zt22zb5_an_unarmed_process_offers_no_flip_cell() {
    // x18's lock: licm's coexistence test flips the read gate between
    // this read and the asserts otherwise.
    let _flip = crate::x64::licm::zgc_read_gate_flip_test_lock();
    if zgc_read_barrier_blocks_inline_fields() {
        return;
    }
    assert_eq!(zgc_colored_flip_bad_mask_cell(), None);
    assert!(!zgc_ref_element_load_calls_helper());
}

/// The optimizing tier: its `ArrayLoad(Ref)` helper arm dispatches to the
/// flip lowering, which loads the raw word through the shared element
/// emitter and appends the shared head; the predicate the allocator and
/// the scan ask covers the flip. Needles are assembled so this file does
/// not match them.
#[test]
fn zt22zb5_the_optimizing_tier_emits_the_same_head() {
    let lower = concat!(include_str!("../../ir_lower/mod.rs"), "\n", include_str!("../../ir_lower/virtual_object_info.rs"), "\n", include_str!("../../ir_lower/mir_plan_and_init.rs"), "\n", include_str!("../../ir_lower/vector_preloop.rs"), "\n", include_str!("../../ir_lower/value_phi_copies.rs"), "\n", include_str!("../../ir_lower/field_proof_entry_is_rechecked_osr.rs"), "\n", include_str!("../../ir_lower/int_expr_bounds.rs"), "\n", include_str!("../../ir_lower/character_to_chars_prefix.rs"), "\n", include_str!("../../ir_lower/getfield_legacy_read.rs"), "\n", include_str!("../../ir_lower/post_call_exit_site_for.rs"), "\n", include_str!("../../ir_lower/entry_fold_node.rs"), "\n", include_str!("../../ir_lower/call_loaded_ic_entry_releasing.rs"), "\n", include_str!("../../ir_lower/fallthrough_poll_spills.rs"), "\n", include_str!("../../ir_lower/ir_ic_cascade_admits_kind.rs"), "\n", include_str!("../../ir_lower/implicit_kind_matches_catch.rs"), "\n", include_str!("../../ir_lower/scoped_raw_tail.rs"), "\n", include_str!("../../ir_lower/frame_needs.rs"), "\n", include_str!("../../ir_lower/back_edge_dead_ref_homes.rs"), "\n", include_str!("../../ir_lower/block_lazy_clear_survivors.rs"), "\n", include_str!("../../ir_lower/promoted_pending_mark.rs"), "\n", include_str!("../../ir_lower/site_value_live_at.rs"), "\n", include_str!("../../ir_lower/plan_register_residency_with.rs"), "\n", include_str!("../../ir_lower/inner_attempt.rs"), "\n", include_str!("../../ir_lower/g1_aastore_screen.rs"), "\n", include_str!("../../ir_lower/scoped_session_confined_rows.rs"));
    for needle in [
        concat!(
            "self.lower_ref_array_load_colored_",
            "flip(id, node, slot, bci, base, cell)"
        ),
        concat!("self.emit_ref_array_elem_load_", "at(array_base, index, true)"),
        concat!(
            "crate::x64::emit_zgc_colored_flip_",
            "head(&mut self.buf, cell, heap_base)"
        ),
    ] {
        assert!(lower.contains(needle), "ir_lower.rs lacks {needle}");
    }
    let licm = include_str!("../licm.rs");
    assert!(
        licm.contains(concat!(
            "|| zgc_colored_flip_bad_mask_",
            "cell().is_some())"
        )),
        "zgc_ref_element_load_calls_helper covers the flip"
    );
}

/// The head EXECUTED: `extern "C" fn(word) -> word'`, with the slow
/// branch replaced by `MOV RAX, SLOW` so a bad word is visible. Agrees
/// with the collector's executable statement of the same fast path,
/// `cratonvm_gc::zgc::barrier::inline_barrier_fast`, for every color
/// under the bad mask of each good color -- and the cell is re-read on
/// every call: the mask changes between calls with no recompile.
#[cfg(target_arch = "x86_64")]
#[test]
fn zt22zb5_the_flip_head_runs_and_agrees_with_the_barriers_fast_path() {
    use cratonvm_gc::zgc::barrier::{inline_barrier_fast, ZInlineFast};
    use cratonvm_gc::zgc::vaddr::{ZColor, Z_METADATA_MASK};
    use std::sync::atomic::{AtomicU64, Ordering};
    const SLOW: u64 = 0x5107_5107_5107_5107;
    // A frame local, not a `static` (JIT r25 wave 8, W8-2): it outlives
    // every call of the head below, which is all the baked address needs.
    let cell_word = AtomicU64::new(0);
    let cell = &cell_word as *const AtomicU64 as u64;

    let mut buf = crate::ExecutableBuffer::new(256).expect("an executable buffer");
    // MOV RAX, ARG0 (RCX on Win64, RDI on SysV; both below R8).
    let arg0 = ARG_REGS[0];
    buf.emit(&[0x48, 0x89, 0xC0 | ((arg0 & 7) << 3)]);
    let (jns, jmp) = emit_zgc_colored_flip_head(&mut buf, cell, BASE);
    // .slow: MOV RAX, imm64 SLOW
    buf.emit(&[0x48, 0xB8]);
    buf.emit(&SLOW.to_le_bytes());
    // .done
    crate::runtime_lowering::patch_rel32_to_here(&mut buf, jns);
    crate::runtime_lowering::patch_rel32_to_here(&mut buf, jmp);
    buf.emit(&[0xC3]); // RET
    buf.finalize();
    // SAFETY: a finalized `extern "C" fn(u64) -> u64` that reads its
    // argument and `cell_word`, and writes RAX, R11 and the flags (all
    // caller-saved).
    let f =
        unsafe { std::mem::transmute::<*const u8, extern "C" fn(u64) -> u64>(buf.as_ptr()) };
    let tag = 1u64 << 63;
    let max_off = (1u64 << 42) - 1;
    for good in [ZColor::Remapped, ZColor::Marked0, ZColor::Marked1] {
        let bad = good.bit() ^ Z_METADATA_MASK;
        cell_word.store(bad, Ordering::Relaxed);
        let mut words = vec![0u64, 0x0000_7FFF_1234_5678, 0x0000_0000_0040_0010];
        for c in ZColor::ALL {
            for off in [0u64, 0x1238, max_off] {
                words.push(tag | c.bit() | off);
            }
        }
        for word in words {
            let want = match inline_barrier_fast(word, bad, BASE) {
                ZInlineFast::Done(v) => v,
                ZInlineFast::Slow(_) => SLOW,
            };
            assert_eq!(f(word), want, "good {good:?}, word {word:#018x}");
        }
        // The good color itself passes and decodes; offset 0 is `base`.
        assert_eq!(f(tag | good.bit() | 0x40), BASE + 0x40);
        assert_eq!(f(tag | good.bit()), BASE);
    }
}
