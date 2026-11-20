//! gc-common round 2026-09-23, wave 4, lane C4: VM-internal
//! `NativeContextImpl` allocations outside a native callback are fallible
//! and collect (`native_alloc_collecting`).

use super::*;
use crate::config::{GcAlgorithm, VmConfig};
use crate::error::RuntimeError;
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::vm::SharedVm;
use cratonvm_types::ClassId;
use std::sync::atomic::Ordering;

fn gen_vm() -> SharedVm {
    SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    })
}

/// An attempt refused by the heap (the native allocators' unwind) is
/// collected for and retried, and the unwind permission is back to what
/// it was afterwards -- this context is NOT a native callback.
#[test]
fn an_unwound_attempt_is_collected_for_and_retried() {
    let shared = gen_vm();
    let mut thread = JvmThread::new(ThreadId(0), "w4c-native-ladder");
    let gcs_before = shared.mem.gc_cycle_count.load(Ordering::Relaxed);
    assert!(!crate::runtime::native_oom::unwind_ok());
    let mut attempts = 0u32;
    let got = native_alloc_collecting(&shared, &mut thread, "w4c-test", |_, _| {
        attempts += 1;
        assert!(
            crate::runtime::native_oom::unwind_ok(),
            "the attempt must run with the unwind permission, so the \
             allocators take their fallible arm"
        );
        if attempts == 1 {
            crate::runtime::native_oom::raise(4, "object");
        }
        7u32
    });
    assert!(matches!(got, Ok(7)), "got {got:?}");
    assert_eq!(attempts, 2);
    assert!(
        shared.mem.gc_cycle_count.load(Ordering::Relaxed) > gcs_before,
        "the retry must follow a collection"
    );
    assert!(!crate::runtime::native_oom::unwind_ok());
}

/// A length HotSpot refuses outright is reported with its own message and
/// without collecting, like `newarray`.
#[test]
fn a_length_refusal_is_reported_without_collecting() {
    let shared = gen_vm();
    let mut thread = JvmThread::new(ThreadId(0), "w4c-native-limit");
    let gcs_before = shared.mem.gc_cycle_count.load(Ordering::Relaxed);
    let mut attempts = 0u32;
    let got: Result<(), RuntimeError> =
        native_alloc_collecting(&shared, &mut thread, "w4c-test", |_, _| {
            attempts += 1;
            crate::runtime::native_oom::raise(i32::MAX as usize, "primitive array");
        });
    match got {
        Err(RuntimeError::OutOfMemoryError { message }) => {
            assert_eq!(message, ARRAY_SIZE_EXCEEDS_VM_LIMIT)
        }
        other => panic!("expected OutOfMemoryError, got {other:?}"),
    }
    assert_eq!(attempts, 1);
    assert_eq!(
        shared.mem.gc_cycle_count.load(Ordering::Relaxed),
        gcs_before
    );
}

/// Heap exhaustion that survives the whole ladder is an
/// `OutOfMemoryError` (the heap text), after exactly the ladder's two
/// retries; and pins an unwound attempt pushed are dropped, as the
/// native funnel drops them.
#[test]
fn exhaustion_after_the_ladder_is_an_oome_and_drops_stray_pins() {
    let shared = gen_vm();
    let mut thread = JvmThread::new(ThreadId(0), "w4c-native-oome");
    let pin_base = thread.native_pin_roots.len();
    let mut attempts = 0u32;
    let got: Result<(), RuntimeError> =
        native_alloc_collecting(&shared, &mut thread, "w4c-test", |shared, thread| {
            attempts += 1;
            let o = shared
                .mem
                .heap
                .try_alloc_object(ClassId::new(0), 0)
                .expect("an empty heap has room for one object");
            thread.native_pin_roots.push(o);
            crate::runtime::native_oom::raise(4, "object");
        });
    match got {
        Err(RuntimeError::OutOfMemoryError { message }) => {
            assert!(message.starts_with("Java heap space"), "{message}")
        }
        other => panic!("expected OutOfMemoryError, got {other:?}"),
    }
    // The first attempt plus the ladder's retries: two, or one when the
    // GC-overhead limit cuts the ladder short -- plus, since gcd d4/j, the
    // retry after the second major on Generational
    // (`second_major_before_oome`).
    assert!((2..=4).contains(&attempts), "attempts = {attempts}");
    assert_eq!(thread.native_pin_roots.len(), pin_base);
}
