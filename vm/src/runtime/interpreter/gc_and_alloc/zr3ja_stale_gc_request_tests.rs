//! zr z3/ja: a collection request decided before another collection
//! completed is dropped, counted, and costs no cycle; an unstamped or
//! current request collects as before. gci i48/zl2: on every collector
//! (it was ZGC only); `=0` stamps nothing anywhere.

use super::*;
use crate::config::{GcAlgorithm, VmConfig};
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::vm::SharedVm;
use std::sync::atomic::Ordering;

#[test]
fn the_rule_is_any_collection_since_the_stamp() {
    assert!(!stamp_is_stale(4, 4));
    assert!(stamp_is_stale(4, 5));
    assert!(stamp_is_stale(4, 6));
}

/// zr s1/ca: the opt-in `jit_newarray` headroom arm's ZGC rate limit --
/// one collection per 1/32nd of the heap allocated since the last one,
/// never less than the futile window.
#[test]
fn zs1ca_the_newarray_headroom_rate_is_a_32nd_of_the_heap() {
    let cap = 512 * 1024 * 1024;
    let quantum = cap / ZGC_JIT_NEWARRAY_HEADROOM_RATE_DIVISOR;
    assert!(newarray_headroom_rate_skips(0, cap));
    assert!(newarray_headroom_rate_skips(quantum - 1, cap));
    assert!(!newarray_headroom_rate_skips(quantum, cap));
    // A tiny heap: the futile window is the floor.
    let floor = ZGC_REFUSAL_FUTILE_WINDOW;
    assert!(newarray_headroom_rate_skips(floor - 1, 1024));
    assert!(!newarray_headroom_rate_skips(floor, 1024));
}


#[test]
#[cfg(feature = "zgc")]
fn a_stale_zgc_request_is_dropped_and_a_current_one_collects() {
    let shared = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Zgc,
        ..VmConfig::default()
    });
    if !shared.mem.heap.is_zgc() {
        return; // a build without the `zgc` feature
    }
    let mut thread = JvmThread::new(ThreadId(0), "zr3ja-stale");
    let stale = zgc_gc_request_stamp(&shared).expect("ZGC stamps (default ON)");
    // Another thread's collection completes after the decision.
    shared
        .mem
        .tlab_wedge
        .collections_sealed
        .fetch_add(1, Ordering::Relaxed);
    let cycles = shared.mem.gc_cycle_count.load(Ordering::Relaxed);
    assert!(!maybe_gc_forced_stamped_pub_at(
        &shared,
        &mut thread,
        "zr3ja-test",
        Some(stale)
    ));
    assert_eq!(
        shared.mem.gc_cycle_count.load(Ordering::Relaxed),
        cycles,
        "a dropped request runs no cycle"
    );
    let line = zgc_stale_gc_requests_line(&shared).expect("a ZGC line");
    assert_eq!(
        line,
        "[GC] zgc_stale_gc_requests: stamped=1 dropped=1 newarray_headroom_rate_skips=0"
    );
    // A current stamp is the old door: this thread collects.
    let current = zgc_gc_request_stamp(&shared);
    assert!(maybe_gc_forced_stamped_pub_at(
        &shared,
        &mut thread,
        "zr3ja-test",
        current
    ));
    assert!(shared.mem.gc_cycle_count.load(Ordering::Relaxed) > cycles);
    assert!(
        zgc_stale_gc_requests_line(&shared).is_some_and(|l| l.contains("stamped=2 dropped=1 "))
    );
}

/// zr z4/rv4: a request decided AFTER a pause released the world, while
/// the finishing thread had not yet run `gc_event_finish`, is current:
/// the late `gc_cycle_count` bump must not make it stale. Stamped against
/// `gc_cycle_count` (z3/ja) this request was dropped.
#[test]
#[cfg(feature = "zgc")]
fn a_request_decided_after_the_release_survives_the_late_cycle_bump() {
    let shared = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Zgc,
        ..VmConfig::default()
    });
    if !shared.mem.heap.is_zgc() {
        return; // a build without the `zgc` feature
    }
    // The pause sealed (inside the pause, before `complete_gc`) ...
    shared
        .mem
        .tlab_wedge
        .collections_sealed
        .fetch_add(1, Ordering::Release);
    // ... a released mutator decides and stamps ...
    let stamp = zgc_gc_request_stamp(&shared);
    assert!(stamp.is_some());
    // ... and only then does the finishing thread's `gc_event_finish`
    // advance the ordinal.
    shared.mem.gc_cycle_count.fetch_add(1, Ordering::Relaxed);
    assert!(
        !zgc_gc_request_is_stale(&shared, stamp),
        "the request was made on the post-collection heap"
    );
    // A collection sealed after the decision still serves it.
    shared
        .mem
        .tlab_wedge
        .collections_sealed
        .fetch_add(1, Ordering::Release);
    assert!(zgc_gc_request_is_stale(&shared, stamp));
}


/// gci i48/zl2: on Generational and G1 a request decided before another
/// collection was sealed is dropped, counted, and runs no cycle, as on ZGC
/// (HotSpot's `skip_operation` on every collector); a request decided
/// after it is current. The epoch is the shared door's
/// (`run_collection_pause` bumps `collections_sealed` on every collector).
#[test]
fn gci_i48_zl2_a_stale_request_is_dropped_on_generational_and_g1() {
    for algorithm in [GcAlgorithm::Generational, GcAlgorithm::G1] {
        let shared = SharedVm::new(VmConfig {
            gc_algorithm: algorithm,
            ..VmConfig::default()
        });
        let mut thread = JvmThread::new(ThreadId(0), "gci-i48-zl2-stale");
        let stale = zgc_gc_request_stamp(&shared)
            .unwrap_or_else(|| panic!("{algorithm:?} stamps (default ON)"));
        shared
            .mem
            .tlab_wedge
            .collections_sealed
            .fetch_add(1, Ordering::Release);
        let cycles = shared.mem.gc_cycle_count.load(Ordering::Relaxed);
        assert!(
            !maybe_gc_forced_stamped_pub_at(&shared, &mut thread, "zl2-test", Some(stale)),
            "{algorithm:?}: a stale request is dropped"
        );
        assert_eq!(
            shared.mem.gc_cycle_count.load(Ordering::Relaxed),
            cycles,
            "{algorithm:?}: a dropped request runs no cycle"
        );
        assert_eq!(
            zgc_stale_gc_requests_line(&shared).as_deref(),
            Some("[GC] zgc_stale_gc_requests: stamped=1 dropped=1"),
            "{algorithm:?}"
        );
        // A stamp taken after that collection is current.
        let current = zgc_gc_request_stamp(&shared);
        assert!(current.is_some());
        assert!(!zgc_gc_request_is_stale(&shared, current), "{algorithm:?}");
    }
}

/// gci i48/zl2: source pin -- the stamp has no collector gate left, and
/// the epoch is still sealed by the shared pause door, which every
/// collector's collection takes.
#[test]
fn gci_i48_zl2_the_stamp_and_its_epoch_are_collector_neutral() {
    let src = concat!(include_str!("../gc_and_alloc/mod.rs"), "\n", include_str!("../gc_and_alloc/mtroots_on.rs"), "\n", include_str!("../gc_and_alloc/gc_notifications_go_to_delivery.rs"), "\n", include_str!("../gc_and_alloc/zgc_refusal_line.rs"), "\n", include_str!("../gc_and_alloc/finalizers_forced.rs"), "\n", include_str!("../gc_and_alloc/reference_round.rs"), "\n", include_str!("../gc_and_alloc/on_out_of_memory_error_commands.rs"), "\n", include_str!("../gc_and_alloc/frozen_peer_referents.rs"), "\n", include_str!("../gc_and_alloc/g1_force_full_cycle.rs"));
    let start = src
        .find("\npub(crate) fn zgc_gc_request_stamp(")
        .expect("zgc_gc_request_stamp");
    let end = start + src[start..].find("\n\u{7d}\n").expect("its end");
    assert!(!src[start..end].contains("VmHeap::Zgc"));
    let pause = src
        .find("\npub(in crate::runtime::interpreter::gc_and_alloc) fn run_collection_pause(")
        .expect("run_collection_pause");
    let pause_end = pause + src[pause..].find("\n\u{7d}\n").expect("its end");
    let sealed = src[pause..pause_end]
        .find(".collections_sealed")
        .expect("the pause seals the epoch");
    let release = src[pause..pause_end]
        .find(".complete_gc(result.pointer_map)")
        .expect("the release");
    assert!(sealed < release, "sealed before the world is released");
}
