use super::{legal_class_name, ClassNameVerdict as V};

#[test]
fn hotspot_legal_class_names() {
    for ok in ["p/X", "X", "p/q/X$1", "[I", "[[Lp/X;", "module-info", "p/\u{e9}t\u{e9}"] {
        assert_eq!(legal_class_name(ok, 52), V::Legal, "{ok}");
    }
    for bad in ["", "p.X", "p//X", "/p/X", "p/", "p/X;Y", "p[X", "[V", "[Lp/X;;", "[Lp/X"] {
        assert_eq!(legal_class_name(bad, 52), V::Illegal, "{bad}");
    }
    assert_eq!(legal_class_name("[Lp.X;", 52), V::IllegalDescriptorElement);
    assert_eq!(legal_class_name("[L;", 52), V::IllegalDescriptorElement);
    // From version 57 a name ending in `;` is judged without its ends.
    assert_eq!(legal_class_name("Lp/X;", 57), V::Legal);
    assert_eq!(legal_class_name("Lp/X;", 56), V::Illegal);
    assert_eq!(legal_class_name("\u{e9}p/X;", 61), V::Legal);
}
