use super::{hidden_class_package_of, package_of_internal_name};

#[test]
fn a_hidden_class_is_in_its_class_file_names_package() {
    assert_eq!(hidden_class_package_of("p/q/Foo$$Hidden/0x1f"), "p/q");
    assert_eq!(
        hidden_class_package_of("p/Foo$$Lambda/0x00000000000000ab"),
        "p"
    );
    // Default package: the class-file name has no `/`.
    assert_eq!(hidden_class_package_of("Foo/0x2"), "");
    // What the name split alone answered, and why every call paid the
    // validation: the class-file name itself.
    assert_eq!(
        package_of_internal_name("p/q/Foo$$Hidden/0x1f"),
        "p/q/Foo$$Hidden"
    );
}
