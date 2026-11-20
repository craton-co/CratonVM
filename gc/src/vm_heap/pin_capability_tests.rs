use super::{GcBackend, VmHeap};

/// `honours_conservative_pins` is spent by the cross-thread JIT coverage
/// handshake: `conservative_roots::refresh_moving_young_coverage_for_collection`
/// passes it to `pinned_credit_admissible`, and a `true` there lets a moving
/// cycle proceed while a peer thread holds compiled frames nobody proved
/// rewritable — on the promise that the objects those frames name will not
/// move. Only a backend that can WITHHOLD an object can keep that promise.
///
/// The generational young collector cannot, and the reason is structural
/// rather than a policy choice: it is Cheney copying, from-space is
/// reclaimed wholesale, so every live object in it moves by construction and
/// there is no "withhold this one" to implement. `gen_heap.rs` and
/// `gen_evac.rs` accordingly contain no reader of
/// `gc_quiescence::pinned_jit_roots_snapshot()` at all.
///
/// That arm read `true` until 2026-09-06, and the cost was a wrong ANSWER,
/// not a slow one:
/// `docs/feature-designs/jit-ir-optimizations.md`
/// (19 netty classes, Generational only, an NPE on a live JUnit object) and
/// the ten-second H2 SIGSEGV in `70c486744`'s call-site comment. The rule
/// itself is tested next to `pinned_credit_admissible`; this is the other
/// half of it — the input — and without this test a "simplification" of the
/// match below fails a netty suite instead of a unit test.
#[test]
fn honours_conservative_pins_is_a_capability_not_a_policy() {
    // Small heaps: this asks a question about the backend, not about
    // capacity, and the sizes match the ones the sibling tests in this file
    // already construct.
    const BYTES: usize = 8 * 1024 * 1024;
    assert!(
        !VmHeap::new(GcBackend::Generational, BYTES).honours_conservative_pins(),
        "the Cheney young collector reclaims from-space wholesale, so it \
         cannot honour a pin and must not let one discharge a peer's \
         coverage obligation"
    );
    assert!(
        VmHeap::new(GcBackend::G1, BYTES).honours_conservative_pins(),
        "G1 withholds the pinned regions from the collection set"
    );
    #[cfg(feature = "zgc")]
    assert!(
        VmHeap::new(GcBackend::Zgc, BYTES).honours_conservative_pins(),
        "ZGC withholds the pinned pages; `relocate_stw` reads the same snapshot"
    );
}
