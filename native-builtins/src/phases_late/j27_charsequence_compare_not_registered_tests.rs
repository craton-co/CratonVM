use cratonvm_native_api::NativeMethodRegistry;

#[test]
fn j27_charsequence_compare_is_left_to_the_jdk_body() {
    let mut r = NativeMethodRegistry::new();
    // The removed row sat in `register_p69_misc`, reached only from the
    // `synthetic-jdk` overrides; the real-JDK path must stay clean too.
    crate::register_essential_natives(&mut r);
    #[cfg(feature = "synthetic-jdk")]
    crate::register_builtins(&mut r);
    assert!(
        r.find(
            "java/lang/CharSequence",
            "compare",
            "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;)I",
        )
        .is_none(),
        "CharSequence.compare must run its JDK body (UTF-16 difference), not a native"
    );
}
