use super::*;

/// A retired index is listed for ITS realm only, and the listing is what
/// `resolve_jni_native_in_libraries` skips.
#[test]
fn mcs21_a_retired_library_index_is_listed_for_its_own_realm_only() {
    let a = SharedVm::new(crate::config::VmConfig::default());
    let b = SharedVm::new(crate::config::VmConfig::default());
    let key_a = std::ptr::from_ref(&a.natives) as usize;
    assert!(unloaded_native_library_indices(&a.natives).is_empty());
    unloaded_native_libraries()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert((key_a, 3));
    assert_eq!(unloaded_native_library_indices(&a.natives), vec![3]);
    assert!(
        unloaded_native_library_indices(&b.natives).is_empty(),
        "another VM's realm sees no tombstone"
    );
    unloaded_native_libraries()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(key_a, 3));
    assert!(unloaded_native_library_indices(&a.natives).is_empty());
}
