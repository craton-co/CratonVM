use super::*;

fn strings(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|s| s.to_string()).collect()
}

#[test]
fn i138_l1b_vm_uptime_date_option_and_text() {
    assert_eq!(vm_uptime_date_option(&strings(&["-date"])), Some(true));
    assert_eq!(vm_uptime_date_option(&strings(&["-date=true"])), Some(true));
    assert_eq!(vm_uptime_date_option(&strings(&["-date=false"])), Some(false));
    assert_eq!(vm_uptime_date_option(&strings(&[])), None);
    assert_eq!(vm_uptime_date_option(&strings(&["date=false"])), None);
    assert_eq!(
        hotspot_vm_uptime_date_text("2026-10-21T12:52:23.074-0300", 2.886),
        "2026-10-21T12:52:23.074-0300: 2.886 s\n"
    );
    // `yyyy-MM-ddTHH:mm:ss.SSS±HHMM`, 28 characters.
    let stamp = dcmd_iso8601_local_time();
    let b = stamp.as_bytes();
    assert_eq!(b.len(), 28, "{stamp}");
    assert_eq!((b[4], b[7], b[10], b[13], b[16], b[19]), (b'-', b'-', b'T', b':', b':', b'.'));
    assert!(b[23] == b'+' || b[23] == b'-', "{stamp}");
    assert!(stamp[24..].bytes().all(|c| c.is_ascii_digit()), "{stamp}");
}
