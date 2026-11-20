//! zr z7/rv6: the JIT box door after a futile-skipped refusal.
use super::*;
use crate::config::{GcAlgorithm, VmConfig};
use crate::threading::jvm_thread::ThreadId;
use cratonvm_gc::heap::ArrayElementType;
use std::sync::atomic::Ordering;

/// A full ZGC heap collected with every byte pinned live (nothing
/// allocated since: the futile window), whose pins the program then
/// DROPS. A compiled `Integer.valueOf` box allocation must collect and
/// succeed, as HotSpot's does. Before the fix the cr rule skipped the
/// door's forced collection (window < 4 KiB) and the door, which has no
/// clear-soft rung to pay it, fell through to the OOME.
#[test]
#[cfg(feature = "zgc")]
fn a_box_after_a_futile_skip_pays_the_owed_collection() {
    let shared = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Zgc,
        max_heap_size: 16 << 20,
        initial_heap_size: 16 << 20,
        ..VmConfig::default()
    });
    if !shared.mem.heap.is_zgc() {
        return; // a build without the `zgc` feature
    }
    let mut thread = JvmThread::new(ThreadId(0), "zr7rv6-box");
    let heap = &shared.mem.heap;
    let fill = |thread: &mut JvmThread| -> usize {
        let mut n = 0usize;
        for len in [64 * 1024usize, 4096, 256, 16, 1] {
            while let Some(a) =
                heap.try_alloc_array(ClassId::new(0), ArrayElementType::Byte, len)
            {
                thread.native_pin_roots.push(a);
                n += 1;
            }
        }
        while let Some(o) = heap.try_alloc_object_full(ClassId::new(77), 1) {
            thread.native_pin_roots.push(o);
            n += 1;
        }
        n
    };
    let mut full = false;
    for _ in 0..16 {
        fill(&mut thread);
        crate::runtime::interpreter::maybe_gc_forced_pub_at(
            &shared,
            &mut thread,
            "zr7rv6-fill",
        );
        if fill(&mut thread) == 0 {
            full = true;
            break;
        }
    }
    assert!(full, "the pins never filled the heap");
    let window = heap.zgc_bytes_allocated_since_cycle().expect("ZGC");
    assert!(
        window < crate::runtime::interpreter::gc_and_alloc::ZGC_REFUSAL_FUTILE_WINDOW,
        "fixture: window {window}"
    );
    // The program drops everything; no collection has seen it yet.
    thread.native_pin_roots.clear();
    shared
        .mem
        .gc_unproductive_streak
        .store(0, Ordering::Relaxed);
    let cycles = shared.mem.gc_cycle_count.load(Ordering::Relaxed);
    let boxed = jit_box_cold_alloc(&shared, &mut thread, ClassId::new(77));
    assert!(boxed.is_some(), "a heap of dropped garbage refused one box");
    assert!(shared.mem.gc_cycle_count.load(Ordering::Relaxed) > cycles);
    assert!(!shared
        .mem
        .tlab_wedge
        .zgc_refusal
        .soft_rung_owed
        .load(Ordering::Relaxed));
}

