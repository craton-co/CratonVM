use super::*;

/// `java/lang/String` is `final`, so `invokevirtual_site_final_owner`
/// answers for every one of its call sites and `try_compile_inner` used to
/// rewrite `invoke_kind` 0 -> 1 on that answer — putting the site into the
/// inline/direct-bind ladder, which `continue`s, past an instance-intrinsic
/// gate that is `invoke_kind == 0 || invoke_kind == 2`.
///
/// The result was a real `CALL` into `String.charAt` on every character,
/// and it was invisible: the three `string-intrinsic` diagnostics all sit
/// past the point the site left. Measured on `probes/CharAtDoorProbe.java`
/// at 349.64 ns/char against 3.2-4.3 for four byte-identical siblings the
/// OSR door compiled.
#[test]
fn the_three_string_accessors_hold_the_site_back_from_a_static_bind() {
    let layout = Some(STRING_INTRINSIC_NAME_PROBE);
    for (name, desc) in [("charAt", "(I)C"), ("length", "()I"), ("isEmpty", "()Z")] {
        assert!(
            site_yields_to_call_site_intrinsic("java/lang/String", name, desc, layout),
            "String.{name}{desc} must keep its call-site intrinsic"
        );
    }
}

/// The JVMS 5.4.6 rule the rewrite exists for is untouched, and this is why:
/// no intrinsic matches a private method, so a private target never yields
/// and stays pinned to its declaring class. `String.isLatin1()Z` is the
/// concrete one — private, and reached constantly from the very chain the
/// missing intrinsic sends the program down.
#[test]
fn a_private_target_never_yields_so_the_dispatch_rule_is_intact() {
    let layout = Some(STRING_INTRINSIC_NAME_PROBE);
    for (name, desc) in [
        ("isLatin1", "()Z"),
        ("coder", "()B"),
        ("checkIndex", "(II)V"),
    ] {
        assert!(
            !site_yields_to_call_site_intrinsic("java/lang/String", name, desc, layout),
            "String.{name}{desc} is not an intrinsic and must stay statically bound"
        );
    }
}

/// Without a resolved `StringFieldLayout` the intrinsic cannot be emitted
/// either, so there is nothing to hold the site back FOR — it keeps the
/// static bind it had. Handing `None` is exactly what a door with no
/// layout resolver does.
#[test]
fn no_layout_means_no_yield() {
    assert!(
        !site_yields_to_call_site_intrinsic("java/lang/String", "charAt", "(I)C", None),
        "with no layout the intrinsic is unemittable; do not cost the site its bind"
    );
}

/// An ordinary method on a non-intrinsic class is unaffected in both
/// directions — this predicate must not become a blanket "never
/// devirtualise".
#[test]
fn an_ordinary_site_is_untouched() {
    let layout = Some(STRING_INTRINSIC_NAME_PROBE);
    assert!(!site_yields_to_call_site_intrinsic(
        "com/example/Widget",
        "charAt",
        "(I)C",
        layout
    ));
    assert!(!site_yields_to_call_site_intrinsic(
        "java/lang/String",
        "trim",
        "()Ljava/lang/String;",
        layout
    ));    }