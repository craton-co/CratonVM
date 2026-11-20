use super::*;

/// A hand-out forgets every anchor strictly inside its bytes (an earlier
/// layout's), keeps one at its base, and leaves anchors outside alone.
#[test]
fn r23_w4c_a_hand_out_forgets_the_anchors_inside_it() {
    let mut a = Arena::new(256 * 1024);
    let bucket = 1usize << a.anchor_shift;
    // Stale anchors from an "earlier layout" of bytes not yet bumped over.
    a.note_object_start(8);
    a.note_object_start(bucket + 16);
    a.note_object_start(5 * bucket);
    let p = a.alloc(2 * bucket, 8).expect("bump hand-out") as usize;
    let off = p - a.data.as_ptr() as usize;
    assert_eq!(off, 0, "a fresh arena bumps from its base");
    a.note_object_start(off);
    assert_eq!(
        a.alloc_anchors_snapshot(),
        vec![0, 5 * bucket],
        "8 and bucket+16 lay inside [0, 2 buckets): forgotten; the base and \
         the anchor past the hand-out stay"
    );
}

/// The hand-out's base is never forgotten, even when it is the anchor.
#[test]
fn r23_w4c_the_base_anchor_survives_its_own_hand_out() {
    let mut a = Arena::new(256 * 1024);
    a.note_object_start(0);
    let _ = a.alloc(4096, 8).expect("bump hand-out");
    assert_eq!(a.alloc_anchors_snapshot(), vec![0]);
}
