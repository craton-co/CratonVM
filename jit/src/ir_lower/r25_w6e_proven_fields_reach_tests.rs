use super::*;
use crate::ir::IrBuilder;
use crate::ir_schedule;
use std::collections::HashMap;
use std::collections::HashSet;


fn no_helpers() -> JitRuntimeHelpers {
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; each test wires only the slots it exercises.
    unsafe { std::mem::zeroed() }
}

/// A live-looking read-bounds table (a heap slot: the statics ratchet
/// counts test statics too).
fn read_bounds() -> usize {
    let table: &'static [u64; 6] = Box::leak(Box::new([0u64; 6]));
    table.as_ptr() as usize // Cast: the table's address
}

/// The inline guarded, trusted-receiver arm is the one this build takes.
fn arm_available() -> bool {
    !crate::x64::narrow_oops_block_inline_fields()
        && crate::x64::guarded_inline_getfield_enabled()
        && !crate::x64::inline_getfield_enabled()
        && crate::x64::trusted_oop_receiver_getfield_enabled()
}

/// `TEST byte [RAX + GC_FLAGS_BYTE_OFFSET], GC_FLAG_COMPACT` in `code`.
fn compact_tests(code: &[u8]) -> usize {
    let mut needle: Vec<u8> = vec![0xF6, 0x80];
    needle.extend_from_slice(&(cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32).to_le_bytes());
    needle.push(cratonvm_types::GC_FLAG_COMPACT);
    code.windows(needle.len())
        .filter(|w| *w == needle.as_slice())
        .count()
}

/// Epoch compares in `code`, both encodings, resolved to the epoch cell
/// (the item-4 tests' census).
fn epoch_guards(code: &[u8]) -> usize {
    let (epoch, _) = cratonvm_types::layout_replace_epoch_guard();
    let epoch = epoch as usize; // Cast: the epoch cell's address
    let base = code.as_ptr() as usize; // Cast: the code's address
    let mut n = 0usize;
    for i in 0..code.len() {
        if i + 10 <= code.len() && code[i] == 0x81 && code[i + 1] == 0x3D {
            let d = i32::from_le_bytes([code[i + 2], code[i + 3], code[i + 4], code[i + 5]]);
            // Cast: a signed displacement added in address space.
            let target = base.wrapping_add(i).wrapping_add(10).wrapping_add(d as isize as usize);
            if target == epoch {
                n += 1;
            }
        }
        if i + 10 <= code.len() && code[i] == 0x49 && code[i + 1] == 0xBB {
            let mut imm = [0u8; 8];
            imm.copy_from_slice(&code[i + 2..i + 10]);
            // Cast: an address read back.
            if u64::from_le_bytes(imm) as usize == epoch {
                n += 1;
            }
        }
    }
    n
}

/// A 64-byte object whose flags byte says `compact` and whose 4-byte
/// cells at `+8` and `+12` hold `a` and `b`.
fn object(compact: bool, a: i32, b: i32) -> Box<[u64; 8]> {
    let mut obj = Box::new([0u64; 8]);
    // SAFETY: `obj` is 64 bytes, 8-byte aligned; every write lands inside it.
    unsafe {
        let p = obj.as_mut_ptr() as *mut u8; // Cast: array base → byte cursor
        if compact {
            *p.add(cratonvm_types::GC_FLAGS_BYTE_OFFSET) = cratonvm_types::GC_FLAG_COMPACT;
        }
        *(p.add(8) as *mut i32) = a; // Cast: the first cell
        *(p.add(12) as *mut i32) = b; // Cast: the second cell
    }
    obj
}




/// Source pins: the co-test keeps its admission rules.
#[test]
fn the_coestablishment_keeps_its_rules() {
    let src = concat!(include_str!("../ir_lower/mod.rs"), "\n", include_str!("../ir_lower/virtual_object_info.rs"), "\n", include_str!("../ir_lower/mir_plan_and_init.rs"), "\n", include_str!("../ir_lower/vector_preloop.rs"), "\n", include_str!("../ir_lower/value_phi_copies.rs"), "\n", include_str!("../ir_lower/field_proof_entry_is_rechecked_osr.rs"), "\n", include_str!("../ir_lower/int_expr_bounds.rs"), "\n", include_str!("../ir_lower/character_to_chars_prefix.rs"), "\n", include_str!("../ir_lower/getfield_legacy_read.rs"), "\n", include_str!("../ir_lower/post_call_exit_site_for.rs"), "\n", include_str!("../ir_lower/entry_fold_node.rs"), "\n", include_str!("../ir_lower/call_loaded_ic_entry_releasing.rs"), "\n", include_str!("../ir_lower/fallthrough_poll_spills.rs"), "\n", include_str!("../ir_lower/ir_ic_cascade_admits_kind.rs"), "\n", include_str!("../ir_lower/implicit_kind_matches_catch.rs"), "\n", include_str!("../ir_lower/scoped_raw_tail.rs"), "\n", include_str!("../ir_lower/frame_needs.rs"), "\n", include_str!("../ir_lower/back_edge_dead_ref_homes.rs"), "\n", include_str!("../ir_lower/block_lazy_clear_survivors.rs"), "\n", include_str!("../ir_lower/promoted_pending_mark.rs"), "\n", include_str!("../ir_lower/site_value_live_at.rs"), "\n", include_str!("../ir_lower/plan_register_residency_with.rs"), "\n", include_str!("../ir_lower/inner_attempt.rs"), "\n", include_str!("../ir_lower/g1_aastore_screen.rs"), "\n", include_str!("../ir_lower/scoped_session_confined_rows.rs"));
    let body = |head: &str| -> &str {
        src.split(head)
            .nth(1)
            .and_then(|s| s.split("\n    fn ").next())
            .expect(head)
    };
    let co = body("fn coestablish_receiver_proof(");
    assert!(co.contains("self.pc_is_in_a_spliced_body(pc)"));
    assert!(co.contains("self.field_site_in_a_dominated_block(recv, block)"));
}
