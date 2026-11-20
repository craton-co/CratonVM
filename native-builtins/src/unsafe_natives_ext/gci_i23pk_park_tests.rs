use super::unsafe_park_relative_raw_timeout_field;

const SRC: &str = concat!(include_str!("../unsafe_natives_ext/mod.rs"), "\n", include_str!("../unsafe_natives_ext/unsafe_offset_is_heap_slot.rs"), "\n", include_str!("../unsafe_natives_ext/native_unsafe_get_long_volatile.rs"), "\n", include_str!("../unsafe_natives_ext/unsafe_array_read_bytes.rs"));

fn body_of(src: &'static str, sig: &str) -> &'static str {
    let start = src.find(sig).unwrap_or_else(|| panic!("`{sig}` not found"));
    let len = src[start..].find("\n\u{7d}\n").expect("its end");
    &src[start..start + len]
}

#[test]
fn gci_i23pk_only_a_negative_relative_time_is_carried_raw() {
    assert_eq!(unsafe_park_relative_raw_timeout_field(-1), Some(-1));
    assert_eq!(
        unsafe_park_relative_raw_timeout_field(i64::MIN),
        Some(i64::MIN)
    );
    assert_eq!(unsafe_park_relative_raw_timeout_field(0), None);
    assert_eq!(unsafe_park_relative_raw_timeout_field(1), None);
    assert_eq!(unsafe_park_relative_raw_timeout_field(i64::MAX), None);
    let door = body_of(SRC, "fn unsafe_park_door(");
    let abs = door.find("if is_absolute \u{7b}").expect("the absolute arm");
    let raw = door
        .find("ctx.park_relative_reporting_raw(timeout, raw);")
        .expect("the raw arm");
    let plain = door
        .find("ctx.park_reporting_timeout(timeout);")
        .expect("the plain arm");
    assert!(abs < raw && raw < plain, "{door}");
}
