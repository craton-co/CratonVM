use super::*;

#[test]
fn new_view_shares_parent_arc() {
    let parent: Arc<[u8]> = Arc::from(vec![1u8, 2, 3, 4, 5, 6, 7, 8]);
    let strong_before = Arc::strong_count(&parent);
    let view = ByteView::try_new(Arc::clone(&parent), 2..6).expect("in-bounds range");
    assert_eq!(Arc::strong_count(&parent), strong_before + 1);
    assert_eq!(view.as_bytes(), &[3u8, 4, 5, 6][..]);
    assert_eq!(view.len(), 4);
    assert!(!view.is_empty());

    // Clone is a refcount bump, no extra allocation.
    let cloned = view.clone();
    assert_eq!(Arc::strong_count(&parent), strong_before + 2);
    assert_eq!(cloned.as_bytes(), view.as_bytes());
}

#[test]
fn deref_yields_slice() {
    let view = ByteView::from_vec(vec![10u8, 20, 30]);
    let slice: &[u8] = &view;
    assert_eq!(slice, &[10u8, 20, 30][..]);
    assert_eq!(view[0], 10);
    assert_eq!(view.len(), 3);
}

#[test]
fn from_slice_and_from_vec_round_trip() {
    let v = ByteView::from_slice(&[1u8, 2, 3]);
    assert_eq!(v.as_bytes(), &[1u8, 2, 3][..]);
    let v2 = ByteView::from_vec(vec![4, 5]);
    assert_eq!(v2.as_bytes(), &[4u8, 5][..]);
}

#[test]
fn empty_view_is_empty() {
    let v = ByteView::empty();
    assert!(v.is_empty());
    assert_eq!(v.len(), 0);
    assert_eq!(v.as_bytes(), &[] as &[u8]);
}

#[test]
fn to_arc_returns_independent_arc() {
    let parent: Arc<[u8]> = Arc::from(vec![1u8, 2, 3, 4]);
    let view = ByteView::try_new(Arc::clone(&parent), 1..3).expect("in-bounds range");
    let owned: Arc<[u8]> = view.to_arc();
    assert_eq!(&*owned, &[2u8, 3][..]);
    // owned is decoupled from parent.
    drop(view);
    drop(parent);
    assert_eq!(&*owned, &[2u8, 3][..]);
}

#[test]
#[should_panic]
#[allow(deprecated)]
fn new_panics_on_out_of_bounds_range() {
    let parent: Arc<[u8]> = Arc::from(vec![1u8, 2, 3]);
    let _ = ByteView::new(parent, 0..10);
}

#[test]
fn try_new_returns_err_for_invalid_ranges() {
    let parent: Arc<[u8]> = Arc::from(vec![1u8, 2, 3, 4]);
    // Out-of-bounds end produces an InvalidClassData error rather
    // than panicking — this is the primary contract that lets the
    // reader's hot path tolerate malformed offsets.
    let err = ByteView::try_new(Arc::clone(&parent), 0..10).unwrap_err();
    match err {
        ClassReaderError::InvalidClassData { message } => {
            assert!(
                message.contains("out of bounds"),
                "unexpected message: {message}"
            );
        }
        other => panic!("expected InvalidClassData, got {other:?}"),
    }
    // Inverted range.
    assert!(ByteView::try_new(Arc::clone(&parent), 3..1).is_err());
    // Valid range succeeds and yields the expected slice.
    let v = ByteView::try_new(Arc::clone(&parent), 1..3).expect("in-bounds range");
    assert_eq!(v.as_bytes(), &[2u8, 3][..]);
    // Whole-buffer and empty-at-end ranges are valid.
    assert!(ByteView::try_new(Arc::clone(&parent), 0..4).is_ok());
    assert!(ByteView::try_new(parent, 4..4).is_ok());
}

/// Direct OOB test required by the soundness task: passing an
/// `offset > buf.len()` must return Err (never panic).
#[test]
fn try_new_offset_past_end_returns_err() {
    let buf: Arc<[u8]> = Arc::from(vec![0u8; 4]);
    let err = ByteView::try_new(Arc::clone(&buf), 5..5).unwrap_err();
    assert!(matches!(err, ClassReaderError::InvalidClassData { .. }));
    let err = ByteView::try_new(buf, 2..100).unwrap_err();
    assert!(matches!(err, ClassReaderError::InvalidClassData { .. }));
}
