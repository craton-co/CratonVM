fn body_of(src: &str, sig: &str) -> String {
    let start = src.find(sig).expect("the function exists");
    let rest = &src[start + 1..];
    rest[..rest.find("\n\x7d\n").map_or(rest.len(), |e| e + 3)].to_string()
}

#[test]
fn the_thin_set_helper_declines_a_set_override_before_it_stores() {
    let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let body = body_of(&src, "\npub unsafe fn jit_thread_local_set_direct_body(");
    let ask = body
        .find("tl_set_receiver_ok(")
        .expect("the override is asked");
    let find = body.find("tl_find_entry(").expect("the probe");
    let store = body.find("heap.set_field(entry").expect("the store");
    assert!(ask < find && find < store);
    let miss = body_of(&src, "\npub unsafe fn tl_miss_private_ok(");
    assert!(miss.contains("if for_set && !tl_set_receiver_ok(vm, &l, cid)"));
    let ok = body_of(&src, "\npub fn tl_set_receiver_ok(");
    assert!(ok.contains("!tl_class_declares_set(vm, l, cid)"));
}
