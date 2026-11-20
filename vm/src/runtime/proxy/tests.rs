use super::*;

#[test]
fn proxy_class_name_constant() {
    assert_eq!(PROXY_INSTANCE_CLASS, "java/lang/reflect/Proxy$Instance");
}

#[test]
fn field_layout_is_consistent() {
    // Slot indices must be unique and < FIELD_COUNT.
    assert!(PROXY_FIELD_HANDLER < PROXY_INSTANCE_FIELD_COUNT);
    assert!(PROXY_FIELD_INTERFACES < PROXY_INSTANCE_FIELD_COUNT);
    assert!(PROXY_FIELD_IDENTITY_HASH < PROXY_INSTANCE_FIELD_COUNT);
    assert_ne!(PROXY_FIELD_HANDLER, PROXY_FIELD_INTERFACES);
    assert_ne!(PROXY_FIELD_HANDLER, PROXY_FIELD_IDENTITY_HASH);
    assert_ne!(PROXY_FIELD_INTERFACES, PROXY_FIELD_IDENTITY_HASH);
}

#[test]
fn count_descriptor_params_voids() {
    assert_eq!(count_descriptor_params("()V"), 0);
    assert_eq!(count_descriptor_params("()I"), 0);
    assert_eq!(count_descriptor_params("()Ljava/lang/String;"), 0);
}

#[test]
fn count_descriptor_params_primitives() {
    assert_eq!(count_descriptor_params("(I)V"), 1);
    assert_eq!(count_descriptor_params("(IJ)V"), 2);
    assert_eq!(count_descriptor_params("(IJDFZBCS)V"), 8);
}

#[test]
fn count_descriptor_params_objects_and_arrays() {
    assert_eq!(count_descriptor_params("(Ljava/lang/String;)V"), 1);
    assert_eq!(
        count_descriptor_params("(Ljava/lang/String;Ljava/lang/Object;)V"),
        2
    );
    assert_eq!(count_descriptor_params("([I)V"), 1);
    assert_eq!(count_descriptor_params("([[I)V"), 1);
    assert_eq!(count_descriptor_params("([Ljava/lang/String;)V"), 1);
}

#[test]
fn count_descriptor_params_mixed() {
    assert_eq!(count_descriptor_params("(ILjava/lang/String;[J)V"), 3);
}

#[test]
fn count_descriptor_params_malformed() {
    assert_eq!(count_descriptor_params(""), 0);
    assert_eq!(count_descriptor_params("()"), 0);
    // missing closing paren — we return 0 rather than panic
    assert_eq!(count_descriptor_params("(I"), 0);
}

#[test]
fn count_descriptor_params_class_name_containing_a_paren() {
    assert_eq!(count_descriptor_params("(LA)B;I)V"), 2);
    assert_eq!(count_descriptor_params("([LA)B;)LC)D;"), 1);
}
