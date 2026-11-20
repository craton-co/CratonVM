use super::*;


/// The transaction calls the release with the ids it removed, after the
/// mirror maps (a source check: the unit-test VM cannot run the JDK's
/// `getName()` through the memo).
#[test]
fn zt9cl5_the_unload_transaction_releases_the_get_name_memo() {
    let src = include_str!("../gc.rs");
    let start = src
        .find("pub fn unload_dead_class_metadata(")
        .expect("the unload transaction");
    let len = src[start..].find("\n}\n").expect("its closing brace");
    let body = &src[start..start + len];
    let mirrors = body.find(".class_mirrors_reverse").expect("mirror maps");
    let release = body
        .find("release_unloaded_class_name_memo(shared, &raw_ids);")
        .expect("the memo release");
    assert!(mirrors < release, "after the mirror maps");
}

/// `CRATONVM_DBG_MIRRORPIN_WHY` takes an explicit class-name prefix;
/// boolean words and an absent value do not select an application prefix.
#[test]
fn zt9cl5_mirrorpin_why_takes_a_class_name_prefix() {
    assert_eq!(mirrorpin_why_prefix(None), None);
    assert_eq!(mirrorpin_why_prefix(Some("0")), None);
    assert_eq!(mirrorpin_why_prefix(Some("")), None);
    assert_eq!(mirrorpin_why_prefix(Some("1")), None);
    assert_eq!(mirrorpin_why_prefix(Some("TRUE")), None);
    assert_eq!(
        mirrorpin_why_prefix(Some("zt7cl/")).as_deref(),
        Some("zt7cl/")
    );
    assert_eq!(
        mirrorpin_why_prefix(Some("zt7cl.R1")).as_deref(),
        Some("zt7cl/R1")
    );
}

/// A VM whose memo was never made releases nothing.
#[test]
fn zt9cl5_no_memo_releases_nothing() {
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    assert_eq!(release_unloaded_class_name_memo(&shared, &[]), 0);
    assert_eq!(release_unloaded_class_name_memo(&shared, &[1, 2, 3]), 0);
}
