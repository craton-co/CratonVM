use super::iis_read_argument_error;
use cratonvm_types::error::RuntimeError;

/// Interpreter round i3 wave 80, lane L5: the `InflaterInputStream`
/// `read([BII)` bridge's checks are JDK 25's, in its order (`ensureOpen`,
/// the null buffer, `Objects.checkFromIndexSize`).
#[test]
fn i80_the_read_bridge_checks_closed_then_null_then_the_range() {
    // Closed first, whatever else is wrong.
    assert!(matches!(
        iis_read_argument_error(true, None, -1, -1),
        Some(RuntimeError::IOException { message }) if message == "Stream closed"
    ));
    // A null buffer: a bare NPE, not `-1`.
    assert!(matches!(
        iis_read_argument_error(false, None, 0, 4),
        Some(RuntimeError::NullPointerException { message: None })
    ));
    // A bad range: `Preconditions.outOfBoundsCheckFromIndexSize`'s text.
    for (off, len, n) in [(-1, 2, 8usize), (2, -1, 8), (6, 3, 8), (9, 0, 8)] {
        match iis_read_argument_error(false, Some(n), off, len) {
            Some(RuntimeError::IndexOutOfBoundsException { message: Some(m) }) => assert_eq!(
                m,
                format!("Range [{off}, {off} + {len}) out of bounds for length {n}")
            ),
            _ => panic!("({off}, {len}, {n}) must be an IndexOutOfBoundsException"),
        }
    }
    // In range, including an empty read at the end.
    assert!(iis_read_argument_error(false, Some(8), 0, 8).is_none());
    assert!(iis_read_argument_error(false, Some(8), 8, 0).is_none());
    assert!(iis_read_argument_error(false, Some(0), 0, 0).is_none());
}
