use super::*;
use std::sync::atomic::AtomicUsize;

#[test]
fn r22_memory_box_cache_slot_reads_the_table_by_its_own_length() {
    // A raised `IntegerCache.high` of 300: 429 slots.
    let table: Vec<AtomicUsize> = (0..429).map(|_| AtomicUsize::new(0)).collect();
    table[(127 + 128) as usize].store(0xABC0, std::sync::atomic::Ordering::Release);
    let raw = Some((table.as_ptr() as usize, table.len()));
    assert_eq!(box_cache_slot(None, 5), BoxCacheSlot::NoTable);
    match box_cache_slot(raw, 127) {
        BoxCacheSlot::Slot { current, .. } => assert_eq!(current, 0xABC0),
        other => panic!("127 is cached: {other:?}"),
    }
    match box_cache_slot(raw, 300) {
        BoxCacheSlot::Slot { addr, current } => {
            assert_eq!(current, 0, "an empty slot");
            assert_eq!(addr, table.as_ptr() as usize + 428 * 8);
        }
        other => panic!("300 is the raised top: {other:?}"),
    }
    assert_eq!(box_cache_slot(raw, 301), BoxCacheSlot::OutOfRange);
    assert_eq!(box_cache_slot(raw, -129), BoxCacheSlot::OutOfRange);
    assert_eq!(box_cache_slot(raw, i64::MIN), BoxCacheSlot::OutOfRange);
    match box_cache_slot(raw, -128) {
        BoxCacheSlot::Slot { addr, .. } => assert_eq!(addr, table.as_ptr() as usize),
        other => panic!("-128 is slot 0: {other:?}"),
    }
}

