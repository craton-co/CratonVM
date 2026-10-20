// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Deterministic mark-ingress checks, independent of concurrent race timing.
use super::*;
use crate::config::{GcAlgorithm, VmConfig};

fn arm(shared: &SharedVm) {
    match &shared.mem.heap {
        crate::memory::VmHeap::Generational(_) => shared.mem.concurrent_satb.activate(),
        crate::memory::VmHeap::G1(heap) => heap.satb_queue().activate(),
        #[cfg(feature = "zgc")]
        crate::memory::VmHeap::Zgc(heap) => heap.set_mark_active(true),
    }
}

fn take(shared: &SharedVm) -> Vec<usize> {
    let queue = match &shared.mem.heap {
        crate::memory::VmHeap::Generational(_) => &shared.mem.concurrent_satb,
        crate::memory::VmHeap::G1(heap) => heap.satb_queue(),
        #[cfg(feature = "zgc")]
        crate::memory::VmHeap::Zgc(heap) => {
            let mut entries = Vec::new();
            heap.drain_mark_ingress(&mut entries);
            return entries.into_iter().map(|v| v as usize).collect();
        }
    };
    cratonvm_gc::satb::flush_thread_satb_buffer(queue);
    queue.drain()
}

fn fixture(algorithm: GcAlgorithm) -> (SharedVm, ClassId, ClassId) {
    let shared = SharedVm::new(VmConfig {
        gc_algorithm: algorithm,
        ..VmConfig::with_host_jdk_default()
    });
    let (weak, object) = {
        let mut cm = shared.classes.class_manager_write();
        let weak = cm
            .load_class("java/lang/ref/WeakReference")
            .expect("reference fixture");
        let object = cm.load_class("java/lang/Object").expect("object fixture");
        let reference = cm
            .find_bootstrap_class_by_name("java/lang/ref/Reference")
            .expect("fixture: bootstrap Reference");
        assert_eq!(
            resolve_field_index_in_hierarchy(reference, "referent", cm.class_store()),
            Some(0)
        );
        assert!(
            cm.is_subclass_of(weak, reference),
            "fixture: inherited referent"
        );
        (weak, object)
    };
    (shared, weak, object)
}

fn algorithms() -> Vec<GcAlgorithm> {
    #[allow(unused_mut)]
    let mut algorithms = vec![GcAlgorithm::G1, GcAlgorithm::Generational];
    #[cfg(feature = "zgc")]
    algorithms.push(GcAlgorithm::Zgc);
    algorithms
}

fn markable_payload(shared: &SharedVm, object: ClassId) -> ObjectRef {
    // Generational's concurrent marker and default SATB filter visit old
    // objects only. A young payload is intentionally not a mark-queue entry.
    match &shared.mem.heap {
        crate::memory::VmHeap::Generational(_) => shared
            .mem
            .heap
            .try_alloc_object_old(object, 0)
            .expect("old generational payload"),
        _ => shared.mem.heap.alloc_object(object, 0),
    }
}

#[test]
fn native_referent_reads_publish_the_returned_value_on_every_collector() {
    for algorithm in algorithms() {
        let (shared, weak, object) = fixture(algorithm);
        let receiver = shared.mem.heap.alloc_object(weak, 4);
        let payload = markable_payload(&shared, object);
        shared
            .mem
            .heap
            .set_field(receiver, 0, Value::Object(Some(payload)));
        arm(&shared);
        let mut thread = JvmThread::new(ThreadId(55101), "referent-read");
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        let _ = take(&shared);
        assert_eq!(ctx.get_field(receiver, 0), Value::Object(Some(payload)));
        assert!(
            take(&shared).is_empty(),
            "raw-read control must not publish a keep-alive"
        );
        for method in [
            "getReference",
            "getReferenceVolatile",
            "compareAndExchangeReference",
        ] {
            let desc = if method.starts_with("compare") {
                "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"
            } else {
                "(Ljava/lang/Object;J)Ljava/lang/Object;"
            };
            let callback = shared
                .natives
                .native_methods
                .find("jdk/internal/misc/Unsafe", method, desc)
                .expect("genuine Unsafe native");
            // The genuine Unsafe offset is a byte offset when that per-VM
            // policy is enabled, rather than the heap accessor's slot index.
            let offset = if ctx.unsafe_byte_field_offsets() {
                16
            } else {
                0
            };
            let args = [
                Value::Object(None),
                Value::Object(Some(receiver)),
                Value::Long(offset),
                Value::Object(None),
                Value::Object(None),
            ];
            let _ = take(&shared);
            assert_eq!(
                callback(&mut ctx, &args).unwrap(),
                Some(Value::Object(Some(payload))),
                "{algorithm:?} {method}"
            );
            assert!(
                take(&shared).contains(&(payload.as_ptr() as usize)),
                "{algorithm:?} {method}: referent must enter the active mark queue"
            );
            assert_eq!(
                shared.mem.heap.get_field(receiver, 0),
                Value::Object(Some(payload)),
                "failed compare/exchange must not mutate the reference"
            );
        }
    }
}

#[test]
fn referent_read_screen_excludes_ordinary_fields_arrays_and_other_slots() {
    for algorithm in algorithms() {
        let (shared, weak, object) = fixture(algorithm);
        let reference = shared.mem.heap.alloc_object(weak, 4);
        let ordinary = shared.mem.heap.alloc_object(object, 2);
        let payload = markable_payload(&shared, object);
        let array =
            shared
                .mem
                .heap
                .alloc_array(weak, cratonvm_types::ArrayElementType::Reference, 2);
        arm(&shared);
        let value = Value::Object(Some(payload));
        for (receiver, slot) in [(ordinary, 0), (reference, 1), (array, 0)] {
            let _ = take(&shared);
            crate::memory::ref_discovery::keep_alive_referent_read(&shared, receiver, slot, value);
            assert!(
                take(&shared).is_empty(),
                "{algorithm:?}: unrelated field was retained"
            );
        }
        crate::memory::ref_discovery::keep_alive_referent_read(&shared, reference, 0, value);
        assert!(
            take(&shared).contains(&(payload.as_ptr() as usize)),
            "{algorithm:?}: positive control"
        );
    }
}

#[test]
fn synthetic_embedding_referent_reads_keep_the_native_slot_alive() {
    for algorithm in algorithms() {
        let shared = SharedVm::new(VmConfig {
            gc_algorithm: algorithm,
            ..VmConfig::default()
        });
        let (weak, object) = {
            let mut cm = shared.classes.class_manager_write();
            let weak = cm
                .load_class("java/lang/ref/WeakReference")
                .expect("synthetic weak reference");
            let object = cm.load_class("java/lang/Object").expect("synthetic object");
            assert!(cm.get_class(weak).unwrap().origin.is_compatibility_stub());
            (weak, object)
        };
        let receiver = shared.mem.heap.alloc_object(weak, 4);
        let payload = markable_payload(&shared, object);
        let ordinary = shared.mem.heap.alloc_object(object, 1);
        let array =
            shared
                .mem
                .heap
                .alloc_array(weak, cratonvm_types::ArrayElementType::Reference, 1);
        shared
            .mem
            .heap
            .set_field(receiver, 0, Value::Object(Some(payload)));
        arm(&shared);
        crate::memory::ref_discovery::keep_alive_referent_read(
            &shared,
            receiver,
            0,
            Value::Object(Some(payload)),
        );
        assert!(
            take(&shared).contains(&(payload.as_ptr() as usize)),
            "{algorithm:?}: synthetic referent"
        );
        let _ = take(&shared);
        crate::memory::ref_discovery::keep_alive_referent_read_raw(
            &shared,
            receiver,
            0,
            payload.as_ptr() as usize,
        );
        assert!(
            take(&shared).contains(&(payload.as_ptr() as usize)),
            "{algorithm:?}: raw compiled referent"
        );
        for obj in [ordinary, array] {
            crate::memory::ref_discovery::keep_alive_referent_read(
                &shared,
                obj,
                0,
                Value::Object(Some(payload)),
            );
            assert!(
                take(&shared).is_empty(),
                "{algorithm:?}: unrelated synthetic field"
            );
        }
        for (obj, slot) in [(ordinary, 0), (receiver, 1), (array, 0)] {
            crate::memory::ref_discovery::keep_alive_referent_read_raw(
                &shared,
                obj,
                slot,
                payload.as_ptr() as usize,
            );
            assert!(
                take(&shared).is_empty(),
                "{algorithm:?}: unrelated raw field"
            );
        }
        crate::memory::ref_discovery::keep_alive_referent_read_raw(
            &shared,
            receiver,
            0,
            0x2222_2222,
        );
        assert!(
            take(&shared).is_empty(),
            "{algorithm:?}: raw non-oop referent is refused"
        );
    }
}

#[test]
fn referent_read_identity_is_per_vm() {
    let (first, weak, object) = fixture(GcAlgorithm::G1);
    let second = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::G1,
        ..VmConfig::with_host_jdk_default()
    });
    let reference = first.mem.heap.alloc_object(weak, 4);
    let payload = markable_payload(&first, object);
    arm(&first);
    arm(&second);
    crate::memory::ref_discovery::keep_alive_referent_read(
        &first,
        reference,
        0,
        Value::Object(Some(payload)),
    );
    assert!(take(&first).contains(&(payload.as_ptr() as usize)));
    assert!(
        take(&second).is_empty(),
        "the second VM must not receive the first VM's keep-alive"
    );
}
