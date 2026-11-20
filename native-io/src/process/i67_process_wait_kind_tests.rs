use super::{classified_stand_in_waits, process_timed_wait_kind, stand_in_wait_kind};
use cratonvm_native_api::BlockKind;


/// Interpreter round i2 wave 68, lane L1 (`CRATONVM_STAND_IN_WAIT_OBJECTS`):
/// a context that does not publish wait objects (the trait default, as
/// the VM with `=0`) makes no condition, so the `waitFor` regions run no
/// Java and publish nothing, as before.
#[test]
fn no_condition_is_made_where_wait_objects_are_not_published() {
    use cratonvm_native_api::NativeThreadAccess;
    let mut ctx = crate::test_support::MockNativeContext::new();
    assert!(!ctx.stand_in_wait_objects());
    assert_eq!(super::process_wait_condition(&mut ctx), None);
}


/// Wave 69: a condition already installed in the process's slot wins over
/// the one a racing first caller made; an empty slot keeps the caller's.
/// The slot is past the six reserved `java.lang.Process` slots and the six
/// bookkeeping slots, inside the layout.
#[test]
fn the_installed_condition_is_the_processs_one() {
    use cratonvm_native_api::NativeHeapAccess;
    assert_eq!(
        super::PROC_FIELD_CONDITION,
        super::JAVA_PROCESS_FIELD_COUNT + 6
    );
    assert!(super::PROC_FIELD_CONDITION < super::PROC_FIELD_COUNT);
    let mut ctx = crate::test_support::MockNativeContext::new();
    let process =
        ctx.alloc_object_with_class(super::PROC_FIELD_COUNT, super::SYNTHETIC_PROCESS_CLASS);
    let made = ctx.alloc_object(0);
    // The mock's compare-and-swap never succeeds, so an empty slot reads
    // back empty: the caller keeps (and publishes) its own.
    assert_eq!(
        super::install_process_wait_condition(&mut ctx, process, made),
        made
    );
    let kept = ctx.alloc_object(0);
    ctx.set_field(
        process,
        super::PROC_FIELD_CONDITION,
        cratonvm_types::Value::Object(Some(kept)),
    );
    assert_eq!(
        super::install_process_wait_condition(&mut ctx, process, made),
        kept
    );
}
