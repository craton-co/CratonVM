use super::mark_intrinsic_cache_bypass;
use cratonvm_native_api::{NativeContext, NativeKind, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

fn dummy_native(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// Reproduces the exact two-step this file performs — `resolve_id` on the
/// **declaring** class while the class-manager read guard is still alive,
/// then `mark_intrinsic_cache_bypass` once the guard is dropped — and pins
/// that it lands on the declaring row.
///
/// The receiver's own class is the tempting wrong answer, and it is wrong
/// twice over: the superclass walk above the call site has already
/// established that nothing is registered strictly below the declaring
/// class (`native_override_below_declaring`, which would have *vetoed* the
/// intrinsic if it had found one), so a receiver-keyed lookup would resolve
/// nothing and mark nothing — leaving the row that actually loses the calls
/// still claiming to be a total.
///
/// `java/lang/Object.hashCode()I` is the concrete case: G33-1 §2 measured
/// it reporting **1** for 100,000 interpreted calls.
#[test]
fn the_declaring_classes_row_is_the_one_that_loses_the_calls() {
    let mut registry = NativeMethodRegistry::new();
    { let r = &mut registry; 
        r.register("java/lang/Object", "hashCode", "()I", dummy_native, NativeKind::Bridge);
     }
    // The receiver class in this scenario registers nothing — that is
    // precisely why the intrinsic was allowed to win.
    assert!(
        registry
            .resolve_id("p/Receiver", "hashCode", "()I")
            .is_none(),
        "the walk would have vetoed the intrinsic if the receiver had a native"
    );

    let declaring_name = "java/lang/Object";
    let bypassed_native_id = registry.resolve_id(declaring_name, "hashCode", "()I");
    let id = bypassed_native_id.expect("the declaring class carries the row");

    registry.record_invocation(id);
    mark_intrinsic_cache_bypass(&registry, id);

    assert_eq!(
        registry.invocations_of_id(id),
        Some(1),
        "the floor survives — this is the measured `Object.hashCode` = 1"
    );
    assert_eq!(registry.invocations_complete(id), Some(false));
    assert_eq!(registry.slots_with_incomplete_invocations(), 1);
}

/// A record's `hashCode`/`equals` intrinsic (`record_object_intrinsic`) is
/// not keyed on a fixed class and has no registry row at all, so the
/// resolve yields `None` and the fill proceeds unmarked. That is correct:
/// a triple with no row has no `invocations` cell to mislead anyone.
///
/// It must also not panic — this is the inline-cache fill path for every
/// virtual call in the VM.
#[test]
fn an_intrinsic_with_no_registry_row_leaves_the_census_untouched() {
    let registry = NativeMethodRegistry::new();
    assert!(registry
        .resolve_id("p/SomeRecord", "hashCode", "()I")
        .is_none());
    assert_eq!(registry.slots_with_incomplete_invocations(), 0);
}
