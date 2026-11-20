use super::*;

/// A plain class name bound to a second `ClassId` raises both latches:
/// the field-retarget gate and the byte the JIT's inline `instanceof`
/// final-class miss tests at run time.
#[test]
fn a_duplicated_plain_class_name_raises_the_jit_latch() {
    let mut mgr = ClassManager::new(&[], &[], &[]);
    let name = "r9w5/DupName";
    mgr.register_class_name(ClassLoaderId::Application, name, ClassId::new(900_001));
    mgr.register_class_name(ClassLoaderId::Bootstrap, name, ClassId::new(900_002));
    assert!(any_duplicate_class_name());
    assert!(cratonvm_types::duplicate_class_name_seen());
}

/// Array class names are exempt from the JIT byte (no plain object's class
/// chain carries one); every other name, `java/lang` included, is not.
#[test]
fn only_non_array_names_raise_the_jit_latch() {
    for n in ["java/lang/String", "java/lang/Integer", "p/X", "p/X$Inner"] {
        assert!(duplicate_name_raises_jit_latch(n), "{n}");
    }
    for n in ["[Lp/X;", "[[Ljava/lang/String;", "[I"] {
        assert!(!duplicate_name_raises_jit_latch(n), "{n}");
    }
}
