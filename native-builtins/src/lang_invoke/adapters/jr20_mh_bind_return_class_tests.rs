use super::mh_bind_return_class_name;

#[test]
fn jr20_mh_bind_return_class_name_reads_the_return_class_only() {
    assert_eq!(mh_bind_return_class_name("(I)Lp/X;"), Some("p/X"));
    assert_eq!(mh_bind_return_class_name("(Lp/A;)[[Lp/X;"), Some("p/X"));
    assert_eq!(mh_bind_return_class_name("(Lp/A;)V"), None);
    assert_eq!(mh_bind_return_class_name("(Lp/A;)[I"), None);
    assert_eq!(mh_bind_return_class_name("no-parens"), None);
}
