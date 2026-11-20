use super::string_regex_helper;

/// The three triples, their table words and shapes, and `=0` kills them.
#[test]
fn string_regex_binds_the_three_triples() {
    let table = crate::DirectHelperTable {
        string_matches: 0x1000,
        string_replace: [0x2000, 0x3000],
        ..crate::DirectHelperTable::EMPTY
    };
    let key = concat!("CRATON", "VM_JIT_STRING_REGEX_DIRECT");
    let s = "java/lang/String";
    let repl = "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;";
    let bind = |c: &str, m: &str, d: &str| string_regex_helper(&table, false, None, c, m, d);
    cratonvm_types::flags::with_thread_overrides(&[(key, None)], || {
        assert_eq!(
            bind(s, "matches", "(Ljava/lang/String;)Z"),
            Some((0x1000, 1, b'Z'))
        );
        assert_eq!(bind(s, "replaceAll", repl), Some((0x2000, 2, b'L')));
        assert_eq!(bind(s, "replaceFirst", repl), Some((0x3000, 2, b'L')));
        assert_eq!(
            bind(s, "split", "(Ljava/lang/String;)[Ljava/lang/String;"),
            None
        );
        assert_eq!(bind(s, "replace", repl), None);
        assert_eq!(
            bind("java/lang/Object", "matches", "(Ljava/lang/String;)Z"),
            None
        );
        let refuse = |_: &str, _: &str, _: &str| false;
        assert_eq!(
            string_regex_helper(&table, true, Some(&refuse), s, "replaceAll", repl),
            None
        );
        let empty = crate::DirectHelperTable::EMPTY;
        assert_eq!(
            string_regex_helper(&empty, false, None, s, "replaceAll", repl),
            None
        );
    });
}
