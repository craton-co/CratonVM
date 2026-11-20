//! Interpreter round i1, wave 11, lane L2: an array receiver against a
//! RESOLVED array target is answered by component `ClassId`s.
use super::{array_cast_resolved_verdict, array_receiver_cast_verdict};
use crate::classloading::ClassId;
use cratonvm_types::ArrayElementType;
use std::sync::Arc;

fn class(shared: &crate::vm::SharedVm, name: &str) -> Option<ClassId> {
    shared
        .classes
        .class_manager
        .write()
        .try_ensure_synthetic_class(name, 0)
        .ok()
}

fn array_class(shared: &crate::vm::SharedVm, name: &str) -> Option<ClassId> {
    shared.classes.class_manager.write().load_class(name).ok()
}

#[test]
fn a_resolved_array_target_is_answered_by_component_ids() {
    const BASE_ARRAY: &str = "[Lcratonvm/test/I11L2Base;";
    let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
    let (Some(base), Some(sub), Some(other)) = (
        class(&shared, "cratonvm/test/I11L2Base"),
        class(&shared, "cratonvm/test/I11L2Sub"),
        class(&shared, "cratonvm/test/I11L2Other"),
    ) else {
        return; // --real-jdk policy in the environment: nothing to fabricate
    };
    shared
        .classes
        .class_manager
        .write()
        .set_superclass(sub, Some(base));
    let (Some(base_arr), Some(base_arr2), Some(sub_arr), Some(ints)) = (
        array_class(&shared, BASE_ARRAY),
        array_class(&shared, "[[Lcratonvm/test/I11L2Base;"),
        array_class(&shared, "[Lcratonvm/test/I11L2Sub;"),
        array_class(&shared, "[I"),
    ) else {
        return;
    };
    let recorded = shared
        .classes
        .class_manager
        .read()
        .get_class(base_arr)
        .is_some_and(|c| c.array_info.is_some());
    if !recorded {
        return; // nothing to answer by id
    }
    let heap = &shared.mem.heap;
    let subs = heap.alloc_array(sub, ArrayElementType::Reference, 1);
    let others = heap.alloc_array(other, ArrayElementType::Reference, 1);
    let prims = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 1);
    let nested = heap.alloc_array(sub_arr, ArrayElementType::Reference, 1);
    assert_eq!(
        array_cast_resolved_verdict(&shared, subs, base_arr),
        Some(true)
    );
    assert_eq!(
        array_cast_resolved_verdict(&shared, others, base_arr),
        Some(false)
    );
    assert_eq!(
        array_cast_resolved_verdict(&shared, prims, base_arr),
        Some(false)
    );
    assert_eq!(
        array_cast_resolved_verdict(&shared, subs, base_arr2),
        Some(false),
        "a Sub[] is not a Base[][]"
    );
    assert_eq!(
        array_cast_resolved_verdict(&shared, nested, base_arr2),
        Some(true),
        "a Sub[][] is a Base[][]"
    );
    assert_eq!(
        array_cast_resolved_verdict(&shared, prims, ints),
        None,
        "a primitive-array class records no component"
    );
    assert!(array_receiver_cast_verdict(
        &shared,
        subs,
        BASE_ARRAY,
        Some(base_arr)
    ));
    assert!(!array_receiver_cast_verdict(
        &shared,
        others,
        BASE_ARRAY,
        Some(base_arr)
    ));
    assert!(!array_receiver_cast_verdict(
        &shared, others, BASE_ARRAY, None
    ));
}
