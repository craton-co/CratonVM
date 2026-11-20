use super::*;

fn fn_body<'a>(src: &'a str, start: &str) -> &'a str {
    let from = src.find(start).unwrap_or_else(|| panic!("{start}"));
    let to = from + src[from..].find("\n}").expect("end of fn");
    &src[from..to]
}

/// `Module.ensureNativeAccess`'s `<mod>`: named / unnamed, with and
/// without the caller's code source location.
#[test]
fn zt12df4_the_deny_text_is_hotspots() {
    assert_eq!(
        illegal_native_access_module_text(None, Some("file:/C:/probes/")),
        "an unnamed module (file:/C:/probes/)"
    );
    assert_eq!(
        illegal_native_access_module_text(None, None),
        "an unnamed module"
    );
    assert_eq!(
        illegal_native_access_module_text(Some("ffm.probe"), Some("file:/m/")),
        "module ffm.probe (file:/m/)"
    );
    assert_eq!(
        illegal_native_access_module_text(Some("ffm.probe"), None),
        "module ffm.probe"
    );
    // The refusal is `"Illegal native access from " + mod`: no colon and
    // no `Module.toString()` (`unnamed module @<hash>`) on the default arm.
    let src = concat!(include_str!("../panama/mod.rs"), "\n", include_str!("../panama/alloc_segment_carrier.rs"), "\n", include_str!("../panama/register_pe_memory_segment_on.rs"), "\n", include_str!("../panama/pe_of_array_alias.rs"), "\n", include_str!("../panama/pe_swap_width.rs"));
    let gate = fn_body(src, "fn restricted_caller_check(");
    let hotspot = gate
        .find("format!(\"Illegal native access from {description}\")")
        .expect("HotSpot's text");
    // zt t14/df5: the JDK's own `Module.ensureNativeAccess` (location and
    // all) answers first when it sees `deny`; the VM builder is the
    // fallback (t12 rows `zt12df4_f3` / `_c` printed no location).
    let jdk = gate
        .find("if illegal_native_access_reaches_jdk() {")
        .expect("the JDK's refusal first");
    let delegated = gate[jdk..]
        .find("jdk_module_ensure_native_access(")
        .map(|at| jdk + at)
        .expect("the JDK call");
    assert!(jdk < delegated && delegated < hotspot, "{gate}");
}


