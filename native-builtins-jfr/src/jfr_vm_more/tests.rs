use super::*;

#[test]
fn method_keys_split_into_class_method_descriptor() {
    assert_eq!(
        split_method_key("java.util.HashMap.put:(Ljava/lang/Object;)V"),
        (
            "java/util/HashMap".to_owned(),
            "put".to_owned(),
            "(Ljava/lang/Object;)V".to_owned()
        )
    );
    assert_eq!(
        split_method_key("p/C::m(I)I"),
        ("p/C".to_owned(), "m".to_owned(), "(I)I".to_owned())
    );
}

#[test]
fn the_sampler_class_is_a_well_formed_runnable() {
    let b = sampler_class_bytes();
    assert_eq!(&b[..8], &[0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52]);
    // constant pool count 16, then access flags public final super after the pool
    assert_eq!(u16::from_be_bytes([b[8], b[9]]), 16);
    let parsed = cratonvm_reader::read_class(&b).expect("parses");
    assert_eq!(&*parsed.this_class, SAMPLER_CLASS);
    assert_eq!(parsed.methods.len(), 3);
    assert!(parsed
        .interfaces
        .iter()
        .any(|i| &**i == "java/lang/Runnable"));
}
