// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;
use crate::config::VmConfig;
use crate::threading::jvm_thread::ThreadId;

fn frame(code: Vec<u8>, method_name: &str) -> Frame {
    Frame::new(
        ClassId::new(0),
        "T".to_string(),
        method_name.to_string(),
        "()V".to_string(),
        None,
        code,
        vec![],
        8,
        4,
        &[],
    )
}

#[test]
fn local_write_invalidates_cached_deep_frame_roots() {
    if !crate::runtime::env_cache::rootsnap_cache() {
        return;
    }

    let shared = SharedVm::new(VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "root-snapshot-cache-test");

    // Frame 0 reads LOCAL[2] at pc 0, so the normal local-liveness filter
    // must keep that slot live. Frames 1 and 2 make frame 0 reusable by the
    // frozen-frame cache; the current top is never cached.
    thread.frames.push(frame(vec![0x2c, 0x57, 0xb1], "deep")); // aload_2; pop; return
    thread.frames.push(frame(vec![0xb1], "middle"));
    thread.frames.push(frame(vec![0xb1], "top"));

    update_root_snapshot(&shared, &mut thread);
    assert_eq!(thread.rs_cache.len(), 2);
    assert!(thread.rs_cache[0].1.is_empty());

    let obj = shared.mem.heap.alloc_object(ClassId::new(7), 0);
    let obj_addr = obj.as_ptr();
    let cached_key = thread.rs_cache[0].0;
    thread.frames[0].set_local_unchecked(2, Value::Object(Some(obj)));
    assert_ne!(
        (thread.frames[0].seq, thread.frames[0].exec_epoch),
        cached_key,
        "a local write must invalidate this frame's cached root set"
    );

    update_root_snapshot(&shared, &mut thread);
    let snapshot = thread.root_snapshot.lock();
    assert!(
        snapshot.iter().any(|root| root.as_ptr() == obj_addr),
        "updated root snapshot must include the object stored into deep LOCAL[2]"
    );
}

#[test]
fn root_snapshot_includes_live_native_handle_slots() {
    let shared = SharedVm::new(VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "handle-snapshot-test");
    let handled = shared.mem.heap.alloc_object(ClassId::new(9), 0);
    thread.handle_slots.push(Some(handled));

    update_root_snapshot(&shared, &mut thread);

    let handled_addr = handled.as_ptr();
    let snapshot = thread.root_snapshot.lock();
    assert!(
        snapshot.iter().any(|root| root.as_ptr() == handled_addr),
        "peer collectors must see objects owned only by a native handle scope"
    );
}

/// `thread.printed` and `thread.scoped_values` are rewritten by all three
/// post-GC remaps but used to be published by NEITHER snapshot path, so a
/// peer thread's only reference to such an object was invisible to a
/// collection running on another thread. Both live in `JvmThread` fields
/// rather than on a frame, so the frame walk cannot cover them.
#[test]
fn root_snapshot_includes_off_frame_thread_roots() {
    let shared = SharedVm::new(VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "off-frame-roots-test");

    let printed = shared.mem.heap.alloc_object(ClassId::new(9), 0);
    thread.printed.push(Value::Object(Some(printed)));

    let sv_key = shared.mem.heap.alloc_object(ClassId::new(9), 0);
    let sv_value = shared.mem.heap.alloc_object(ClassId::new(9), 0);
    thread
        .scoped_values
        .push((1, Some(sv_key), Value::Object(Some(sv_value))));

    update_root_snapshot(&shared, &mut thread);

    let snapshot = thread.root_snapshot.lock();
    let published =
        |o: cratonvm_types::ObjectRef| snapshot.iter().any(|root| root.as_ptr() == o.as_ptr());
    assert!(
        published(printed),
        "a peer's print-buffer object must be published to cross-thread collectors"
    );
    assert!(
        published(sv_key),
        "a peer's ScopedValue KEY must be published — Carrier.get reaches it"
    );
    assert!(
        published(sv_value),
        "a peer's ScopedValue binding must be published to cross-thread collectors"
    );
}
