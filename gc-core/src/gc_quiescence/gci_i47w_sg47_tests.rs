//! gci i47w/sg47: the `System.gc()` tenure-all request
//! (`CRATONVM_GC_SYSTEM_GC_TENURES_ALL`) is armed only under the switch,
//! is read without being consumed, and is withdrawn by the take.
use cratonvm_types::flags::with_thread_overrides;

#[test]
fn gci_i47w_sg47_tenure_request_is_armed_and_consumed_by_take() {
    let _ = super::take_system_gc_tenure_request();
    assert!(super::system_gc_tenures_all_enabled());
    super::request_system_gc_tenure();
    assert!(super::system_gc_tenure_requested());
    assert!(
        super::system_gc_tenure_requested(),
        "a read does not consume: the door's refusal-and-retry reads it again"
    );
    assert!(super::take_system_gc_tenure_request());
    assert!(!super::system_gc_tenure_requested(), "withdrawn by the take");
    assert!(!super::take_system_gc_tenure_request());    }