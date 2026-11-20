use super::{
    jit_code_range_view, lookup_jit_code_range, register_jit_code_range,
    unregister_jit_code_range, CompiledMethod, ExecutableBuffer, OopMapEntry,
};

fn artifact() -> CompiledMethod {
    CompiledMethod::new(
        ExecutableBuffer::new(64).expect("executable buffer alloc must succeed in tests"),
    )
}

fn map(id: u32) -> OopMapEntry {
    let mut m = OopMapEntry::new(0);
    m.bytecode_pc = id;
    m
}

/// The index answers exactly what the scan answers, for ids present (in
/// any order, repeated), absent, and at the edges of `u32`.
#[test]
fn the_index_answers_the_scan() {
    let mut cm = artifact();
    for id in [40, 7, 7, 0, 0xFFFF_FFFE, 19, 40] {
        cm.push_oop_map(map(id));
    }
    for id in [0, 1, 6, 7, 8, 19, 20, 39, 40, 41, 0xFFFF_FFFE, 0xFFFF_FFFF] {
        assert_eq!(
            cm.has_oop_map_for_safepoint_id(id),
            cm.find_oop_map_for_safepoint_id(id).is_some(),
            "{id}"
        );
    }
    let empty = artifact();
    assert!(!empty.has_oop_map_for_safepoint_id(0));
}

/// A map added after the index was built is seen: by `push_oop_map`
/// (which drops the index) and by a direct push onto the public vector
/// (whose count no longer matches the index's).
#[test]
fn a_map_added_after_the_index_is_seen() {
    let mut cm = artifact();
    cm.push_oop_map(map(3));
    assert!(!cm.has_oop_map_for_safepoint_id(9));
    cm.push_oop_map(map(9));
    assert!(cm.has_oop_map_for_safepoint_id(9));
    assert!(!cm.has_oop_map_for_safepoint_id(12));
    cm.oop_maps.push(map(12));
    assert!(cm.has_oop_map_for_safepoint_id(12));
    assert!(cm.has_oop_map_for_safepoint_id(3));
}

/// A view answers what `lookup_jit_code_range` answers for every address
/// in, at the edges of, and around a registered range.
#[test]
fn a_view_answers_the_lookup() {
    // Far from any address a real body or another test registers.
    let entry = 0x7E15_0F00_0000usize;
    let cm_ptr = 0x7E15_0F10_0000usize;
    register_jit_code_range(entry, 0x40, cm_ptr);
    let view = jit_code_range_view();
    for addr in [
        entry - 1,
        entry,
        entry + 1,
        entry + 0x3F,
        entry + 0x40,
        entry + 0x1000,
    ] {
        assert_eq!(view.lookup(addr), lookup_jit_code_range(addr), "{addr:#x}");
    }
    assert_eq!(view.lookup(entry + 8), Some(cm_ptr));
    drop(view);
    unregister_jit_code_range(entry);
    assert_eq!(jit_code_range_view().lookup(entry + 8), None);
}

/// A walk's batched verdicts land in the same columns as one note each.
/// The census is process-wide and other tests add to it concurrently, so
/// only a lower bound on each column's growth is asserted.
#[test]
fn batched_verdicts_land_in_their_columns() {
    let before = super::compiled_frame_line_counts();
    let mut batch = [0u64; super::FRAME_LINE_SLOTS];
    batch[super::FRAME_LINE_ANSWERED_SINGLE_PASS] = 5;
    batch[super::FRAME_LINE_REFUSED_ID_UNRECORDED] = 2;
    batch[super::FRAME_LINE_INNERMOST_UNRECORDED] = 1;
    super::note_compiled_frame_lines(&batch);
    let after = super::compiled_frame_line_counts();
    assert_eq!(super::FRAME_LINE_SLOT_NAMES[8], "innermost-unrecorded");
    for slot in 0..super::FRAME_LINE_SLOTS {
        assert!(after[slot] - before[slot] >= batch[slot], "{slot}");
    }
}
