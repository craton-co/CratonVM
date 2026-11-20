//! JIT round 19 proposal wave 9, lane gc9 (GC819-2): the anchor of the
//! return address the innermost-mirror check found, against the frame's
//! active safepoint id, over synthetic artifacts.
use super::*;

/// A single-pass artifact with anchored maps `(native_pc_offset, id)`.
fn sp_artifact(maps: &[(u32, u32)]) -> cratonvm_jit::CompiledMethod {
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit_byte(0xC3);
    let mut cm = cratonvm_jit::CompiledMethod::new(buf);
    cm.sp_id_slot_off = 8;
    cm.method_label = "p/S.run:()V".to_string();
    for &(at, id) in maps {
        let mut m = cratonvm_jit::OopMapEntry::new(at);
        m.bytecode_pc = id;
        cm.push_oop_map(m);
    }
    cm
}

#[test]
fn a_return_address_at_or_before_the_active_anchor_is_anchored() {
    let cm = sp_artifact(&[(0x40, 3), (0x80, 9), (0xc0, 3)]);
    assert_eq!(
        mirror_return_anchor(&cm, Some(9), 0x80),
        MirrorAnchor::Exact
    );
    assert_eq!(
        mirror_return_anchor(&cm, Some(3), 0xc0),
        MirrorAnchor::Exact
    );
    // Inside the shadow-reload window before the id's own anchor.
    assert_eq!(
        mirror_return_anchor(&cm, Some(9), 0x70),
        MirrorAnchor::Window
    );
    assert_eq!(
        mirror_return_anchor(&cm, Some(3), 0x10),
        MirrorAnchor::Window
    );
}

#[test]
fn a_return_address_another_ids_anchor_follows_is_foreign() {
    let cm = sp_artifact(&[(0x40, 3), (0x80, 9), (0xc0, 3)]);
    // Before 0x80, which is id 9's: the frame's slot says 3.
    assert_eq!(
        mirror_return_anchor(&cm, Some(3), 0x70),
        MirrorAnchor::Foreign
    );
    // Past every anchor.
    assert_eq!(
        mirror_return_anchor(&cm, Some(3), 0xd0),
        MirrorAnchor::Foreign
    );
    // An id no map carries, and no id at all.
    assert_eq!(
        mirror_return_anchor(&cm, Some(5), 0x40),
        MirrorAnchor::NoActiveMap
    );
    assert_eq!(
        mirror_return_anchor(&cm, None, 0x40),
        MirrorAnchor::NoActiveMap
    );
}


#[test]
fn the_call_out_scan_answers_the_highest_word_it_found() {
    const CM: usize = 0x5000_0000;
    let mut words = [0usize; 32];
    words[10] = CM + 0x40;
    words[16] = CM + 0x80;
    let base = words.as_ptr() as usize;
    let rbp = base + 8 * 24;
    let found = innermost_mirror_call_out_with(
        rbp,
        base,
        CM,
        |addr| words[(addr - base) / 8],
        |w| (CM..CM + 0x1000).contains(&w).then_some(CM),
        |_| true,
    );
    assert_eq!(found, Ok(CM + 0x80));
}
