use super::*;

/// gci i3/hd: `<secs>[,continue|,repeat]`; the old `<secs>` form and a
/// bad value keep their meaning (abort, 7 s default).
#[test]
fn gci_i3hd_hangwalk_spec_parses() {
    assert_eq!(parse_spec("120"), (120, AfterDump::Abort));
    assert_eq!(parse_spec(" 30 "), (30, AfterDump::Abort));
    assert_eq!(parse_spec("30,continue"), (30, AfterDump::Continue));
    assert_eq!(parse_spec("30, repeat"), (30, AfterDump::Repeat));
    assert_eq!(parse_spec("30,bogus"), (30, AfterDump::Abort));
    assert_eq!(parse_spec("x"), (7, AfterDump::Abort));
    assert_eq!(parse_spec("0,continue"), (7, AfterDump::Continue));
    assert_eq!(parse_spec(""), (7, AfterDump::Abort));
}

/// gci i3/hd: words are read from the copy only, little-endian, and
/// nothing outside it.
#[test]
fn gci_i3hd_copied_word_reads_inside_the_copy_only() {
    let mut bytes = vec![0u8; 24];
    bytes[8..16].copy_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes());
    assert_eq!(copied_word(0x1000, &bytes, 0x1008), Some(0x1122_3344_5566_7788));
    assert_eq!(copied_word(0x1000, &bytes, 0x1010), Some(0));
    assert_eq!(copied_word(0x1000, &bytes, 0x1011), None, "would cross the end");
    assert_eq!(copied_word(0x1000, &bytes, 0x0ff8), None, "below the copy");
    assert_eq!(copied_word(0x1000, &[], 0x1000), None);
    assert_eq!(copied_word(0, &bytes, usize::MAX), None, "no overflow");
}
