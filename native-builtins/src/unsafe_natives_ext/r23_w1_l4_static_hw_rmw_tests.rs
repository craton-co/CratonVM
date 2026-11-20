fn body<'a>(src: &'a str, sig: &str) -> &'a str {
    let start = src.find(sig).unwrap_or_else(|| panic!("{sig} moved"));
    let rest = &src[start..];
    let end = rest.find("\n}\n").expect("function end");
    &rest[..end]
}

#[test]
fn static_rmw_paths_ask_the_hardware_update_first() {
    const SRC: &str = concat!(include_str!("../unsafe_natives_ext/mod.rs"), "\n", include_str!("../unsafe_natives_ext/unsafe_offset_is_heap_slot.rs"), "\n", include_str!("../unsafe_natives_ext/native_unsafe_get_long_volatile.rs"), "\n", include_str!("../unsafe_natives_ext/unsafe_array_read_bytes.rs"));
    for sig in [
        "fn unsafe_static_get_and_add_long(",
        "fn unsafe_static_get_and_set_long(",
        "fn unsafe_static_get_and_add_int(",
        "fn unsafe_static_get_and_set_int(",
        "fn unsafe_static_get_and_set_object(",
        "fn unsafe_static_cas(",
    ] {
        let b = body(SRC, sig);
        let hw = b.find("unsafe_static_rmw(").unwrap_or_else(|| panic!("{sig}: no hw"));
        let plain = b
            .find("ctx.get_static_field(")
            .unwrap_or_else(|| panic!("{sig}: no plain read"));
        assert!(hw < plain, "{sig}: the hardware update must come first");
    }
    const LIB: &str = include_str!("../lib_parts/native_printwriter_flush.rs");
    let b = body(LIB, "pub fn synthetic_cas(");
    let hw = b.find("unsafe_static_rmw(").expect("synthetic_cas: no hw");
    let plain = b.find("ctx.get_static_field(").expect("synthetic_cas: no plain read");
    assert!(hw < plain, "synthetic_cas: the hardware update must come first");
}
