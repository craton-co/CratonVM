use super::parse_filter;

#[test]
fn empty_entries_do_not_match_everything() {
    assert_eq!(parse_filter("Foo,"), Some(vec!["Foo".to_string()]));
    assert_eq!(
        parse_filter(" Foo , ,Bar.baz "),
        Some(vec!["Foo".to_string(), "Bar.baz".to_string()])
    );
    // Nothing but separators is no filter at all, not "match everything".
    assert_eq!(parse_filter(","), None);
    assert_eq!(parse_filter(" , "), None);
}

#[test]
fn off_values_and_wildcards() {
    assert_eq!(parse_filter(""), None);
    assert_eq!(parse_filter("0"), None);
    assert_eq!(parse_filter("1"), Some(vec!["1".to_string()]));
    assert_eq!(parse_filter("*"), Some(vec!["*".to_string()]));
}
