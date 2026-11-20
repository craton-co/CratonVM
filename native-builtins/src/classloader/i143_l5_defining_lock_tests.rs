use super::*;

/// The shared side and the exclusive side read the same rows, and a
/// shared reader does not keep a later writer out once it is dropped.
#[test]
fn both_sides_read_what_the_writer_wrote() {
    let lock = DefiningLock::new(DefiningLoaderTable::default());
    let loader = unsafe { ObjectRef::from_raw(0x1000 as *mut u8) };
    lock.lock().unwrap_or_else(|e| e.into_inner()).insert(7, loader);
    let shared = DefiningRead::Shared(lock.inner.read().unwrap_or_else(|e| e.into_inner()));
    assert_eq!(shared.rows.get(&7).map(|o| o.as_ptr() as usize), Some(0x1000));
    drop(shared);
    let exclusive = DefiningRead::Exclusive(lock.lock().unwrap_or_else(|e| e.into_inner()));
    assert_eq!(exclusive.rows.get(&7).map(|o| o.as_ptr() as usize), Some(0x1000));
    drop(exclusive);
    lock.lock().unwrap_or_else(|e| e.into_inner()).insert(8, loader);
    assert_eq!(lock.inner.read().unwrap().rows.len(), 2);
}
