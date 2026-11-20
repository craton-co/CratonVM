//! gc-common round 2026-09-23, wave 2, lane D: the allocation front end's
//! escalation ladder, slow-path accounting and multianewarray VM limit.

use super::*;
use crate::config::{GcAlgorithm, VmConfig};
use crate::error::{MethodCallFailed, RuntimeError, VmError};
use crate::memory::heap::ArrayElementType;
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::vm::SharedVm;
use cratonvm_gc::heap::{HEADER_SIZE, SLOT_SIZE};
use cratonvm_types::ClassId;
use std::sync::atomic::Ordering;

fn gen_vm() -> SharedVm {
    SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    })
}

/// The shared ladder returns the first successful retry, after exactly
/// the one forced collection -- no last-ditch cycle when it was not
/// needed.
#[test]
fn collect_and_retry_returns_the_first_successful_retry() {
    let shared = gen_vm();
    let mut thread = JvmThread::new(ThreadId(0), "w2d-ladder-1");
    let gcs_before = shared.mem.gc_cycle_count.load(Ordering::Relaxed);
    let mut attempts = 0u32;
    let got = collect_and_retry(&shared, &mut thread, "w2d-test", |_| {
        attempts += 1;
        Some(7u32)
    });
    assert_eq!(got, Some(7));
    assert_eq!(attempts, 1, "a successful retry must not be retried again");
    assert!(
        shared.mem.gc_cycle_count.load(Ordering::Relaxed) > gcs_before,
        "the ladder must collect before its first retry"
    );
}

/// A failed first retry gets exactly one more attempt (after the
/// last-ditch reclaim), and `None` comes back only when both fail.
#[test]
fn collect_and_retry_makes_exactly_two_attempts() {
    let shared = gen_vm();
    let mut thread = JvmThread::new(ThreadId(0), "w2d-ladder-2");
    let mut attempts = 0u32;
    let got = collect_and_retry(&shared, &mut thread, "w2d-test", |_| {
        attempts += 1;
        (attempts == 2).then_some(attempts)
    });
    assert_eq!(got, Some(2));

    let mut attempts = 0u32;
    let got: Option<u32> = collect_and_retry(&shared, &mut thread, "w2d-test", |_| {
        attempts += 1;
        None
    });
    assert_eq!(got, None);
    // gcd d4/j: on Generational with `CRATONVM_GC_OVERHEAD_PROGRESS` on,
    // a third attempt follows the second major this (sole) thread runs
    // after the last-ditch rung (`second_major_before_oome`).
    let expected = if cratonvm_types::flags().gc.gc_overhead_progress {
        3
    } else {
        2
    };
    assert_eq!(attempts, expected);
}

/// All four interpreter ladders go through the one helper, so they share
/// one policy -- in particular the String ladders now stop at the
/// GC-overhead limit like `new` and `newarray` do
/// (`common-f-string-ladder-skips-overhead-limit`). A source witness,
/// because driving the overhead streak to its limit needs a wedged old
/// generation a unit test cannot build.
#[test]
fn every_interpreter_ladder_uses_the_shared_escalation() {
    let src = concat!(include_str!("../gc_and_alloc/mod.rs"), "\n", include_str!("../gc_and_alloc/mtroots_on.rs"), "\n", include_str!("../gc_and_alloc/gc_notifications_go_to_delivery.rs"), "\n", include_str!("../gc_and_alloc/zgc_refusal_line.rs"), "\n", include_str!("../gc_and_alloc/finalizers_forced.rs"), "\n", include_str!("../gc_and_alloc/reference_round.rs"), "\n", include_str!("../gc_and_alloc/on_out_of_memory_error_commands.rs"), "\n", include_str!("../gc_and_alloc/frozen_peer_referents.rs"), "\n", include_str!("../gc_and_alloc/g1_force_full_cycle.rs"));
    for head in [
        "pub(crate) fn create_string_or_oom(",
        "pub(crate) fn create_string_from_units_or_oom(",
        "pub(crate) fn intern_string_literal_or_oom(",
        "pub(crate) fn alloc_object_shared(",
        // Wave 63: `gc_alloc_array` samples around this body.
        "fn gc_alloc_array_unsampled(",
    ] {
        let start = src.find(head).unwrap_or_else(|| panic!("{head} not found"));
        // `\u{7d}` is a closing brace, spelled so this line's braces
        // balance for the brace-counting production-panic scanner
        // (`interpreter::tests::scan_production_section`).
        let body_len = src[start..].find("\n\u{7d}\n").expect("function end");
        let body = &src[start..start + body_len];
        assert!(
            body.contains("collect_and_retry("),
            "{head} must escalate through `collect_and_retry`"
        );
        assert!(
            !body.contains("last_ditch_reclaim("),
            "{head} carries its own copy of the ladder again"
        );
    }
}

/// An object from a backend that did not plan a compact body (here: a
/// class with no registered layout) is charged its legacy footprint --
/// read back from its header, not assumed.
#[test]
fn a_legacy_object_is_charged_its_legacy_footprint() {
    let shared = gen_vm();
    let obj = shared
        .mem
        .heap
        .try_alloc_object(ClassId::new(0), 3)
        .expect("an empty heap has room for one object");
    assert_eq!(
        shared_object_footprint(&shared, obj, 3),
        HEADER_SIZE + 3 * SLOT_SIZE
    );
}

/// `alloc_object_shared` charges the thread counter (and the VM counter)
/// what the header says the object occupies.
/// (`common-f-shared-path-accounts-legacy-size`.)
#[test]
fn the_shared_object_path_charges_the_real_footprint() {
    let shared = gen_vm();
    let mut thread = JvmThread::new(ThreadId(0), "w2d-footprint");
    let thread_before = thread.tlab.thread_allocated_bytes();
    let vm_before = shared.mem.bytes_allocated_total.load(Ordering::Relaxed);
    let obj = alloc_object_shared(&shared, &mut thread, ClassId::new(0), 3)
        .expect("an empty heap has room for one object");
    let expected = cratonvm_gc::gc::object_total_size(shared.mem.heap.get_header(obj));
    assert_eq!(
        thread.tlab.thread_allocated_bytes() - thread_before,
        expected as u64
    );
    assert_eq!(
        shared.mem.bytes_allocated_total.load(Ordering::Relaxed) - vm_before,
        expected as u64
    );
}

/// `init_primitive_fields` reports the class's `has_finalizer` bit under
/// its one class-manager read. A fresh VM's class 0 (unloaded, or
/// `java/lang/Object`, whose empty `finalize()` is not a finalizer) must
/// answer `false`, or every `new Object()` would be registered.
#[test]
fn class_zero_is_not_finalizable() {
    let shared = gen_vm();
    let obj = shared
        .mem
        .heap
        .try_alloc_object(ClassId::new(0), 0)
        .expect("an empty heap has room for one object");
    assert!(!init_primitive_fields(&shared, obj, ClassId::new(0)));
}

/// `multianewarray` refuses a dimension past HotSpot's array limit with
/// HotSpot's message, before allocating it -- and, as HotSpot does, never
/// examines an inner dimension under a zero-length outer one.
#[test]
fn multianewarray_refuses_a_dimension_past_the_vm_limit() {
    let shared = gen_vm();
    let sizes = [i32::MAX as usize, 1];
    let err = super::super::alloc_multi_array(
        &shared,
        &sizes,
        0,
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

    let sizes = [0usize, i32::MAX as usize];
    let outer = super::super::alloc_multi_array(
        &shared,
        &sizes,
        0,
        ArrayElementType::Int,
        sizes.len(),
        &[],
    )
    .expect("an inner dimension under a zero-length outer one is never allocated");
    assert_eq!(shared.mem.heap.array_length(outer), 0);
}
