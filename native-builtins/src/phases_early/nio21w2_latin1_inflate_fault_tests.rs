use super::{latin1_inflate_fault, Latin1InflateFault as F};

/// The bytecode's failure order: nothing for `len <= 0` (null arrays
/// included); `src` before `dst` within one iteration; the earlier
/// iteration wins across the two arrays, after copying what precedes it.
#[test]
fn the_fault_is_where_the_bytecode_first_fails() {
    assert_eq!(latin1_inflate_fault(None, 0, None, 0, 0), (0, None));
    assert_eq!(latin1_inflate_fault(None, 0, None, 0, -3), (0, None));
    assert_eq!(latin1_inflate_fault(Some(8), 0, Some(8), 0, 8), (8, None));
    assert_eq!(
        latin1_inflate_fault(None, 0, None, 0, 1),
        (0, Some(F::SrcNull))
    );
    assert_eq!(
        latin1_inflate_fault(None, 0, Some(4), 0, 1),
        (0, Some(F::SrcNull))
    );
    assert_eq!(
        latin1_inflate_fault(Some(4), 0, None, 0, 1),
        (0, Some(F::DstNull))
    );
    // A null dst behind a bad src offset: the src load fails first.
    assert_eq!(
        latin1_inflate_fault(Some(4), 9, None, 0, 1),
        (0, Some(F::Range(9, 4)))
    );
    assert_eq!(
        latin1_inflate_fault(Some(4), -1, Some(4), 0, 1),
        (0, Some(F::Range(-1, 4)))
    );
    assert_eq!(
        latin1_inflate_fault(Some(4), 0, Some(4), -2, 1),
        (0, Some(F::Range(-2, 4)))
    );
    // src runs out at i = 2, dst at i = 3: two chars copied, then src.
    assert_eq!(
        latin1_inflate_fault(Some(5), 3, Some(6), 3, 4),
        (2, Some(F::Range(5, 5)))
    );
    // dst runs out first.
    assert_eq!(
        latin1_inflate_fault(Some(9), 0, Some(3), 1, 4),
        (2, Some(F::Range(3, 3)))
    );
    // Same iteration: the src load comes first.
    assert_eq!(
        latin1_inflate_fault(Some(4), 2, Some(4), 2, 3),
        (2, Some(F::Range(4, 4)))
    );
}
