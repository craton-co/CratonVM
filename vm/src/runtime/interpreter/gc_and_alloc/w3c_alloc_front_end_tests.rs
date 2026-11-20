//! gc-common round 2026-09-23, wave 3, lane C3: `multianewarray`'s
//! collecting wrapper shares the one escalation ladder.

use super::*;
use crate::config::{GcAlgorithm, VmConfig};
use crate::error::{MethodCallFailed, RuntimeError, VmError};
use crate::memory::heap::ArrayElementType;
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::vm::SharedVm;
use std::sync::atomic::Ordering;

/// `new int[Integer.MAX_VALUE][1]` is refused with HotSpot's message and
/// WITHOUT a collection, exactly like `newarray` -- the collecting wrapper
/// used to run a forced collection and a last-ditch cycle for it first,
/// because the VM-limit refusal is also an `OutOfMemoryError`.
#[test]
fn multianewarray_vm_limit_is_refused_without_collecting() {
    let shared = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    });
    let mut thread = JvmThread::new(ThreadId(0), "w3c-multi-limit");
    let gcs_before = shared.mem.gc_cycle_count.load(Ordering::Relaxed);
    let sizes = [i32::MAX as usize, 1];
    let err = super::super::alloc_multi_array_collecting(
        &shared,
        &mut thread,
        &sizes,
        ArrayElementType::Int,
        sizes.len(),
        &[],
    )
    .expect_err("a dimension past the VM limit must be refused");
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

/// A tree that fits is built on the first attempt, with no collection.
#[test]
fn multianewarray_that_fits_does_not_collect() {
    let shared = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    });
    let mut thread = JvmThread::new(ThreadId(0), "w3c-multi-fits");
    let gcs_before = shared.mem.gc_cycle_count.load(Ordering::Relaxed);
    let sizes = [3usize, 4];
    let outer = super::super::alloc_multi_array_collecting(
        &shared,
        &mut thread,
        &sizes,
        ArrayElementType::Int,
        sizes.len(),
        &[],
    )
    .expect("a 3x4 int array fits an empty heap");
    assert_eq!(shared.mem.heap.array_length(outer), 3);
    assert_eq!(
        shared.mem.gc_cycle_count.load(Ordering::Relaxed),
        gcs_before
    );
}

/// zr z2/a2 (`CRATONVM_ZGC_MULTIARRAY_TLAB`, default ON): on ZGC the
/// collecting wrapper builds each level that fits a TLAB from the
/// executing thread's VM TLAB. The tree must be whole and correctly
/// shaped, nothing may collect, and -- whenever this heap hands VM TLABs
/// out at all -- the thread's buffer must be what paid for it (the
/// positive control: with the switch off, or before this change, the
/// cursor never moves because every level went through
/// `try_alloc_array_full`).
#[test]
#[cfg(feature = "zgc")]
fn zgc_multianewarray_levels_come_from_the_threads_tlab() {
    let shared = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Zgc,
        ..VmConfig::default()
    });
    if !shared.mem.heap.is_zgc() {
        // A build without the `zgc` feature: nothing to test.
        return;
    };
    let vm_tlab_on = shared.mem.heap.vm_tlab_enabled();
    let mut thread = JvmThread::new(ThreadId(0), "zr2a2-multi");
    let gcs_before = shared.mem.gc_cycle_count.load(Ordering::Relaxed);
    let before = thread.tlab.thread_allocated_bytes() + thread.tlab.consumed_bytes() as u64;
    let sizes = [5usize, 3];
    let outer = super::super::alloc_multi_array_collecting(
        &shared,
        &mut thread,
        &sizes,
        ArrayElementType::Int,
        sizes.len(),
        &[],
    )
    .expect("a 5x3 int array fits an empty heap");
    assert_eq!(shared.mem.heap.array_length(outer), 5);
    for i in 0..5 {
        match shared.mem.heap.get_array_element(outer, i) {
            Ok(Value::Object(Some(inner))) => {
                assert_eq!(shared.mem.heap.array_length(inner), 3, "inner {i}");
                let addr = inner.as_ptr() as usize;
                assert!(
                    shared.mem.heap.is_object_address(addr).is_some(),
                    "inner {i} must be announced to the start registry"
                );
            }
            other => panic!("outer[{i}] must hold an int[3], got {other:?}"),
        }
    }
    assert_eq!(
        shared.mem.gc_cycle_count.load(Ordering::Relaxed),
        gcs_before,
        "building a tree that fits must not collect"
    );
    if vm_tlab_on {
        let after = thread.tlab.thread_allocated_bytes() + thread.tlab.consumed_bytes() as u64;
        assert!(
            after > before,
            "the levels must come from the thread's TLAB (before={before} after={after})"
        );
    }
}

/// Source witness: the wrapper escalates through `collect_and_retry`
/// (overhead limit, soft-reference rung, last-ditch reclaim) rather than a
/// copy of it -- the copy had drifted twice. Driving the ladder to its
/// last rung needs a heap full of live data a unit test cannot build.
#[test]
fn multianewarray_escalates_through_the_shared_ladder() {
    let src = concat!(include_str!("../../interpreter/mod.rs"), "\n", include_str!("../../interpreter/cached_invoke_target.rs"), "\n", include_str!("../../interpreter/execute.rs"), "\n", include_str!("../../interpreter/resume_continuation.rs"), "\n", include_str!("../../interpreter/execute_frame_from_index_loop.rs"), "\n", include_str!("../../interpreter/resolve_array_class_loader_aware.rs"), "\n", include_str!("../../interpreter/park_for_debugger_under.rs"), "\n", include_str!("../../interpreter/double_arith_apply.rs"));
    let head = "fn alloc_multi_array_collecting(";
    let start = src
        .find(head)
        .expect("alloc_multi_array_collecting not found");
    // `\u{7d}` is a closing brace, spelled so this line's braces balance
    // for the brace-counting production-panic scanner.
    let body_len = src[start..].find("\n\u{7d}\n").expect("function end");
    let body = &src[start..start + body_len];
    assert!(body.contains("collect_and_retry("));
    assert!(!body.contains("last_ditch_reclaim("));
    assert!(!body.contains("maybe_gc_forced_at("));
}
