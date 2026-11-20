use super::*;

/// A misaligned locked CAS answers the bytes it saw, writes only its own
/// bytes, and refuses on a stale expected value, at every width.
#[test]
fn nio21_w6_misaligned_locked_cas_is_exact() {
    for width in [2usize, 4, 8] {
        let mut buf = [0u8; 32];
        for (i, b) in buf.iter_mut().enumerate() {
            *b = i as u8;
        }
        let p = unsafe { buf.as_mut_ptr().add(3) };
        let mask = if width == 8 {
            u64::MAX
        } else {
            (1u64 << (width * 8)) - 1
        };
        let mut want = [0u8; 8];
        want[..width].copy_from_slice(&buf[3..3 + width]);
        let before = u64::from_le_bytes(want);
        let got = unsafe { x86_locked_load(p, width) } & mask;
        assert_eq!(got, before, "width {width} load");
        let seen = unsafe { x86_locked_cas(p, width, before ^ 1, 0) } & mask;
        assert_eq!(seen, before, "width {width} stale expected");
        let seen = unsafe { x86_locked_cas(p, width, before, mask) } & mask;
        assert_eq!(seen, before, "width {width} success");
        assert!(
            buf[3..3 + width].iter().all(|b| *b == 0xff),
            "width {width} wrote"
        );
        assert_eq!(buf[2], 2, "width {width} left neighbour");
        assert_eq!(
            buf[3 + width],
            (3 + width) as u8,
            "width {width} right neighbour"
        );
    }
    assert!(misaligned_access_splits_a_line(61, 4));
    assert!(!misaligned_access_splits_a_line(60, 4));
    assert!(!misaligned_access_splits_a_line(1, 8));
}
