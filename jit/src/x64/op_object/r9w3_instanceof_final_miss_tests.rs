use super::*;

#[test]
fn only_final_java_lang_value_classes_are_answered_inline() {
    for n in [
        "java/lang/String",
        "java/lang/Integer",
        "java/lang/Long",
        "java/lang/Short",
        "java/lang/Byte",
        "java/lang/Character",
        "java/lang/Boolean",
        "java/lang/Float",
        "java/lang/Double",
    ] {
        assert!(instanceof_miss_is_final_boot_class(n), "{n}");
    }
    // Not final, an interface, an abstract class, or a final class
    // outside `java/` that another loader could duplicate by name.
    for n in [
        "java/lang/Object",
        "java/lang/Number",
        "java/lang/CharSequence",
        "java/lang/Comparable",
        "java/util/HashMap",
        "com/example/Final",
        "[J",
        "java/lang/StringBuilder",
    ] {
        assert!(!instanceof_miss_is_final_boot_class(n), "{n}");
    }
}




/// The latch the emitted miss tests is a real, stable, byte-addressable
/// location: a `0` address would make `MOV R11, 0 ; CMP BYTE [R11], 0`
/// fault instead of answering.
#[test]
fn the_duplicate_name_latch_has_a_stable_address() {
    let a = cratonvm_types::duplicate_class_name_seen_addr();
    assert_ne!(a, 0);
    assert_eq!(a, cratonvm_types::duplicate_class_name_seen_addr());
}
