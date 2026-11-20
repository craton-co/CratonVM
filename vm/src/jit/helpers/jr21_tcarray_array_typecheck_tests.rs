//! JIT round 21, lane tcarray: the reference-array component query the
//! optimizing tier's array arm compiles against
//! (`WELL_KNOWN_VH_CONST_ARRAY_COMPONENT_TAG`) and the type-check helpers'
//! array memo (`CRATONVM_JIT_TYPECHECK_ARRAY_MEMO`).
use super::*;
use crate::config::VmConfig;
use cratonvm_jit::ir_lower as jl;
use std::sync::Arc;

const ELEM: &str = "cratonvm/test/jr21tcarray/Elem";
const ELEM_ARRAY: &str = "[Lcratonvm/test/jr21tcarray/Elem;";

fn ask(shared: &SharedVm, cid: u32) -> Option<i64> {
    vh_const_well_known_answer(
        shared,
        jl::vh_const_query(jl::WELL_KNOWN_VH_CONST_ARRAY_COMPONENT_TAG, cid, 0),
    )
}

/// `(component, its array class)`, or `None` where the environment's
/// policy refuses a synthetic class (`--real-jdk`).
fn classes(shared: &SharedVm) -> Option<(ClassId, ClassId)> {
    let mut cm = shared.classes.class_manager.write();
    let component = cm.try_ensure_synthetic_class(ELEM, 0).ok()?;
    let array = cm
        .load_array_class_for_loader(ELEM_ARRAY, cratonvm_types::ClassLoaderId::Bootstrap)
        .ok()?;
    Some((component, array))
}

#[test]
fn the_component_query_answers_reference_array_classes_only() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    assert_eq!(ask(&shared, 61_999), Some(0), "an unknown class");
    let Some((component, array)) = classes(&shared) else {
        return;
    };
    assert_eq!(
        ask(&shared, array.as_u32()),
        Some(i64::from(component.as_u32()) + 1)
    );
    assert_eq!(ask(&shared, component.as_u32()), Some(0), "not an array");
}

/// A reference array's `true` is memoized under its own key (the length
/// word's array bit), never under the plain-receiver key of its component
/// class; a primitive array's verdict is never memoized.
#[test]
fn a_reference_array_true_is_memoized_apart_from_its_component() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let Some((component, array)) = classes(&shared) else {
        return;
    };
    let (ptr, len) = cratonvm_jit::intern_typecheck_target(ELEM_ARRAY, Some(array.as_u32()));
    let mut receiver =
        shared
            .mem
            .heap
            .alloc_array(component, cratonvm_types::ArrayElementType::Reference, 1);
    let mut raise = None;
    // SAFETY: `receiver` is a live array of `shared`'s own heap; no
    // compiled code or JIT thread is involved.
    let verdict = unsafe {
        jit_typecheck_memoized(
            &shared,
            component,
            &mut receiver,
            ptr,
            len,
            true,
            &mut raise,
        )
    };
    assert_eq!(verdict, Some(true), "an Elem[] is an Elem[]");
    assert!(raise.is_none());
    let epoch = typecheck_memo_epoch(&shared);
    let vm_key = shared.vm_identity;
    let array_key = (
        vm_key,
        ptr as usize,
        len | JIT_TYPECHECK_ARRAY_MEMO_BIT,
        component.as_u32(),
        true,
    );
    assert_eq!(
        jit_typecheck_answer_cached(array_key, epoch),
        shared.jit.supers_displays.array_cast_memo()
    );
    let plain_key = (vm_key, ptr as usize, len, component.as_u32(), true);
    assert!(
        !jit_typecheck_answer_cached(plain_key, epoch),
        "a plain Elem is no Elem[]"
    );
    // An int[] carries header id 0: refused, and nothing is recorded.
    let mut ints =
        shared
            .mem
            .heap
            .alloc_array(ClassId::new(0), cratonvm_types::ArrayElementType::Int, 1);
    // SAFETY: as above.
    let verdict = unsafe {
        jit_typecheck_memoized(
            &shared,
            ClassId::new(0),
            &mut ints,
            ptr,
            len,
            true,
            &mut raise,
        )
    };
    assert_eq!(verdict, Some(false), "an int[] is no Elem[]");
    let int_key = (
        vm_key,
        ptr as usize,
        len | JIT_TYPECHECK_ARRAY_MEMO_BIT,
        0,
        true,
    );
    assert!(!jit_typecheck_answer_cached(int_key, epoch));
}
