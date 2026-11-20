use super::*;


/// Text pin: the closure asks the policy questions the own-site ladder
/// asks, and `append_ir_inline_site` keeps `invokespecial` rows off the
/// helper (its decline edge re-dispatches virtually).
#[test]
fn jr21_tl2_the_spliced_bind_keeps_the_own_site_gates() {
    let src = concat!(
        include_str!("lib_parts/ir_nested_hot_cap_pcs.rs"),
        "\n",
        include_str!("lib_parts/ir_tier_attempt.rs")
    )
    .replace("\r\n", "\n");
    let closure = &src[src
        .find("let spliced_thin_helper = |cn: &str, mn: &str, desc: &str|")
        .expect("the closure")..];
    let arm = &closure[closure
        .find("spliced_thread_local_thin_cell(direct_helpers, cn, mn, desc)")
        .expect("the arm")..];
    let arm = &arm[..arm
        .find("is_unsafe_accessor_op(cn, mn, desc)")
        .expect("next arm")];
    for needle in [
        "(jdk_only || mn == \"get\") && thread_local_get_direct_enabled()",
        "direct_native_helper(",
        "direct_bind_admission",
        "note_thread_local_get_direct_site()",
    ] {
        assert!(arm.contains(needle), "{needle}");
    }
    assert!(src.contains("t.invoke_kind == 0\n                    || !thread_local_thin_row("));
}
