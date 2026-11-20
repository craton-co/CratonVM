//! gc-common round 2026-09-23, lane F: the VM allocation front end.

use super::*;
use crate::config::{GcAlgorithm, VmConfig};
use crate::error::{MethodCallFailed, RuntimeError, VmError};
use crate::memory::heap::ArrayElementType;
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::vm::SharedVm;
use cratonvm_types::ClassId;
use std::sync::atomic::Ordering;

/// The HotSpot boundary, both sides of it.
#[test]
fn the_vm_array_limit_is_integer_max_value_minus_two() {
    let max = i32::MAX as usize;
    assert!(!array_length_exceeds_vm_limit(0));
    assert!(!array_length_exceeds_vm_limit(max - 8)); // ArrayList's own cap
    assert!(!array_length_exceeds_vm_limit(max - 2));
    assert!(array_length_exceeds_vm_limit(max - 1));
    assert!(array_length_exceeds_vm_limit(max));
    assert!(array_length_exceeds_vm_limit(usize::MAX));
}

/// `new byte[Integer.MAX_VALUE]` is refused with HotSpot's message and
/// WITHOUT a collection: the request is invalid, not the heap full.
#[test]
fn an_array_past_the_vm_limit_is_refused_without_collecting() {
    let shared = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    });
    let mut thread = JvmThread::new(ThreadId(0), "w1f-limit");
    let gcs_before = shared.mem.gc_cycle_count.load(Ordering::Relaxed);
    let err = gc_alloc_array(
        &shared,
        &mut thread,
        ClassId::new(0),
        ArrayElementType::Byte,
        i32::MAX as usize,
    )
    .expect_err("an array past the VM limit must not be allocated");
    match err {
        MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::OutOfMemoryError {
            message,
        })) => assert_eq!(message, ARRAY_SIZE_EXCEEDS_VM_LIMIT),
        other => panic!("expected OutOfMemoryError, got {other:?}"),
    }
    assert_eq!(
        shared.mem.gc_cycle_count.load(Ordering::Relaxed),
        gcs_before,
        "the VM-limit refusal must not run the collection ladder"
    );
}
