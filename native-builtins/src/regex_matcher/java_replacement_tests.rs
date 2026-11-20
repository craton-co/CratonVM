use super::{compile_anchored_cached, compile_java_regex};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn ra(text: &str, pat: &str, rep: &str) -> String {
    compile_java_regex(pat, 0)
        .unwrap()
        .replace_all_java(text, rep)
        .expect("replacement references a group the pattern does not have")
}
fn rf(text: &str, pat: &str, rep: &str) -> String {
    compile_java_regex(pat, 0)
        .unwrap()
        .replace_first_java(text, rep)
        .expect("replacement references a group the pattern does not have")
}

// Each expectation below is the exact output of the equivalent
// `String.replaceAll` / `replaceFirst` under HotSpot JDK 25
// (see scratch/regexperf/RegexParity golden).
#[test]
fn literal_and_groups() {
    assert_eq!(ra("a.b.c.d", "[.]", "/"), "a/b/c/d");
    assert_eq!(ra("a {@code X} b", "\\{@code (.*?)}", "`$1`"), "a `X` b");
    assert_eq!(ra("a {@code X} b", "\\{@code (.*?)}", "XX"), "a XX b");
    assert_eq!(ra("abc", "(b)", "$1Z"), "abZc");
    assert_eq!(ra("a1b22c333", "(\\d+)", "<$1>"), "a<1>b<22>c<333>");
}

#[test]
fn escapes() {
    // Java `\$` -> literal `$`; `\\` -> literal backslash.
    assert_eq!(ra("abc", "b", "\\$"), "a$c");
    assert_eq!(ra("abc", "b", "\\\\"), "a\\c");
}

#[test]
fn digit_bounded_group_ref() {
    // One capturing group; `$10` must mean group 1 then a literal '0'.
    assert_eq!(ra("abc", "(b)", "$10"), "ab0c");
}

#[test]
fn multiref_and_named() {
    assert_eq!(ra("John Smith", "(\\w+) (\\w+)", "$2 $1"), "Smith John");
    assert_eq!(
        ra(
            "2026-01-15",
            "(?<y>\\d{4})-(?<m>\\d{2})-(?<d>\\d{2})",
            "${d}/${m}/${y}"
        ),
        "15/01/2026"
    );
}

#[test]
fn zero_width_and_greedy() {
    assert_eq!(ra("abc", "", "-"), "-a-b-c-");
    assert_eq!(ra("<a><b>", "<(.*)>", "[$1]"), "[a><b]");
    assert_eq!(ra("<a><b>", "<(.*?)>", "[$1]"), "[a][b]");
}

#[test]
fn replace_first_only() {
    assert_eq!(rf("a1b2c3", "(\\d)", "<$1>"), "a<1>b2c3");
    assert_eq!(rf("x.y.z", "[.]", "/"), "x/y.z");
}

fn m(text: &str, pat: &str) -> bool {
    let re = compile_java_regex(pat, 0).unwrap();
    let anchored = format!("^(?:{})$", re.as_str());
    match compile_anchored_cached(&anchored) {
        Some(full) => full.is_match(text),
        None => re.is_match(text),
    }
}

// ASCII-default Perl classes must match Java (NOT Rust's Unicode default).
#[test]
fn ascii_perl_classes_parity() {
    // \d ASCII-only: Arabic-Indic digit U+0663 is NOT \d in Java default.
    assert!(m("5", "\\d"));
    assert!(!m("\u{0663}", "\\d"));
    // \w ASCII-only: 'é' (Latin-1) is NOT \w in Java default.
    assert!(m("a", "\\w"));
    assert!(m("_", "\\w"));
    assert!(!m("\u{00e9}", "\\w"));
    // \s ASCII-only set.
    assert!(m(" ", "\\s"));
    assert!(m("\t", "\\s"));
    // U+00A0 NBSP is whitespace in Unicode but NOT Java's ASCII \s.
    assert!(!m("\u{00a0}", "\\s"));
    // Inside a class: \d expands to the bare range, still ASCII.
    assert_eq!(ra("a1\u{0663}b2", "[\\d]", "#"), "a#\u{0663}b#");
    // Negated forms.
    assert!(m("\u{0663}", "\\D")); // non-ASCII digit IS \D (any non-[0-9])
    assert!(!m("7", "\\D"));
    // Replace using \d+ stays ASCII-bounded.
    assert_eq!(ra("a12\u{0663}34b", "\\d+", "N"), "aN\u{0663}Nb");
}

// \b word boundary uses ASCII \w; (?-u:\b) must compile and match ASCII-style.
#[test]
fn ascii_word_boundary() {
    assert_eq!(ra("foo bar", "\\bbar\\b", "X"), "foo X");
    assert_eq!(ra("foobar", "\\bbar\\b", "X"), "foobar");
}

#[test]
fn java_all_property_classes_compile() {
    let all = compile_java_regex("\\p{all}", 0).unwrap();
    assert!(all.is_match("x"));

    let none = compile_java_regex("(\\P{all})+", 0).unwrap();
    assert!(!none.is_match(""));
    assert!(!none.is_match("x"));
}
