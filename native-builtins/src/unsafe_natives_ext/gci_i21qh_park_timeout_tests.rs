use super::unsafe_park_timeout;
use std::time::Duration;

const NOW: i64 = 1_760_000_000_000; // an epoch millisecond in 2025

#[test]
fn gci_i21qh_an_absolute_zero_returns_at_once_not_never() {
    assert_eq!(unsafe_park_timeout(true, 0, NOW), Some(Duration::ZERO));
}

#[test]
fn gci_i21qh_a_relative_zero_is_untimed() {
    assert_eq!(unsafe_park_timeout(false, 0, NOW), None);
}

#[test]
fn gci_i21qh_negative_times_return_at_once_in_both_forms() {
    assert_eq!(unsafe_park_timeout(false, -1, NOW), Some(Duration::ZERO));
    assert_eq!(unsafe_park_timeout(true, -1, NOW), Some(Duration::ZERO));
    // No overflow (a debug panic before i21).
    assert_eq!(unsafe_park_timeout(true, i64::MIN, NOW), Some(Duration::ZERO));
    assert_eq!(unsafe_park_timeout(false, i64::MIN, NOW), Some(Duration::ZERO));
}

#[test]
fn gci_i21qh_absolute_deadlines_wait_out_the_rest() {
    assert_eq!(
        unsafe_park_timeout(true, NOW + 250, NOW),
        Some(Duration::from_millis(250))
    );
    assert_eq!(unsafe_park_timeout(true, NOW - 5, NOW), Some(Duration::ZERO));
    assert_eq!(unsafe_park_timeout(true, NOW, NOW), Some(Duration::ZERO));
    // A far deadline saturates instead of wrapping.
    assert_eq!(
        unsafe_park_timeout(true, i64::MAX, -1),
        Some(Duration::from_millis(i64::MAX as u64))
    );
}

#[test]
fn gci_i21qh_relative_nanos_are_kept() {
    assert_eq!(
        unsafe_park_timeout(false, 50_000, NOW),
        Some(Duration::from_nanos(50_000))
    );
}

/// Every absolute park takes the absolute door (an absolute 0 included).
#[test]
fn gci_i21qh_every_absolute_park_takes_the_until_door() {
    const SRC: &str = concat!(include_str!("../unsafe_natives_ext/mod.rs"), "\n", include_str!("../unsafe_natives_ext/unsafe_offset_is_heap_slot.rs"), "\n", include_str!("../unsafe_natives_ext/native_unsafe_get_long_volatile.rs"), "\n", include_str!("../unsafe_natives_ext/unsafe_array_read_bytes.rs"));
    let start = SRC.find("fn unsafe_park_door(").expect("the door");
    let len = SRC[start..].find("\n\u{7d}\n").expect("its end");
    let door = &SRC[start..start + len];
    assert!(door.contains("    if is_absolute \u{7b}\n"), "{door}");
    assert!(!door.contains("time != 0"), "{door}");
}
