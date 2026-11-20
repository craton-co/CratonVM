use super::runtime_package_name;

/// Interpreter round i2 wave 59, lane L3: the package half of
/// `Reflection.isSameClassPackage` (the loader half is compared by the
/// caller). A hidden class's `/0x<hex>` tail is part of its name.
#[test]
fn runtime_package_name_strips_the_class_and_a_hidden_tail() {
    assert_eq!(runtime_package_name("p/q/C"), "p/q");
    assert_eq!(runtime_package_name("C"), "");
    assert_eq!(runtime_package_name("p/C$Nested"), "p");
    assert_eq!(runtime_package_name("p/C$$Lambda/0x1f"), "p");
    assert_eq!(runtime_package_name("C/0x3"), "");
}

/// Interpreter round i3 wave 79, lane L3: `Method.invoke`'s
/// `crosses_package` compares these slices, so a hidden subclass is in its
/// declaring superclass's package (the old `rfind('/')` read `p/Sub`).
#[test]
fn i79_a_hidden_subclass_shares_its_superclasss_package() {
    assert_eq!(
        runtime_package_name("p/Sub/0x0000000801001234"),
        runtime_package_name("p/Base")
    );
    assert_eq!(
        runtime_package_name("Sub/0x1"),
        runtime_package_name("Base")
    );
    assert_ne!(
        runtime_package_name("q/Sub/0x1"),
        runtime_package_name("p/Base")
    );
}
