use super::*;

#[test]
fn bootstrap_loader_id() {
    let loader = BootstrapClassFinder::new(&[]);
    assert_eq!(loader.loader_id(), ClassLoaderId::Bootstrap);
    assert_eq!(loader.name(), "bootstrap");
}

#[test]
fn extension_loader_id() {
    let loader = ExtensionClassFinder::new(&[]);
    assert_eq!(loader.loader_id(), ClassLoaderId::Extension);
    assert_eq!(loader.name(), "extension");
}

#[test]
fn application_loader_id() {
    let loader = ApplicationClassFinder::new(&[]);
    assert_eq!(loader.loader_id(), ClassLoaderId::Application);
    assert_eq!(loader.name(), "application");
}

#[test]
fn empty_loader_finds_nothing() {
    let loader = BootstrapClassFinder::new(&[]);
    assert!(loader.find_class_bytes("java/lang/Object").is_err());
}

/// Round 5 audit fix (LOW #11): the delegation chain is the
/// canonical parent-delegation order; callers (the class-manager
/// `find_class_bytes_delegated` path) walk this slice instead of
/// chasing trait-object parent pointers.
#[test]
fn delegation_chain_is_bootstrap_extension_application() {
    assert_eq!(
        BUILTIN_LOADER_DELEGATION_CHAIN,
        &[
            ClassLoaderId::Bootstrap,
            ClassLoaderId::Extension,
            ClassLoaderId::Application,
        ]
    );
    assert_eq!(
        BUILTIN_LOADER_DELEGATION_CHAIN.len(),
        MAX_BUILTIN_LOADER_DEPTH
    );
}
