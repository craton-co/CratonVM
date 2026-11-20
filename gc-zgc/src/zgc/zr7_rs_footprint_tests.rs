use super::*;

const MIB: usize = 1024 * 1024;


/// The advice changes nothing a reader can see: a committed, written
/// granule keeps its bytes, an uncommitted one stays uncommitted. On the
/// owned fallback and off Linux the call reports `false`.
#[test]
fn zr7rs_no_huge_page_advice_keeps_contents_and_commit_state() {
    use cratonvm_gc_core::reservation::{HeapStore, GRANULE};
    let mut store = HeapStore::new(4 * GRANULE, "zr7rs-test");
    if !matches!(store, HeapStore::Reserved(_)) {
        // No reservation on this platform / switch: nothing to test.
        assert!(!store.advise_no_huge_pages());
        return;
    }
    assert!(store.commit_range(0, GRANULE));
    // SAFETY: the first granule is committed read-write and ours.
    unsafe { std::ptr::write_bytes(store.as_mut_ptr(), 0xA7, 4096) };
    let applied = store.advise_no_huge_pages();
    if !cfg!(target_os = "linux") {
        assert!(!applied);
    }
    // SAFETY: still committed; reading what was written.
    let b = unsafe { *store.as_ptr().add(100) };
    assert_eq!(b, 0xA7);
    assert!(store.is_committed_at(0));
    assert!(!store.is_committed_at(2 * GRANULE));
    let owned = HeapStore::Owned(vec![0u8; 4096]);
    assert!(!owned.advise_no_huge_pages());
}
