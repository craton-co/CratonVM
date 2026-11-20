//! gc-common w8 verification: `ldc` of a String literal on a full heap is
//! a catchable `OutOfMemoryError`, not a process abort
//! (`intern_string_literal_or_oom`).

use super::*;
use crate::config::{GcAlgorithm, VmConfig};
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::vm::SharedVm;
use cratonvm_types::ClassId;

fn small_gen_vm() -> SharedVm {
    SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        max_heap_size: 8 * 1024 * 1024,
        initial_heap_size: 8 * 1024 * 1024,
        ..VmConfig::default()
    })
}

/// Two resolutions of one literal answer the identical pooled object.
#[test]
fn a_literal_is_pooled() {
    let shared = small_gen_vm();
    let mut thread = JvmThread::new(ThreadId(0), "w8v-ldc-pool");
    let a = intern_string_literal_or_oom(&shared, &mut thread, "w8v-literal").expect("room");
    let b = intern_string_literal_or_oom(&shared, &mut thread, "w8v-literal").expect("hit");
    assert_eq!(a, b);
}

/// With every byte of the heap held live, the ladder runs and the literal
/// is refused with `OutOfMemoryError` (the process is still here to see it).
#[test]
fn a_literal_on_a_full_heap_is_an_oome() {
    let shared = small_gen_vm();
    let mut thread = JvmThread::new(ThreadId(0), "w8v-ldc-full");
    // Fill, collect, and fill again until a collection frees nothing: a
    // young collection promotes the pinned objects and empties eden, so
    // one pass of filling leaves room behind it.
    let mut full = false;
    for _ in 0..64 {
        // Both the young (TLAB-less) path and the whole-heap path, so
        // neither the young generation nor the old one keeps room.
        for full_heap in [false, true] {
            for slots in [1024usize, 64, 1] {
                for _ in 0..1_000_000 {
                    let heap = &shared.mem.heap;
                    let got = if full_heap {
                        heap.try_alloc_object_full(ClassId::new(0), slots)
                    } else {
                        heap.try_alloc_object(ClassId::new(0), slots)
                    };
                    match got {
                        Some(o) => thread.native_pin_roots.push(o),
                        None => break,
                    }
                }
            }
        }
        maybe_gc_forced_at(&shared, &mut thread, "w8v-test-fill");
        // JIT r20 lane gcI: a probe that SUCCEEDS is pinned too. Left
        // unpinned it is garbage the next forced collection frees, which
        // re-opens exactly one object's room for the next probe: since the
        // old generation may grow past its 2/3 cap on a refused request
        // (`OldGen::grow_after_refusal`), the first post-growth probe
        // succeeds and the recipe oscillated on its own garbage for all
        // 64 passes. Pinned, every pass only ever adds live data, so the
        // fill is monotone and ends once the (bounded) heap is full.
        let heap = &shared.mem.heap;
        match heap.try_alloc_object(ClassId::new(0), 1) {
            Some(o) => thread.native_pin_roots.push(o),
            None => match heap.try_alloc_object_full(ClassId::new(0), 1) {
                Some(o) => thread.native_pin_roots.push(o),
                None => {
                    full = true;
                    break;
                }
            },
        }
    }
    assert!(full, "the pinned objects never filled the 8 MB heap");
    let got = intern_string_literal_or_oom(&shared, &mut thread, "w8v-literal-on-a-full-heap");
    match got {
        Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::OutOfMemoryError { message },
        ))) => assert!(message.starts_with("Java heap space"), "{message}"),
        Ok(_) => panic!("a pinned-full heap cannot hold the literal"),
        Err(other) => panic!("expected OutOfMemoryError, got {other:?}"),
    }
}
