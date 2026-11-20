use super::LeafVerdictMemo;
use std::cell::Cell;
use std::sync::Arc;

#[test]
fn a_body_is_scanned_once_and_a_stranger_every_time() {
    let memo = LeafVerdictMemo::default();
    let code: Arc<[u8]> = Arc::from(&[0x1a_u8, 0xac][..]);
    let scans = Cell::new(0u32);
    let scan = |answer: bool| {
        scans.set(scans.get() + 1);
        answer
    };
    assert!(memo.verdict(&code, || scan(true)));
    assert!(memo.verdict(&code, || scan(false)), "the memoized answer");
    assert_eq!(scans.get(), 1);
    // A different body: its own slot, or a shared one it does not own.
    let other: Arc<[u8]> = Arc::from(&[0x2a_u8, 0xb6, 0, 1, 0xb1][..]);
    assert!(!memo.verdict(&other, || scan(false)));
    let before = scans.get();
    let again = memo.verdict(&other, || scan(false));
    assert!(!again);
    assert!(scans.get() <= before + 1);
    // The same bytes in another allocation are another body.
    let copy: Arc<[u8]> = Arc::from(&code[..]);
    let before = scans.get();
    let _ = memo.verdict(&copy, || scan(true));
    assert_eq!(scans.get(), before + 1, "not answered from `code`'s slot");
}
