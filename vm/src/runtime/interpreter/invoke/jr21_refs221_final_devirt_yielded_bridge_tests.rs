#[test]
fn jr21_refs221_final_devirt_screen_asks_the_yielded_bridge_rule() {
    const END: &str = "\n\x7d\n";
    let here = include_str!("../invoke.rs").replace("\r\n", "\n");
    let production = &here[..here.find("\n#[cfg(test)]").unwrap_or(here.len())];
    let body = &production[production
        .find("fn final_devirt_native_shadow(")
        .expect("the screen")..];
    let body = &body[..body.find(END).expect("end of the screen")];
    assert!(body.contains("let yielded_bridge_rule =") && body.contains("true;"));
    assert!(body.contains("bridge_yields_to_loaded_bytecode_for_gates("));
    assert!(body.contains("find_with_kind(owner, method_name, descriptor)"));
    // Both halves ask through the one closure: the exact-name loop and the
    // subclass walk (the third match is the closure's own binding).
    assert_eq!(body.matches("shadows(").count(), 2, "{body}");
    assert!(body.contains("let shadows = |owner: &str|"));
}
