use super::*;
use crate::config::{GcAlgorithm, VmConfig};

fn heap() -> SharedVm {
    SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Zgc,
        max_heap_size: 16 << 20,
        initial_heap_size: 16 << 20,
        ..VmConfig::default()
    })
}

#[test]
fn a_primitive_mirror_name_refusal_is_fallible_without_an_unwind_scope() {
    let shared = heap();
    assert!(!crate::runtime::native_oom::unwind_ok());
    // The mirror itself fits. Its name cannot fit even in an empty heap.
    // Previously this went through fatal_string_heap_exhausted despite
    // the caller choosing the fallible mirror API.
    let name = "x".repeat(32 << 20);
    assert!(try_get_or_create_primitive_mirror(&shared, &name).is_none());
    assert!(!shared.classes.primitive_mirrors.read().contains_key(&name));
    let mirror = try_get_or_create_primitive_mirror(&shared, "int").expect("recovery");
    assert_eq!(
        Some(mirror),
        try_get_or_create_primitive_mirror(&shared, "I")
    );
    assert!(!crate::runtime::native_oom::unwind_ok());
}

#[test]
fn an_exhausted_heap_refuses_mirrors_and_streams_without_partial_publication() {
    let shared = heap();
    *shared.system_out.write() = None;
    *shared.system_err.write() = None;
    let before = shared.classes.primitive_mirrors.read().len();
    while shared
        .mem
        .heap
        .try_alloc_array_full(ClassId::new(0), ArrayElementType::Byte, 4096)
        .is_some()
    {}
    while shared
        .mem
        .heap
        .try_alloc_object_full(ClassId::new(0), 0)
        .is_some()
    {}
    assert!(try_get_or_create_primitive_mirror(&shared, "never-minted").is_none());
    assert_eq!(before, shared.classes.primitive_mirrors.read().len());
    assert!(matches!(
        shared.try_ensure_system_streams(),
        Err(crate::error::RuntimeError::OutOfMemoryError { .. })
    ));
    assert!(shared.system_out.read().is_none());
    assert!(shared.system_err.read().is_none());
    assert!(shared.try_get_class_lock_object(ClassId::new(0)).is_none());
}

#[test]
fn an_infallible_string_inside_a_native_raises_the_allocation_unwind() {
    let shared = heap();
    let name = "x".repeat(32 << 20);
    let refusal =
        crate::runtime::native_oom::catch_alloc_oom(|| create_java_string(&shared, &name));
    assert!(refusal.is_err());
    assert!(!crate::runtime::native_oom::unwind_ok());
}
