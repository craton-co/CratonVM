use super::charset_name_is_legal;

#[test]
fn charset_check_name_matches_the_jdk() {
    for ok in ["UTF-8", "x-foo", "a.b_c:d+e", "9abc"] {
        assert!(charset_name_is_legal(ok), "{ok:?}");
    }
    for bad in ["", "-utf8", "utf 8", "utf/8", ".x", "ü"] {
        assert!(!charset_name_is_legal(bad), "{bad:?}");
    }
}
