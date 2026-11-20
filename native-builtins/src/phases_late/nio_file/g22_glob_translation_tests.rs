use super::{p57_globs_to_regex, P57_MATCHER_FLAGS};

/// Translate a Windows-branch glob, or panic with the refusal.
fn dos(glob: &str) -> String {
    let units: Vec<u16> = glob.encode_utf16().collect();
    match p57_globs_to_regex(&units, true) {
        Ok(u) => String::from_utf16_lossy(&u),
        Err(e) => panic!("unexpected refusal for {glob:?}: {e:?}"),
    }
}

/// The refusal `(desc, index)` for a glob that must not translate.
fn dos_err(glob: &str) -> (&'static str, usize) {
    let units: Vec<u16> = glob.encode_utf16().collect();
    match p57_globs_to_regex(&units, true) {
        Ok(u) => panic!(
            "{glob:?} translated to {:?} but HotSpot refuses it",
            String::from_utf16_lossy(&u)
        ),
        Err(e) => e,
    }
}

/// Every expectation here is the regex the JDK's own `Globs
/// .toWindowsRegexPattern` produces, cross-checked against the MEASURED
/// match/no-match answers of HotSpot 25.0.3+9 on Windows (G22-1 §Glob).
///
/// That claim was false for the first block until 2026-08-18: ten
/// expectations spelled the separator class with ONE backslash where the
/// JDK emits two — and one backslash is not even a legal Java character
/// class, because it escapes the closing bracket and the class never
/// closes. The rest of this function already had it right, which is what a
/// partial hand-edit looks like.
///
/// Re-derived rather than reasoned about: `Globs.toWindowsRegexPattern`
/// called by reflection on JDK 25 under `--add-opens java.base/sun.nio.fs`,
/// against this VM's `p57_globs_to_regex` over the same inputs. They agree
/// on all 21 patterns, so the translator was right and only these strings
/// were wrong.
///
/// A line asserting that the escaped form translates BOTH to `^suba'\'.txt$`
/// and to `^sub'\'a'\'.txt$`, two lines apart, is also gone: no
/// implementation can satisfy both, so this test could never have passed.
/// The JDK gives the first.
#[test]
fn windows_translation_matches_the_jdk() {
    // `*` and `?` stop at the separator; `**` crosses it.
    assert_eq!(dos("*.txt"), r"^[^\\]*\.txt$");
    assert_eq!(dos("a?c"), r"^a[^\\]c$");
    assert_eq!(dos("**.txt"), r"^.*\.txt$");
    assert_eq!(dos("**/*.txt"), r"^.*\\[^\\]*\.txt$");
    assert_eq!(dos("src/**"), r"^src\\.*$");
    // A `/` in the PATTERN is the separator; a lone `\` is the ESCAPE, so
    // `sub\a.txt` is the file name `suba.txt` and matches no directory.
    assert_eq!(dos("sub/a.txt"), r"^sub\\a\.txt$");
    assert_eq!(dos(r"sub\a.txt"), r"^suba\.txt$");
    // Alternation, and a stray `}` / `,` outside a group is a literal.
    assert_eq!(dos("*.{java,class}"), r"^[^\\]*\.(?:(?:java)|(?:class))$");
    assert_eq!(dos("a}b"), "^a}b$");
    assert_eq!(dos("a,b"), "^a,b$");
    // Classes: `!` negates, `^` is a LITERAL, a leading `-` is literal.
    assert_eq!(dos("[abc].txt"), r"^[[^\\]&&[abc]]\.txt$");
    assert_eq!(dos("[!a-z].txt"), r"^[[^\\]&&[^a-z]]\.txt$");
    assert_eq!(dos("[^abc].txt"), r"^[[^\\]&&[\^abc]]\.txt$");
    assert_eq!(dos("[a-]"), r"^[[^\\]&&[a-]]$");
    assert_eq!(dos("[-a]"), r"^[[^\\]&&[-a]]$");
    // `regexMetaChars` is escaped, and nothing outside it is.
    assert_eq!(dos("a+b"), r"^a\+b$");
    assert_eq!(dos("a-b"), "^a-b$");
    assert_eq!(dos("a&b"), "^a&b$");
    assert_eq!(dos("a(b)c"), r"^a\(b\)c$");
    // Escaping a non-meta is a no-op; escaping a meta emits it literally.
    assert_eq!(dos(r"\a"), "^a$");
    assert_eq!(dos(r"\*.txt"), r"^\*\.txt$");
    // The empty glob is legal and matches only the empty path.
    assert_eq!(dos(""), "^$");
    // `&&` inside a class is escaped so it is not regex-class
    // intersection; `[` inside a class is escaped too.
    assert_eq!(dos("[a&&b]"), r"^[[^\\]&&[a\&&b]]$");
    assert_eq!(dos("[[]"), r"^[[^\\]&&[\[]]$");
    // A doubled `/` becomes two literal separators — a pattern no path
    // `Paths.get` produces can ever match, because the parser collapses
    // separator runs. Faithful, not useful.
    assert_eq!(dos("a//b"), r"^a\\\\b$");
    // An empty alternative and an empty group are both legal.
    assert_eq!(dos("{a,}"), "^(?:(?:a)|(?:))$");
    assert_eq!(dos("{}"), "^(?:(?:))$");
    // `***` is `**` followed by `*`, not a third wildcard.
    assert_eq!(dos("***.txt"), r"^.*[^\\]*\.txt$");
    assert_eq!(dos("a**b"), "^a.*b$");
    assert_eq!(dos("C:/**"), r"^C:\\.*$");
    // A `]` as the first class member closes the class; the next one is a
    // literal handled by the DEFAULT arm, which escapes it.
    assert_eq!(dos("[!]"), r"^[[^\\]&&[^]]$");
    assert_eq!(dos("[]]"), r"^[[^\\]&&[]]\]$");
}

/// The Unix branch differs in exactly the four separator sites.
#[test]
fn unix_branch_uses_the_forward_slash() {
    let unix = |g: &str| {
        let units: Vec<u16> = g.encode_utf16().collect();
        String::from_utf16_lossy(&p57_globs_to_regex(&units, false).unwrap())
    };
    assert_eq!(unix("*.txt"), r"^[^/]*\.txt$");
    assert_eq!(unix("a?c"), "^a[^/]c$");
    assert_eq!(unix("a/b"), "^a/b$");
    assert_eq!(unix("[abc]"), "^[[^/]&&[abc]]$");
}

/// The six refusals `Globs` raises, with the exact `desc` and the exact
/// index HotSpot reports. The unbalanced apostrophes in `Missing ']` and
/// `Missing '}` are the JDK's, transcribed — not typos.
#[test]
fn the_six_refusals_carry_the_jdk_text_and_index() {
    assert_eq!(dos_err(r"abc\"), ("No character to escape", 3));
    assert_eq!(dos_err(r"\"), ("No character to escape", 0));
    assert_eq!(dos_err("[/]"), ("Explicit 'name separator' in class", 1));
    assert_eq!(dos_err("[a/b]"), ("Explicit 'name separator' in class", 2));
    assert_eq!(dos_err(r"[a\b]"), ("Explicit 'name separator' in class", 2));
    assert_eq!(dos_err("[abc"), ("Missing ']", 3));
    assert_eq!(dos_err("["), ("Missing ']", 0));
    assert_eq!(dos_err("[z-a]"), ("Invalid range", 1));
    assert_eq!(dos_err("[a-c-e]"), ("Invalid range", 4));
    assert_eq!(dos_err("{a,{b,c}}.txt"), ("Cannot nest groups", 3));
    assert_eq!(dos_err("{a,b"), ("Missing '}", 3));
    assert_eq!(dos_err("{"), ("Missing '}", 0));
}

/// `glob:[]` and `glob:{a}{b}` are NOT refused by `Globs` — the first is
/// refused later, by `Pattern.compile`, and the second is legal. Getting
/// this wrong would move an exception between two different classes.
#[test]
fn globs_defers_two_cases_to_the_regex_engine() {
    // `[]` translates; `Pattern.compile` then reports
    // "Unclosed character class near index 12" over THIS string.
    assert_eq!(dos("[]"), r"^[[^\\]&&[]]$");
    // Sequential groups are legal; only NESTING is banned.
    assert_eq!(dos("{a}{b}"), "^(?:(?:a))(?:(?:b))$");
}

/// `Pattern.CASE_INSENSITIVE | Pattern.UNICODE_CASE`.
#[test]
fn matcher_flags_are_case_insensitive_plus_unicode_case() {
    assert_eq!(P57_MATCHER_FLAGS, 66);
}
