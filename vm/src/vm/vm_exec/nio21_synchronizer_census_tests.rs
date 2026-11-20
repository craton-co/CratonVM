use super::*;

/// No `AbstractOwnableSynchronizer` class, no synchronizer and no pause:
/// the census declines before it stops anything.
#[test]
fn nio21_census_without_the_synchronizer_class_takes_no_pause() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let loaded = shared
        .classes
        .class_manager
        .read()
        .find_unique_class_by_name("java/util/concurrent/locks/AbstractOwnableSynchronizer")
        .is_some();
    if loaded {
        return;
    }
    let mut thread = JvmThread::new(ThreadId(1), "nio21-census");
    assert!(owned_synchronizer_census_in(&shared, &mut thread).is_none());
}

/// gci i8/hd (`gci-i3hd-heap-walks-and-diagnostics-review-findings-FIXED-20261017`
/// item 8): a thread already GC-safe only sleeps between two refused
/// requests -- it neither leaves nor re-enters its region.
#[test]
fn gci_i8hd_declined_slice_leaves_a_blocked_thread_blocked() {
    use std::sync::atomic::Ordering;
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(81), "i8hd-blocked");
    thread.gc_block_state.in_blocked_region.store(true, Ordering::Release);
    wait_out_declined_pause_slice(&shared, &mut thread);
    assert!(thread.gc_block_state.in_blocked_region.load(Ordering::Acquire));
    thread.gc_block_state.in_blocked_region.store(false, Ordering::Release);
}

/// gci i8/hd: a running thread waits its slice inside a blocked region and
/// comes out a running mutator again (flag down, no TLAB kept across).
#[test]
fn gci_i8hd_declined_slice_runs_again_after_its_region() {
    use std::sync::atomic::Ordering;
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(82), "i8hd-running");
    wait_out_declined_pause_slice(&shared, &mut thread);
    assert!(!thread.gc_block_state.in_blocked_region.load(Ordering::Acquire));
}

/// gci i8/hd: the census retries through the GC-safe slice, bounded, and
/// no longer sleeps as a running mutator. (Source pin: a competing pause
/// that counts the caller cannot be staged without a registered thread.)
#[test]
fn gci_i8hd_census_retry_arrives_at_the_refusing_pause() {
    let src = concat!(include_str!("../vm_exec/mod.rs"), "\n", include_str!("../vm_exec/method.rs"), "\n", include_str!("../vm_exec/report_pad_access.rs"), "\n", include_str!("../vm_exec/diagnose_sync_on_value_based_class.rs"), "\n", include_str!("../vm_exec/thread_interrupted_field_slot_of.rs"), "\n", include_str!("../vm_exec/clear_breakpoints_of_redefined_class.rs"), "\n", include_str!("../vm_exec/load_class.rs"), "\n", include_str!("../vm_exec/native_new_initializes_class_first.rs"), "\n", include_str!("../vm_exec/new_object.rs"), "\n", include_str!("../vm_exec/gen_referent_read_keep_alive.rs"), "\n", include_str!("../vm_exec/append_native_standin_frames.rs"), "\n", include_str!("../vm_exec/unloaded_native_libraries.rs"), "\n", include_str!("../vm_exec/extract_annotations_from_attributes.rs"), "\n", include_str!("../vm_exec/invoke_static_settling_with_first.rs"), "\n", include_str!("../vm_exec/invoke_special_shared.rs"), "\n", include_str!("../vm_exec/proxy_resolve_declaring_method.rs"), "\n", include_str!("../vm_exec/invoke_on_class_shared_inner.rs"), "\n", include_str!("../vm_exec/jni_table_native_call.rs")).replace("\r\n", "\n");
    let start = src
        .find("\npub(crate) fn owned_synchronizer_census_in(")
        .expect("the census");
    let rest = &src[start..];
    let body = &rest[..rest.find("\n\u{7d}\n").expect("its end")];
    assert!(body.contains("wait_out_declined_pause_slice(shared, thread);"));
    assert!(body.contains("CENSUS_PAUSE_RETRY_LIMIT"));
    assert!(!body.contains("std::thread::sleep"));
}
