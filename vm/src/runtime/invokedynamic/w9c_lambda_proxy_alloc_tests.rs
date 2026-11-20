//! gc-common w9-c: a capturing lambda's proxy is allocated through `new`'s
//! shared-path front end (`alloc_object_shared`), not a private copy of
//! the pre-w2-d ladder that skipped the old generation, the soft-reference
//! rung and the allocation counters.

use super::*;
use crate::runtime::interpreter::constants::w9c_resolution_oom_tests::{
    fill_heap_with_pins, small_gen_vm,
};
use crate::threading::jvm_thread::ThreadId;
use std::sync::atomic::Ordering;

/// The proxy's bytes reach this VM's allocation counter, and the captures
/// land in its fields. Two captures, so the zero-capture singleton table
/// (process-wide) is never touched.
#[test]
fn a_capturing_proxy_is_charged_and_filled() {
    let shared = small_gen_vm();
    let mut thread = JvmThread::new(ThreadId(0), "w9c-lambda-charge");
    let proxy_class_id = shared.alloc_lambda_proxy_id();
    let before = shared.mem.bytes_allocated_total.load(Ordering::Relaxed);
    let proxy = allocate_lambda_proxy_from_values(
        &shared,
        &mut thread,
        proxy_class_id,
        &mut [Value::Int(7), Value::Int(-9)],
        (0, 0),
    )
    .expect("an empty heap has room for a proxy");
    // gcg triage: `CRATONVM_LAMBDA_PROXY_TLAB` is default ON, so the proxy
    // may be a TLAB bump, charged in the thread's bounded batch as `new`'s
    // are; publish the batch before reading the VM total.
    thread.tlab.flush_vm_allocation_batch();
    assert!(
        shared.mem.bytes_allocated_total.load(Ordering::Relaxed) > before,
        "the proxy bypassed the TLAB and was not charged"
    );
    assert_eq!(shared.mem.heap.get_field(proxy, 0), Value::Int(7));
    assert_eq!(thread.native_pin_roots.len(), 0);
}

/// On a heap held full by pins the proxy is refused with
/// `OutOfMemoryError` after the shared ladder, and the capture pins are
/// released.
#[test]
fn a_capturing_proxy_on_a_full_heap_is_an_oome_and_releases_its_pins() {
    let shared = small_gen_vm();
    let mut thread = JvmThread::new(ThreadId(0), "w9c-lambda-full");
    assert!(
        fill_heap_with_pins(&shared, &mut thread),
        "the pinned objects never filled the 8 MB heap"
    );
    let pins = thread.native_pin_roots.len();
    let capture = thread.native_pin_roots[0];
    let proxy_class_id = shared.alloc_lambda_proxy_id();
    let got = allocate_lambda_proxy_from_values(
        &shared,
        &mut thread,
        proxy_class_id,
        &mut [Value::Object(Some(capture)), Value::Int(1)],
        (0, 0),
    );
    match got {
        Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::OutOfMemoryError { message },
        ))) => assert!(message.starts_with("Java heap space"), "{message}"),
        Ok(_) => panic!("a pinned-full heap cannot hold the proxy"),
        Err(other) => panic!("expected OutOfMemoryError, got {other:?}"),
    }
    assert_eq!(thread.native_pin_roots.len(), pins);
}
