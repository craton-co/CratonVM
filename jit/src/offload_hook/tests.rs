use super::*;
use std::sync::{Mutex, MutexGuard};

/// Every test here drives the same process-wide table and the same two
/// atomics, and `reset_for_test` clears them — so under `cargo test`'s
/// default thread pool they interleave and wipe each other's rows. Take
/// this first in every test and they are serial regardless.
///
/// Poison is not interesting here: a panicking test has already failed, and
/// the next one resets whatever state it would have inherited.
fn exclusive() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[test]
fn unarmed_says_no_without_touching_the_table() {
    let _g = exclusive();
    reset_for_test();
    assert!(!is_kernel(1, "A", "f", "([I)V"));
    // The compile-time door is the same single bool when offload is off,
    // which is what keeps a non-`--gpu` run paying nothing for any of this.
    assert!(!keeps_dispatch_helper("A", "f", "([I)V"));
}

#[test]
fn a_registered_target_is_recognised_exactly() {
    let _g = exclusive();
    reset_for_test();
    note_kernel(1, "com/example/K", "vectorAdd", "([I[I)V");
    assert!(is_kernel(1, "com/example/K", "vectorAdd", "([I[I)V"));
    // All three components are part of the identity: an overload with a
    // different descriptor is a different method, and matching it would
    // keep a hook on a site that can never offload.
    assert!(!is_kernel(1, "com/example/K", "vectorAdd", "([J[J)V"));
    assert!(!is_kernel(1, "com/example/K", "other", "([I[I)V"));
    assert!(!is_kernel(1, "com/other/K", "vectorAdd", "([I[I)V"));
    // And so is the VM (wave 40): another VM's same-named class is not
    // the registering VM's kernel.
    assert!(!is_kernel(2, "com/example/K", "vectorAdd", "([I[I)V"));
    reset_for_test();
}

/// The regression this predicate exists for: `GpuComputeWarm`'s shape, where
/// the caller is compiled before the callee's class is loaded, so the
/// registry cannot yet know `GpuCompute.heavy` is a kernel. Gated on
/// `is_kernel`, that site was bound directly and could never offload again.
#[test]
fn an_unregistered_kernel_shape_still_keeps_its_helper() {
    let _g = exclusive();
    reset_for_test();
    arm(1024);
    assert!(!is_kernel(1, "bench/GpuCompute", "heavy", "([I[I[I)V"));
    assert!(keeps_dispatch_helper(
        "bench/GpuCompute",
        "heavy",
        "([I[I[I)V"
    ));
    reset_for_test();
}

#[test]
fn armed_the_door_still_refuses_shapes_that_can_never_dispatch() {
    let _g = exclusive();
    reset_for_test();
    arm(1024);
    // `try_dispatch` launches only a `)V` map or a proven `)I`/`)J`
    // reduction.
    assert!(!keeps_dispatch_helper("A", "f", "([I)D"));
    assert!(!keeps_dispatch_helper("A", "f", "([I)Ljava/lang/Object;"));
    // No array parameter, so `largest_primitive_array_len` is 0 on every
    // call, for every argument value, and `--gpu-min-work` can never be
    // cleared.
    assert!(!keeps_dispatch_helper("A", "f", "(II)V"));
    assert!(!keeps_dispatch_helper("A", "f", "()V"));
    // The shapes that survive.
    assert!(keeps_dispatch_helper("A", "f", "([I[I)V"));
    assert!(keeps_dispatch_helper("A", "f", "([DI)I"));
    assert!(keeps_dispatch_helper("A", "f", "([J)J"));
    reset_for_test();
}

/// `--gpu-min-work 0` turns the work threshold off, so the array test that
/// mirrors it has to come off too — otherwise this door would be stricter
/// than the runtime gate it stands in for, and refuse a site the gate would
/// have accepted.
/// Two VMs arm with different thresholds: the compile-time door keeps a
/// helper whenever either could dispatch (wave 40), whichever armed last.
#[test]
fn the_smallest_min_work_of_the_armed_vms_decides() {
    let _g = exclusive();
    reset_for_test();
    arm(0);
    arm(1024);
    assert!(keeps_dispatch_helper("A", "f", "(II)V"));
    reset_for_test();
}

#[test]
fn min_work_zero_drops_the_array_requirement() {
    let _g = exclusive();
    reset_for_test();
    arm(0);
    assert!(keeps_dispatch_helper("A", "f", "(II)V"));
    assert!(!keeps_dispatch_helper("A", "f", "(II)D"));
    reset_for_test();
}

#[test]
fn ir_splicer_refuses_kernel_shaped_static_when_armed() {
    let _g = exclusive();
    reset_for_test();
    assert!(!crate::ir_splice_keeps_gpu_dispatch_helper(
        true,
        "GpuProbe",
        "dotReduce",
        "([I[I)J",
    ));
    arm(1024);
    assert!(crate::ir_splice_keeps_gpu_dispatch_helper(
        true,
        "GpuProbe",
        "dotReduce",
        "([I[I)J",
    ));
    assert!(crate::ir_splice_keeps_gpu_dispatch_helper(
        true,
        "GpuForwardRef",
        "scaleHere",
        "([I[I)V",
    ));
    assert!(!crate::ir_splice_keeps_gpu_dispatch_helper(
        false,
        "GpuForwardRef",
        "scaleHere",
        "([I[I)V",
    ));
    reset_for_test();
}
