use super::*;

fn link(is_class_loader: bool, declares: bool, application_bytes: bool) -> LoadClassLink {
    LoadClassLink {
        is_class_loader,
        declares,
        application_bytes,
    }
}

/// `B extends A extends ClassLoader`, A (application bytes) overrides:
/// B must not take the base native.
#[test]
fn prx23_a_superclass_override_defeats_the_base_native() {
    let chain = [
        link(false, false, true),
        link(false, true, true),
        link(true, false, false),
    ];
    assert!(!load_class_uses_base_native(chain));
}

/// The historical rules stay: the receiver's own declaration wins whatever
/// its origin, and a plain subclass of `ClassLoader` or a bare
/// `ClassLoader` receiver uses the base native.
#[test]
fn prx23_the_historical_rules_are_unchanged() {
    assert!(!load_class_uses_base_native([
        link(false, true, false),
        link(true, false, false)
    ]));
    assert!(load_class_uses_base_native([
        link(false, false, true),
        link(true, false, false)
    ]));
    assert!(load_class_uses_base_native([link(true, true, false)]));
    assert!(!load_class_uses_base_native(
        std::iter::empty::<LoadClassLink>()
    ));
}

/// A JDK (non-application) superclass that declares the method, and
/// anything above `ClassLoader`, do not count.
#[test]
fn prx23_a_jdk_declarer_and_classes_above_the_root_are_ignored() {
    assert!(load_class_uses_base_native([
        link(false, false, true),
        link(false, true, false),
        link(true, false, false),
    ]));
    assert!(load_class_uses_base_native([
        link(false, false, true),
        link(true, false, false),
        link(false, true, true),
    ]));
}
