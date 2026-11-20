//! j27 wave 8 (lane W8INTR): which rows of a published JDWP listing the
//! OSR'd-frame home read serves, by frame id.
use super::*;

fn entry(frame_id: u64) -> crate::debug::FrameEntry {
    crate::debug::FrameEntry {
        frame_id,
        class_id: 7,
        method_id: 9,
        offset: 12,
    }
}

fn raw(frame: Option<usize>, runs_compiled: bool, location: i64) -> super::super::BlockedRow {
    super::super::BlockedRow {
        class_id: 7,
        method_id: 9,
        location,
        frame,
        runs_compiled,
        chain: None,
    }
}

#[test]
fn j27_w8intr_only_an_osr_frame_of_the_listing_is_a_jdwp_home_row() {
    // Top first: a native head, a compiled activation (published rows
    // carry no chain), an OSR'd frame (interpreter frame 1) and a plain
    // interpreter frame 0.
    let entries = [entry(40), entry(41), entry(42), entry(43)];
    let rows = vec![
        raw(None, false, -1),
        raw(None, false, 5),
        raw(Some(1), true, 12),
        raw(Some(0), false, 3),
    ];
    let got = osr_home_rows(&entries, rows);
    assert_eq!(got.len(), 1, "{got:?}");
    let (frame_id, row) = got[0];
    assert_eq!(frame_id, 42);
    assert_eq!(row.activation, HomeActivation::OsrBody { frame: 1 });
    assert_eq!((row.class_id, row.method_id, row.location), (7, 9, 12));
}

#[test]
fn j27_w8intr_an_osr_row_without_a_frame_index_is_not_read() {
    // `published_rows` without a depth names no interpreter frame: the
    // read must not guess one.
    let got = osr_home_rows(&[entry(42)], vec![raw(None, true, 12)]);
    assert!(got.is_empty(), "{got:?}");
}
