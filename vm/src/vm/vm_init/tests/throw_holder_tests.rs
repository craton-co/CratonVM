// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;
use std::sync::atomic::Ordering;

#[test]
fn retain_captured_backtrace_shared_borrows_for_holder_and_shares_fallback() {
    let shared = SharedVm::new(VmConfig::default());
    let cache = &shared.threads.throwable_layout_cache;
    cache.set_backtrace_in_object_for_test(true);
    cache.backtrace.store(0, Ordering::Relaxed);

    let throwable = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    let label: Arc<str> = Arc::from("probe/Shared.m()V");
    let frames: Arc<[BacktraceFrame]> = Arc::from(vec![BacktraceFrame::Compiled {
        label: Arc::clone(&label),
        owner_class_id: 0,
        bci: 7,
        cp_stamp: None,
    }]);
    shared.retain_captured_backtrace_shared(throwable, Arc::clone(&frames));
    assert_eq!(
        Arc::strong_count(&frames),
        1,
        "holder path must not keep a registry Arc"
    );
    assert!(shared.backtrace_holder_of(throwable).is_some());
    assert!(shared
        .threads
        .throwable_stacks
        .iter()
        .all(|s| s.read().is_empty()));
    assert_eq!(shared.throwable_stack_trace_len_for(throwable), Some(1));

    let pooled = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    shared.mem.preallocated_oome.write().push_traced(pooled);
    shared.retain_captured_backtrace_shared(pooled, Arc::clone(&frames));
    assert!(shared.backtrace_holder_of(pooled).is_none());
    assert_eq!(
        shared.mem.heap.get_field_as(pooled, 0, b'L'),
        Value::Object(Some(pooled))
    );
    assert_eq!(
        Arc::strong_count(&frames),
        2,
        "pooled fallback shares the caller's Arc directly"
    );

    let entry_throwable = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    let built = crate::runtime::stackwalker::synthetic_entry(
        Arc::from("probe/SharedEntry"),
        Arc::from("m"),
    );
    let entry_frames: Arc<[BacktraceFrame]> = Arc::from(vec![BacktraceFrame::Entry(built)]);
    shared.retain_captured_backtrace_shared(entry_throwable, Arc::clone(&entry_frames));
    assert!(shared.backtrace_holder_of(entry_throwable).is_none());
    assert_eq!(
        shared.mem.heap.get_field_as(entry_throwable, 0, b'L'),
        Value::Object(Some(entry_throwable))
    );
    assert_eq!(Arc::strong_count(&entry_frames), 2);

    cache.set_backtrace_in_object_for_test(false);
    let disabled_throwable = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    shared.retain_captured_backtrace_shared(disabled_throwable, Arc::clone(&frames));
    assert_eq!(
        Arc::strong_count(&frames),
        3,
        "disabled path shares the caller's Arc directly"
    );
    assert_eq!(
        shared.throwable_stack_trace_len_for(disabled_throwable),
        Some(1)
    );
}
