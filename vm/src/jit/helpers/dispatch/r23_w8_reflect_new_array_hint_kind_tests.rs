use super::reflect_new_array_hint_kind;

/// No hint for an array or a primitive-named class (the inline prefix
/// must never resolve them: IAE / primitive-array / multi-dimension arms),
/// misses withheld for a synthetic target, a full hint otherwise.
#[test]
fn the_kind_is_the_assignable_filler_s_rule() {
    for name in [
        "[Ljava/lang/String;",
        "[I",
        "int",
        "void",
        "boolean",
        "double",
    ] {
        assert_eq!(reflect_new_array_hint_kind(name), 0, "{name}");
    }
    for t in cratonvm_types::synthetic_targets::SYNTHETIC_IMPLEMENTS_TARGETS {
        assert_eq!(reflect_new_array_hint_kind(t), 1, "{t}");
    }
    for name in ["java/lang/String", "java/util/HashMap", "R23RefArraycopy", "V", "I"] {
        assert_eq!(reflect_new_array_hint_kind(name), 2, "{name}");
    }
}
