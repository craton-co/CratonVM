use super::*;

#[test]
fn read_u8() {
    let data = [0xCA];
    let mut buf = ClassFileBuffer::new(&data);
    assert_eq!(buf.read_u8().unwrap(), 0xCA);
}

#[test]
fn read_u16_big_endian() {
    let data = [0xCA, 0xFE];
    let mut buf = ClassFileBuffer::new(&data);
    assert_eq!(buf.read_u16().unwrap(), 0xCAFE);
}

#[test]
fn read_u32_big_endian() {
    let data = [0xCA, 0xFE, 0xBA, 0xBE];
    let mut buf = ClassFileBuffer::new(&data);
    assert_eq!(buf.read_u32().unwrap(), 0xCAFEBABE);
}

#[test]
fn read_past_end_returns_error() {
    let data = [0x01];
    let mut buf = ClassFileBuffer::new(&data);
    assert!(buf.read_u16().is_err());
}

#[test]
fn read_bytes_slice() {
    let data = [1, 2, 3, 4, 5];
    let mut buf = ClassFileBuffer::new(&data);
    let bytes = buf.read_bytes(3).unwrap();
    assert_eq!(bytes, &[1, 2, 3]);
    assert_eq!(buf.position(), 3);
}

/// Regression: `read_bytes` / `skip` must NOT panic or silently
/// succeed when `position + count` overflows `usize`.  This is the
/// `usize::MAX - 1` + `count == 3` corner case called out by the
/// round-7 audit (MED #7) — `checked_add` returns `None`, which
/// we map to `UnexpectedEndOfData` instead of wrapping.
///
/// Round-9 fix (CRIT-3): the previous version of this test used
/// `count == 2`, which yields `pos + count == usize::MAX` — a
/// valid (non-overflowing) `checked_add` result that then fails
/// the *bounds* check, not the *overflow* check.  Using `count == 3`
/// makes `pos + count` actually overflow so we exercise the
/// `checked_add` path the comment claims to test.
#[test]
fn read_bytes_overflow_returns_error() {
    let data = [0u8; 4];
    let mut buf = ClassFileBuffer::new(&data);
    // Manually drive position to near-MAX to exercise the overflow
    // branch.  We can't actually have a buffer this large, so the
    // bounds check below would also fail — but `checked_add` MUST
    // fire FIRST so that `pos + count` never wraps to a small `end`.
    buf.position = usize::MAX - 1;
    // pos + 3 overflows usize::MAX → checked_add returns None.
    assert!(matches!(buf.read_bytes(3), Err(_)));
    // `read_bytes` failure leaves position untouched so the next
    // call observes the same overflow.
    assert_eq!(buf.position(), usize::MAX - 1);
    assert!(matches!(buf.skip(3), Err(_)));
    assert_eq!(buf.position(), usize::MAX - 1);
}
