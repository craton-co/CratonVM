use super::*;
use crate::config::VmConfig;
use crate::jit::alloc_class_cache::{alloc_class_cache_enabled, ClassAllocInfo, PrimKind};
use std::sync::Arc;

#[test]
pub fn only_a_published_empty_recipe_skips_the_call() {
    if !alloc_class_cache_enabled() {
        return;
    }
    let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
    let (plain, prim, fin, unseen) = (
        ClassId::new(0x0007_0001),
        ClassId::new(0x0007_0002),
        ClassId::new(0x0007_0003),
        ClassId::new(0x0007_0004),
    );
    let cache = &shared.jit.jit_alloc_class_cache;
    cache.insert(
        plain.as_u32(),
        ClassAllocInfo {
            has_finalizer: false,
            reference_kind: 0,
            prim_inits: Vec::new().into_boxed_slice(),
            ref_inits: Box::new([]),
        },
    );
    cache.insert(
        prim.as_u32(),
        ClassAllocInfo {
            has_finalizer: false,
            reference_kind: 0,
            prim_inits: vec![(0u32, PrimKind::Int), (1u32, PrimKind::Long)].into_boxed_slice(),
            ref_inits: Box::new([]),
        },
    );
    cache.insert(
        fin.as_u32(),
        ClassAllocInfo {
            has_finalizer: true,
            reference_kind: 0,
            prim_inits: Vec::new().into_boxed_slice(),
            ref_inits: Box::new([]),
        },
    );
    // Round i1 wave 5: int-family defaults are the zeroed body's own
    // reading, so an int-only class owes the helper nothing either.
    let int_only = ClassId::new(0x0007_0005);
    cache.insert(
        int_only.as_u32(),
        ClassAllocInfo {
            has_finalizer: false,
            reference_kind: 0,
            prim_inits: vec![(0u32, PrimKind::Int), (1u32, PrimKind::Int)].into_boxed_slice(),
            ref_inits: Box::new([]),
        },
    );
    assert!(jit_post_alloc_init_is_noop(&shared, plain));
    assert!(
        jit_post_alloc_init_is_noop(&shared, int_only),
        "an int-family default needs no store"
    );
    assert!(
        !jit_post_alloc_init_is_noop(&shared, prim),
        "a long default is owed"
    );
    assert!(
        !jit_post_alloc_init_is_noop(&shared, fin),
        "a finalizer must be registered"
    );
    assert!(
        !jit_post_alloc_init_is_noop(&shared, unseen),
        "no recipe yet: the call is what builds it"
    );
}

/// The cached arm stores no int-family default, and the slot still reads
/// `Int(0)` from the allocator's zeroed body; tagged kinds are still
/// written.
#[test]
pub fn the_cached_arm_leaves_int_defaults_to_the_zeroed_body() {
    if !alloc_class_cache_enabled() {
        return;
    }
    let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
    let cid = ClassId::new(0x0007_0011);
    shared.jit.jit_alloc_class_cache.insert(
        cid.as_u32(),
        ClassAllocInfo {
            has_finalizer: false,
            reference_kind: 0,
            prim_inits: vec![
                (0u32, PrimKind::Int),
                (1u32, PrimKind::Long),
                (2u32, PrimKind::Double),
            ]
            .into_boxed_slice(),
            ref_inits: Box::new([]),
        },
    );
    let obj = shared.mem.heap.alloc_object(cid, 3);
    jit_post_alloc_init(&shared, obj, cid);
    assert_eq!(shared.mem.heap.get_field(obj, 0), Value::Int(0));
    assert_eq!(shared.mem.heap.get_field(obj, 1), Value::Long(0));
    assert_eq!(shared.mem.heap.get_field(obj, 2), Value::Double(0.0));
}

/// The announce helper's degenerate inputs return the object untouched and
/// never reach the heap (a null VM, a null object, a zero footprint).
#[test]
pub fn the_zgc_announce_helper_passes_degenerate_inputs_through() {
    // SAFETY: every call below takes an early return before any
    // dereference of `vm_ptr` or `obj_ptr`.
    unsafe {
        assert_eq!(jit_zgc_note_tlab_object(0, 0x1000, 32), 0x1000);
        assert_eq!(jit_zgc_note_tlab_object(0x10, 0, 32), 0);
        assert_eq!(jit_zgc_note_tlab_object(0x10, 0x1000, 0), 0x1000);
    }
}

/// Round 9 wave 8 (review8c F1): a trap that reaches the helper sink --
/// a callee trapped under a COMPILED caller -- applies the per-bci de-spec
/// rule too. Before the fix only `deopt_resume.rs`'s sink recorded
/// `(method, bci)`, so `TryLoop.sumThrow`'s failing loop-header BCE guard
/// was re-emitted on every recompile until the method was retired.
#[test]
pub fn trapped_callee_sink_despecs_a_bci_after_the_per_bci_limit() {
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let key = "DespecTrappedCallee.sumThrow:([II)J";
    for n in 1..=4u32 {
        despeculate_trapped_method(
            &shared,
            "DespecTrappedCallee",
            "sumThrow",
            "([II)J",
            cratonvm_types::ClassId::new(0),
            cratonvm_jit::deopt::DeoptReason::BoundsCheck,
            5,
        );
        let despec_now = shared.jit.despec_registry.contains(key, 5);
        if n < 4 {
            assert!(
                !despec_now,
                "must NOT de-spec before the per-bci limit (n={n})"
            );
        } else {
            assert!(despec_now, "must de-spec once the per-bci limit is reached");
        }
    }
    let bounds = cratonvm_jit::deopt::DeoptReason::BoundsCheck;
    let spec = cratonvm_jit::deopt::speculation_id(5, bounds);
    assert!(
        shared
            .jit
            .despec_registry
            .contains_speculation(key, 5, spec),
        "the recorded speculation is the BoundsCheck one at bci 5"
    );
    // Per-site: another bci of the same method is untouched.
    assert!(!shared.jit.despec_registry.contains(key, 9));
    // The superseded-artifact sentinel never de-specs.
    for _ in 0..6 {
        despeculate_trapped_method(
            &shared,
            "DespecTrappedCallee",
            "other",
            "()V",
            cratonvm_types::ClassId::new(0),
            cratonvm_jit::deopt::DeoptReason::BoundsCheck,
            u32::MAX,
        );
    }
    let other_key = "DespecTrappedCallee.other:()V";
    assert!(!shared.jit.despec_registry.contains(other_key, u32::MAX));    }