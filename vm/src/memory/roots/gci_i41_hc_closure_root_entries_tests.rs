//! `cargo test -j 5 -p cratonvm-vm --lib gci_i41_hc_`: the census's pure
//! parts and its default.
use super::*;
use crate::threading::jvm_thread::SlotOrigin;


#[test]
fn gci_i41_hc_pre_collection_word_keeps_the_interior_offset() {
    // addr-keyed: a test fixture's post -> pre map.
    let mut pre_of: rustc_hash::FxHashMap<usize, usize> = rustc_hash::FxHashMap::default();
    pre_of.insert(0x2000, 0x9000);
    // A moved member: its pre-collection start plus the same offset.
    assert_eq!(closure_pre_collection_word(0x2010, 0x2000, &pre_of), 0x9010);
    assert_eq!(closure_pre_collection_word(0x2000, 0x2000, &pre_of), 0x9000);
    // A member of a kept region did not move.
    assert_eq!(closure_pre_collection_word(0x3008, 0x3000, &pre_of), 0x3008);
}

#[test]
fn gci_i41_hc_snapshot_positions_and_slot_origins() {
    assert_eq!(closure_snapshot_positions(&[1, 2, 1, 3], 1), vec![0, 2]);
    assert!(closure_snapshot_positions(&[1, 2], 9).is_empty());
    let origins = [
        SlotOrigin {
            frame: 3,
            idx: 1,
            is_stack: false,
            live: false,
            orig: 0x40,
            cur: 0x80,
        },
        SlotOrigin {
            frame: 0,
            idx: 2,
            is_stack: true,
            live: true,
            orig: 0x50,
            cur: 0x50,
        },
    ];
    assert_eq!(closure_slot_origin_token(&origins, 0x80), "frame#3 local[1] live=false");
    assert_eq!(closure_slot_origin_token(&origins, 0x50), "frame#0 stack[2] live=true");
    assert_eq!(closure_slot_origin_token(&origins, 0x60), "-");
}

#[test]
fn gci_i41_hc_section_label_names_the_door_s_tail() {
    assert_eq!(
        closure_root_section_label(INITIATOR_SCAN_END_SECTION),
        "after-initiator (registry snapshot or frozen peer)"
    );
    assert_eq!(
        closure_root_section_label("14: NEW-1.5 — conservative scan"),
        "14: NEW-1.5 — conservative scan"
    );
}
