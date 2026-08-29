// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `java.util.regex` real-JDK Matcher/Pattern fast paths and the Java replacement-string parser.
//!
//! Pure code move out of `lib.rs` (no logic, signature or ordering changes).
//! Registration call sites are untouched, so the native registration sequence
//! is byte-identical to before the split.

use super::*;

/// Wrapper that abstracts over the two regex engines we use to evaluate Java
/// patterns: the fast `regex` crate (DFA, no backtracking, no lookaround) and
/// `fancy-regex` (NFA + backtracking, supports lookaround/backrefs). We try
/// `regex` first and fall back to `fancy-regex` only when `regex` rejects the
/// pattern — most real-world Java regexes compile fine on the simpler engine.
///
/// Lookaround appears in real workloads: log4j2's
/// `PropertySource$Util.PREFIX_PATTERN` is
/// `(^log4j2?[-._/]?|^org\.apache\.logging\.log4j\.)|(?=AsyncLogger(Config)?\.)`
/// — the trailing `(?=...)` is a positive lookahead the `regex` crate refuses
/// with `look-around, including look-ahead and look-behind, is not supported`.
/// Without `fancy-regex` the resulting `IllegalArgumentException` escapes
/// `<clinit>` and aborts WildFly boot.
#[derive(Clone)]
pub(crate) enum JavaRegex {
    Std(regex::Regex),
    Fancy(Box<fancy_regex::Regex>),
}

impl JavaRegex {
    pub fn as_str(&self) -> &str {
        match self {
            JavaRegex::Std(r) => r.as_str(),
            JavaRegex::Fancy(r) => r.as_str(),
        }
    }

    pub fn is_match(&self, text: &str) -> bool {
        match self {
            JavaRegex::Std(r) => r.is_match(text),
            JavaRegex::Fancy(r) => r.is_match(text).unwrap_or(false),
        }
    }

    pub fn find(&self, text: &str) -> Option<JavaMatch> {
        match self {
            JavaRegex::Std(r) => r.find(text).map(|m| JavaMatch {
                start: m.start(),
                end: m.end(),
                text: m.as_str().to_string(),
            }),
            JavaRegex::Fancy(r) => r.find(text).ok().flatten().map(|m| JavaMatch {
                start: m.start(),
                end: m.end(),
                text: m.as_str().to_string(),
            }),
        }
    }

    /// Like [`Self::captures`], but starts the search at byte offset `start`
    /// within `text` while keeping `text`'s own bounds as the anchor context
    /// for `^`/`$`/`\A`/`\z` (i.e. NOT the same as `captures(&text[start..])`,
    /// which would incorrectly let `^` match at `start`). This is exactly the
    /// semantics `java.util.regex.Matcher.find()` needs: search resumes after
    /// the previous match, but anchors still refer to the matcher's region
    /// bounds, not to the resume point. See the `regex`/`fancy-regex` crate
    /// docs for `captures_at`/`captures_from_pos` for the same contract.
    pub fn captures_at(&self, text: &str, start: usize) -> Option<JavaCaptures> {
        match self {
            JavaRegex::Std(r) => {
                let caps = r.captures_at(text, start)?;
                let groups: Vec<Option<JavaMatch>> = (0..caps.len())
                    .map(|i| {
                        caps.get(i).map(|m| JavaMatch {
                            start: m.start(),
                            end: m.end(),
                            text: m.as_str().to_string(),
                        })
                    })
                    .collect();
                let mut named = std::collections::HashMap::new();
                for (idx, name) in r.capture_names().enumerate() {
                    if let Some(n) = name {
                        named.insert(n.to_string(), idx);
                    }
                }
                Some(JavaCaptures { groups, named })
            }
            JavaRegex::Fancy(r) => {
                let caps = r.captures_from_pos(text, start).ok().flatten()?;
                let groups: Vec<Option<JavaMatch>> = (0..caps.len())
                    .map(|i| {
                        caps.get(i).map(|m| JavaMatch {
                            start: m.start(),
                            end: m.end(),
                            text: m.as_str().to_string(),
                        })
                    })
                    .collect();
                let mut named = std::collections::HashMap::new();
                for (idx, name) in r.capture_names().enumerate() {
                    if let Some(n) = name {
                        named.insert(n.to_string(), idx);
                    }
                }
                Some(JavaCaptures { groups, named })
            }
        }
    }

    /// Visit capture byte ranges for one search without materialising owned
    /// match strings, a capture `Vec`, or a named-group map. Stateful Matcher
    /// fast paths only need offsets to update the Java `groups[]` array; using
    /// `captures_at` there would otherwise allocate several Rust objects for
    /// every successful `find()`.
    pub fn visit_capture_ranges_at(
        &self,
        text: &str,
        start: usize,
        mut visit: impl FnMut(usize, Option<(usize, usize)>),
    ) -> Option<usize> {
        match self {
            JavaRegex::Std(r) => {
                let caps = r.captures_at(text, start)?;
                let len = caps.len();
                for i in 0..len {
                    visit(i, caps.get(i).map(|m| (m.start(), m.end())));
                }
                Some(len)
            }
            JavaRegex::Fancy(r) => {
                let caps = r.captures_from_pos(text, start).ok().flatten()?;
                let len = caps.len();
                for i in 0..len {
                    visit(i, caps.get(i).map(|m| (m.start(), m.end())));
                }
                Some(len)
            }
        }
    }

    pub fn captures(&self, text: &str) -> Option<JavaCaptures> {
        match self {
            JavaRegex::Std(r) => {
                let caps = r.captures(text)?;
                let groups: Vec<Option<JavaMatch>> = (0..caps.len())
                    .map(|i| {
                        caps.get(i).map(|m| JavaMatch {
                            start: m.start(),
                            end: m.end(),
                            text: m.as_str().to_string(),
                        })
                    })
                    .collect();
                let mut named = std::collections::HashMap::new();
                for (idx, name) in r.capture_names().enumerate() {
                    if let Some(n) = name {
                        named.insert(n.to_string(), idx);
                    }
                }
                Some(JavaCaptures { groups, named })
            }
            JavaRegex::Fancy(r) => {
                let caps = r.captures(text).ok().flatten()?;
                let groups: Vec<Option<JavaMatch>> = (0..caps.len())
                    .map(|i| {
                        caps.get(i).map(|m| JavaMatch {
                            start: m.start(),
                            end: m.end(),
                            text: m.as_str().to_string(),
                        })
                    })
                    .collect();
                let mut named = std::collections::HashMap::new();
                for (idx, name) in r.capture_names().enumerate() {
                    if let Some(n) = name {
                        named.insert(n.to_string(), idx);
                    }
                }
                Some(JavaCaptures { groups, named })
            }
        }
    }

    pub fn captures_len(&self) -> usize {
        match self {
            JavaRegex::Std(r) => r.captures_len(),
            JavaRegex::Fancy(r) => r.captures_len(),
        }
    }

    /// Split, dropping empty trailing pieces is the caller's responsibility.
    pub fn split(&self, text: &str) -> Vec<String> {
        match self {
            JavaRegex::Std(r) => r.split(text).map(|s| s.to_string()).collect(),
            JavaRegex::Fancy(r) => fancy_split_all(r, text),
        }
    }

    pub fn splitn(&self, text: &str, limit: usize) -> Vec<String> {
        match self {
            JavaRegex::Std(r) => r.splitn(text, limit).map(|s| s.to_string()).collect(),
            JavaRegex::Fancy(r) => fancy_splitn(r, text, limit),
        }
    }

    /// Replace all non-overlapping matches. `replacement` follows Java/regex
    /// `$N` group-reference semantics (delegated to the underlying engine).
    pub fn replace_all(&self, text: &str, replacement: &str) -> String {
        match self {
            JavaRegex::Std(r) => r.replace_all(text, replacement).into_owned(),
            JavaRegex::Fancy(r) => r.replace_all(text, replacement).into_owned(),
        }
    }

    /// Replace only the first match.
    pub fn replace_first(&self, text: &str, replacement: &str) -> String {
        match self {
            JavaRegex::Std(r) => r.replace(text, replacement).into_owned(),
            JavaRegex::Fancy(r) => r.replace(text, replacement).into_owned(),
        }
    }

    /// Number of *capturing* groups (excluding the whole-match group 0).
    fn group_count(&self) -> usize {
        self.captures_len().saturating_sub(1)
    }

    /// Replace all non-overlapping matches, expanding `replacement` with
    /// **Java** `Matcher.appendReplacement` semantics rather than the `regex`
    /// crate's: `$N` (digit-bounded by group count), `${name}`, and `\`-escapes
    /// (`\$` → literal `$`, `\\` → literal `\`). This is what
    /// `String.replaceAll` must do; the engine's own `$`-syntax differs
    /// (`$$` for a literal `$`, greedy `$NN`, no `\`-escape), so we drive the
    /// expansion ourselves via a per-match closure.
    ///
    /// `Err(n)` means the replacement referenced group `n`, which this pattern
    /// does not have; the caller raises `IndexOutOfBoundsException("No group
    /// n")` exactly as `Matcher.appendReplacement` does.
    pub fn replace_all_java(&self, text: &str, replacement: &str) -> Result<String, usize> {
        let tokens = parse_java_replacement(replacement, self.group_count())?;
        Ok(match self {
            JavaRegex::Std(r) => r
                .replace_all(text, |caps: &regex::Captures| {
                    render_java_replacement(
                        &tokens,
                        |i| caps.get(i).map(|m| m.as_str()),
                        |n| caps.name(n).map(|m| m.as_str()),
                    )
                })
                .into_owned(),
            JavaRegex::Fancy(r) => r
                .replace_all(text, |caps: &fancy_regex::Captures| {
                    render_java_replacement(
                        &tokens,
                        |i| caps.get(i).map(|m| m.as_str()),
                        |n| caps.name(n).map(|m| m.as_str()),
                    )
                })
                .into_owned(),
        })
    }

    /// Like [`replace_all_java`](Self::replace_all_java) but only the first
    /// match (`String.replaceFirst`).
    pub fn replace_first_java(&self, text: &str, replacement: &str) -> Result<String, usize> {
        let tokens = parse_java_replacement(replacement, self.group_count())?;
        Ok(match self {
            JavaRegex::Std(r) => r
                .replace(text, |caps: &regex::Captures| {
                    render_java_replacement(
                        &tokens,
                        |i| caps.get(i).map(|m| m.as_str()),
                        |n| caps.name(n).map(|m| m.as_str()),
                    )
                })
                .into_owned(),
            JavaRegex::Fancy(r) => r
                .replace(text, |caps: &fancy_regex::Captures| {
                    render_java_replacement(
                        &tokens,
                        |i| caps.get(i).map(|m| m.as_str()),
                        |n| caps.name(n).map(|m| m.as_str()),
                    )
                })
                .into_owned(),
        })
    }
}

/// Parse a Java replacement string into tokens once (reused across every match
/// in a `replaceAll`). Mirrors `Matcher.appendExpandedReplacement`:
/// * `\X` emits `X` literally (escape — including `\$` and `\\`).
/// * `${name}` is a named-group reference.
/// * `$NN` is a numeric reference; the digit run is consumed **greedily but
///   bounded by `group_count`** (so `$10` with 2 groups means group 1 then a
///   literal `0`, exactly like Java).
/// * A `$` followed by neither a digit nor `{` is emitted as a literal `$`
///   (Java throws `IllegalArgumentException`; we choose the lenient path —
///   malformed replacements are programming errors and rare).
fn parse_java_replacement(rep: &str, group_count: usize) -> Result<Vec<JavaReplToken>, usize> {
    let mut tokens: Vec<JavaReplToken> = Vec::new();
    let mut lit = String::new();
    let bytes = rep.as_bytes();
    let mut i = 0;
    macro_rules! flush_lit {
        () => {
            if !lit.is_empty() {
                tokens.push(JavaReplToken::Lit(std::mem::take(&mut lit)));
            }
        };
    }
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\\' {
            if i + 1 < bytes.len() {
                let start = i + 1;
                let clen = utf8_char_len(bytes[start]);
                let end = (start + clen).min(bytes.len());
                lit.push_str(&rep[start..end]);
                i = end;
            } else {
                // Trailing backslash: Java throws; drop it leniently.
                i += 1;
            }
            continue;
        }
        if c == b'$' {
            // ${name}
            if i + 1 < bytes.len() && bytes[i + 1] == b'{' {
                if let Some(close) = rep[i + 2..].find('}') {
                    let name = &rep[i + 2..i + 2 + close];
                    flush_lit!();
                    tokens.push(JavaReplToken::Named(name.to_string()));
                    i = i + 2 + close + 1;
                    continue;
                }
                lit.push('$');
                i += 1;
                continue;
            }
            // $NN (digit-bounded by group_count)
            if i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() {
                let mut j = i + 1;
                let mut num = (bytes[j] - b'0') as usize;
                // The FIRST digit was previously accepted unconditionally, and a
                // reference past the last group then rendered as "" -- so
                // `"abc".replaceAll("(b)", "$9")` returned "bc" where HotSpot
                // throws `IndexOutOfBoundsException: No group 9`. A silently
                // wrong answer, not a missing message. The greedy loop below
                // already bounds the SUBSEQUENT digits; only the first was
                // unchecked.
                if num > group_count {
                    return Err(num);
                }
                j += 1;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    let trial = num * 10 + (bytes[j] - b'0') as usize;
                    if trial <= group_count {
                        num = trial;
                        j += 1;
                    } else {
                        break;
                    }
                }
                flush_lit!();
                tokens.push(JavaReplToken::Group(num));
                i = j;
                continue;
            }
            // Dangling `$`: emit literally.
            lit.push('$');
            i += 1;
            continue;
        }
        let clen = utf8_char_len(c);
        let end = (i + clen).min(bytes.len());
        lit.push_str(&rep[i..end]);
        i = end;
    }
    flush_lit!();
    Ok(tokens)
}

/// Render parsed replacement tokens against one match's capture groups. A
/// missing/non-participating group contributes the empty string (Java would
/// throw for an out-of-range numeric reference, but a group that simply did
/// not participate yields `""` — and the common case is a valid reference).
fn render_java_replacement<'a>(
    tokens: &[JavaReplToken],
    get_num: impl Fn(usize) -> Option<&'a str>,
    get_name: impl Fn(&str) -> Option<&'a str>,
) -> String {
    let mut out = String::new();
    for t in tokens {
        match t {
            JavaReplToken::Lit(s) => out.push_str(s),
            JavaReplToken::Group(n) => {
                if let Some(s) = get_num(*n) {
                    out.push_str(s);
                }
            }
            JavaReplToken::Named(n) => {
                if let Some(s) = get_name(n) {
                    out.push_str(s);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod java_replacement_tests {
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
}

/// Convert a Java regex pattern string + flags into a `JavaRegex` engine
/// wrapper. Returns the compiled engine or a PatternSyntaxException error.
///
/// Java workloads recompile the same pattern repeatedly — `String.matches`,
/// `String.split`, `String.replaceAll` each call `Pattern.compile` internally
/// on every invocation, and regex compilation (especially the `fancy-regex`
/// fallback) is expensive relative to the match itself. We memoise successful
/// compilations in a bounded cache keyed by `(pattern, flags)`. Both
/// `regex::Regex` and `fancy_regex::Regex` are cheap-ish to clone (the `regex`
/// crate clones share an `Arc` internally), so returning a clone preserves the
/// existing by-value API while avoiding the recompile. Compile *failures* are
/// not cached — they are rare and re-deriving the error message is harmless.
pub(crate) fn compile_java_regex(
    pattern: &str,
    flags: i32,
) -> Result<JavaRegex, cratonvm_types::error::RuntimeError> {
    // Bounded cache of compiled regexes. The capacity guard prevents unbounded
    // growth from programs that generate distinct patterns; when full we drop
    // the whole map and start over (simple, allocation-free eviction that keeps
    // the common steady-state — a small fixed working set — fully cached).
    const REGEX_CACHE_CAP: usize = 512;
    type RegexCache = std::collections::HashMap<(String, i32), JavaRegex>;
    static CACHE: OnceLock<Mutex<RegexCache>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));

    if let Ok(guard) = cache.lock() {
        if let Some(re) = guard.get(&(pattern.to_string(), flags)) {
            return Ok(re.clone());
        }
    }

    let compiled = compile_java_regex_uncached(pattern, flags)?;

    if let Ok(mut guard) = cache.lock() {
        if guard.len() >= REGEX_CACHE_CAP {
            guard.clear();
        }
        guard.insert((pattern.to_string(), flags), compiled.clone());
    }
    Ok(compiled)
}

/// Build a `PatternSyntaxException` detail message in **Java's** layout.
///
/// `java.util.regex.PatternSyntaxException.getMessage()` is three lines:
///
/// ```text
/// Unclosed character class near index 0
/// [
/// ^
/// ```
///
/// `<description> near index <i>`, then the pattern, then a caret under column
/// `i`. Rust's engines report something quite different ("Parsing error at
/// position 1: ..."), so code that reads the message -- and every differential
/// against HotSpot -- disagreed even though the exception CLASS was right.
///
/// # Why guessing a description here is safe
///
/// This runs **only after both engines have already rejected the pattern**. It
/// cannot turn a valid regex into an error; the worst it can do is describe an
/// already-invalid pattern differently from HotSpot. That asymmetry is what
/// makes a hand-written scanner acceptable on a path as hot and as widely used
/// as `Pattern.compile`, where a false rejection would be far worse than an
/// imperfect message.
///
/// When the scan cannot name a Java-defined cause it keeps the ENGINE's text as
/// the description rather than inventing one, so the message is never less
/// informative than before.
fn java_pattern_syntax_error(
    pattern: &str,
    engine_text: &str,
) -> cratonvm_types::error::RuntimeError {
    let (description, index) = diagnose_java_pattern(pattern)
        .unwrap_or_else(|| (engine_text.replace('\n', " ").trim().to_string(), -1));
    // Parts, not a formatted string: `PatternSyntaxException.getMessage()` is
    // an override that assembles them itself, with `System.lineSeparator()`
    // (so `\r\n` on Windows, which a Rust-side format! would get wrong), and
    // `getDescription()` / `getPattern()` / `getIndex()` need them anyway.
    cratonvm_types::error::RuntimeError::PatternSyntaxException {
        description,
        pattern: pattern.to_string(),
        index,
    }
}

/// Find the first Java-named syntax fault in `pattern`, as
/// `(description, char index)`.
///
/// Deliberately covers only the causes `java.util.regex.Pattern` names in its
/// own error strings, and returns `None` for anything else so the caller falls
/// back to the engine's wording.
fn diagnose_java_pattern(pattern: &str) -> Option<(String, i32)> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0usize;
    let mut group_stack: Vec<usize> = Vec::new();
    // Whether a quantifier could legally attach at this point.
    let mut quantifiable = false;

    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' => {
                if i + 1 >= chars.len() {
                    return Some(("Trailing backslash".to_string(), i as i32));
                }
                let e = chars[i + 1];
                // Java rejects an alphabetic escape it does not define. The set
                // below is what `Pattern` accepts; any other letter is
                // "Illegal/unsupported escape sequence".
                if e.is_ascii_alphabetic() && !"aefnrtdDsSwWbBAZzGQERhHvVXpPuxcNk".contains(e) {
                    return Some((
                        "Illegal/unsupported escape sequence".to_string(),
                        (i + 1) as i32,
                    ));
                }
                i += 2;
                quantifiable = true;
                continue;
            }
            '[' => {
                // Scan to the matching ']'. A ']' first (or right after '^') is
                // a literal, per Java and POSIX.
                let open = i;
                let mut j = i + 1;
                if j < chars.len() && chars[j] == '^' {
                    j += 1;
                }
                if j < chars.len() && chars[j] == ']' {
                    j += 1;
                }
                let mut closed = false;
                while j < chars.len() {
                    if chars[j] == '\\' {
                        j += 2;
                        continue;
                    }
                    if chars[j] == ']' {
                        closed = true;
                        break;
                    }
                    j += 1;
                }
                if !closed {
                    // Reported at the '[', which is what HotSpot does.
                    return Some(("Unclosed character class".to_string(), open as i32));
                }
                i = j + 1;
                quantifiable = true;
                continue;
            }
            '(' => {
                group_stack.push(i);
                i += 1;
                quantifiable = false;
                continue;
            }
            ')' => {
                if group_stack.pop().is_none() {
                    return Some(("Unmatched closing ')'".to_string(), i as i32));
                }
                i += 1;
                quantifiable = true;
                continue;
            }
            '*' | '+' | '?' => {
                if !quantifiable {
                    return Some((format!("Dangling meta character '{c}'"), i as i32));
                }
                i += 1;
                // `a*?` and `a*+` are legal (reluctant / possessive), so one
                // quantifier after another is not by itself an error; a third in
                // a row is, and the engines already reject that.
                quantifiable = false;
                continue;
            }
            _ => {
                i += 1;
                quantifiable = true;
            }
        }
    }
    if !group_stack.is_empty() {
        // HotSpot reports an unclosed group at the END of the pattern, not at
        // the '(' -- `"(a"` is "Unclosed group near index 2".
        return Some(("Unclosed group".to_string(), chars.len() as i32));
    }
    None
}

/// Compile a Java regex without consulting the cache. See `compile_java_regex`.
fn compile_java_regex_uncached(
    pattern: &str,
    flags: i32,
) -> Result<JavaRegex, cratonvm_types::error::RuntimeError> {
    // LITERAL flag: treat pattern as a literal string (no regex metacharacters).
    if flags & JAVA_REGEX_LITERAL != 0 {
        let escaped = regex::escape(pattern);
        return regex::Regex::new(&escaped)
            .map(JavaRegex::Std)
            .map_err(|e| java_pattern_syntax_error(pattern, &format!("{e}")));
    }

    // Build the Rust regex pattern with flag prefixes
    let mut prefix = String::new();
    if flags & JAVA_REGEX_CASE_INSENSITIVE != 0 {
        prefix.push_str("(?i)");
    }
    if flags & JAVA_REGEX_MULTILINE != 0 {
        prefix.push_str("(?m)");
    }
    if flags & JAVA_REGEX_DOTALL != 0 {
        prefix.push_str("(?s)");
    }
    if flags & JAVA_REGEX_COMMENTS != 0 {
        prefix.push_str("(?x)");
    }
    // UNICODE_CASE: Rust regex is Unicode-case-aware by default, so this flag is
    // effectively always honored for `(?i)`.
    let _ = JAVA_REGEX_UNICODE_CASE;

    let translated = translate_java_regex(pattern);
    // ASCII-default Perl classes. Java's `\d` / `\w` / `\s` / `\b` (and the
    // negated forms) are **ASCII-only** unless `UNICODE_CHARACTER_CLASS` is set,
    // whereas the `regex` crate's are Unicode by default (`\d` == `\p{Nd}` etc.).
    // Without this rewrite, `"٣".matches("\\d")` would be `true` here but
    // `false` on HotSpot. Rewrite to explicit ASCII classes that match Java's
    // default exactly; when the caller passes `UNICODE_CHARACTER_CLASS`, leave the
    // engine's Unicode classes (which is what that flag requests).
    let translated = if flags & JAVA_REGEX_UNICODE_CHARACTER_CLASS == 0 {
        std::borrow::Cow::Owned(ascii_perl_classes(&translated))
    } else {
        translated
    };
    let full = format!("{prefix}{translated}");
    // Fast path: try the `regex` crate first.
    if let Ok(r) = regex::Regex::new(&full) {
        return Ok(JavaRegex::Std(r));
    }
    // Fallback: `fancy-regex` for lookaround/backrefs/etc.
    match fancy_regex::Regex::new(&full) {
        Ok(r) => Ok(JavaRegex::Fancy(Box::new(r))),
        // The concrete `PatternSyntaxException`, not its
        // `IllegalArgumentException` parent. Both engines rejecting the pattern
        // is the same event HotSpot reports from `Pattern.compile`, and code
        // that validates a user-supplied regex catches the concrete class by
        // name.
        Err(e) => Err(java_pattern_syntax_error(pattern, &format!("{e}"))),
    }
}

/// Translate Java regex constructs that aren't directly supported by the Rust
/// regex crate into equivalents the crate understands.
///
/// Currently handles:
/// - `\p{InBlockName}` / `\P{InBlockName}` (Java Unicode-block prefix) →
///   `\p{BlockName}` / `\P{BlockName}` (Rust accepts Unicode block names
///   directly without the `In` prefix). Encountered in log4j2's
///   `StatusLogger$PropertiesUtilsDouble.normalizePropertyName` which uses
///   `\P{InBasic_Latin}` to scrub non-ASCII characters from property names —
///   without this translation the regex compile fails with
///   `Unicode property not found`, surfacing as an `IllegalArgumentException`
///   that escapes `StatusLogger$Config.<clinit>` and aborts WildFly boot.
/// - `\p{IsScriptName}` / `\P{IsScriptName}` (Java Unicode-script prefix) →
///   `\p{ScriptName}` / `\P{ScriptName}` (same accepted-without-prefix shape).
/// - `\p{java*}` / `\P{java*}` (the predefined Java character-class aliases
///   from `java.util.regex.Pattern`'s `CharPredicates.forProperty` table,
///   e.g. `\p{javaWhitespace}`, `\p{javaDigit}`) → an explicit Rust-regex
///   class body matching the corresponding `java.lang.Character.isXxx(int)`
///   predicate (see `map_java_predefined_class`). These are direct method
///   aliases, not Unicode property/category names, so neither `regex` nor
///   `fancy-regex` understands them by name. `java.util.Scanner`'s static
///   initializer depends on `\p{javaWhitespace}` and `\p{javaDigit}`
///   (`WHITESPACE_PATTERN` / `NON_ASCII_DIGIT`), so leaving these
///   untranslated makes `Scanner`'s `<clinit>` throw unconditionally.
///   Residual: does not honor `Pattern.CASE_INSENSITIVE` widening
///   `javaLowerCase`/`javaUpperCase`/`javaTitleCase` into a tri-case union
///   (rare — needs both the flag and one of these three classes together).
fn translate_java_regex(pattern: &str) -> std::borrow::Cow<'_, str> {
    // Fast path: if the pattern doesn't contain `\p{In`, `\p{Is`, or `\p{java`
    // (or the capital-P negated forms), and no `\Q...\E` quoted-literal
    // blocks, there's nothing to rewrite.
    let has_quote_block = pattern.contains("\\Q");
    // Any `\p{...}`/`\P{...}` needs the loop below: it covers Unicode
    // block/script prefixes (In/Is), `Character.is*` names (java*), the
    // `all` alias, AND the POSIX character classes (Alpha, Digit, XDigit,
    // ...) handled by `map_java_character_property`, which don't share a
    // common prefix so can't be cheaply pre-filtered individually.
    let has_property_class = pattern.contains("\\p{") || pattern.contains("\\P{");
    if !has_quote_block && !has_property_class {
        return std::borrow::Cow::Borrowed(pattern);
    }
    let mut out = String::with_capacity(pattern.len());
    let bytes = pattern.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Translate `\Q...\E` Java "quoted literal" blocks: content between
        // `\Q` and `\E` is treated as a literal string by Java's regex engine,
        // but neither the `regex` crate nor `fancy-regex` accepts the
        // `\Q` escape. Rewrite each block by emitting `\x` for every
        // regex-metacharacter byte inside, preserving everything else. Used by
        // Keycloak's `DeclarativeUserProfileProviderFactory.getRegexPatternString`
        // to build allow-lists like `(\Qfoo\E|\Qbar\E)`; without translation
        // the regex compile fails and aborts Keycloak's Quarkus boot during
        // `setDefaultUserProfileConfiguration`.
        if i + 1 < bytes.len() && bytes[i] == b'\\' && bytes[i + 1] == b'Q' {
            let start = i + 2;
            // Find terminating `\E`; if missing, Java treats the rest of the
            // pattern as literal up to end-of-input.
            let end = pattern[start..].find("\\E").map(|p| start + p);
            let lit_end = end.unwrap_or(bytes.len());
            for &b in &bytes[start..lit_end] {
                // Escape every ASCII regex metacharacter; leave non-ASCII /
                // alphanumerics alone (they are literal in regex anyway).
                let is_meta = matches!(
                    b,
                    b'\\'
                        | b'.'
                        | b'+'
                        | b'*'
                        | b'?'
                        | b'('
                        | b')'
                        | b'['
                        | b']'
                        | b'{'
                        | b'}'
                        | b'^'
                        | b'$'
                        | b'|'
                        | b'#'
                        | b'-'
                        | b'&'
                        | b'~'
                        | b'/'
                        | b' '
                        | b'\t'
                );
                if is_meta {
                    out.push('\\');
                }
                out.push(b as char);
            }
            i = match end {
                Some(e) => e + 2,
                None => bytes.len(),
            };
            continue;
        }
        // Java's `all` property denotes every character. Rust regexes do not
        // have that alias, but they do accept explicit Unicode scalar ranges.
        // The negated form becomes a class that compiles and can never match.
        if i + 7 <= bytes.len()
            && bytes[i] == b'\\'
            && (bytes[i + 1] == b'p' || bytes[i + 1] == b'P')
            && &pattern[i + 2..i + 7] == "{all}"
        {
            if bytes[i + 1] == b'P' {
                out.push_str("[^\\x{0}-\\x{10FFFF}]");
            } else {
                out.push_str("[\\x{0}-\\x{10FFFF}]");
            }
            i += 7;
            continue;
        }
        // Look for `\p{Name}` / `\P{Name}` POSIX character classes (Lower,
        // Upper, ASCII, Alpha, Digit, Alnum, Punct, Graph, Print, Blank,
        // Cntrl, XDigit, Space -- java.util.regex.Pattern javadoc, "POSIX
        // character classes (US-ASCII only)"). Rust's `regex` crate only
        // recognizes Unicode property names in `\p{...}` (no notion of
        // "XDigit"/"Alpha"/etc.), so without this these patterns fail to
        // compile in BOTH the `regex` and `fancy-regex` fallback, and
        // `compile_java_regex` returns an Err. That Err was observed to
        // silently corrupt `String.matches` (see `native_string_matches`'s
        // literal-equality fallback on compile failure) -- e.g.
        // `"5".matches("\\p{XDigit}+")` returned `false` instead of `true`,
        // while `Pattern.matches("\\p{XDigit}+", "5")` (real bytecode,
        // unaffected by this translation layer) correctly returned `true`.
        // Found via Tomcat's `TestHttp2Limits.testPostWithTrailerHeadersSize0`.
        // Checked before the `In`/`Is`/`java*` branches below since POSIX
        // names never collide with those prefixes; unrecognized names fall
        // through unchanged to let those branches (or the literal copy at
        // the bottom) handle them. See `map_posix_character_class` for the
        // US-ASCII-only rationale (matches Java's default; the rarely-used
        // `UNICODE_CHARACTER_CLASS` flag is not special-cased here, same as
        // the `java*` branch below).
        if i + 3 < bytes.len()
            && bytes[i] == b'\\'
            && (bytes[i + 1] == b'p' || bytes[i + 1] == b'P')
            && bytes[i + 2] == b'{'
        {
            if let Some(close_off) = pattern[i + 3..].find('}') {
                let name = &pattern[i + 3..i + 3 + close_off];
                if let Some(class_body) = map_posix_character_class(name) {
                    let negated = bytes[i + 1] == b'P';
                    if negated {
                        out.push_str("[^");
                        out.push_str(class_body);
                        out.push(']');
                    } else {
                        out.push('[');
                        out.push_str(class_body);
                        out.push(']');
                    }
                    i = i + 3 + close_off + 1;
                    continue;
                }
                // Not a POSIX name: fall through to the In/Is/java* checks
                // below (or literal copy if none match either).
            }
        }
        // Look for `\p{In` or `\p{Is` (both cases of p) followed by a name and `}`.
        // All matched bytes are ASCII so byte indexing is safe.
        if i + 5 < bytes.len()
            && bytes[i] == b'\\'
            && (bytes[i + 1] == b'p' || bytes[i + 1] == b'P')
            && bytes[i + 2] == b'{'
            && bytes[i + 3] == b'I'
            && (bytes[i + 4] == b'n' || bytes[i + 4] == b's')
        {
            if let Some(close_off) = pattern[i + 5..].find('}') {
                let name = &pattern[i + 5..i + 5 + close_off];
                let negated = bytes[i + 1] == b'P';
                let prefix = bytes[i + 4]; // b'n' (block) or b's' (script)
                if let Some(range_class) = map_java_unicode_block(name, prefix == b'n') {
                    if negated {
                        out.push_str("[^");
                        out.push_str(range_class);
                        out.push(']');
                    } else {
                        out.push('[');
                        out.push_str(range_class);
                        out.push(']');
                    }
                } else {
                    // Fall back: drop the In/Is prefix and let Rust regex try
                    // (works for some script names even with Is prefix).
                    out.push('\\');
                    out.push(bytes[i + 1] as char);
                    out.push('{');
                    out.push_str(name);
                    out.push('}');
                }
                i = i + 5 + close_off + 1;
                continue;
            }
        }
        // Look for `\p{java...}` / `\P{java...}` — the predefined Java
        // character-class aliases (see `map_java_predefined_class`).
        if i + 3 < bytes.len()
            && bytes[i] == b'\\'
            && (bytes[i + 1] == b'p' || bytes[i + 1] == b'P')
            && bytes[i + 2] == b'{'
            && pattern[i + 3..].starts_with("java")
        {
            if let Some(close_off) = pattern[i + 3..].find('}') {
                let name = &pattern[i + 3..i + 3 + close_off];
                let negated = bytes[i + 1] == b'P';
                if let Some(class_body) = map_java_predefined_class(name) {
                    if negated {
                        out.push_str("[^");
                        out.push_str(class_body);
                        out.push(']');
                    } else {
                        out.push('[');
                        out.push_str(class_body);
                        out.push(']');
                    }
                    i = i + 3 + close_off + 1;
                    continue;
                }
                // Unknown `java*` name: fall through and copy the escape
                // literally below, same as any other unrecognized pattern.
            }
        }
        // Copy one full UTF-8 character.
        let ch_len = utf8_char_len(bytes[i]);
        let end = (i + ch_len).min(bytes.len());
        out.push_str(&pattern[i..end]);
        i = end;
    }
    std::borrow::Cow::Owned(out)
}

/// Read the pattern source string + flags from a Pattern object and compile.
pub(crate) fn read_pattern_regex(
    ctx: &mut dyn NativeContext,
    pattern_obj: cratonvm_types::ObjectRef,
) -> Result<JavaRegex, cratonvm_types::error::RuntimeError> {
    let source = match ctx.get_field(pattern_obj, PAT_FIELD_SOURCE) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let flags = match ctx.get_field(pattern_obj, PAT_FIELD_FLAGS) {
        Value::Int(f) => f,
        _ => 0,
    };
    compile_java_regex(&source, flags)
}

pub(crate) fn register_regex_natives(registry: &mut NativeMethodRegistry) {
    // Tomcat VirtualContext uses UriUtil.makeSafeForJarUrl, which calls
    // Pattern.compile(...).matcher(...).replaceAll(...). In real-JDK mode we
    // still force Pattern.matcher through the synthetic Rust regex bridge
    // during bootstrap, so every Matcher method that may touch the same object
    // must stay on that coherent synthetic surface. Letting real Matcher
    // bytecode run on the bridge-allocated object leaves real-JDK internals
    // such as `locals` null and fails in Matcher.reset().
    registry.register(
        "java/util/regex/Pattern",
        "compile",
        "(Ljava/lang/String;)Ljava/util/regex/Pattern;",
        native_pattern_compile,
    );
    registry.register(
        "java/util/regex/Pattern",
        "compile",
        "(Ljava/lang/String;I)Ljava/util/regex/Pattern;",
        native_pattern_compile_flags,
    );
    registry.register(
        "java/util/regex/Pattern",
        "matcher",
        "(Ljava/lang/CharSequence;)Ljava/util/regex/Matcher;",
        native_pattern_matcher,
    );
    registry.register(
        "java/util/regex/Pattern",
        "matches",
        "(Ljava/lang/String;Ljava/lang/CharSequence;)Z",
        native_pattern_matches_static,
    );
    registry.register(
        "java/util/regex/Pattern",
        "pattern",
        "()Ljava/lang/String;",
        native_pattern_pattern,
    );
    registry.register(
        "java/util/regex/Pattern",
        "flags",
        "()I",
        native_pattern_flags,
    );
    registry.register(
        "java/util/regex/Pattern",
        "split",
        "(Ljava/lang/CharSequence;)[Ljava/lang/String;",
        native_pattern_split,
    );
    registry.register(
        "java/util/regex/Pattern",
        "split",
        "(Ljava/lang/CharSequence;I)[Ljava/lang/String;",
        native_pattern_split_limit,
    );
    registry.register(
        "java/util/regex/Pattern",
        "quote",
        "(Ljava/lang/String;)Ljava/lang/String;",
        native_pattern_quote,
    );

    registry.register(
        "java/util/regex/Matcher",
        "find",
        "()Z",
        native_matcher_find,
    );
    registry.register(
        "java/util/regex/Matcher",
        "find",
        "(I)Z",
        native_matcher_find_at,
    );
    registry.register(
        "java/util/regex/Matcher",
        "matches",
        "()Z",
        native_matcher_matches,
    );
    registry.register(
        "java/util/regex/Matcher",
        "lookingAt",
        "()Z",
        native_matcher_looking_at,
    );
    registry.register(
        "java/util/regex/Matcher",
        "group",
        "()Ljava/lang/String;",
        native_matcher_group,
    );
    registry.register(
        "java/util/regex/Matcher",
        "group",
        "(I)Ljava/lang/String;",
        native_matcher_group_idx,
    );
    registry.register(
        "java/util/regex/Matcher",
        "group",
        "(Ljava/lang/String;)Ljava/lang/String;",
        native_matcher_group_named,
    );
    registry.register(
        "java/util/regex/Matcher",
        "groupCount",
        "()I",
        native_matcher_group_count,
    );
    registry.register(
        "java/util/regex/Matcher",
        "start",
        "()I",
        native_matcher_start,
    );
    registry.register(
        "java/util/regex/Matcher",
        "start",
        "(I)I",
        native_matcher_start,
    );
    registry.register("java/util/regex/Matcher", "end", "()I", native_matcher_end);
    registry.register("java/util/regex/Matcher", "end", "(I)I", native_matcher_end);
    registry.register(
        "java/util/regex/Matcher",
        "reset",
        "()Ljava/util/regex/Matcher;",
        native_matcher_reset,
    );
    registry.register(
        "java/util/regex/Matcher",
        "reset",
        "(Ljava/lang/CharSequence;)Ljava/util/regex/Matcher;",
        native_matcher_reset_input,
    );
    registry.register(
        "java/util/regex/Matcher",
        "replaceAll",
        "(Ljava/lang/String;)Ljava/lang/String;",
        native_matcher_replace_all,
    );
    registry.register(
        "java/util/regex/Matcher",
        "replaceFirst",
        "(Ljava/lang/String;)Ljava/lang/String;",
        native_matcher_replace_first,
    );
    registry.register(
        "java/util/regex/Matcher",
        "region",
        "(II)Ljava/util/regex/Matcher;",
        native_matcher_region,
    );
    registry.register(
        "java/util/regex/Matcher",
        "regionStart",
        "()I",
        native_matcher_region_start,
    );
    registry.register(
        "java/util/regex/Matcher",
        "regionEnd",
        "()I",
        native_matcher_region_end,
    );
    registry.register(
        "java/util/regex/Matcher",
        "appendReplacement",
        "(Ljava/lang/StringBuffer;Ljava/lang/String;)Ljava/util/regex/Matcher;",
        native_matcher_append_replacement,
    );
    registry.register(
        "java/util/regex/Matcher",
        "appendTail",
        "(Ljava/lang/StringBuffer;)Ljava/lang/StringBuffer;",
        native_matcher_append_tail,
    );
    registry.register(
        "java/util/regex/Matcher",
        "quoteReplacement",
        "(Ljava/lang/String;)Ljava/lang/String;",
        native_matcher_quote_replacement,
    );
    registry.register(
        "java/util/regex/Matcher",
        "hasMatch",
        "()Z",
        native_matcher_has_match,
    );
}

pub(crate) fn native_pattern_compile(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_pattern_compile_cached(ctx, args, 0)
}

pub(crate) fn native_pattern_compile_flags(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let flags = match args.get(1) {
        Some(Value::Int(f)) => *f,
        _ => 0,
    };
    native_pattern_compile_cached(ctx, args, flags)
}

fn native_pattern_compile_cached(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    flags: i32,
) -> MethodCallResult {
    let source_obj = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Real JDK mode: construct through the actual private constructor once,
    // then return the immutable Pattern from a moving-GC-aware global root.
    if pattern_realjdk_field_indices(ctx).is_some() {
        let source = ctx.read_string(source_obj).unwrap_or_default();
        let key = (ctx.vm_identity(), source, flags);
        if let Ok(cache) = real_pattern_cache().lock() {
            if let Some(handle) = cache.get(&key) {
                if let Some(pattern) = ctx.resolve_global_root(*handle) {
                    return Ok(Some(Value::Object(Some(pattern))));
                }
            }
        }
        let pattern = ctx.new_object_initialized(
            "java/util/regex/Pattern",
            "(Ljava/lang/String;I)V",
            &[Value::Object(Some(source_obj)), Value::Int(flags)],
        )?;
        if let Some(Value::Object(Some(pattern))) = pattern {
            let root = ctx.add_global_root(pattern);
            if root != 0 {
                if let Ok(mut cache) = real_pattern_cache().lock() {
                    // Bound the process-local cache; entries are per-VM and
                    // Pattern is immutable, so evicting an old global root is
                    // semantically invisible to callers.
                    if cache.len() >= 1024 {
                        if let Some((old_key, old_root)) = cache
                            .iter()
                            .find(|((vm, _, _), _)| *vm == key.0)
                            .map(|(k, v)| (k.clone(), *v))
                        {
                            cache.remove(&old_key);
                            let _ = ctx.remove_global_root(old_root);
                        }
                    }
                    cache.insert(key, root);
                }
            }
            return Ok(Some(Value::Object(Some(pattern))));
        }
        return Ok(pattern);
    }
    // Validate the pattern compiles
    let source = ctx.read_string(source_obj).unwrap_or_default();
    let _ = compile_java_regex(&source, flags)?;

    // Allocate with the real `java/util/regex/Pattern` class_id so the
    // interpreter dispatcher knows the receiver's class — otherwise
    // invokevirtual on the returned object lands in `java/lang/Object`
    // and methods like `matcher()` are NoSuchMethodError. Prefer the
    // already-loaded id (no clinit side-effects); only force loading if
    // Pattern hasn't been touched yet; fall back to ClassId(0) on
    // failure.
    let pat_cid = ctx
        .class_id_by_name("java/util/regex/Pattern")
        .or_else(|| ctx.ensure_class_initialized("java/util/regex/Pattern").ok())
        .unwrap_or(cratonvm_types::ClassId::new(0));
    let n = ctx.class_num_total_fields(pat_cid).max(PAT_NUM_FIELDS);
    let pat = ctx.alloc_object(pat_cid, n);
    ctx.set_field(pat, PAT_FIELD_SOURCE, Value::Object(Some(source_obj)));
    ctx.set_field(pat, PAT_FIELD_FLAGS, Value::Int(flags));
    Ok(Some(Value::Object(Some(pat))))
}

pub(crate) fn native_pattern_matcher(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let input_obj = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Allocate with the real `java/util/regex/Matcher` class_id so the
    // interpreter dispatcher resolves invokevirtual against Matcher
    // (find/matches/group/etc.) rather than against java/lang/Object.
    // Prefer the already-loaded id to avoid clinit side-effects.
    let mat_cid = ctx
        .class_id_by_name("java/util/regex/Matcher")
        .or_else(|| ctx.ensure_class_initialized("java/util/regex/Matcher").ok())
        .unwrap_or(cratonvm_types::ClassId::new(0));
    let n = ctx.class_num_total_fields(mat_cid).max(MAT_NUM_FIELDS);
    let mat = ctx.alloc_object(mat_cid, n);
    ctx.set_field(mat, MAT_FIELD_PATTERN, Value::Object(Some(this)));
    ctx.set_field(mat, MAT_FIELD_INPUT, Value::Object(Some(input_obj)));
    ctx.set_field(mat, MAT_FIELD_OFFSET, Value::Int(0));
    ctx.set_field(mat, MAT_FIELD_MATCH_START, Value::Int(-1));
    ctx.set_field(mat, MAT_FIELD_MATCH_END, Value::Int(-1));
    ctx.set_field(mat, MAT_FIELD_LAST_APPEND, Value::Int(0));
    Ok(Some(Value::Object(Some(mat))))
}

fn native_pattern_matches_static(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let pattern_str = match args.first() {
        Some(Value::Object(Some(r))) => ctx.read_string(*r).unwrap_or_default(),
        _ => return Ok(Some(Value::Int(0))),
    };
    let input_str = match args.get(1) {
        Some(Value::Object(Some(r))) => ctx.read_string(*r).unwrap_or_default(),
        _ => return Ok(Some(Value::Int(0))),
    };
    let re = compile_java_regex(&pattern_str, 0)?;
    let anchored = format!("^(?:{})$", re.as_str());
    let matched = match compile_anchored_cached(&anchored) {
        Some(full) => full.is_match(&input_str),
        None => re.is_match(&input_str),
    };
    Ok(Some(Value::Int(if matched { 1 } else { 0 })))
}

fn native_pattern_pattern(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let src = ctx.get_field(this, PAT_FIELD_SOURCE);
    Ok(Some(src))
}

fn native_pattern_flags(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let f = ctx.get_field(this, PAT_FIELD_FLAGS);
    Ok(Some(f))
}

fn native_pattern_split(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_pattern_split_impl(ctx, args, 0)
}

fn native_pattern_split_limit(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let limit = match args.get(2) {
        Some(Value::Int(n)) => *n,
        _ => 0,
    };
    native_pattern_split_impl(ctx, args, limit)
}

fn native_pattern_split_impl(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    limit: i32,
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let input_str = match args.get(1) {
        Some(Value::Object(Some(r))) => ctx.read_string(*r).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(None))),
    };
    let re = read_pattern_regex(ctx, this)?;

    let parts: Vec<String> = if limit > 0 {
        re.splitn(&input_str, limit as usize)
    } else {
        re.split(&input_str)
    };

    let parts: Vec<String> = if limit == 0 {
        let mut v = parts;
        while v.last().map(|s| s.is_empty()).unwrap_or(false) {
            v.pop();
        }
        if v.is_empty() {
            vec![String::new()]
        } else {
            v
        }
    } else {
        parts
    };

    let string_class_id = match ctx.ensure_class_initialized("java/lang/String") {
        Ok(id) => id,
        // Fallible since 2026-08-10 (JDK-only wave 2, step 3). `java.lang.String`
        // is in every image, so the `Ok` arm is what runs; a run reaching this
        // one has no `java.base`, and fabricating a `String` stand-in there is
        // a second failure wearing the first one's name.
        Err(_) => crate::util_concurrent_ext::refused_class(ctx, "java/lang/String", 8)?,
    };
    let arr = ctx.new_ref_array(string_class_id, parts.len());
    for (i, part) in parts.iter().enumerate() {
        let str_ref = ctx.create_string(part);
        ctx.set_array_element(arr, i, Value::Object(Some(str_ref)));
    }
    Ok(Some(Value::Object(Some(arr))))
}

/// Keyed by `(vm_identity, ctx.identity_hash_code(matcher))`, NOT `ObjectRef`.
///
/// The VM half of the key is NOT optional. `identity_hash_code` is minted from
/// a PER-HEAP counter (`GenerationalHeap::next_hash`), so two VMs alive at the
/// same time in one process — every parallel `cargo test` thread builds its own
/// `SharedVm` — hand out the SAME small integers for their first objects. With
/// a bare `i32` key, VM A's `Matcher` #k found VM B's entry and, because VM B's
/// input `String` had also been minted the same `input_identity`, passed the
/// freshness check and returned VM B's decoded text: `find()` then searched the
/// wrong string and reported no match. Same defect family as the
/// `widened_obj_key` collection aliasing.
///
/// `ObjectRef` is a raw heap pointer that CratonVM's moving GC relocates on
/// every collection — an early version of this cache keyed by `ObjectRef`
/// (and separately gated on an unchanged `ctx.gc_collection_count()`) was
/// *correct* but had near-zero hit rate on any allocation-heavy find()-loop
/// (this Matcher benchmark's `group()`/`create_string`/`Long.parseLong`
/// churn triggers young-gen collections often enough that almost every call
/// saw a bumped collection count, so almost every call redecoded anyway —
/// confirmed by a before/after benchmark showing no measurable improvement).
///
/// `identity_hash_code` (`vm/src/vm/vm_exec.rs`) is a value stored in the
/// object header and explicitly carried across a move by the GC — the same
/// "stable across moves" property `register_var_handle_root`
/// (`vm/src/vm/vm_exec.rs`, "Keyed by identity hash (stable across moves)
/// for dedup") already relies on elsewhere in this VM. Using it as the cache
/// key means a benign relocation (same logical object, new address) is
/// invisible to this cache — only a genuinely different object (a fresh
/// allocation reusing a freed address gets its own freshly-assigned identity
/// hash, vanishingly unlikely to collide with the old one) causes a miss.
/// [`MatcherInputCacheEntry::input_identity`] gives the same treatment to
/// `MAT_FIELD_INPUT`, so a `reset(CharSequence)` swap is still caught.
fn matcher_input_cache(
) -> &'static parking_lot::Mutex<std::collections::HashMap<(usize, i32), MatcherInputCacheEntry>> {
    static CACHE: std::sync::OnceLock<
        parking_lot::Mutex<std::collections::HashMap<(usize, i32), MatcherInputCacheEntry>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

/// The `(vm, identity)` key every [`matcher_input_cache`] access must use.
fn matcher_cache_key(ctx: &dyn NativeContext, mat: cratonvm_types::ObjectRef) -> (usize, i32) {
    (ctx.vm_identity(), ctx.identity_hash_code(mat))
}

/// Store capture-group spans for the match `find()` just produced, so a
/// following `group(N)`/`start(N)`/`end(N)` on the SAME match can read them
/// back instead of re-running the regex engine. No-op if the input cache
/// entry is somehow absent (shouldn't happen — `find()` always calls
/// [`matcher_read_input_cached`], which inserts one, before this); callers
/// that miss simply take the slower, always-correct re-search fallback.
fn matcher_cache_store_captures(
    mat_key: (usize, i32),
    match_start: usize,
    groups: Vec<Option<(usize, usize)>>,
    named: std::collections::HashMap<String, usize>,
) {
    let cache = matcher_input_cache();
    let mut guard = cache.lock();
    if let Some(entry) = guard.get_mut(&mat_key) {
        entry.captures = Some(MatcherCaptures {
            match_start,
            groups,
            named,
        });
    }
}

/// Fetch cached capture-group spans for `match_start` on this `Matcher`, if
/// [`matcher_cache_store_captures`] populated them for exactly this match.
fn matcher_cache_lookup_captures(
    mat_key: (usize, i32),
    match_start: usize,
) -> Option<(
    Vec<Option<(usize, usize)>>,
    std::collections::HashMap<String, usize>,
)> {
    let cache = matcher_input_cache();
    let guard = cache.lock();
    let entry = guard.get(&mat_key)?;
    let caps = entry.captures.as_ref()?;
    if caps.match_start == match_start {
        Some((caps.groups.clone(), caps.named.clone()))
    } else {
        None
    }
}

/// Read a `Matcher`'s input `String`, reusing a cached UTF-16→UTF-8 decode
/// across repeated `find()`/`group()`/`start()`/`end()` calls on the same
/// `Matcher` instead of re-decoding the entire backing array from the Java
/// heap on every single native dispatch. Without this, an n-match `find()`
/// loop over an n-length string cost O(n) per call * O(n) calls = O(n^2)
/// (see `fixed-suite-bugs/matcher-native-full-input-redecode-quadratic-FIXED.md`).
///
/// Returns `Arc<str>` rather than `String` so a cache HIT is an O(1)
/// refcount bump, not an O(n) copy — the point of caching is lost if every
/// caller clones the decoded string back out.
///
/// Cache safety: see [`matcher_input_cache`] for why this is keyed by
/// identity hash rather than by `ObjectRef` or gated on a GC-collection
/// counter.
fn matcher_read_input_cached(
    ctx: &mut dyn NativeContext,
    mat: cratonvm_types::ObjectRef,
) -> std::sync::Arc<str> {
    let input_obj = match ctx.get_field(mat, MAT_FIELD_INPUT) {
        Value::Object(Some(r)) => r,
        _ => return std::sync::Arc::from(""),
    };
    let mat_key = matcher_cache_key(ctx, mat);
    let input_identity = ctx.identity_hash_code(input_obj);

    let cache = matcher_input_cache();
    {
        let guard = cache.lock();
        if let Some(entry) = guard.get(&mat_key) {
            if entry.input_identity == input_identity {
                return entry.decoded.clone();
            }
        }
    }

    let decoded: std::sync::Arc<str> = ctx.read_string(input_obj).unwrap_or_default().into();

    // Bounded cache: Matchers have no finalizer hook back into this table, so
    // long-running programs that create many short-lived Matchers need a
    // cap. Mirrors `compile_java_regex`'s eviction policy (simple,
    // allocation-free: drop the whole map and start over) — the steady-state
    // working set for realistic find()-loop usage is far below the cap.
    const MATCHER_INPUT_CACHE_CAP: usize = 4096;
    let mut guard = cache.lock();
    if guard.len() >= MATCHER_INPUT_CACHE_CAP {
        guard.clear();
    }
    guard.insert(
        mat_key,
        MatcherInputCacheEntry {
            input_identity,
            decoded: decoded.clone(),
            // A fresh entry (new input generation) invalidates any cached
            // captures — they'd be offsets into a DIFFERENT decoded string.
            captures: None,
        },
    );
    decoded
}

fn matcher_get_pattern(
    ctx: &mut dyn NativeContext,
    mat: cratonvm_types::ObjectRef,
) -> Option<cratonvm_types::ObjectRef> {
    match ctx.get_field(mat, MAT_FIELD_PATTERN) {
        Value::Object(Some(r)) => Some(r),
        _ => None,
    }
}

pub(crate) fn native_matcher_find(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let input = matcher_read_input_cached(ctx, this);
    let pat_obj = match matcher_get_pattern(ctx, this) {
        Some(p) => p,
        None => return Ok(Some(Value::Int(0))),
    };
    let re = read_pattern_regex(ctx, pat_obj)?;
    let offset = match ctx.get_field(this, MAT_FIELD_OFFSET) {
        Value::Int(o) => o.max(0) as usize,
        _ => 0,
    };

    if offset > input.len() {
        ctx.set_field(this, MAT_FIELD_MATCH_START, Value::Int(-1));
        ctx.set_field(this, MAT_FIELD_MATCH_END, Value::Int(-1));
        return Ok(Some(Value::Int(0)));
    }

    // Use `captures` rather than `find` so a following `group(N)`/`start(N)`/
    // `end(N)` — the common `while (m.find()) { m.group(N); }` idiom — can be
    // served from `matcher_cache_store_captures` below instead of re-running
    // the regex engine a second time. This was the dominant remaining O(n)
    // cost even after fixing the input-redecode: `group(N)`'s old
    // unconditional `re.captures(&input[start..])` re-search, confirmed by an
    // isolated find()-only benchmark scaling linearly while find()+group()
    // stayed superlinear.
    if let Some(caps) = re.captures(&input[offset..]) {
        let Some(whole) = caps.get(0) else {
            ctx.set_field(this, MAT_FIELD_MATCH_START, Value::Int(-1));
            ctx.set_field(this, MAT_FIELD_MATCH_END, Value::Int(-1));
            return Ok(Some(Value::Int(0)));
        };
        let abs_start = offset + whole.start;
        let abs_end = offset + whole.end;
        ctx.set_field(this, MAT_FIELD_MATCH_START, Value::Int(abs_start as i32));
        ctx.set_field(this, MAT_FIELD_MATCH_END, Value::Int(abs_end as i32));
        let groups: Vec<Option<(usize, usize)>> = (0..caps.len())
            .map(|i| caps.get(i).map(|g| (offset + g.start, offset + g.end)))
            .collect();
        matcher_cache_store_captures(
            matcher_cache_key(ctx, this),
            abs_start,
            groups,
            caps.named.clone(),
        );
        // Java `Matcher.find()` semantics: the next search starts at the end
        // of this match. But for a **zero-width** match (`abs_end == abs_start`)
        // the next search MUST advance by one position — otherwise `find()`
        // re-matches the same empty string forever (Tomcat's `Bootstrap.getPaths`
        // loops `Matcher.find()` over `(\"[^\"]*\")|(([^,])*)`, whose second
        // branch matches empty, and hangs the whole VM). HotSpot's `Matcher`
        // does this via `if (nextSearchIndex == first) nextSearchIndex++`.
        let next_offset = if abs_end == abs_start {
            advance_one_char(&input, abs_end)
        } else {
            abs_end
        };
        ctx.set_field(this, MAT_FIELD_OFFSET, Value::Int(next_offset as i32));
        Ok(Some(Value::Int(1)))
    } else {
        ctx.set_field(this, MAT_FIELD_MATCH_START, Value::Int(-1));
        ctx.set_field(this, MAT_FIELD_MATCH_END, Value::Int(-1));
        Ok(Some(Value::Int(0)))
    }
}

/// `Matcher.find(int start)` — reset the search position to `start` and look
/// for the next match.  Mirrors `native_matcher_find` but ignores the stored
/// MAT_FIELD_OFFSET in favour of the explicit argument.
pub(crate) fn native_matcher_find_at(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let start = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let input = matcher_read_input_cached(ctx, this);
    let pat_obj = match matcher_get_pattern(ctx, this) {
        Some(p) => p,
        None => return Ok(Some(Value::Int(0))),
    };
    let re = read_pattern_regex(ctx, pat_obj)?;
    let offset = (start.max(0) as usize).min(input.len());

    if offset > input.len() {
        ctx.set_field(this, MAT_FIELD_MATCH_START, Value::Int(-1));
        ctx.set_field(this, MAT_FIELD_MATCH_END, Value::Int(-1));
        return Ok(Some(Value::Int(0)));
    }

    if let Some(m) = re.find(&input[offset..]) {
        let abs_start = offset + m.start;
        let abs_end = offset + m.end;
        ctx.set_field(this, MAT_FIELD_MATCH_START, Value::Int(abs_start as i32));
        ctx.set_field(this, MAT_FIELD_MATCH_END, Value::Int(abs_end as i32));
        // Zero-width match: advance the stored offset so a following no-arg
        // `find()` does not re-match the empty string in place. See
        // `native_matcher_find` for the full rationale.
        let next_offset = if abs_end == abs_start {
            advance_one_char(&input, abs_end)
        } else {
            abs_end
        };
        ctx.set_field(this, MAT_FIELD_OFFSET, Value::Int(next_offset as i32));
        Ok(Some(Value::Int(1)))
    } else {
        ctx.set_field(this, MAT_FIELD_MATCH_START, Value::Int(-1));
        ctx.set_field(this, MAT_FIELD_MATCH_END, Value::Int(-1));
        Ok(Some(Value::Int(0)))
    }
}

pub(crate) fn native_matcher_matches(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let input = matcher_read_input_cached(ctx, this);
    let pat_obj = match matcher_get_pattern(ctx, this) {
        Some(p) => p,
        None => return Ok(Some(Value::Int(0))),
    };
    let re = read_pattern_regex(ctx, pat_obj)?;
    // Full match: anchor with ^ and $
    let anchored = format!("^(?:{})$", re.as_str());
    let full_re = compile_anchored_cached(&anchored).unwrap_or(re);
    if full_re.is_match(&input) {
        if let Some(m) = full_re.find(&input) {
            ctx.set_field(this, MAT_FIELD_MATCH_START, Value::Int(m.start as i32));
            ctx.set_field(this, MAT_FIELD_MATCH_END, Value::Int(m.end as i32));
        }
        Ok(Some(Value::Int(1)))
    } else {
        ctx.set_field(this, MAT_FIELD_MATCH_START, Value::Int(-1));
        ctx.set_field(this, MAT_FIELD_MATCH_END, Value::Int(-1));
        Ok(Some(Value::Int(0)))
    }
}

pub(crate) fn native_matcher_group(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let start = match ctx.get_field(this, MAT_FIELD_MATCH_START) {
        Value::Int(s) if s >= 0 => s as usize,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
                message: "No match found".to_string(),
            }
            .into());
        }
    };
    let end = match ctx.get_field(this, MAT_FIELD_MATCH_END) {
        Value::Int(e) if e >= 0 => e as usize,
        _ => return Ok(Some(Value::Object(None))),
    };
    let input = matcher_read_input_cached(ctx, this);
    let group = &input[start..end.min(input.len())];
    Ok(Some(Value::Object(Some(ctx.create_string(group)))))
}

pub(crate) fn native_matcher_group_idx(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let idx = match args.get(1) {
        Some(Value::Int(i)) => *i as usize,
        _ => 0,
    };

    if idx == 0 {
        return native_matcher_group(ctx, args);
    }

    let input = matcher_read_input_cached(ctx, this);
    let start = match ctx.get_field(this, MAT_FIELD_MATCH_START) {
        Value::Int(s) if s >= 0 => s as usize,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
                message: "No match found".to_string(),
            }
            .into());
        }
    };

    // Fast path: `find()` already computed this match's capture-group spans
    // (`matcher_cache_store_captures`) — reuse them instead of re-running the
    // regex engine over `input[start..]` a second time.
    if let Some((groups, _named)) =
        matcher_cache_lookup_captures(matcher_cache_key(ctx, this), start)
    {
        return match groups.get(idx).copied().flatten() {
            Some((g_start, g_end)) => Ok(Some(Value::Object(Some(
                ctx.create_string(&input[g_start..g_end.min(input.len())]),
            )))),
            None => Ok(Some(Value::Object(None))),
        };
    }

    // Fallback (cache miss — e.g. the current match came from `find(int)`/
    // `matches()`/`lookingAt()`/`region()`, none of which populate the
    // capture cache, or a GC invalidated it since `find()` ran): re-run the
    // regex on the last match region. Always correct, just not fast.
    let pat_obj = match matcher_get_pattern(ctx, this) {
        Some(p) => p,
        None => return Ok(Some(Value::Object(None))),
    };
    let re = read_pattern_regex(ctx, pat_obj)?;
    if let Some(caps) = re.captures(&input[start..]) {
        if let Some(g) = caps.get(idx) {
            return Ok(Some(Value::Object(Some(ctx.create_string(&g.text)))));
        }
    }
    Ok(Some(Value::Object(None)))
}

fn matcher_group_boundary(
    ctx: &mut dyn NativeContext,
    mat: cratonvm_types::ObjectRef,
    idx: usize,
    want_end: bool,
) -> MethodCallResult {
    let input = matcher_read_input_cached(ctx, mat);
    let pat_obj = match matcher_get_pattern(ctx, mat) {
        Some(p) => p,
        None => return Ok(Some(Value::Int(-1))),
    };
    let re = read_pattern_regex(ctx, pat_obj)?;
    let match_start = match ctx.get_field(mat, MAT_FIELD_MATCH_START) {
        Value::Int(s) if s >= 0 => s as usize,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
                message: "No match found".to_string(),
            }
            .into());
        }
    };

    if idx >= re.captures_len() {
        return Err(cratonvm_types::error::RuntimeError::aioobe_index_only(idx as i32).into());
    }

    // Fast path: reuse `find()`'s cached capture-group spans for this match
    // instead of re-running the regex engine (see `native_matcher_group_idx`
    // for the full rationale).
    if let Some((groups, _named)) =
        matcher_cache_lookup_captures(matcher_cache_key(ctx, mat), match_start)
    {
        return match groups.get(idx).copied().flatten() {
            Some((g_start, g_end)) => Ok(Some(Value::Int(
                (if want_end { g_end } else { g_start }) as i32,
            ))),
            None => Ok(Some(Value::Int(-1))),
        };
    }

    let Some(caps) = re.captures(&input[match_start..]) else {
        return Ok(Some(Value::Int(-1)));
    };
    let Some(group) = caps.get(idx) else {
        return Ok(Some(Value::Int(-1)));
    };
    let offset = if want_end { group.end } else { group.start };
    Ok(Some(Value::Int((match_start + offset) as i32)))
}

pub(crate) fn native_matcher_start(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(-1))),
    };
    Ok(Some(ctx.get_field(this, MAT_FIELD_MATCH_START)))
}

pub(crate) fn native_matcher_start_idx(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let idx = match args.get(1) {
        Some(Value::Int(i)) if *i >= 0 => *i as usize,
        Some(Value::Int(i)) => {
            return Err(cratonvm_types::error::RuntimeError::aioobe_index_only(*i).into());
        }
        _ => 0,
    };
    matcher_group_boundary(ctx, this, idx, false)
}

pub(crate) fn native_matcher_end(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(-1))),
    };
    Ok(Some(ctx.get_field(this, MAT_FIELD_MATCH_END)))
}

pub(crate) fn native_matcher_end_idx(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let idx = match args.get(1) {
        Some(Value::Int(i)) if *i >= 0 => *i as usize,
        Some(Value::Int(i)) => {
            return Err(cratonvm_types::error::RuntimeError::aioobe_index_only(*i).into());
        }
        _ => 0,
    };
    matcher_group_boundary(ctx, this, idx, true)
}

/// `Matcher.appendReplacement`'s error for a `$N` naming a group the pattern
/// does not have: `IndexOutOfBoundsException("No group N")`. The message text
/// is HotSpot's, verbatim.
pub(crate) fn no_group_error(n: usize) -> cratonvm_types::error::VmError {
    cratonvm_types::error::RuntimeError::ioobe(format!("No group {n}")).into()
}

pub(crate) fn native_matcher_replace_all(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let replacement = match args.get(1) {
        Some(Value::Object(Some(r))) => ctx.read_string(*r).unwrap_or_default(),
        _ => String::new(),
    };
    let input = matcher_read_input_cached(ctx, this);
    let pat_obj = match matcher_get_pattern(ctx, this) {
        Some(p) => p,
        None => return Ok(Some(Value::Object(Some(ctx.create_string(&input))))),
    };
    let re = read_pattern_regex(ctx, pat_obj)?;
    let result = re
        .replace_all_java(&input, replacement.as_str())
        .map_err(no_group_error)?;
    Ok(Some(Value::Object(Some(ctx.create_string(&result)))))
}

pub(crate) fn native_matcher_replace_first(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let replacement = match args.get(1) {
        Some(Value::Object(Some(r))) => ctx.read_string(*r).unwrap_or_default(),
        _ => String::new(),
    };
    let input = matcher_read_input_cached(ctx, this);
    let pat_obj = match matcher_get_pattern(ctx, this) {
        Some(p) => p,
        None => return Ok(Some(Value::Object(Some(ctx.create_string(&input))))),
    };
    let re = read_pattern_regex(ctx, pat_obj)?;
    let result = re
        .replace_first_java(&input, replacement.as_str())
        .map_err(no_group_error)?;
    Ok(Some(Value::Object(Some(ctx.create_string(&result)))))
}

fn native_matcher_reset(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    ctx.set_field(this, MAT_FIELD_OFFSET, Value::Int(0));
    ctx.set_field(this, MAT_FIELD_MATCH_START, Value::Int(-1));
    ctx.set_field(this, MAT_FIELD_MATCH_END, Value::Int(-1));
    Ok(Some(Value::Object(Some(this))))
}

fn native_matcher_reset_input(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let new_input = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(Some(this)))),
    };
    ctx.set_field(this, MAT_FIELD_INPUT, Value::Object(Some(new_input)));
    ctx.set_field(this, MAT_FIELD_OFFSET, Value::Int(0));
    ctx.set_field(this, MAT_FIELD_MATCH_START, Value::Int(-1));
    ctx.set_field(this, MAT_FIELD_MATCH_END, Value::Int(-1));
    Ok(Some(Value::Object(Some(this))))
}

fn native_matcher_looking_at(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let input = matcher_read_input_cached(ctx, this);
    let pat_obj = match matcher_get_pattern(ctx, this) {
        Some(p) => p,
        None => return Ok(Some(Value::Int(0))),
    };
    let re = read_pattern_regex(ctx, pat_obj)?;
    // lookingAt: match at the beginning of the input
    let anchored = format!("^(?:{})", re.as_str());
    let start_re = compile_anchored_cached(&anchored);
    if let Some(start_re) = start_re {
        if let Some(m) = start_re.find(&input) {
            ctx.set_field(this, MAT_FIELD_MATCH_START, Value::Int(0));
            ctx.set_field(this, MAT_FIELD_MATCH_END, Value::Int(m.end as i32));
            return Ok(Some(Value::Int(1)));
        }
    }
    ctx.set_field(this, MAT_FIELD_MATCH_START, Value::Int(-1));
    ctx.set_field(this, MAT_FIELD_MATCH_END, Value::Int(-1));
    Ok(Some(Value::Int(0)))
}

fn native_matcher_group_count(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let pat_obj = match matcher_get_pattern(ctx, this) {
        Some(p) => p,
        None => return Ok(Some(Value::Int(0))),
    };
    let re = read_pattern_regex(ctx, pat_obj)?;
    Ok(Some(Value::Int(re.captures_len() as i32 - 1)))
}

/// Pattern.quote(String) — returns a literal pattern string for the given input.
fn native_pattern_quote(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let s = match args.first() {
        Some(Value::Object(Some(r))) => ctx.read_string(*r).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(None))),
    };
    // Java's Pattern.quote wraps in \Q...\E
    let quoted = format!("\\Q{s}\\E");
    let result = ctx.create_string(&quoted);
    Ok(Some(Value::Object(Some(result))))
}

/// Matcher.region(start, end) — restrict future matching to a subsequence.
/// We store region bounds in extra synthetic fields (5 = regionStart, 6 = regionEnd).
/// Since MAT_NUM_FIELDS is 5, we use the existing offset field creatively:
/// region is implemented by modifying the input string to a substring.
fn native_matcher_region(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let start = match args.get(1) {
        Some(Value::Int(s)) => *s as usize,
        _ => 0,
    };
    let end = match args.get(2) {
        Some(Value::Int(e)) => *e as usize,
        _ => 0,
    };
    // Read current input, take the substring, store as new input
    let input = matcher_read_input_cached(ctx, this);
    let sub = if start <= end && end <= input.len() {
        &input[start..end]
    } else {
        &input
    };
    let sub_str = ctx.create_string(sub);
    ctx.set_field(this, MAT_FIELD_INPUT, Value::Object(Some(sub_str)));
    // Reset match state
    ctx.set_field(this, MAT_FIELD_OFFSET, Value::Int(0));
    ctx.set_field(this, MAT_FIELD_MATCH_START, Value::Int(-1));
    ctx.set_field(this, MAT_FIELD_MATCH_END, Value::Int(-1));
    Ok(Some(Value::Object(Some(this))))
}

fn native_matcher_region_start(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Since we implement region by substring replacement, regionStart is always 0
    Ok(Some(Value::Int(0)))
}

fn native_matcher_region_end(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let input = matcher_read_input_cached(ctx, this);
    Ok(Some(Value::Int(input.len() as i32)))
}

/// Matcher.group(String name) — return the named capture group.
fn native_matcher_group_named(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let name = match args.get(1) {
        Some(Value::Object(Some(r))) => ctx.read_string(*r).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(None))),
    };
    let input = matcher_read_input_cached(ctx, this);
    let pat_obj = match matcher_get_pattern(ctx, this) {
        Some(p) => p,
        None => return Ok(Some(Value::Object(None))),
    };
    let re = read_pattern_regex(ctx, pat_obj)?;

    let match_start = match ctx.get_field(this, MAT_FIELD_MATCH_START) {
        Value::Int(s) if s >= 0 => s as usize,
        _ => return Ok(Some(Value::Object(None))),
    };

    if let Some(caps) = re.captures(&input[match_start..]) {
        if let Some(m) = caps.name(&name) {
            let result = ctx.create_string(&m.text);
            return Ok(Some(Value::Object(Some(result))));
        }
    }
    Ok(Some(Value::Object(None)))
}

/// Process a regex replacement string — expand `$N` group references and `\\` escapes.
fn matcher_expand_replacement(replacement: &str, groups: &[Option<String>]) -> String {
    let mut result = String::with_capacity(replacement.len());
    let mut chars = replacement.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            // Escape: next char is literal
            if let Some(next) = chars.next() {
                result.push(next);
            }
        } else if c == '$' {
            // Group reference: $N or ${name} (we support only $N)
            let mut num_str = String::new();
            while let Some(&d) = chars.peek() {
                if d.is_ascii_digit() {
                    num_str.push(d);
                    chars.next();
                } else {
                    break;
                }
            }
            if let Ok(idx) = num_str.parse::<usize>() {
                if let Some(Some(g)) = groups.get(idx) {
                    result.push_str(g);
                }
            } else {
                result.push('$');
            }
        } else {
            result.push(c);
        }
    }
    result
}

fn native_matcher_append_replacement(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let sb = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(Some(this)))),
    };
    let replacement = match args.get(2) {
        Some(Value::Object(Some(r))) => ctx.read_string(*r).unwrap_or_default(),
        _ => String::new(),
    };

    let input = matcher_read_input_cached(ctx, this);
    let match_start = ctx
        .get_field(this, MAT_FIELD_MATCH_START)
        .as_int()
        .unwrap_or(-1);
    let match_end = ctx
        .get_field(this, MAT_FIELD_MATCH_END)
        .as_int()
        .unwrap_or(-1);
    let last_append = ctx
        .get_field(this, MAT_FIELD_LAST_APPEND)
        .as_int()
        .unwrap_or(0);

    if match_start < 0 {
        return Err(RuntimeError::IllegalStateException {
            message: "No match found".into(),
        }
        .into());
    }

    // Capture groups from the current match
    let pat_obj = match matcher_get_pattern(ctx, this) {
        Some(p) => p,
        None => return Ok(Some(Value::Object(Some(this)))),
    };
    let re = read_pattern_regex(ctx, pat_obj)?;
    let search_start = match_start as usize;
    let groups: Vec<Option<String>> = if let Some(caps) = re.captures(&input[search_start..]) {
        (0..caps.len())
            .map(|i| caps.get(i).map(|m| m.text.clone()))
            .collect()
    } else {
        Vec::new()
    };

    // Append the text between lastAppendPosition and the start of the match
    let between = input
        .get(last_append as usize..match_start as usize)
        .unwrap_or("")
        .to_string();
    let between_s = ctx.create_string(&between);
    let _ = ctx.invoke_virtual(
        sb,
        "append",
        "(Ljava/lang/String;)Ljava/lang/StringBuffer;",
        &[Value::Object(Some(between_s))],
    );

    // Expand and append the replacement string
    let expanded = matcher_expand_replacement(&replacement, &groups);
    let expanded_s = ctx.create_string(&expanded);
    let _ = ctx.invoke_virtual(
        sb,
        "append",
        "(Ljava/lang/String;)Ljava/lang/StringBuffer;",
        &[Value::Object(Some(expanded_s))],
    );

    // Update lastAppendPosition to end of current match
    ctx.set_field(this, MAT_FIELD_LAST_APPEND, Value::Int(match_end));
    Ok(Some(Value::Object(Some(this))))
}

fn native_matcher_append_tail(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let sb = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let input = matcher_read_input_cached(ctx, this);
    let last_append = ctx
        .get_field(this, MAT_FIELD_LAST_APPEND)
        .as_int()
        .unwrap_or(0) as usize;
    let tail = input.get(last_append..).unwrap_or("").to_string();
    let tail_s = ctx.create_string(&tail);
    let _ = ctx.invoke_virtual(
        sb,
        "append",
        "(Ljava/lang/String;)Ljava/lang/StringBuffer;",
        &[Value::Object(Some(tail_s))],
    );
    Ok(Some(Value::Object(Some(sb))))
}

fn native_matcher_quote_replacement(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Static method: args[0] is the string to quote
    let input = match args.first() {
        Some(Value::Object(Some(r))) => ctx.read_string(*r).unwrap_or_default(),
        _ => String::new(),
    };
    // Escape `$` and `\` for use as a regex replacement
    let mut result = String::with_capacity(input.len());
    for c in input.chars() {
        if c == '\\' || c == '$' {
            result.push('\\');
        }
        result.push(c);
    }
    let s = ctx.create_string(&result);
    Ok(Some(Value::Object(Some(s))))
}

fn native_matcher_has_match(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let match_start = ctx
        .get_field(this, MAT_FIELD_MATCH_START)
        .as_int()
        .unwrap_or(-1);
    Ok(Some(Value::Int(if match_start >= 0 { 1 } else { 0 })))
}

// ===========================================================================
// java.util.regex.Matcher — real-JDK-layout `find()`/`find(int)` fast path
// ===========================================================================
//
// Everything above this banner (`native_matcher_find` etc.) is the LEGACY
// synthetic-layout bridge, unconditionally dropped in real-JDK mode by
// `NativeMethodRegistry::register` (see `drop_real_layout_synthetic` in
// `native-api/src/registry.rs`) — see
// `fixed-suite-bugs/matcher-native-full-input-redecode-quadratic-FIXED.md`. It is
// dead code for every program this VM actually runs by default.
//
// This section is different: it operates on the REAL OpenJDK
// `java.util.regex.Matcher`/`Pattern` object layout (fields resolved BY NAME
// via `get_field_by_name`/`resolve_field_index`, never by hardcoded slot
// index — the exact same real-vs-synthetic-layout corruption the doc above
// warns about is avoided by construction). It intercepts only
// `Matcher.find()Z` / `Matcher.find(I)Z`, the two methods the interpreted
// `java.util.regex` engine spends the vast majority of its time in for the
// extremely common `while (m.find()) { ...; m.group(N); }` idiom (see
// `fixed-suite-bugs/wildfly/bug-03-regex-perf-deployment-build.md`
// for the interpreter-throughput root cause this works around). Every other
// `Matcher` method — `group`/`start`/`end`/`region`/`appendReplacement`/
// `matches`/`lookingAt`/`reset`/... — is left as real JDK bytecode, reading
// the SAME `groups`/`first`/`last`/`modCount`/... fields this fast path
// writes, so a program can freely mix accelerated `find()` calls with
// unaccelerated calls to any other `Matcher` method and see fully consistent
// state either way (the same "own the object model, not just an isolated
// method" contract SBR-02's `String.replaceAll` native established for
// literal String methods, extended here to a stateful object's method
// surface).
//
// Any usage this fast path cannot faithfully reproduce (a non-`String`
// `CharSequence` input — `charAt`-loop decoding it would reintroduce the
// exact per-call boundary-crossing cost this exists to eliminate;
// `transparentBounds(true)`; `anchoringBounds(false)`; a group-count
// mismatch between the compiled Rust regex and the real `Pattern`'s own
// `capturingGroupCount`) makes the native decline via
// `ctx.invoke_virtual_bytecode_only`, which re-enters the SAME (class,
// method, descriptor) triple but skips the native-override check — i.e. it
// runs the real `Matcher.find()`/`find(int)` bytecode body directly, with
// zero risk of infinite native-to-native recursion and zero correctness
// risk (real Java semantics, just without the speedup for that one call).
//
// Known, accepted residual: `hitEnd()`/`requireEnd()` after a call serviced
// by this fast path are best-effort approximations, not bit-identical to
// HotSpot's backtracking-engine bookkeeping (see `matcher_realjdk_set_hit_end`
// below) — the same category of documented, opt-out-guarded residual SBR-02
// already accepted for Rust-regex-vs-`java.util.regex` engine differences
// (possessive quantifiers, `\p{...}` property names, Unicode case folding).
// `hitEnd`/`requireEnd` are consulted almost exclusively by `java.util.Scanner`
// deciding whether to pull more data from an underlying `Readable` before
// giving up — for the single-shot, already-fully-buffered `CharSequence`
// inputs this fast path requires, an imprecise `hitEnd` cannot change the
// match RESULT a caller observes, only (in the Scanner case) whether it
// makes one extra, ultimately-irrelevant read attempt against a stream that
// has no more data anyway.

/// Real `java.util.regex.Matcher` field names (by name, not slot index —
/// see the module banner above for why that distinction matters here).
mod matcher_realjdk_fields {
    pub const PARENT_PATTERN: &str = "parentPattern";
    pub const TEXT: &str = "text";
    pub const FROM: &str = "from";
    pub const TO: &str = "to";
    pub const FIRST: &str = "first";
    pub const LAST: &str = "last";
    pub const OLD_LAST: &str = "oldLast";
    pub const GROUPS: &str = "groups";
    pub const HIT_END: &str = "hitEnd";
    pub const REQUIRE_END: &str = "requireEnd";
    pub const TRANSPARENT_BOUNDS: &str = "transparentBounds";
    pub const ANCHORING_BOUNDS: &str = "anchoringBounds";
    pub const MOD_COUNT: &str = "modCount";
}

mod pattern_realjdk_fields {
    pub const PATTERN: &str = "pattern";
    pub const FLAGS: &str = "flags";
    pub const CAPTURING_GROUP_COUNT: &str = "capturingGroupCount";
}

fn matcher_realjdk_field_indices(ctx: &mut dyn NativeContext) -> Option<MatcherFieldIndices> {
    static CACHE: OnceLock<Option<MatcherFieldIndices>> = OnceLock::new();
    *CACHE.get_or_init(|| {
        use matcher_realjdk_fields as f;
        const CLASS: &str = "java/util/regex/Matcher";
        Some(MatcherFieldIndices {
            parent_pattern: ctx.resolve_field_index(CLASS, f::PARENT_PATTERN)?,
            text: ctx.resolve_field_index(CLASS, f::TEXT)?,
            from: ctx.resolve_field_index(CLASS, f::FROM)?,
            to: ctx.resolve_field_index(CLASS, f::TO)?,
            first: ctx.resolve_field_index(CLASS, f::FIRST)?,
            last: ctx.resolve_field_index(CLASS, f::LAST)?,
            old_last: ctx.resolve_field_index(CLASS, f::OLD_LAST)?,
            groups: ctx.resolve_field_index(CLASS, f::GROUPS)?,
            hit_end: ctx.resolve_field_index(CLASS, f::HIT_END)?,
            require_end: ctx.resolve_field_index(CLASS, f::REQUIRE_END)?,
            transparent_bounds: ctx.resolve_field_index(CLASS, f::TRANSPARENT_BOUNDS)?,
            anchoring_bounds: ctx.resolve_field_index(CLASS, f::ANCHORING_BOUNDS)?,
            mod_count: ctx.resolve_field_index(CLASS, f::MOD_COUNT)?,
        })
    })
}

fn pattern_realjdk_field_indices(ctx: &mut dyn NativeContext) -> Option<PatternFieldIndices> {
    static CACHE: OnceLock<Option<PatternFieldIndices>> = OnceLock::new();
    *CACHE.get_or_init(|| {
        use pattern_realjdk_fields as f;
        const CLASS: &str = "java/util/regex/Pattern";
        Some(PatternFieldIndices {
            pattern: ctx.resolve_field_index(CLASS, f::PATTERN)?,
            flags: ctx.resolve_field_index(CLASS, f::FLAGS)?,
            capturing_group_count: ctx.resolve_field_index(CLASS, f::CAPTURING_GROUP_COUNT)?,
        })
    })
}

fn matcher_realjdk_capture_layout_valid(
    rust_capture_count: usize,
    java_capture_count: usize,
    groups_len: usize,
) -> bool {
    java_capture_count > 0
        && rust_capture_count == java_capture_count
        && groups_len >= java_capture_count.saturating_mul(2)
}

fn matcher_realjdk_capture_layout_ok(
    ctx: &mut dyn NativeContext,
    capture_count: usize,
    groups: ObjectRef,
) -> bool {
    ctx.array_length(groups) >= capture_count.saturating_mul(2)
}

fn matcher_realjdk_cache(
) -> &'static Mutex<std::collections::HashMap<i32, std::sync::Arc<MatcherRealCache>>> {
    static CACHE: OnceLock<
        Mutex<std::collections::HashMap<i32, std::sync::Arc<MatcherRealCache>>>,
    > = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn matcher_realjdk_group_slice<'a>(
    utf8: &'a str,
    utf16_to_byte: &[u32],
    start: i32,
    end: i32,
) -> Option<&'a str> {
    let start = usize::try_from(start).ok()?;
    let end = usize::try_from(end).ok()?;
    if start > end || end >= utf16_to_byte.len() {
        return None;
    }
    let is_scalar_boundary = |offset: usize| {
        offset == 0
            || offset + 1 == utf16_to_byte.len()
            || utf16_to_byte[offset] != utf16_to_byte[offset - 1]
    };
    if !is_scalar_boundary(start) || !is_scalar_boundary(end) {
        return None;
    }
    let start_byte = utf16_to_byte[start] as usize;
    let end_byte = utf16_to_byte[end] as usize;
    utf8.get(start_byte..end_byte)
}

/// Materialize a group directly from the decoded String cached by the native
/// find path. Returns `None` when the Matcher was populated by bytecode, its
/// text changed, or a boundary falls inside a surrogate pair.
fn matcher_realjdk_cached_group_string(
    ctx: &mut dyn NativeContext,
    matcher: ObjectRef,
    text: ObjectRef,
    start: i32,
    end: i32,
) -> Option<ObjectRef> {
    let matcher_raw = matcher.as_ptr() as usize;
    let text_raw = text.as_ptr() as usize;
    let last = MATCHER_REAL_LAST_CACHE.with(|cache| {
        cache
            .borrow()
            .as_ref()
            .filter(|(cached_matcher, cached_text, _, _)| {
                *cached_matcher == matcher_raw && *cached_text == text_raw
            })
            .map(|(_, _, _, entry)| entry.clone())
    });
    let entry = match last {
        Some(entry) => Some(entry),
        None => {
            let matcher_identity = ctx.identity_hash_code(matcher);
            let text_identity = ctx.identity_hash_code(text);
            let guard = matcher_realjdk_cache().lock().ok()?;
            guard
                .get(&matcher_identity)
                .filter(|entry| entry.text_identity == text_identity)
                .cloned()
        }
    }?;

    let text = matcher_realjdk_group_slice(&entry.utf8, &entry.utf16_to_byte, start, end)?;
    Some(ctx.create_string_uninterned(text))
}

/// Build both offset tables in one O(n) pass over `s`.
fn matcher_realjdk_build_offset_tables(s: &str) -> (Vec<u32>, Vec<u32>) {
    let mut byte_to_utf16 = vec![0u32; s.len() + 1];
    let mut utf16_to_byte = Vec::with_capacity(s.len() + 1);
    let mut utf16_pos: u32 = 0;
    for (byte_idx, ch) in s.char_indices() {
        byte_to_utf16[byte_idx] = utf16_pos;
        let units = ch.len_utf16();
        for _ in 0..units {
            utf16_to_byte.push(byte_idx as u32);
        }
        utf16_pos += units as u32;
    }
    byte_to_utf16[s.len()] = utf16_pos;
    utf16_to_byte.push(s.len() as u32);
    (byte_to_utf16, utf16_to_byte)
}

/// Fetch (rebuilding + caching if needed) the decoded text, offset tables,
/// and compiled regex for `matcher`'s current `text`/`parentPattern`
/// fields. Returns `None` if `text` is null or not a `java/lang/String`
/// (caller falls back to real bytecode for any other `CharSequence` — see
/// module banner), or if `parentPattern`/its `pattern`/`flags` fields can't
/// be read.
///
/// The `text`-is-a-`String` class check only runs on a cache MISS — once a
/// given `text` identity has been confirmed a `String`, later calls skip
/// straight to the identity-hash comparison, which is what a class-manager
/// `RwLock` read on every single call would otherwise force.
fn matcher_realjdk_cached(
    ctx: &mut dyn NativeContext,
    matcher: ObjectRef,
    idx: MatcherFieldIndices,
    pattern_idx: PatternFieldIndices,
) -> Option<std::sync::Arc<MatcherRealCache>> {
    let text_obj = match ctx.get_field(matcher, idx.text) {
        Value::Object(Some(r)) => r,
        _ => return None,
    };
    let pattern_obj = match ctx.get_field(matcher, idx.parent_pattern) {
        Value::Object(Some(p)) => p,
        _ => return None,
    };
    let matcher_raw = matcher.as_ptr() as usize;
    let text_raw = text_obj.as_ptr() as usize;
    let pattern_raw = pattern_obj.as_ptr() as usize;
    let last = MATCHER_REAL_LAST_CACHE.with(|cache| {
        cache
            .borrow()
            .as_ref()
            .filter(|(cached_matcher, cached_text, cached_pattern, _)| {
                *cached_matcher == matcher_raw
                    && *cached_text == text_raw
                    && *cached_pattern == pattern_raw
            })
            .map(|(_, _, _, entry)| entry.clone())
    });
    if last.is_some() {
        return last;
    }

    // Raw references change when a moving collector forwards any member of
    // the triple, producing a safe TLS miss. Stable identity hashes retain the
    // cross-GC/global lookup semantics on that cold refresh path.
    let text_identity = ctx.identity_hash_code(text_obj);
    let pattern_identity = ctx.identity_hash_code(pattern_obj);
    let matcher_identity = ctx.identity_hash_code(matcher);

    if let Ok(guard) = matcher_realjdk_cache().lock() {
        if let Some(entry) = guard.get(&matcher_identity) {
            if entry.text_identity == text_identity && entry.pattern_identity == pattern_identity {
                let entry = entry.clone();
                MATCHER_REAL_LAST_CACHE.with(|cache| {
                    cache
                        .borrow_mut()
                        .replace((matcher_raw, text_raw, pattern_raw, entry.clone()));
                });
                return Some(entry);
            }
        }
    }

    // Cache miss: this is the only path that pays a class-manager lookup
    // (confirming `text` is a real `String`, not some other `CharSequence`)
    // and a fresh regex compile (itself cached by `compile_java_regex`, so
    // even a cross-matcher-instance repeat of the same pattern text is
    // cheap — just not as cheap as this cache's own zero-lock-contention
    // clone-out on a hit).
    let text_class = ctx.class_name_of_id(ctx.class_id_of_object(text_obj));
    if text_class.as_deref() != Some("java/lang/String") {
        return None;
    }
    let decoded = ctx.read_string(text_obj)?;
    let pattern_str_obj = match ctx.get_field(pattern_obj, pattern_idx.pattern) {
        Value::Object(Some(r)) => r,
        _ => return None,
    };
    let pattern_text = ctx.read_string(pattern_str_obj)?;
    let flags = ctx
        .get_field(pattern_obj, pattern_idx.flags)
        .as_int()
        .unwrap_or(0);
    let re = compile_java_regex(&pattern_text, flags).ok()?;
    let capture_count = ctx
        .get_field(pattern_obj, pattern_idx.capturing_group_count)
        .as_int()
        .and_then(|count| (count > 0).then_some(count as usize))?;
    if re.captures_len() != capture_count {
        return None;
    }

    let (byte_to_utf16, utf16_to_byte) = matcher_realjdk_build_offset_tables(&decoded);
    let utf8: std::sync::Arc<str> = std::sync::Arc::from(decoded.into_boxed_str());
    let byte_to_utf16: std::sync::Arc<[u32]> =
        std::sync::Arc::from(byte_to_utf16.into_boxed_slice());
    let utf16_to_byte: std::sync::Arc<[u32]> =
        std::sync::Arc::from(utf16_to_byte.into_boxed_slice());

    let entry = std::sync::Arc::new(MatcherRealCache {
        text_identity,
        pattern_identity,
        capture_count,
        utf8,
        byte_to_utf16,
        utf16_to_byte,
        re,
    });
    if let Ok(mut guard) = matcher_realjdk_cache().lock() {
        guard.insert(matcher_identity, entry.clone());
    }
    MATCHER_REAL_LAST_CACHE.with(|cache| {
        cache
            .borrow_mut()
            .replace((matcher_raw, text_raw, pattern_raw, entry.clone()));
    });
    Some(entry)
}

/// Decline this fast path for the current call: re-enter the SAME (class,
/// method, descriptor) triple with the native-override check skipped, so it
/// runs the real JDK `Matcher` bytecode body. See module banner.
fn matcher_realjdk_bail(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    method_name: &str,
    descriptor: &str,
    extra_args: &[Value],
) -> MethodCallResult {
    ctx.invoke_virtual_bytecode_only(this, method_name, descriptor, extra_args)
}

/// Shared core of `find()`/`find(int)`: given the search should resume at
/// `next_search_utf16` (already clamped/advanced per the caller's own
/// method-specific rule), run the search, write every field real
/// `Matcher.search(int)` bytecode would write, and return whether it
/// matched.
///
/// `groups_obj` MUST already be sized `capturingGroupCount * 2` by the real
/// `Matcher` constructor (true for any real-JDK-allocated Matcher — real
/// bytecode always runs the constructor since this native only ever
/// intercepts `find`/`find(int)`, never `<init>`).
#[allow(clippy::too_many_arguments)]
fn matcher_realjdk_search(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    idx: MatcherFieldIndices,
    re: &JavaRegex,
    utf8: &str,
    byte_to_utf16: &[u32],
    utf16_to_byte: &[u32],
    groups_obj: ObjectRef,
    region_from_utf16: i32,
    region_to_utf16: i32,
    next_search_utf16: i32,
    previous_last_utf16: i32,
    previous_mod_count: i32,
) -> MethodCallResult {
    let region_from_byte = utf16_to_byte[region_from_utf16.max(0) as usize] as usize;
    let region_to_byte = utf16_to_byte[region_to_utf16.max(0) as usize] as usize;
    let search_from_byte =
        utf16_to_byte[next_search_utf16.max(0) as usize] as usize - region_from_byte;
    let region_slice = &utf8[region_from_byte..region_to_byte];

    // Group-count safety net (checked by both callers via
    // `matcher_realjdk_capture_layout_ok` BEFORE any field is mutated — doing
    // it here instead would mean bailing to real bytecode after already
    // having overwritten `first`/`oldLast` above, corrupting the state the
    // bytecode fallback itself depends on). Trust the caller: by the time
    // we're here, Rust's capture count matches Pattern.capturingGroupCount and
    // `groups_obj` has enough capacity, so every write below is in bounds.
    let mut whole_start = -1i32;
    let mut whole_end = -1i32;
    let capture_count = re.visit_capture_ranges_at(region_slice, search_from_byte, |i, range| {
        let (s, e) = match range {
            Some((start, end)) => {
                let abs_start_byte = region_from_byte + start;
                let abs_end_byte = region_from_byte + end;
                (
                    byte_to_utf16[abs_start_byte] as i32,
                    byte_to_utf16[abs_end_byte] as i32,
                )
            }
            None => (-1, -1),
        };
        if i == 0 {
            whole_start = s;
            whole_end = e;
        }
        ctx.set_array_element(groups_obj, 2 * i, Value::Int(s));
        ctx.set_array_element(groups_obj, 2 * i + 1, Value::Int(e));
    });
    let matched = match capture_count {
        Some(_) => {
            // `this.first`/`this.last` are the WHOLE MATCH's actual bounds
            // (== groups[0]/groups[1]), NOT the position the search resumed
            // from — real `Pattern$Start.match`'s own scan loop overwrites
            // `matcher.first` as it tries each candidate position, so by the
            // time of a successful match `first` reflects where the match
            // itself begins, which for an unanchored pattern is commonly
            // LATER than the resume position search started scanning at
            // (e.g. `(a+)(b)` found starting at index 2 while the scan began
            // at index 0).
            ctx.set_field(this, idx.first, Value::Int(whole_start));
            ctx.set_field(this, idx.last, Value::Int(whole_end));
            // `hitEnd` approximation (see module banner): real HotSpot's
            // value depends on which internal `Pattern$Node` subclass the
            // compiler chose for this exact pattern (e.g. a greedy
            // quantifier's expansion reaching the region boundary vs. a
            // literal/Boyer-Moore node concluding definitively) — verified
            // against a 141-case parity battery that a plain "match end ==
            // region end" boundary check is neither uniformly right nor
            // uniformly wrong (JDK itself returns both `true` and `false`
            // for different patterns whose match happens to end exactly at
            // the region boundary), so no cheap boundary-only heuristic can
            // close this without reimplementing HotSpot's backtracking
            // engine. Kept as the closest-available signal; core `find()`/
            // `group()`/`start()`/`end()` results are unaffected and fully
            // parity-verified.
            ctx.set_field(
                this,
                idx.hit_end,
                Value::Int(if whole_end == region_to_utf16 { 1 } else { 0 }),
            );
            ctx.set_field(this, idx.require_end, Value::Int(0));
            true
        }
        None => {
            for i in 0..(ctx.array_length(groups_obj) / 2) {
                ctx.set_array_element(groups_obj, 2 * i, Value::Int(-1));
                ctx.set_array_element(groups_obj, 2 * i + 1, Value::Int(-1));
            }
            ctx.set_field(this, idx.first, Value::Int(-1));
            // Best-effort: a failed search plausibly means the engine
            // examined input through the region end. See module banner.
            ctx.set_field(this, idx.hit_end, Value::Int(1));
            false
        }
    };
    // Rust's regex engine cannot call back into Java while a search is in
    // progress, so the real bytecode's provisional `first`/`oldLast` writes
    // are unobservable. Commit only the final state, using the caller's
    // already-read previous `last` on failure instead of reading it again.
    let completed_last = if matched {
        whole_end
    } else {
        previous_last_utf16
    };
    ctx.set_field(this, idx.old_last, Value::Int(completed_last));
    let completed_first = if matched { whole_start } else { -1 };
    let completed_mod_count = previous_mod_count.wrapping_add(1);
    ctx.set_field(this, idx.mod_count, Value::Int(completed_mod_count));
    MATCHER_REAL_LAST_STATE.with(|state| {
        state.set(Some(MatcherRealState {
            matcher_raw: this.as_ptr() as usize,
            groups_raw: groups_obj.as_ptr() as usize,
            mod_count: completed_mod_count,
            first: completed_first,
            last: completed_last,
            from: region_from_utf16,
            to: region_to_utf16,
        }));
    });

    Ok(Some(Value::Int(if matched { 1 } else { 0 })))
}

/// Resolve the default-on real-layout Matcher intrinsics for exact-receiver
/// JIT dispatch. The VM applies the same feature gate as registration before
/// consulting this table.
pub fn matcher_realjdk_native_callback(
    method_name: &str,
    descriptor: &str,
) -> Option<cratonvm_native_api::NativeCallback> {
    match (method_name, descriptor) {
        ("find", "()Z") => Some(native_matcher_find_realjdk),
        ("find", "(I)Z") => Some(native_matcher_find_at_realjdk),
        ("start", "()I") => Some(native_matcher_start_realjdk),
        ("start", "(I)I") => Some(native_matcher_start_idx_realjdk),
        ("end", "()I") => Some(native_matcher_end_realjdk),
        ("end", "(I)I") => Some(native_matcher_end_idx_realjdk),
        ("group", "()Ljava/lang/String;") => Some(native_matcher_group_realjdk),
        ("group", "(I)Ljava/lang/String;") => Some(native_matcher_group_idx_realjdk),
        _ => None,
    }
}

/// Whether a cached native target is one of the real-layout Matcher leaves.
/// Used by the interpreter after its monomorphic receiver-class guard has
/// already succeeded.
#[inline]
pub fn is_matcher_realjdk_native_callback(callback: cratonvm_native_api::NativeCallback) -> bool {
    let callback = callback as usize;
    [
        native_matcher_find_realjdk as cratonvm_native_api::NativeCallback,
        native_matcher_find_at_realjdk,
        native_matcher_start_realjdk,
        native_matcher_start_idx_realjdk,
        native_matcher_end_realjdk,
        native_matcher_end_idx_realjdk,
        native_matcher_group_realjdk,
        native_matcher_group_idx_realjdk,
    ]
    .into_iter()
    .any(|candidate| candidate as usize == callback)
}

/// Real-JDK-layout `Matcher.find()Z`. See module banner for the full
/// contract; mirrors real bytecode's own two-step algorithm exactly
/// (`find()` computes the resume position from `first`/`last`/`from`/`to`,
/// then what would be a `search(int)` call) so a zero-width match's
/// next-position advance-by-one-UTF16-unit quirk — including its
/// mid-surrogate-pair edge case — matches HotSpot bit-for-bit, rather than
/// reimplementing a "nicer" char-boundary-safe advance.
pub(crate) fn native_matcher_find_realjdk(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        // No valid receiver at all — cannot even attempt a bytecode
        // fallback (no object to dispatch on). Should be unreachable for a
        // real instance-method call; mirror the legacy bridge's convention
        // for this impossible case.
        _ => return Ok(Some(Value::Int(0))),
    };
    let idx = match matcher_realjdk_field_indices(ctx) {
        Some(idx) => idx,
        None => return matcher_realjdk_bail(ctx, this, "find", "()Z", &[]),
    };

    if ctx
        .get_field(this, idx.transparent_bounds)
        .as_int()
        .unwrap_or(0)
        != 0
        || ctx
            .get_field(this, idx.anchoring_bounds)
            .as_int()
            .unwrap_or(1)
            == 0
    {
        return matcher_realjdk_bail(ctx, this, "find", "()Z", &[]);
    }

    let pattern_idx = match pattern_realjdk_field_indices(ctx) {
        Some(p) => p,
        None => return matcher_realjdk_bail(ctx, this, "find", "()Z", &[]),
    };
    let groups_obj = match ctx.get_field(this, idx.groups) {
        Value::Object(Some(g)) => g,
        _ => return matcher_realjdk_bail(ctx, this, "find", "()Z", &[]),
    };
    let mod_count = ctx.get_field(this, idx.mod_count).as_int().unwrap_or(0);
    let matcher_raw = this.as_ptr() as usize;
    let groups_raw = groups_obj.as_ptr() as usize;
    let state = MATCHER_REAL_LAST_STATE.with(|state| {
        state.get().filter(|state| {
            state.matcher_raw == matcher_raw
                && state.groups_raw == groups_raw
                && state.mod_count == mod_count
        })
    });
    let steady_cached = state.and_then(|_| {
        MATCHER_REAL_LAST_CACHE.with(|cache| {
            cache
                .borrow()
                .as_ref()
                .filter(|(cached_matcher, _, _, _)| *cached_matcher == matcher_raw)
                .map(|(_, _, _, entry)| entry.clone())
        })
    });
    let cached = match steady_cached.or_else(|| matcher_realjdk_cached(ctx, this, idx, pattern_idx))
    {
        Some(cached) => cached,
        None => return matcher_realjdk_bail(ctx, this, "find", "()Z", &[]),
    };
    // Group-count safety net, checked BEFORE any field mutation below. The
    // Pattern field is the semantic count; groups[] may be overallocated.
    if !matcher_realjdk_capture_layout_ok(ctx, cached.capture_count, groups_obj) {
        return matcher_realjdk_bail(ctx, this, "find", "()Z", &[]);
    }

    let (from, to, first, last) = state
        .map(|state| (state.from, state.to, state.first, state.last))
        .unwrap_or_else(|| {
            (
                ctx.get_field(this, idx.from).as_int().unwrap_or(0),
                ctx.get_field(this, idx.to)
                    .as_int()
                    .unwrap_or(cached.byte_to_utf16[cached.utf8.len()] as i32),
                ctx.get_field(this, idx.first).as_int().unwrap_or(-1),
                ctx.get_field(this, idx.last).as_int().unwrap_or(0),
            )
        });

    let mut next_search = last;
    if next_search == first {
        next_search += 1;
    }
    if next_search < from {
        next_search = from;
    }
    if next_search > to {
        // Real `find()`'s own early-return branch: clear `groups[]` only,
        // leave `first`/`last`/`modCount`/`hitEnd` untouched — replicated
        // exactly (not a bug we're "fixing").
        for i in 0..(ctx.array_length(groups_obj) / 2) {
            ctx.set_array_element(groups_obj, 2 * i, Value::Int(-1));
            ctx.set_array_element(groups_obj, 2 * i + 1, Value::Int(-1));
        }
        return Ok(Some(Value::Int(0)));
    }

    matcher_realjdk_search(
        ctx,
        this,
        idx,
        &cached.re,
        &cached.utf8,
        &cached.byte_to_utf16,
        &cached.utf16_to_byte,
        groups_obj,
        from,
        to,
        next_search,
        last,
        mod_count,
    )
}

/// Real-JDK-layout `Matcher.find(I)Z`. Real bytecode's `find(int start)`
/// calls `reset()` first (discards region/first/last/append-position, resets
/// `from=0, to=length`) and THEN searches from `start` — the reset field
/// writes below replicate that inline before running the shared search core.
pub(crate) fn native_matcher_find_at_realjdk(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        // No valid receiver at all — see the analogous branch in
        // `native_matcher_find_realjdk`.
        _ => return Ok(Some(Value::Int(0))),
    };
    let start = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let idx = match matcher_realjdk_field_indices(ctx) {
        Some(idx) => idx,
        None => return matcher_realjdk_bail(ctx, this, "find", "(I)Z", &[Value::Int(start)]),
    };

    if ctx
        .get_field(this, idx.transparent_bounds)
        .as_int()
        .unwrap_or(0)
        != 0
        || ctx
            .get_field(this, idx.anchoring_bounds)
            .as_int()
            .unwrap_or(1)
            == 0
    {
        return matcher_realjdk_bail(ctx, this, "find", "(I)Z", &[Value::Int(start)]);
    }

    let pattern_idx = match pattern_realjdk_field_indices(ctx) {
        Some(p) => p,
        None => return matcher_realjdk_bail(ctx, this, "find", "(I)Z", &[Value::Int(start)]),
    };
    let cached = match matcher_realjdk_cached(ctx, this, idx, pattern_idx) {
        Some(cached) => cached,
        None => return matcher_realjdk_bail(ctx, this, "find", "(I)Z", &[Value::Int(start)]),
    };
    let text_len_utf16 = cached.byte_to_utf16[cached.utf8.len()] as i32;

    // Out-of-range `start`: let the real bytecode throw the exact
    // `IndexOutOfBoundsException` Java specifies (no matching RuntimeError
    // variant exists for this generic exception here, and this is a rare,
    // non-hot-loop-shaped call), rather than fabricating one.
    if start < 0 || start > text_len_utf16 {
        return matcher_realjdk_bail(ctx, this, "find", "(I)Z", &[Value::Int(start)]);
    }

    let groups_obj = match ctx.get_field(this, idx.groups) {
        Value::Object(Some(g)) => g,
        _ => return matcher_realjdk_bail(ctx, this, "find", "(I)Z", &[Value::Int(start)]),
    };
    // Group-count safety net — checked before any field mutation below. The
    // Pattern field is the semantic count; groups[] may be overallocated.
    if !matcher_realjdk_capture_layout_ok(ctx, cached.capture_count, groups_obj) {
        return matcher_realjdk_bail(ctx, this, "find", "(I)Z", &[Value::Int(start)]);
    }

    // Real bytecode `reset()`: first=-1, last=0, oldLast=-1, groups/locals
    // cleared, lastAppendPosition=0, from=0, to=length, modCount++. We only
    // need the subset later code reads: first/last/oldLast/from/to (groups[]
    // gets fully overwritten by the search below regardless of outcome).
    ctx.set_field(this, idx.first, Value::Int(-1));
    ctx.set_field(this, idx.last, Value::Int(0));
    ctx.set_field(this, idx.old_last, Value::Int(-1));
    ctx.set_field(this, idx.from, Value::Int(0));
    ctx.set_field(this, idx.to, Value::Int(text_len_utf16));
    let mod_count = ctx.get_field(this, idx.mod_count).as_int().unwrap_or(0);
    let reset_mod_count = mod_count.wrapping_add(1);
    ctx.set_field(this, idx.mod_count, Value::Int(reset_mod_count));

    matcher_realjdk_search(
        ctx,
        this,
        idx,
        &cached.re,
        &cached.utf8,
        &cached.byte_to_utf16,
        &cached.utf16_to_byte,
        groups_obj,
        0,
        text_len_utf16,
        start,
        0,
        reset_mod_count,
    )
}

// --- `start()`/`end()`/`start(int)`/`end(int)`/`group()`/`group(int)` ---
//
// A typical `while (m.find()) { ...; m.group(N); }` loop calls one or more
// of these on every iteration — real bytecode dispatch for them, left
// unaccelerated, was measured as a substantial share of the remaining
// per-iteration cost even after `find()` itself became fast. All six read
// ONLY the `first`/`last`/`groups[]` state `find()`/`find(int)` already
// populate correctly (see above) — no regex re-run, no text re-decode.
// `group()`/`group(int)` delegate the actual character extraction to the
// receiver `text` object's own (already-fast, see
// `fixed-suite-bugs/substring-large-parent-quadratic-allocation-FIXED.md`)
// `String.substring(int,int)` via `invoke_virtual` rather than re-deriving a
// UTF-8 slice from this fast path's own cached tables — avoids a redundant
// cache lookup and reuses the exact substring Java itself would produce.

/// `checkGroup(group)`: real JDK throws the GENERIC `java.lang.IndexOutOfBoundsException`
/// (not a subclass CratonVM has a dedicated `RuntimeError` variant for) on an
/// invalid index — the caller bails to real bytecode for that rare case so
/// it throws the exact right exception, rather than fabricating one here.
/// Returns `true` when `group` is in bounds (`0..=groupCount()`). Pattern's
/// semantic capture count is authoritative because groups[] may be larger.
fn matcher_realjdk_group_index_in_bounds(
    group: i32,
    capture_count: usize,
    groups_len: usize,
) -> bool {
    if group < 0 {
        return false;
    }
    let group = group as usize;
    group < capture_count && group.saturating_mul(2).saturating_add(1) < groups_len
}

fn matcher_realjdk_group_in_bounds(
    ctx: &mut dyn NativeContext,
    matcher: ObjectRef,
    idx: MatcherFieldIndices,
    groups_obj: ObjectRef,
    group: i32,
) -> bool {
    let Some(pattern_idx) = pattern_realjdk_field_indices(ctx) else {
        return false;
    };
    let pattern = match ctx.get_field(matcher, idx.parent_pattern) {
        Value::Object(Some(pattern)) => pattern,
        _ => return false,
    };
    let matcher_raw = matcher.as_ptr() as usize;
    let pattern_raw = pattern.as_ptr() as usize;
    let cached_count = MATCHER_REAL_LAST_CACHE.with(|cache| {
        cache
            .borrow()
            .as_ref()
            .filter(|(cached_matcher, _, cached_pattern, _)| {
                *cached_matcher == matcher_raw && *cached_pattern == pattern_raw
            })
            .map(|(_, _, _, entry)| entry.capture_count)
    });
    let capture_count = cached_count.or_else(|| {
        ctx.get_field(pattern, pattern_idx.capturing_group_count)
            .as_int()
            .and_then(|count| (count > 0).then_some(count as usize))
    });
    let Some(capture_count) = capture_count else {
        return false;
    };
    matcher_realjdk_group_index_in_bounds(group, capture_count, ctx.array_length(groups_obj))
}

#[cfg(test)]
mod matcher_realjdk_layout_tests {
    use super::{
        compile_java_regex, is_matcher_realjdk_native_callback,
        matcher_realjdk_build_offset_tables, matcher_realjdk_capture_layout_valid,
        matcher_realjdk_group_index_in_bounds, matcher_realjdk_group_slice,
        matcher_realjdk_native_callback,
    };
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    #[test]
    fn cached_group_slice_preserves_utf16_boundaries() {
        let text = "x😀value12,y";
        let (_, utf16_to_byte) = matcher_realjdk_build_offset_tables(text);
        assert_eq!(
            matcher_realjdk_group_slice(text, &utf16_to_byte, 3, 10),
            Some("value12")
        );
        assert_eq!(
            matcher_realjdk_group_slice(text, &utf16_to_byte, 2, 10),
            None
        );
        assert_eq!(
            matcher_realjdk_group_slice(text, &utf16_to_byte, 1, 2),
            None
        );
    }

    #[test]
    fn jit_dispatch_resolver_covers_only_real_layout_matcher_intrinsics() {
        for (name, descriptor) in [
            ("find", "()Z"),
            ("find", "(I)Z"),
            ("start", "()I"),
            ("start", "(I)I"),
            ("end", "()I"),
            ("end", "(I)I"),
            ("group", "()Ljava/lang/String;"),
            ("group", "(I)Ljava/lang/String;"),
        ] {
            let callback = matcher_realjdk_native_callback(name, descriptor).unwrap();
            assert!(is_matcher_realjdk_native_callback(callback));
        }
        assert!(matcher_realjdk_native_callback("matches", "()Z").is_none());
        assert!(
            matcher_realjdk_native_callback("group", "(Ljava/lang/String;)Ljava/lang/String;")
                .is_none()
        );
    }

    #[test]
    fn capture_range_visitor_reports_std_and_fancy_offsets() {
        let std = compile_java_regex(r"value(\d+),", 0).unwrap();
        let mut std_ranges = Vec::new();
        assert_eq!(
            std.visit_capture_ranges_at("xvalue12,y", 1, |i, range| {
                assert_eq!(i, std_ranges.len());
                std_ranges.push(range);
            }),
            Some(2)
        );
        assert_eq!(std_ranges, vec![Some((1, 9)), Some((6, 8))]);

        let fancy = compile_java_regex(r"(?=(value(\d+),))", 0).unwrap();
        let mut fancy_ranges = Vec::new();
        assert_eq!(
            fancy.visit_capture_ranges_at("xvalue12,y", 1, |i, range| {
                assert_eq!(i, fancy_ranges.len());
                fancy_ranges.push(range);
            }),
            Some(3)
        );
        assert_eq!(fancy_ranges, vec![Some((1, 1)), Some((1, 9)), Some((6, 8))]);
    }

    #[test]
    fn overallocated_groups_array_is_capacity_not_capture_count() {
        assert!(matcher_realjdk_capture_layout_valid(2, 2, 20));
        assert!(matcher_realjdk_group_index_in_bounds(1, 2, 20));
        assert!(!matcher_realjdk_group_index_in_bounds(2, 2, 20));
    }

    #[test]
    fn layout_rejects_semantic_count_mismatch_or_short_capacity() {
        assert!(!matcher_realjdk_capture_layout_valid(3, 2, 20));
        assert!(!matcher_realjdk_capture_layout_valid(2, 3, 20));
        assert!(!matcher_realjdk_capture_layout_valid(2, 2, 3));
        assert!(!matcher_realjdk_capture_layout_valid(0, 0, 0));
        assert!(!matcher_realjdk_group_index_in_bounds(-1, 2, 20));
        assert!(!matcher_realjdk_group_index_in_bounds(1, 2, 3));
    }
}

pub(crate) fn native_matcher_start_realjdk(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let idx = match matcher_realjdk_field_indices(ctx) {
        Some(idx) => idx,
        None => return matcher_realjdk_bail(ctx, this, "start", "()I", &[]),
    };
    let first = ctx.get_field(this, idx.first).as_int().unwrap_or(-1);
    if first < 0 {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "No match found".to_string(),
        }
        .into());
    }
    Ok(Some(Value::Int(first)))
}

pub(crate) fn native_matcher_end_realjdk(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let idx = match matcher_realjdk_field_indices(ctx) {
        Some(idx) => idx,
        None => return matcher_realjdk_bail(ctx, this, "end", "()I", &[]),
    };
    // `end()`'s real bytecode checks `hasMatch()` (== `first >= 0`), same as
    // `start()` — `last` alone isn't a valid "no match" sentinel (it can be
    // 0 on a fresh Matcher that never matched).
    let first = ctx.get_field(this, idx.first).as_int().unwrap_or(-1);
    if first < 0 {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "No match found".to_string(),
        }
        .into());
    }
    let last = ctx.get_field(this, idx.last).as_int().unwrap_or(-1);
    Ok(Some(Value::Int(last)))
}

pub(crate) fn native_matcher_start_idx_realjdk(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let group = match args.get(1) {
        Some(Value::Int(g)) => *g,
        _ => return matcher_realjdk_bail(ctx, this, "start", "(I)I", &[Value::Int(0)]),
    };
    let idx = match matcher_realjdk_field_indices(ctx) {
        Some(idx) => idx,
        None => return matcher_realjdk_bail(ctx, this, "start", "(I)I", &[Value::Int(group)]),
    };
    let first = ctx.get_field(this, idx.first).as_int().unwrap_or(-1);
    if first < 0 {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "No match found".to_string(),
        }
        .into());
    }
    let groups_obj = match ctx.get_field(this, idx.groups) {
        Value::Object(Some(g)) => g,
        _ => return matcher_realjdk_bail(ctx, this, "start", "(I)I", &[Value::Int(group)]),
    };
    if !matcher_realjdk_group_in_bounds(ctx, this, idx, groups_obj, group) {
        return matcher_realjdk_bail(ctx, this, "start", "(I)I", &[Value::Int(group)]);
    }
    Ok(Some(ctx.get_array_element(groups_obj, 2 * group as usize)))
}

pub(crate) fn native_matcher_end_idx_realjdk(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let group = match args.get(1) {
        Some(Value::Int(g)) => *g,
        _ => return matcher_realjdk_bail(ctx, this, "end", "(I)I", &[Value::Int(0)]),
    };
    let idx = match matcher_realjdk_field_indices(ctx) {
        Some(idx) => idx,
        None => return matcher_realjdk_bail(ctx, this, "end", "(I)I", &[Value::Int(group)]),
    };
    let first = ctx.get_field(this, idx.first).as_int().unwrap_or(-1);
    if first < 0 {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "No match found".to_string(),
        }
        .into());
    }
    let groups_obj = match ctx.get_field(this, idx.groups) {
        Value::Object(Some(g)) => g,
        _ => return matcher_realjdk_bail(ctx, this, "end", "(I)I", &[Value::Int(group)]),
    };
    if !matcher_realjdk_group_in_bounds(ctx, this, idx, groups_obj, group) {
        return matcher_realjdk_bail(ctx, this, "end", "(I)I", &[Value::Int(group)]);
    }
    Ok(Some(
        ctx.get_array_element(groups_obj, 2 * group as usize + 1),
    ))
}

pub(crate) fn native_matcher_group_realjdk(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Real bytecode `group()` is itself just `return group(0);`, so
    // delegating here — including any bail-to-bytecode this triggers using
    // the `group(I)` descriptor — produces byte-identical behavior to
    // calling real `group()` directly.
    native_matcher_group_idx_realjdk(ctx, &[Value::Object(Some(this)), Value::Int(0)])
}

pub(crate) fn native_matcher_group_idx_realjdk(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let group = match args.get(1) {
        Some(Value::Int(g)) => *g,
        _ => {
            return matcher_realjdk_bail(
                ctx,
                this,
                "group",
                "(I)Ljava/lang/String;",
                &[Value::Int(0)],
            )
        }
    };
    let idx = match matcher_realjdk_field_indices(ctx) {
        Some(idx) => idx,
        None => {
            return matcher_realjdk_bail(
                ctx,
                this,
                "group",
                "(I)Ljava/lang/String;",
                &[Value::Int(group)],
            )
        }
    };
    let first = ctx.get_field(this, idx.first).as_int().unwrap_or(-1);
    if first < 0 {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "No match found".to_string(),
        }
        .into());
    }
    let groups_obj = match ctx.get_field(this, idx.groups) {
        Value::Object(Some(g)) => g,
        _ => {
            return matcher_realjdk_bail(
                ctx,
                this,
                "group",
                "(I)Ljava/lang/String;",
                &[Value::Int(group)],
            )
        }
    };
    if !matcher_realjdk_group_in_bounds(ctx, this, idx, groups_obj, group) {
        return matcher_realjdk_bail(
            ctx,
            this,
            "group",
            "(I)Ljava/lang/String;",
            &[Value::Int(group)],
        );
    }
    let start = ctx
        .get_array_element(groups_obj, 2 * group as usize)
        .as_int()
        .unwrap_or(-1);
    let end = ctx
        .get_array_element(groups_obj, 2 * group as usize + 1)
        .as_int()
        .unwrap_or(-1);
    if start == -1 || end == -1 {
        return Ok(Some(Value::Object(None)));
    }
    let text_obj = match ctx.get_field(this, idx.text) {
        Value::Object(Some(t)) => t,
        // Non-`String` `CharSequence` text: this fast path never runs
        // `find()`/`find(int)` for that case (bails immediately — see
        // `matcher_realjdk_cached`), so `groups[]` would still be all `-1`
        // and this branch is unreachable in practice; kept as a defensive
        // bail rather than an assumption.
        _ => {
            return matcher_realjdk_bail(
                ctx,
                this,
                "group",
                "(I)Ljava/lang/String;",
                &[Value::Int(group)],
            )
        }
    };
    if let Some(group_string) = matcher_realjdk_cached_group_string(ctx, this, text_obj, start, end)
    {
        return Ok(Some(Value::Object(Some(group_string))));
    }
    // The comment above ("groups[] would still be all -1") only holds for
    // callers that reach `groups[]` via THIS fast path's own `find()`/
    // `find(int)` (which does bail early for non-String text — see
    // `matcher_realjdk_cached`). `Matcher.matches()`/`lookingAt()` are NOT
    // natively intercepted at all, so they run as real interpreted bytecode
    // against ANY `CharSequence` (correctly — that's all the `CharSequence`
    // contract promises) and populate `groups[]` just fine. A subsequent
    // `matcher.group(int)` call then DOES reach here with valid `start`/`end`
    // even though `text` is a non-String `CharSequence` (e.g. Spring's
    // `AntPathMatcher$AntPathStringMatcher$MaxAttemptsCharSequence`, which
    // implements only `subSequence`/`charAt`/`length`/`isEmpty` — no
    // `substring`). Calling `.substring(int,int)` unconditionally then threw
    // a spurious `NoSuchMethodError` instead of returning the matched text.
    // Real JDK's `Matcher.group(int)` never calls `.substring()` either — it
    // calls `getSubSequence(start,end).toString()`, `subSequence` being the
    // one method every `CharSequence` actually guarantees. Keep the fast,
    // allocation-light `substring` shortcut for genuine `String` text (the
    // overwhelming common case) and bail to real bytecode for anything else,
    // instead of assuming the receiver has a `substring` method it never
    // promised to have.
    let text_cid = ctx.class_id_of_object(text_obj);
    let text_cname = ctx.class_name_of_id(text_cid);
    if text_cname.as_deref() != Some("java/lang/String") {
        return matcher_realjdk_bail(
            ctx,
            this,
            "group",
            "(I)Ljava/lang/String;",
            &[Value::Int(group)],
        );
    }
    // Genuine String input: call the registered substring native directly,
    // avoiding another generic virtual dispatch on every captured group.
    lang_string::native_string_substring(
        ctx,
        &[
            Value::Object(Some(text_obj)),
            Value::Int(start),
            Value::Int(end),
        ],
    )
}

// ===========================================================================
// java.util.regex — variable-length look-behind regression tests
//
// `java.util.regex` accepts variable-length look-behind; the fast `regex`
// crate has no look-around at all, so such patterns fall through to the
// `fancy-regex` backend. `fancy-regex` 0.13 only supported *constant-size*
// look-behind and rejected anything else with "Look-behind assertion without
// constant size" — surfacing as a `PatternSyntaxException` that aborted
// Hazelcast 5.4.0 boot inside `AbstractXmlConfigHelper.schemaValidation`.
//
// These tests pin the actual Hazelcast pattern (`(?<!\G\S+)\s`, used by
// `String.split`) and assert the split output matches the reference JDK 25
// (`java.exe T`) byte-for-byte.
// ===========================================================================
#[cfg(test)]
mod regex_lookbehind_tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    /// The exact pattern Hazelcast's `AbstractXmlConfigHelper.schemaValidation`
    /// passes to `String.split` — a negative look-behind whose `\G\S+` body is
    /// variable length. Must compile (via the `fancy-regex` fallback) and split
    /// identically to JDK 25.
    #[test]
    fn hazelcast_variable_lookbehind_pattern_compiles_and_splits() {
        let pat = r"(?<!\G\S+)\s";
        let re = compile_java_regex(pat, 0)
            .expect("Hazelcast variable-length look-behind pattern must compile");
        // Must have fallen through to the fancy-regex backend (no look-around
        // in the `regex` crate).
        assert!(
            matches!(re, JavaRegex::Fancy(_)),
            "look-behind pattern should use the fancy-regex backend"
        );

        // Reference values produced by JDK 25 `String.split("(?<!\\G\\S+)\\s")`:
        //   "hello world\tfoo  bar" -> ["hello world", "foo ", "bar"]
        let got = re.split("hello world\tfoo  bar");
        assert_eq!(
            got,
            vec!["hello world", "foo ", "bar"],
            "split must match JDK 25 reference output"
        );

        //   "<root>\n  <child a=\"1\"/>\n</root>"
        //     -> ["<root>\n", "", "<child a=\"1\"/>", "</root>"]
        let got2 = re.split("<root>\n  <child a=\"1\"/>\n</root>");
        assert_eq!(
            got2,
            vec!["<root>\n", "", "<child a=\"1\"/>", "</root>"],
            "split must match JDK 25 reference output"
        );
    }

    /// A plain variable-length positive look-behind must also compile now.
    #[test]
    fn variable_length_positive_lookbehind_compiles() {
        let re = compile_java_regex(r"(?<=\d+)x", 0)
            .expect("variable-length positive look-behind must compile");
        // "abc123x" -> the `x` is preceded by digits, so it matches.
        assert!(re.is_match("abc123x"));
        assert!(!re.is_match("abcx"));
    }

    /// `pem_block_to_der`: a PEM CERTIFICATE block decodes to its DER body;
    /// raw DER (no armor) and non-base64 garbage pass through unchanged.
    /// Regression guard for http-server-sslengine-identity-singleton-clobber
    /// (Netty `SelfSignedCertificate` PEM → empty `getEncoded()`).
    #[test]
    fn pem_block_to_der_roundtrip() {
        let der = vec![0x30u8, 0x03, 0x02, 0x01, 0x2a]; // trivial DER SEQUENCE
        let b64 = String::from_utf8(b64_encode(&der, B64_VARIANT_BASIC, false)).unwrap();
        let pem = format!("-----BEGIN CERTIFICATE-----\n{b64}\n-----END CERTIFICATE-----\n");
        assert_eq!(pem_block_to_der(pem.as_bytes()), der, "PEM decodes to DER");
        // Raw DER (no armor) passes through untouched.
        assert_eq!(pem_block_to_der(&der), der, "raw DER unchanged");
        // A leading-garbage-then-armor PEM still finds the block.
        let noisy = format!("Bag Attributes\n{pem}");
        assert_eq!(
            pem_block_to_der(noisy.as_bytes()),
            der,
            "armor found past a preamble"
        );
        // Non-base64 body → hand original bytes back (caller's mirror fallback).
        let bad = b"-----BEGIN CERTIFICATE-----\n@@@@\n-----END CERTIFICATE-----\n";
        assert_eq!(
            pem_block_to_der(bad),
            bad.to_vec(),
            "invalid base64 falls back to input"
        );
    }
}
