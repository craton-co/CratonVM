use super::{class_entry_answer_kept, class_entry_record_all_keys};
use cratonvm_classloading::ClassOrigin;
use cratonvm_types::ClassLoaderId;
use std::sync::Arc;

fn app() -> ClassOrigin {
    ClassOrigin::ApplicationClassPath {
        source: Arc::from("file:/app/"),
    }
}

fn stub() -> ClassOrigin {
    ClassOrigin::CompatibilityStub {
        reason: Arc::from("test"),
    }
}

#[test]
fn only_a_real_class_of_a_non_bootstrap_loader_keys_rows() {
    assert!(class_entry_record_all_keys(Some(ClassLoaderId::Application), &app()));
    assert!(!class_entry_record_all_keys(Some(ClassLoaderId::Bootstrap), &app()));
    assert!(!class_entry_record_all_keys(None, &app()));
    // A stub can be upgraded in place under its id with a new pool.
    assert!(!class_entry_record_all_keys(Some(ClassLoaderId::Application), &stub()));
    assert!(!class_entry_record_all_keys(
        Some(ClassLoaderId::Application),
        &ClassOrigin::VmArray
    ));
}

#[test]
fn a_stub_answer_is_not_kept() {
    assert!(class_entry_answer_kept(&app()));
    assert!(class_entry_answer_kept(&ClassOrigin::VmArray));
    assert!(!class_entry_answer_kept(&stub()));
}
