use super::inflater_advance;

#[test]
fn ordinary_progress_advances_the_cursor() {
    assert_eq!(inflater_advance(10, 0, 4, 100, false), (4, false));
    assert_eq!(inflater_advance(10, 4, 6, 20, false), (10, false));
}

#[test]
fn stream_end_marks_finished_without_swallowing_the_tail() {
    // A concatenated stream (jar entry followed by the next LOC header)
    // must leave the unconsumed bytes for the caller to re-read.
    assert_eq!(inflater_advance(10, 0, 6, 50, true), (6, true));
}

#[test]
fn no_progress_consumes_the_pending_input_so_needsinput_turns_true() {
    // The spin: bytes pending, nothing produced, not finished. `pos` must
    // reach the end or `InflaterInputStream.read` loops forever.
    assert_eq!(inflater_advance(10, 2, 0, 0, false), (10, false));
    // Already drained: unchanged, still not finished.
    assert_eq!(inflater_advance(10, 10, 0, 0, false), (10, false));
}
