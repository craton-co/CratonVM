use super::{
    field_signature_verdict, legal_method_name, method_signature_verdict,
    unqualified_member_name_is_legal, SignatureVerdict as V,
};

#[test]
fn hotspot_member_names() {
    for ok in ["f", "$x", "a b", "\u{e9}t\u{e9}", "-"] {
        assert!(unqualified_member_name_is_legal(ok.as_bytes(), false), "{ok}");
        assert!(legal_method_name(ok), "{ok}");
    }
    for bad in ["", "a.b", "a;b", "a[b", "a/b"] {
        assert!(!unqualified_member_name_is_legal(bad.as_bytes(), false), "{bad}");
        assert!(!legal_method_name(bad), "{bad}");
    }
    assert!(unqualified_member_name_is_legal(b"a<b", false), "a field may hold <");
    assert!(!legal_method_name("a<b"));
    assert!(!legal_method_name("<foo>"));
    assert!(legal_method_name("<init>") && legal_method_name("<clinit>"));
}

#[test]
fn hotspot_member_signatures() {
    for ok in ["I", "J", "[I", "Ljava/lang/String;", "[[Lp/X;"] {
        assert_eq!(field_signature_verdict(ok), V::Legal, "{ok}");
    }
    for bad in ["", "V", "II", "L", "Lp/X", "[", "Q", "[V"] {
        assert_eq!(field_signature_verdict(bad), V::Illegal, "{bad}");
    }
    assert_eq!(field_signature_verdict("Lp.X;"), V::BadClassName);
    assert_eq!(field_signature_verdict("L;"), V::BadClassName);
    let deep = format!("{}I", "[".repeat(256));
    assert_eq!(field_signature_verdict(&deep), V::TooManyDimensions);

    for ok in ["()V", "(IJ)Ljava/lang/String;", "([I[[J)[D"] {
        assert_eq!(method_signature_verdict("m", ok, 69, false), V::Legal, "{ok}");
    }
    for bad in ["", "V", "(", "(I", "()", "(V)V", "()VV", "(I)II", "I()V"] {
        assert_eq!(method_signature_verdict("m", bad, 69, false), V::Illegal, "{bad}");
    }
    assert_eq!(method_signature_verdict("<init>", "()I", 69, false), V::Illegal);
    assert_eq!(method_signature_verdict("<clinit>", "(I)V", 69, true), V::Illegal);
    assert_eq!(method_signature_verdict("<clinit>", "(I)V", 50, true), V::Legal);
    assert_eq!(method_signature_verdict("m", "(Lp.X;)V", 69, false), V::BadClassName);
    let many = format!("({})V", "J".repeat(128));
    assert_eq!(method_signature_verdict("m", &many, 69, true), V::TooManyArguments);
    let fits = format!("({})V", "J".repeat(127));
    assert_eq!(method_signature_verdict("m", &fits, 69, false), V::Legal, "254 + this");
}
