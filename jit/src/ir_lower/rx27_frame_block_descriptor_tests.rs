use super::*;

/// The record the GC side decodes: header with the magic and the count,
/// then the offsets / 8, four per word.
#[test]
fn rx27_frame_block_descriptor_packs_offsets_for_the_gc_reader() {
    let block: [i16; 5] = [8, 16, 0x170, 0x258, 0x3A0];
    let words = frame_block_descriptor_words(&block).expect("packs");
    assert_eq!(words.len(), 1 + 2);
    assert_eq!(words[0] >> 32, FRAME_BLOCK_DESCRIPTOR_MAGIC);
    assert_eq!(words[0] & 0xFFFF_FFFF, 5);
    assert_eq!(words[1] & 0xFFFF, 1);
    assert_eq!((words[1] >> 16) & 0xFFFF, 2);
    assert_eq!((words[1] >> 32) & 0xFFFF, 0x170 / 8);
    assert_eq!((words[1] >> 48) & 0xFFFF, 0x258 / 8);
    assert_eq!(words[2], 0x3A0 / 8);
    // The GC's constants agree (header layout, tag).
    assert_eq!(cratonvm_gc::shadow_stack::DESCRIPTOR_MAGIC, FRAME_BLOCK_DESCRIPTOR_MAGIC);
    assert_eq!(cratonvm_gc::shadow_stack::DESCRIPTOR_TAG as i32, FRAME_BLOCK_DESCRIPTOR_TAG);
    assert_eq!(
        cratonvm_gc::shadow_stack::DESCRIPTOR_MAX_HOMES,
        FRAME_BLOCK_DESCRIPTOR_MAX_HOMES
    );
    assert_eq!(
        cratonvm_gc::shadow_stack::descriptor_header(5),
        words[0],
        "the GC's header for five homes"
    );
}

#[test]
fn rx27_a_home_the_record_cannot_carry_keeps_the_indirect_block() {
    assert!(frame_block_descriptor_words(&[]).is_none());
    assert!(frame_block_descriptor_words(&[8, 12]).is_none(), "not a multiple of 8");
    assert!(frame_block_descriptor_words(&[8, -8]).is_none(), "above rbp");
    assert!(frame_block_descriptor_words(&[0]).is_none());
}

