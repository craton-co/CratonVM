use super::PdUrlMemo;
use std::sync::Arc;

/// A row answers only in the epoch it was filed in; a later epoch starts
/// over; a `None` URL is a row too; the table clears at its cap.
#[test]
fn rows_answer_only_in_their_own_epoch() {
    let mut m = PdUrlMemo::default();
    m.insert(3, 0x1000, Some(Arc::from("file:/a.jar")));
    m.insert(3, 0x2000, None);
    assert_eq!(m.get(3, 0x1000).flatten().as_deref(), Some("file:/a.jar"));
    assert_eq!(m.get(3, 0x2000), Some(None));
    assert_eq!(m.get(4, 0x1000), None, "another epoch");
    m.insert(4, 0x3000, None);
    assert_eq!(m.get(4, 0x1000), None, "the old epoch's rows are gone");
    for i in 0..PdUrlMemo::CAP {
        m.insert(4, 0x10_0000 + i, None);
    }
    assert!(m.rows.len() <= PdUrlMemo::CAP);
}
