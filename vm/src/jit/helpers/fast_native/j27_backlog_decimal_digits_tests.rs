fn body_of(src: &str, sig: &str) -> String {
    let start = src.find(sig).expect("the function exists");
    let rest = &src[start + 1..];
    rest[..rest.find("\n\x7d\n").map_or(rest.len(), |e| e + 3)].to_string()
}

#[test]
fn the_decimal_digits_helper_body_counts_nothing_per_call() {
    let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let body = body_of(
        &src,
        "\npub unsafe fn jit_decimal_digits_latin1_direct_body(",
    );
    assert!(body.contains("decimal_digits_latin1_serve(vm, value, index, buf)"));
    assert!(
        body.contains("jit_invoke_dispatch("),
        "the cold arm still runs the JDK body"
    );
    assert!(
        !body.contains("fetch_add"),
        "no per-call counter on the hot path"
    );
    assert!(
        !body.contains("DECIMAL_DIGITS_CENSUS"),
        "the census counts sites only"
    );
}
