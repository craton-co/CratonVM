use super::*;


/// The tag rides in an address bit no cell can use, and masks back to
/// the cell, whose two ways start at 0 -- below `0x8000_0000`, so no
/// proxy id matches an unwritten way.
#[test]
fn the_tag_is_a_free_bit_of_a_cell_address() {
    assert!(
        std::mem::align_of::<[std::sync::atomic::AtomicU32; 2]>() > INSTANCEOF_PROXY_CELL_TAG
    );
    let cell = Box::new([
        std::sync::atomic::AtomicU32::new(0),
        std::sync::atomic::AtomicU32::new(0),
    ]);
    let addr = cell.as_ptr() as usize;
    assert_eq!(addr & INSTANCEOF_PROXY_CELL_TAG, 0);
    let tagged = addr | INSTANCEOF_PROXY_CELL_TAG;
    assert_ne!(tagged, 0);
    assert_eq!(tagged & !INSTANCEOF_PROXY_CELL_TAG, addr);
    assert!(i32::from_ne_bytes(GETCLASS_WORDS_LAMBDA_BASE_ID.to_ne_bytes()) < 0);
    assert!(i32::from_ne_bytes(0u32.to_ne_bytes()) >= 0);
}
