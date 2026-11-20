use super::*;
use crate::config::GcAlgorithm;
use crate::error::RuntimeError;
fn collectors() -> Vec<GcAlgorithm> {
    let mut heaps = vec![GcAlgorithm::Generational, GcAlgorithm::G1];
    #[cfg(feature = "zgc")]
    heaps.push(GcAlgorithm::Zgc);
    heaps
}
#[test]
fn tiny_heap_boot_preserves_the_fallible_api_contract() {
    for collector in collectors() {
        // G1 requires its heap to contain at least one region.
        let bytes = if matches!(collector, GcAlgorithm::G1) { 1 << 20 } else { 1024 };
        let result = Vm::try_new(VmConfig {
            gc_algorithm: collector,
            max_heap_size: bytes,
            initial_heap_size: bytes,
            ..VmConfig::default()
        });
        match result {
            // A metadata-only boot or a backend's minimum reservation can
            // still fit. The returned VM must retain a refusal channel.
            Ok(mut vm) => assert!(matches!(
                vm.try_new_array(ArrayElementType::Byte, 32 << 20),
                Err(VmError::Runtime(RuntimeError::OutOfMemoryError { .. }))
            )),
            Err(error) => {
                assert_eq!(error.detail, "java.lang.OutOfMemoryError: Java heap space")
            }
        }
    }
}
#[test]
fn result_returning_embedding_apis_refuse_without_collecting_on_every_heap() {
    for collector in collectors() {
        let mut vm = Vm::new(VmConfig {
            gc_algorithm: collector,
            max_heap_size: 16 << 20,
            initial_heap_size: 16 << 20,
            ..VmConfig::default()
        });
        let collections = vm.shared.mem.heap.collection_count();
        assert!(matches!(
            vm.try_new_array(ArrayElementType::Byte, 32 << 20),
            Err(VmError::Runtime(RuntimeError::OutOfMemoryError { .. }))
        ));
        assert!(matches!(
            vm.try_new_ref_array(ClassId::new(0), usize::MAX),
            Err(VmError::Runtime(RuntimeError::OutOfMemoryError { .. }))
        ));
        while vm
            .shared
            .mem
            .heap
            .try_alloc_array_full(ClassId::new(0), ArrayElementType::Byte, 4096)
            .is_some()
        {}
        while vm
            .shared
            .mem
            .heap
            .try_alloc_object_full(ClassId::new(0), 0)
            .is_some()
        {}
        assert!(matches!(
            vm.new_object("java/lang/Object"),
            Err(VmError::Runtime(RuntimeError::OutOfMemoryError { .. }))
        ));
        assert!(matches!(
            vm.try_new_ref_array(ClassId::new(0), 0),
            Err(VmError::Runtime(RuntimeError::OutOfMemoryError { .. }))
        ));
        assert_eq!(vm.shared.mem.heap.collection_count(), collections);
    }
}
