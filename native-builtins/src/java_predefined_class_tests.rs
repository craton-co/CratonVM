use super::compile_java_regex;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn m(pat: &str, s: &str) -> bool {
    compile_java_regex(pat, 0)
        .unwrap_or_else(|e| panic!("pattern {pat:?} failed to compile: {e:?}"))
        .is_match(s)
}

const ALL_NAMES: &[&str] = &[
    "javaLowerCase",
    "javaUpperCase",
    "javaAlphabetic",
    "javaIdeographic",
    "javaTitleCase",
    "javaDigit",
    "javaDefined",
    "javaLetter",
    "javaLetterOrDigit",
    "javaJavaIdentifierStart",
    "javaJavaIdentifierPart",
    "javaUnicodeIdentifierStart",
    "javaUnicodeIdentifierPart",
    "javaIdentifierIgnorable",
    "javaSpaceChar",
    "javaWhitespace",
    "javaISOControl",
    "javaMirrored",
];

#[test]
fn all_names_compile_positive_and_negated() {
    for name in ALL_NAMES {
        let pos = format!("\\p{{{name}}}");
        compile_java_regex(&pos, 0)
            .unwrap_or_else(|e| panic!("{pos:?} failed to compile: {e:?}"));
        let neg = format!("\\P{{{name}}}");
        compile_java_regex(&neg, 0)
            .unwrap_or_else(|e| panic!("{neg:?} failed to compile: {e:?}"));
    }
}

#[test]
fn scanner_whitespace_pattern() {
    // java.util.Scanner.WHITESPACE_PATTERN, Scanner.java:415.
    assert!(m(r"\p{javaWhitespace}+", " \t\n"));
    assert!(!m(r"^\p{javaWhitespace}+$", "abc"));
}

#[test]
fn scanner_non_ascii_digit_pattern() {
    // java.util.Scanner.NON_ASCII_DIGIT, Scanner.java:422-423. Must match
    // a non-ASCII decimal digit but NOT an ASCII one.
    assert!(m(r"[\p{javaDigit}&&[^0-9]]", "\u{0660}")); // ARABIC-INDIC DIGIT ZERO
    assert!(!m(r"[\p{javaDigit}&&[^0-9]]", "5"));
}

#[test]
fn java_whitespace_excludes_nbsp_but_includes_controls() {
    assert!(m(r"\p{javaWhitespace}", " "));
    assert!(m(r"\p{javaWhitespace}", "\t"));
    assert!(m(r"\p{javaWhitespace}", "\u{001C}"));
    assert!(m(r"\p{javaWhitespace}", "\u{2028}"));
    assert!(!m(r"\p{javaWhitespace}", "\u{00A0}"));
    assert!(!m(r"\p{javaWhitespace}", "\u{2007}"));
    assert!(!m(r"\p{javaWhitespace}", "\u{202F}"));
    assert!(!m(r"\p{javaWhitespace}", "a"));
}

#[test]
fn java_space_char_includes_nbsp_unlike_whitespace() {
    // isSpaceChar has NO non-breaking-space exclusion, unlike isWhitespace.
    assert!(m(r"\p{javaSpaceChar}", " "));
    assert!(m(r"\p{javaSpaceChar}", "\u{00A0}"));
    assert!(!m(r"\p{javaSpaceChar}", "a"));
}

#[test]
fn java_digit_and_case() {
    assert!(m(r"\p{javaDigit}", "5"));
    assert!(!m(r"\p{javaDigit}", "a"));
    assert!(m(r"\p{javaUpperCase}", "A"));
    assert!(!m(r"\p{javaUpperCase}", "a"));
    assert!(m(r"\p{javaLowerCase}", "a"));
    assert!(!m(r"\p{javaLowerCase}", "A"));
}

#[test]
fn java_letter_vs_alphabetic_letter_number() {
    // U+2160 ROMAN NUMERAL ONE: general category Nl (Letter_Number).
    // isLetter() excludes it; isAlphabetic() includes it.
    assert!(!m(r"\p{javaLetter}", "\u{2160}"));
    assert!(m(r"\p{javaAlphabetic}", "\u{2160}"));
    assert!(m(r"\p{javaLetter}", "a"));
}

#[test]
fn java_ideographic_and_titlecase() {
    assert!(m(r"\p{javaIdeographic}", "\u{4E2D}")); // 中
    assert!(!m(r"\p{javaIdeographic}", "a"));
    assert!(m(r"\p{javaTitleCase}", "\u{01C5}")); // Dz with caron (titlecase)
    assert!(!m(r"\p{javaTitleCase}", "a"));
}

#[test]
fn java_letter_or_digit() {
    assert!(m(r"\p{javaLetterOrDigit}", "a"));
    assert!(m(r"\p{javaLetterOrDigit}", "5"));
    assert!(!m(r"\p{javaLetterOrDigit}", " "));
}

#[test]
fn java_identifier_start_and_part() {
    assert!(m(r"\p{javaJavaIdentifierStart}", "a"));
    assert!(m(r"\p{javaJavaIdentifierStart}", "$"));
    assert!(m(r"\p{javaJavaIdentifierStart}", "_"));
    assert!(!m(r"\p{javaJavaIdentifierStart}", "5"));
    assert!(m(r"\p{javaJavaIdentifierPart}", "5"));
    assert!(m(r"\p{javaJavaIdentifierPart}", "$"));
}

#[test]
fn unicode_identifier_start_excludes_currency_unlike_java_identifier_start() {
    // isUnicodeIdentifierStart is ID_Start-based and does NOT special-case
    // currency symbols, unlike isJavaIdentifierStart.
    assert!(m(r"\p{javaUnicodeIdentifierStart}", "a"));
    assert!(!m(r"\p{javaUnicodeIdentifierStart}", "$"));
    assert!(m(r"\p{javaUnicodeIdentifierPart}", "5"));
    assert!(!m(r"\p{javaUnicodeIdentifierPart}", "$"));
}

#[test]
fn java_identifier_ignorable() {
    assert!(m(r"\p{javaIdentifierIgnorable}", "\u{0000}"));
    assert!(!m(r"\p{javaIdentifierIgnorable}", "a"));
}

#[test]
fn java_iso_control() {
    assert!(m(r"\p{javaISOControl}", "\u{0007}"));
    assert!(!m(r"\p{javaISOControl}", "a"));
}

#[test]
fn java_mirrored() {
    assert!(m(r"\p{javaMirrored}", "(")); // LEFT PARENTHESIS is mirrored
    assert!(!m(r"\p{javaMirrored}", "a"));
}

#[test]
fn java_defined_excludes_noncharacter() {
    assert!(m(r"\p{javaDefined}", "a"));
    // U+FFFE is a permanently-reserved noncharacter (gc=Cn) per the
    // Unicode stability policy, so this holds across Unicode versions.
    assert!(!m(r"\p{javaDefined}", "\u{FFFE}"));
    assert!(m(r"\P{javaDefined}", "\u{FFFE}"));
}

#[test]
fn negated_form_wraps_correctly() {
    assert!(m(r"\P{javaDigit}", "a"));
    assert!(!m(r"\P{javaDigit}", "5"));
}
