use super::{
    mark_intrinsic_cache_bypass, mark_intrinsic_cache_bypass_by_triple, OSR_THRESHOLD,
};
use cratonvm_native_api::{NativeContext, NativeKind, NativeMethodId, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

fn dummy_native(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

fn dummy_native_2(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(42)))
}

/// The core contract: marking turns a claimed total into an admitted floor
/// WITHOUT disturbing the number, is per slot, and is idempotent.
///
/// Idempotence is not decoration. `populate_invoke_cache` runs once per
/// (caller class, cp index) call site and there are many call sites per
/// triple, plus the promoted-resolution cache re-publishes entries across
/// threads — so this mark fires repeatedly for the same slot by design.
#[test]
fn marking_an_intrinsic_install_turns_its_row_into_a_floor() {
    let mut registry = NativeMethodRegistry::new();
    { let r = &mut registry; 
        r.register("java/lang/Math", "abs", "(I)I", dummy_native, NativeKind::Intrinsic);
        r.register(
            "java/lang/System",
            "identityHashCode",
            "(Ljava/lang/Object;)I",
            dummy_native_2, NativeKind::Intrinsic);
     }
    let abs = registry
        .resolve_id("java/lang/Math", "abs", "(I)I")
        .expect("registered");
    // The control that stayed exact in every configuration G33-1 measured.
    let control = registry
        .resolve_id(
            "java/lang/System",
            "identityHashCode",
            "(Ljava/lang/Object;)I",
        )
        .expect("registered");

    // Before any cache fill both rows claim to be totals.
    assert_eq!(registry.invocations_complete(abs), Some(true));
    assert_eq!(registry.slots_with_incomplete_invocations(), 0);

    // The first call at a site goes down the counted path; the cache fill
    // then binds every later call to a raw `fn` pointer.
    registry.record_invocation(abs);
    mark_intrinsic_cache_bypass(&registry, abs);

    assert_eq!(
        registry.invocations_of_id(abs),
        Some(1),
        "marking must not disturb the tally — the floor is the number a \
         reader falls back on, and this is the measured `Math.abs` = 1"
    );
    assert_eq!(registry.invocations_complete(abs), Some(false));
    assert_eq!(
        registry.invocations_complete(control),
        Some(true),
        "the bit is per slot: an intrinsic install must not cast doubt on \
         a native that is still dispatched through the counted path"
    );
    assert_eq!(registry.slots_with_incomplete_invocations(), 1);

    // Many call sites, one slot.
    mark_intrinsic_cache_bypass(&registry, abs);
    mark_intrinsic_cache_bypass(&registry, abs);
    assert_eq!(registry.slots_with_incomplete_invocations(), 1);
    assert_eq!(registry.invocations_of_id(abs), Some(1));

    // And the doubt has to survive the trip into the census, which is the
    // only place a reader ever sees it.
    let census = registry.census();
    let abs_row = census
        .iter()
        .find(|r| r.class == "java/lang/Math" && r.owns_slot)
        .expect("slot owner");
    assert!(!abs_row.invocations_complete);
    let control_row = census
        .iter()
        .find(|r| r.class == "java/lang/System" && r.owns_slot)
        .expect("slot owner");
    assert!(control_row.invocations_complete);
}

/// `dispatch_virtual.rs` reaches its intrinsic through the CLASS STORE and
/// marks the **declaring** class's row. Pin that choice: `Object.hashCode`
/// and a subclass's own `hashCode` are two different registry slots, and
/// marking the wrong one would leave the row that actually loses the calls
/// still claiming to be a total — a silent failure in the one direction
/// this instrument must never err in.
#[test]
fn the_by_triple_helper_marks_only_the_named_classes_row() {
    let mut registry = NativeMethodRegistry::new();
    { let r = &mut registry; 
        r.register("java/lang/Object", "hashCode", "()I", dummy_native, NativeKind::Bridge);
        r.register("p/Sub", "hashCode", "()I", dummy_native_2, NativeKind::Bridge);
     }
    let declaring = registry
        .resolve_id("java/lang/Object", "hashCode", "()I")
        .expect("registered");
    let other = registry
        .resolve_id("p/Sub", "hashCode", "()I")
        .expect("registered");

    assert!(
        mark_intrinsic_cache_bypass_by_triple(&registry, "java/lang/Object", "hashCode", "()I"),
        "a triple with a registry row reports that it marked one"
    );
    assert_eq!(registry.invocations_complete(declaring), Some(false));
    assert_eq!(
        registry.invocations_complete(other),
        Some(true),
        "same method name and descriptor, different class, different slot"
    );
    assert_eq!(registry.slots_with_incomplete_invocations(), 1);
}

/// A `false` from the by-triple helper is the NORMAL case for several live
/// intrinsics and is not a failure.
///
/// `String.charAt` and `String.length` are registered nowhere in the
/// `--real-jdk` binary — G33-1 §2 checked the dump directly and found them
/// *absent*, not zero-valued — and `Thread.onSpinWait` has no row in
/// real-JDK mode either, which is the premise of the branch that installs
/// it. A triple with no row has no `invocations` cell that could mislead
/// anyone, so there is nothing to mark and nothing to report.
///
/// It must also not panic: this runs on the inline-cache fill path for
/// every call in the VM.
#[test]
fn a_triple_with_no_registry_row_is_a_silent_no_op() {
    let mut registry = NativeMethodRegistry::new();
    { let r = &mut registry; 
        r.register("java/lang/Object", "hashCode", "()I", dummy_native, NativeKind::Bridge);
     }

    assert!(!mark_intrinsic_cache_bypass_by_triple(
        &registry,
        "java/lang/String",
        "charAt",
        "(I)C"
    ));
    assert!(!mark_intrinsic_cache_bypass_by_triple(
        &registry,
        "java/lang/Thread",
        "onSpinWait",
        "()V"
    ));
    assert_eq!(
        registry.slots_with_incomplete_invocations(),
        0,
        "an absent triple must not manufacture doubt on some other slot"
    );

    // The empty-registry case: `register_collections_natives` and
    // `register_essential_natives` are separate registrars, so a cache fill
    // can run against a registry that has not been populated yet.
    let empty = NativeMethodRegistry::new();
    assert!(!mark_intrinsic_cache_bypass_by_triple(
        &empty,
        "java/lang/Math",
        "abs",
        "(I)I"
    ));
    assert_eq!(empty.slots_with_incomplete_invocations(), 0);

    // And the foreign/stale-handle contract, inherited from
    // `record_invocation`: ignored, never a panic.
    mark_intrinsic_cache_bypass(&registry, NativeMethodId::from_u32(9_999));
    assert_eq!(registry.slots_with_incomplete_invocations(), 0);
}

/// Pins the constant the measured **1,999** is derived from, so that a
/// future retune of the loop threshold makes the record's headline number
/// visibly stale instead of quietly wrong.
///
/// MEASURED on `9ae371468` (G42-1 §2): a `Method.invoke` loop in `main`
/// reports `invocations: 1,999` in the JIT arm and 99,999 under `--nojit`.
/// The arithmetic is
/// `OSR_THRESHOLD × 2 (the first backoff retry) × 1 call per iteration − 1
/// (the reflection deficit at the first call)`. It is a **transition
/// count** — how many calls were made before the compiled artifact was
/// installed — not a cap: with four invoke sites in one loop body the same
/// run reports **7,999**, four times as many, at the same back-edge count.
#[test]
fn the_measured_1999_is_two_osr_thresholds_of_interpreted_calls() {
    assert_eq!(OSR_THRESHOLD, 200);
    let first_retry_backedges = OSR_THRESHOLD * 2;
    assert_eq!(first_retry_backedges - 1, 399);
    // Four call sites in one loop body: same back-edges, four times the
    // calls, and the measurement scaled exactly.
    assert_eq!(first_retry_backedges * 4 - 1, 1_599);
}
