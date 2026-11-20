/// Round 13 wave 11 (lane mega9, M13-6): the optimizing tier's copy of the
/// "can an array be this receiver" rule (`inline_cache_pic`, which
/// `ir_lower.rs` reads) must answer exactly what the single-pass cascade's
/// `x64::inlining::static_receiver_admits_arrays` answers, or the two tiers
/// disagree about which sites may skip the kind screen.
#[test]
fn the_optimizing_tier_copy_agrees_with_the_single_pass_rule() {
    for owner in [
        "",
        "java/lang/Object",
        "java/lang/Cloneable",
        "java/io/Serializable",
        "[I",
        "[Ljava/lang/String;",
        "[[J",
        "java/lang/String",
        "java/util/HashMap",
        "java/util/AbstractMap",
        "pkg/Obj",
        "java/lang/Objectx",
        "java/lang/Number",
    ] {
        assert_eq!(
            crate::inline_cache_pic::ic_static_receiver_admits_arrays(owner),
            crate::x64::inlining::static_receiver_admits_arrays(owner),
            "owner {owner:?}"
        );
    }
}
