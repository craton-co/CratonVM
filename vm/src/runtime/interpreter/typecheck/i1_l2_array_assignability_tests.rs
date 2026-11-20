use super::{array_is_assignable_to, array_is_instance_of};
use std::sync::Arc;

/// i10-L2: the lenient `Object[] -> T[]` checkcast is gone — refused in
/// both modes (HotSpot's answer), by `checkcast` and `instanceof` alike.
#[test]
fn the_lenient_object_array_cast_is_gone() {
    const SRC: &str = "[Ljava/lang/Object;";
    const TGT: &str = "[Ljava/lang/String;";
    let mut config = crate::config::VmConfig::default();
    config.compatibility_mode = cratonvm_types::compat::CompatibilityMode::JdkOnly;
    config.use_synthetic_jdk = false;
    let jdk_only = Arc::new(crate::vm::SharedVm::new(config));
    assert!(
        !array_is_assignable_to(&jdk_only, SRC, TGT),
        "--real-jdk checkcast"
    );
    assert!(
        !array_is_instance_of(&jdk_only, SRC, TGT),
        "--real-jdk instanceof"
    );
    let compatible = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
    assert!(
        !array_is_instance_of(&compatible, SRC, TGT),
        "--compatible instanceof"
    );
    assert!(
        !array_is_assignable_to(&compatible, SRC, TGT),
        "--compatible checkcast"
    );
    // Nested: `Object[][] -> String[][]` recurses onto the same rule.
    const SRC2: &str = "[[Ljava/lang/Object;";
    const TGT2: &str = "[[Ljava/lang/String;";
    assert!(
        !array_is_assignable_to(&compatible, SRC2, TGT2),
        "--compatible nested checkcast"
    );
}

#[test]
fn every_reference_array_is_an_object_array_without_a_lookup() {
    let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
    const OBJECTS: &str = "[Ljava/lang/Object;";
    // A component name no loader can serve: the verdict must not depend on
    // it resolving, which is what the name-lookup path used to require.
    for src in ["[Lcratonvm/test/NoSuchL2Class;", "[Ljava/lang/String;"] {
        assert!(
            array_is_instance_of(&shared, src, OBJECTS),
            "{src} instanceof"
        );
        assert!(
            array_is_assignable_to(&shared, src, OBJECTS),
            "{src} checkcast"
        );
    }
    // The neighbouring rows the shortcut must not move.
    assert!(
        !array_is_instance_of(&shared, "[I", OBJECTS),
        "int[] is no Object[]"
    );
    assert!(
        array_is_instance_of(&shared, "[[I", OBJECTS),
        "int[][] is an Object[]"
    );
    assert!(
        !array_is_instance_of(&shared, OBJECTS, "[Ljava/lang/String;"),
        "instanceof stays strict: an Object[] is not a String[]"
    );
}

/// The descriptor-free array verdict answers the exact shapes (and,
/// since the lenient rule is gone, `Object[] -> T[]`) and defers the rest.
#[test]
fn array_fast_verdict_answers_exact_shapes_and_defers_the_rest() {
    use super::array_cast_fast_verdict;
    use crate::memory::heap::ArrayElementType;
    let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
    let heap = &shared.mem.heap;
    let ints = heap.alloc_array(
        crate::classloading::ClassId::new(0),
        ArrayElementType::Int,
        2,
    );
    assert_eq!(array_cast_fast_verdict(&shared, ints, "[I"), Some(true));
    assert_eq!(array_cast_fast_verdict(&shared, ints, "[J"), Some(false));
    assert_eq!(array_cast_fast_verdict(&shared, ints, "[[I"), Some(false));
    assert_eq!(
        array_cast_fast_verdict(&shared, ints, "java/lang/Cloneable"),
        Some(true)
    );
    assert_eq!(
        array_cast_fast_verdict(&shared, ints, "java/lang/String"),
        Some(false)
    );
    // `ClassId(0)` is `java/lang/Object` (the first class loaded), so this
    // is an `Object[]`: exact against `[Ljava/lang/Object;`, and refused
    // against a narrower class component (i10-L2: no lenient rule).
    let objs = heap.alloc_array(
        crate::classloading::ClassId::new(0),
        ArrayElementType::Reference,
        2,
    );
    let object_named = shared
        .classes
        .class_manager
        .read()
        .get_class(crate::classloading::ClassId::new(0))
        .is_some_and(|c| &*c.name == "java/lang/Object");
    if object_named {
        assert_eq!(
            array_cast_fast_verdict(&shared, objs, "[Ljava/lang/Object;"),
            Some(true)
        );
        assert_eq!(
            array_cast_fast_verdict(&shared, objs, "[Ljava/lang/String;"),
            Some(false)
        );
    }
}

/// i6-L2: `aastore` of an ARRAY element into an array of arrays takes the
/// descriptor-free verdict first; the answers are the JVMS ones.
#[test]
fn aastore_of_an_array_element_answers_by_component() {
    use super::aastore_element_assignable;
    use crate::memory::heap::ArrayElementType;
    let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
    // Every class is loaded before any array is allocated, so no loading
    // runs while an array is held.
    let load = |name: &str| shared.classes.class_manager.write().load_class(name).ok();
    let int_array_class = load("[I");
    let string = load("java/lang/String");
    let integer = load("java/lang/Integer");
    let object_array_class = load("[Ljava/lang/Object;");
    let string_array_class = load("[Ljava/lang/String;");
    let heap = &shared.mem.heap;
    let no_component = crate::classloading::ClassId::new(0);
    let ints = heap.alloc_array(no_component, ArrayElementType::Int, 1);
    let longs = heap.alloc_array(no_component, ArrayElementType::Long, 1);
    if let Some(int_array_class) = int_array_class {
        let int_2d = heap.alloc_array(int_array_class, ArrayElementType::Reference, 1);
        assert!(
            aastore_element_assignable(&shared, int_2d, ints),
            "int[] into int[][]"
        );
        assert!(
            !aastore_element_assignable(&shared, int_2d, longs),
            "long[] into int[][]"
        );
    }
    if let (Some(string), Some(integer), Some(object_array_class)) =
        (string, integer, object_array_class)
    {
        let strings = heap.alloc_array(string, ArrayElementType::Reference, 1);
        let integers = heap.alloc_array(integer, ArrayElementType::Reference, 1);
        let object_2d = heap.alloc_array(object_array_class, ArrayElementType::Reference, 1);
        assert!(aastore_element_assignable(&shared, object_2d, strings));
        assert!(
            !aastore_element_assignable(&shared, object_2d, ints),
            "int[] is no Object[]"
        );
        if let Some(string_array_class) = string_array_class {
            let string_2d =
                heap.alloc_array(string_array_class, ArrayElementType::Reference, 1);
            assert!(aastore_element_assignable(&shared, string_2d, strings));
            assert!(!aastore_element_assignable(&shared, string_2d, integers));
        }
    }
}

/// i6-L2: the covariant shapes the fast verdict answers from component
/// ids (`T[] -> Object[]`, `T[] -> S[]` by subtype, nested arrays against
/// the array supertypes, primitive targets) agree with the descriptor
/// path through BOTH entry points (`instanceof`, `checkcast`), and a
/// plain `T[] -> Object[]` is always answered.
#[test]
fn array_fast_verdict_agrees_with_the_descriptor_path_on_covariant_shapes() {
    use super::{array_cast_fast_verdict, array_descriptor_of};
    use crate::memory::heap::ArrayElementType;
    let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
    let mut components = Vec::new();
    for name in [
        "java/lang/Object",
        "java/lang/String",
        "java/lang/Integer",
        "java/lang/Number",
        "[Ljava/lang/String;",
    ] {
        let loaded = shared.classes.class_manager.write().load_class(name);
        if let Ok(id) = loaded {
            components.push(id);
        }
    }
    let targets = [
        "[Ljava/lang/Object;",
        "[Ljava/lang/String;",
        "[Ljava/lang/Number;",
        "[Ljava/lang/Comparable;",
        "[Ljava/io/Serializable;",
        "[Ljava/lang/Cloneable;",
        "[I",
        "[[Ljava/lang/Object;",
        "[[Ljava/lang/String;",
    ];
    for component in components {
        let arr = shared
            .mem
            .heap
            .alloc_array(component, ArrayElementType::Reference, 1);
        let Some(desc) = array_descriptor_of(&shared, arr) else {
            panic!("a reference array has a descriptor");
        };
        for target in targets {
            let Some(fast) = array_cast_fast_verdict(&shared, arr, target) else {
                assert_ne!(
                    target, "[Ljava/lang/Object;",
                    "{desc}: T[] -> Object[] deferred"
                );
                continue;
            };
            assert_eq!(
                fast,
                array_is_instance_of(&shared, &desc, target),
                "{desc} instanceof {target}"
            );
            assert_eq!(
                fast,
                array_is_assignable_to(&shared, &desc, target),
                "{desc} checkcast {target}"
            );
        }
    }
}
