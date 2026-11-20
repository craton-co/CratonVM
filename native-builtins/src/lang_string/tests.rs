use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::ArrayElementType;

// -----------------------------------------------------------------------
// Pure helper functions (no context needed)
// -----------------------------------------------------------------------

#[test]
fn format_double_positive() {
    assert_eq!(format_double(1.0), "1.0");
}

#[test]
fn format_double_infinity() {
    assert_eq!(format_double(f64::INFINITY), "Infinity");
}

#[test]
fn format_double_neg_infinity() {
    assert_eq!(format_double(f64::NEG_INFINITY), "-Infinity");
}

#[test]
fn format_double_nan() {
    assert_eq!(format_double(f64::NAN), "NaN");
}

#[test]
fn format_float_positive() {
    assert_eq!(format_float(1.0), "1.0");
}

#[test]
fn format_float_nan() {
    assert_eq!(format_float(f32::NAN), "NaN");
}

fn join_custom_charsequence_to_string(
    ctx: &mut MockNativeContext,
    receiver: cratonvm_types::ObjectRef,
    method_name: &str,
    descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if method_name == "toString" && descriptor == "()Ljava/lang/String;" {
        return Some(Ok(Some(ctx.get_field(receiver, 1))));
    }
    None
}

#[test]
fn string_join_array_uses_to_string_for_custom_charsequence() {
    let mut ctx = mock_ctx();
    let delimiter = ctx.create_string("|");
    let prefix = ctx.create_string("prefix");
    let custom = ctx.fresh_object_ref();
    let custom_text = ctx.create_string("custom");
    // Leave field 0 non-reference so the mock's String-layout reader
    // cannot mistake this custom object for a String.
    ctx.set_field(custom, 1, Value::Object(Some(custom_text)));
    let sequences = ctx.new_ref_array(cratonvm_types::ClassId::new(0), 3);
    ctx.set_array_element(sequences, 0, Value::Object(Some(prefix)));
    ctx.set_array_element(sequences, 1, Value::Object(Some(custom)));
    ctx.set_array_element(sequences, 2, Value::Object(None));
    ctx.set_invoke_virtual_hook(join_custom_charsequence_to_string);

    let result = native_string_join(
        &mut ctx,
        &[
            Value::Object(Some(delimiter)),
            Value::Object(Some(sequences)),
        ],
    )
    .unwrap();
    let Some(Value::Object(Some(joined))) = result else {
        panic!("String.join should return a String");
    };
    assert_eq!(
        ctx.read_string(joined).as_deref(),
        Some("prefix|custom|null")
    );
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "String.join must release native roots"
    );
}

// -----------------------------------------------------------------------
// bug nb-lang-string: UTF-16 code-UNIT indexing helpers
// -----------------------------------------------------------------------

#[test]
fn last_index_of_units_basic() {
    // "abcabc" — last "bc" at code-unit index 4.
    let h: Vec<u16> = "abcabc".encode_utf16().collect();
    let n: Vec<u16> = "bc".encode_utf16().collect();
    assert_eq!(last_index_of_units(&h, &n), 4);
    // absent needle → -1
    let z: Vec<u16> = "zz".encode_utf16().collect();
    assert_eq!(last_index_of_units(&h, &z), -1);
    // empty needle → haystack length (in code units)
    assert_eq!(last_index_of_units(&h, &[]), 6);
}

/// STR20 review: the builder's `lastIndexOf(String, int)` empty-needle rows,
/// derived from JDK `String.lastIndexOf(byte[], .., tgtStr, fromIndex)`
/// (`fromIndex = min(fromIndex, srcCount - tgtCount); if (fromIndex < 0)
/// return -1; if (tgtCount == 0) return fromIndex;`), derived, not measured.
#[test]
fn str20_u16_last_index_of_empty_needle_follows_the_jdk_clamp() {
    let h: Vec<u16> = "abcabc".encode_utf16().collect();
    assert_eq!(u16_last_index_of(&h, &[], 2), 2);
    assert_eq!(u16_last_index_of(&h, &[], 0), 0);
    assert_eq!(u16_last_index_of(&h, &[], 99), 6);
    assert_eq!(u16_last_index_of(&h, &[], 6), 6);
    assert_eq!(u16_last_index_of(&h, &[], -1), -1);
    assert_eq!(u16_last_index_of(&h, &[], i32::MIN), -1);
    // The empty haystack: `""`.lastIndexOf("", 0) is 0, `("", -1)` is -1.
    assert_eq!(u16_last_index_of(&[], &[], 0), 0);
    assert_eq!(u16_last_index_of(&[], &[], -1), -1);
    // Non-empty needles are unchanged: a negative `from` finds nothing.
    let bc: Vec<u16> = "bc".encode_utf16().collect();
    assert_eq!(u16_last_index_of(&h, &bc, 99), 4);
    assert_eq!(u16_last_index_of(&h, &bc, 3), 1);
    assert_eq!(u16_last_index_of(&h, &bc, -1), -1);
}

#[test]
fn last_index_of_units_supplementary() {
    // U+1F600 GRINNING FACE is a surrogate pair = 2 UTF-16 code units.
    // "\u{1F600}a\u{1F600}b": code-unit indices: [0,1]=face,2='a',
    // [3,4]=face,5='b'. lastIndexOf("a") (one unit) must be 2, and
    // lastIndexOf(face) must be 3 — code-unit indices, not code points.
    let s = "\u{1F600}a\u{1F600}b";
    let h: Vec<u16> = s.encode_utf16().collect();
    assert_eq!(h.len(), 6);
    let a: Vec<u16> = "a".encode_utf16().collect();
    assert_eq!(last_index_of_units(&h, &a), 2);
    let face: Vec<u16> = "\u{1F600}".encode_utf16().collect();
    assert_eq!(face.len(), 2);
    assert_eq!(last_index_of_units(&h, &face), 3);
}

fn u16s(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// nio21 w8 L5: `StringUTF16.compareToCIImpl` folds a surrogate PAIR as
/// one code point. `"𐐨".equalsIgnoreCase("𐐀")` (U+10428 / U+10400) is
/// `true` on HotSpot 25; the per-code-unit fold answered `false`.
#[test]
fn string_ci_supplementary_pairs_fold_by_code_point() {
    for (upper, lower) in [
        ("\u{10400}", "\u{10428}"), // Deseret
        ("\u{1E900}", "\u{1E922}"), // Adlam
        ("\u{10C80}", "\u{10CC0}"), // Old Hungarian
        ("\u{16E40}", "\u{16E60}"), // Medefaidrin
    ] {
        let (u, l) = (u16s(upper), u16s(lower));
        assert_eq!(utf16_compare_to_ci_impl(&u, 0, 2, &l, 0, 2), 0, "{upper}");
        assert_eq!(utf16_compare_to_ci_impl(&l, 0, 2, &u, 0, 2), 0, "{upper}");
        assert!(
            region_matches_ci_units(&u, false, 0, &l, false, 0, 2)
        );
        assert!(
            region_matches_ci_units(&l, false, 0, &u, false, 0, 2)
        );
        // The per-unit fold is what was wrong: the low halves differ.
        assert!(!code_unit_eq_ignore_case(u[1], l[1]));
    }
    // Difference of the folded code points: lower(upper(U+10428)) is
    // U+10428, lower(upper(U+10401)) is U+10429.
    assert_eq!(
        utf16_compare_to_ci_impl(&u16s("\u{10428}"), 0, 2, &u16s("\u{10401}"), 0, 2),
        -1
    );
    // Mixed with BMP text, and the length tail.
    let a = u16s("x\u{10400}y\u{0100}");
    let b = u16s("X\u{10428}Y\u{0101}");
    assert_eq!(utf16_compare_to_ci_impl(&a, 0, a.len(), &b, 0, b.len()), 0);
    assert_eq!(utf16_compare_to_ci_impl(&a, 0, 3, &b, 0, a.len()), -2);
}

/// Offsets that split a pair, and unpaired surrogates: the JDK pairs a
/// low surrogate BACKWARD only inside the region (`index > start`), and a
/// high one FORWARD only inside it (`index + 1 < end`).
/// nio21 w9 L5: the per-unit CI fold reads the JDK table over the whole
/// BMP (ASCII inline, the rest `character_case_map`), so it cannot drift
/// from `Character.toUpperCase(char)` / `toLowerCase(char)`.
#[test]
fn string_ci_unit_fold_is_the_jdk_table_over_the_whole_bmp() {
    let mut fold = CiUnitFold::new();
    for c in 0..=0xFFFFu32 {
        let ch = c as u16;
        assert_eq!(
            u32::from(fold.upper(ch)),
            crate::lang_math::character_case_map(c, true),
            "upper {c:#x}"
        );
        assert_eq!(
            u32::from(fold.lower(ch)),
            crate::lang_math::character_case_map(c, false),
            "lower {c:#x}"
        );
    }
}

/// Census, not a gate: where the Rust-derived pair the fold used before
/// wave 9 (`java_char_to_{upper,lower}_case`) differs from the JDK table.
/// The JDK table was checked against HotSpot 25.0.3+9 over every code
/// point, so each row printed here is a row the old per-unit fold got
/// wrong (and `CRATONVM_STRING_CI_JDK_CASE_TABLE=0` would bring back).
#[test]
fn string_ci_rust_derived_case_pair_census() {
    let mut diffs = Vec::new();
    for c in 0..=0xFFFFu32 {
        let ch = c as u16;
        let ju = crate::lang_math::character_case_map(c, true);
        let jl = crate::lang_math::character_case_map(c, false);
        if u32::from(java_char_to_upper_case(ch)) != ju
            || u32::from(java_char_to_lower_case(ch)) != jl
        {
            diffs.push(c);
        }
    }
    eprintln!(
        "[census] rust-derived vs JDK BMP case table: {} code units differ: {:x?}",
        diffs.len(),
        &diffs[..diffs.len().min(64)]
    );
}

/// W8L5-2: the coder is read from a `byte[]` String. A UTF16-coded "a"
/// against "𐐀" takes `compareToCIImpl` (the difference of folded CODE
/// POINTS, `0x61 - 0x10428`); the same text LATIN1-coded takes
/// `compareToCI_UTF16` (`0x61 - 0xD801`). Both are HotSpot's answers for
/// the respective coders.
#[test]
fn string_ci_reads_the_coder_of_a_byte_array_string() {
    fn byte_string(
        ctx: &mut MockNativeContext,
        bytes: &[u8],
        coder: i32,
    ) -> cratonvm_types::ObjectRef {
        let s = ctx.create_string("");
        let arr = ctx.new_array(ArrayElementType::Byte, bytes.len());
        for (i, &b) in bytes.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(i32::from(b as i8)));
        }
        ctx.set_field(s, 0, Value::Object(Some(arr)));
        ctx.set_field(s, 1, Value::Int(coder));
        s
    }
    let mut ctx = mock_ctx();
    let a_utf16 = byte_string(&mut ctx, &[0x61, 0x00], 1);
    let a_latin1 = byte_string(&mut ctx, &[0x61], 0);
    let deseret = byte_string(&mut ctx, &[0x01, 0xD8, 0x00, 0xDC], 1);
    assert!(
        !string_coder_is_latin1(&ctx, a_utf16, &[0x61])
    );
    assert!(string_coder_is_latin1(&ctx, a_latin1, &[0x61]));
    let cmp = |ctx: &mut MockNativeContext, x, y| match native_string_compare_to_ignore_case(
        ctx,
        &[Value::Object(Some(x)), Value::Object(Some(y))],
    )
    .unwrap()
    {
        Some(Value::Int(v)) => v,
        other => panic!("{other:?}"),
    };
    assert_eq!(cmp(&mut ctx, a_latin1, deseret), 0x61 - 0xD801);
    {
        assert_eq!(cmp(&mut ctx, a_utf16, deseret), 0x61 - 0x10428);
        assert_eq!(cmp(&mut ctx, deseret, a_utf16), 0x10428 - 0x61);
    }
}

fn exact(p: &str) -> ExactRegex {
    java_regex_exact_subset(&u16s(p))
        .unwrap_or_else(|| panic!("{p} should be in the exact subset"))
}

fn exact_split(s: &str, p: &str, limit: i32) -> Vec<String> {
    exact_regex_split(&exact(p), s, limit)
        .expect("split")
        .into_iter()
        .map(|(a, b)| s[a..b].to_string())
        .collect()
}

fn exact_replace(s: &str, p: &str, r: &str, first: bool) -> String {
    match exact_regex_replace(&exact(p), s, &u16s(r), first).expect("replace") {
        Some(u) => String::from_utf16(&u).unwrap(),
        None => s.to_string(),
    }
}

/// JIT r23 wave 3, lane L5 (`CRATONVM_STRING_REGEX_MEMO`): the memo's rows
/// answer exactly what the per-call path answers -- matches, replaceAll,
/// split, the "not exact" verdict -- across more patterns than the memo
/// holds (so rows are evicted and rebuilt), each pattern asked twice.
#[test]
fn r23_l5_exact_regex_memo_answers_what_the_per_call_path_answers() {
    let patterns = [
        "[a-z]+\\d*", "o", "\\d+", ",", "\\s+", "a|ab", "(\\w+)=(\\w+)", "[^a-c]", "x*", "",
        "a.c", "[A-C]+", "\\<a\\>", "(a)(b)?", "q+", "z", "[0-9]{2,3}", "(?i)abc",
        "\\B", "ab$",
    ];
    let subjects = ["abc123", "hello world 12345", "a=1 b=22", "", "zzz,y,,x", "ab", "a\nb"];
    for round in 0..2 {
        for p in patterns {
            let units = u16s(p);
            let row = exact_regex_memo(&units);
            let fresh = java_regex_exact_subset(&units);
            assert_eq!(row.exact.is_some(), fresh.is_some(), "{p} round {round}");
            let Some(x) = fresh else {
                continue;
            };
            for s in subjects {
                assert_eq!(
                    row.anchored.as_ref().map(|re| re.is_match(s)),
                    exact_regex_matches(&x, s),
                    "matches {p} {s:?}"
                );
                if let Some(re) = row.plain.as_ref() {
                    assert_eq!(
                        exact_regex_split_with(re, s, 0),
                        exact_regex_split(&x, s, 0),
                        "split {p} {s:?}"
                    );
                    assert_eq!(
                        exact_regex_replace_with(&x, re, s, &u16s("#"), false),
                        exact_regex_replace(&x, s, &u16s("#"), false),
                        "replaceAll {p} {s:?}"
                    );
                }
            }
        }
    }
}

/// nio21 w10 L4: the exact regex path against HotSpot 25.0.3+9 (each
/// expected value printed by `String.split` / `replaceAll` / `matches` on
/// HotSpot; see the w10 L4 page).
#[test]
fn string_regex_exact_answers_are_hotspots() {
    let v = |xs: &[&str]| xs.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    assert_eq!(exact_split("a,b,,c,,", ",", 0), v(&["a", "b", "", "c"]));
    assert_eq!(
        exact_split("a,b,,c,,", ",", -1),
        v(&["a", "b", "", "c", "", ""])
    );
    assert_eq!(exact_split("a,b,,c,,", ",", 2), v(&["a", "b,,c,,"]));
    assert_eq!(exact_split("abc", "", 0), v(&["a", "b", "c"]));
    assert_eq!(exact_split(",a", ",", 0), v(&["", "a"]));
    assert_eq!(exact_split("a1b22c", "\\d+", 0), v(&["a", "b", "c"]));
    assert_eq!(exact_split("aXbXXc", "X*", 0), v(&["a", "", "b", "", "c"]));
    assert_eq!(exact_split(",", ",", 0), Vec::<String>::new());
    assert_eq!(exact_split("", ",", 0), v(&[""]));
    assert_eq!(
        exact_split("boo:and:foo", "o", -1),
        v(&["b", "", ":and:f", "", ""])
    );
    assert_eq!(exact_split("a b\tc", "\\s", 0), v(&["a", "b", "c"]));
    assert_eq!(exact_split("ab", "(?:)", 1), v(&["ab"]));
    assert_eq!(exact_split("abc", "b|", 0), v(&["a", "", "c"]));

    assert_eq!(exact_replace("baaa", "a*", "-", false), "-b--");
    assert_eq!(exact_replace("abc", "(b)", "[$1]", false), "a[b]c");
    assert_eq!(exact_replace("abc", "b", "\\$", false), "a$c");
    assert_eq!(
        exact_replace("x1y22", "(\\d)(\\d)?", "<$2$1>", false),
        "x<1>y<22>"
    );
    assert_eq!(exact_replace("aaa", "a", "b", true), "baa");
    assert_eq!(exact_replace("ab12", "$", "!", false), "ab12!");
    assert_eq!(exact_replace("a.b", ".", "x", false), "xxx");
    assert_eq!(exact_replace("abc", "x*", "-", false), "-a-b-c-");
    assert_eq!(exact_replace("ab", "(a)(b)", "$21$10", false), "b1a0");

    assert_eq!(exact_regex_matches(&exact("a.c"), "abc"), Some(true));
    assert_eq!(exact_regex_matches(&exact("a.c"), "a\rc"), Some(false));
    assert_eq!(exact_regex_matches(&exact("[A-C]+"), "ABC"), Some(true));
    assert_eq!(exact_regex_matches(&exact("a|ab"), "ab"), Some(true));
    assert_eq!(exact_regex_matches(&exact("[a-]+"), "a-"), Some(true));
    assert_eq!(exact_regex_matches(&exact("[^a-c]"), "x"), Some(true));
}

/// The subset declines every construct whose Java meaning the std engine
/// lacks, and every Java syntax error (the JDK body raises those).
#[test]
fn string_regex_exact_subset_declines_what_it_cannot_prove() {
    for p in [
        "(?=a)", "(?<=a)b", "\\1", "a++", "a*+", "(?>a)", "[a&&b]", "[[a]]", "\\p{L}", "(?i)a",
        "a{2,1}", "[z-a]", "\\Q.\\E", "(?<n>a)", "\\x41", "\\u0041", "\\G", "\\Z", "\\R", "[",
        "(", ")", "*a", "a**", "a{", "]", "}", "\\k<n>", "\u{e9}", "a{,2}",
    ] {
        assert!(
            java_regex_exact_subset(&u16s(p)).is_none(),
            "{p} must decline"
        );
    }
    // nio21 w11 L2: `\B` declines (true inside a surrogate pair on the
    // JDK); `\b` is the ASCII boundary on any subject (HotSpot: `\b` in
    // "\u00e9a b" at 1,2,3,4 -- `\u00e9` is not a word char).
    assert!(java_regex_exact_subset(&u16s("\\B")).is_none());
    let re =
        exact_std_regex(&exact("(?:)\\b").rust).expect("(?-u:\\b) must compile in a str regex");
    let at: Vec<usize> = re.find_iter("\u{e9}a b").map(|m| m.start()).collect();
    // Byte offsets: e-acute is two bytes, so UTF-16 1,2,3,4 are bytes 2,3,4,5.
    assert_eq!(at, vec![2, 3, 4, 5]);
    // Subjects: pairs are fine, unpaired halves and (with `$`) line
    // terminators are not.
    let plain = exact("a.");
    assert!(java_regex_exact_subject_ok(
        &plain,
        &u16s("a\u{10400}\u{e9}\n")
    ));
    assert!(!java_regex_exact_subject_ok(&plain, &[0x61, 0xD801]));
    let dollar = exact("a$");
    assert!(!java_regex_exact_subject_ok(&dollar, &u16s("a\u{2028}")));
    assert!(java_regex_exact_subject_ok(&dollar, &u16s("a\u{10400}")));
    // A Java literal that is Rust syntax stays a literal (`\<` is a Rust
    // word-start assertion since regex 1.10).
    assert_eq!(exact_regex_matches(&exact("\\<a\\>"), "<a>"), Some(true));
    // Replacements the JDK throws on, and named refs, decline.
    let x = exact("(a)");
    for r in ["\\", "$", "$x", "${g}", "$2"] {
        assert!(
            exact_regex_replace(&x, "a", &u16s(r), false).is_none(),
            "{r} must decline"
        );
    }
}

#[test]
fn string_ci_split_and_unpaired_surrogates_match_the_jdk_transcription() {
    let u = u16s("\u{10400}");
    let l = u16s("\u{10428}");
    // Region of the high halves only: identical, true.
    assert_eq!(utf16_compare_to_ci_impl(&u, 0, 1, &l, 0, 1), 0);
    // Region of the low halves only (starts at 1, so no look-back): differ.
    assert_ne!(utf16_compare_to_ci_impl(&u, 1, 1, &l, 1, 1), 0);
    // Lone surrogates fold to themselves.
    assert_eq!(
        utf16_compare_to_ci_impl(&[0xD801, 0x61], 0, 2, &[0xD801, 0x41], 0, 2),
        0
    );
    assert_eq!(
        utf16_compare_to_ci_impl(&[0xDC00], 0, 1, &[0xDC28], 0, 1),
        0xDC00 - 0xDC28
    );
    // The JDK's index skew: a forward pair on one side against a
    // back-paired low surrogate on the other. a = D801 D801 DC00,
    // b = D801 DC28 'A': at k=1, a pairs forward (U+10400, k1 skips) and
    // b pairs backward (U+10428); they fold equal, and the scan ends with
    // k1 past the end. HotSpot's loop answers 0.
    assert_eq!(
        utf16_compare_to_ci_impl(
            &[0xD801, 0xD801, 0xDC00],
            0,
            3,
            &[0xD801, 0xDC28, 0x41],
            0,
            3
        ),
        0
    );
    // A LATIN1 side takes the per-unit path (no surrogates there).
    assert!(!region_matches_ci_units(
        &u16s("ab"),
        true,
        0,
        &u,
        false,
        0,
        2
    ));
    assert!(region_matches_ci_units(
        &u16s("AB"),
        true,
        0,
        &u16s("ab"),
        true,
        0,
        2
    ));
}

#[test]
fn code_unit_eq_ignore_case_works() {
    let a: u16 = 'A' as u16;
    let lower_a: u16 = 'a' as u16;
    assert!(code_unit_eq_ignore_case(a, lower_a));
    assert!(code_unit_eq_ignore_case(a, a));
    assert!(!code_unit_eq_ignore_case('a' as u16, 'b' as u16));
    // Lone surrogate code units only match when bit-identical.
    assert!(code_unit_eq_ignore_case(0xD83D, 0xD83D));
    assert!(!code_unit_eq_ignore_case(0xD83D, 0xDE00));
}

/// The rule `equalsIgnoreCase` now shares with `regionMatches(true, …)`.
///
/// Every row is the verbatim OpenJDK 25.0.3+9 answer to the corresponding
/// `String.equalsIgnoreCase` call. The A7Dx rows are the ones Rust's
/// `to_lowercase()` — the implementation this replaced — got wrong.
#[test]
fn equals_ignore_case_folds_the_way_the_jdk_does() {
    // "İ".equalsIgnoreCase("i") is TRUE: Java's 1:1 toLowerCase(U+0130) is
    // a plain `i`, while Rust's full mapping is `i` + U+0307.
    assert!(code_unit_eq_ignore_case(0x0130, 0x0069));
    assert!(code_unit_eq_ignore_case(0x0130, 0x0049));
    // U+212A KELVIN SIGN folds onto `k` through the upper-then-lower
    // composition.
    assert!(code_unit_eq_ignore_case(0x212A, 0x006B));
    // The titlecase letter matches its uppercase partner.
    assert!(code_unit_eq_ignore_case(0x01C5, 0x01C4));
    // The six the JDK does not case-pair, measured `false` on HotSpot and
    // `true` under Rust's newer tables.
    assert!(!code_unit_eq_ignore_case(0xA7D3, 0xA7D2));
    assert!(!code_unit_eq_ignore_case(0xA7D5, 0xA7D4));
    assert!(!code_unit_eq_ignore_case(0xA7CF, 0xA7CE));
    // ... and the neighbours that ARE pairs, so this is not a range.
    assert!(code_unit_eq_ignore_case(0xA7D1, 0xA7D0));
    assert!(code_unit_eq_ignore_case(0xA7D7, 0xA7D6));
    // Identity still holds for all six.
    for cp in [0xA7CEu16, 0xA7CF, 0xA7D2, 0xA7D3, 0xA7D4, 0xA7D5] {
        assert!(code_unit_eq_ignore_case(cp, cp));
    }
}

/// `compareToIgnoreCase` returns a DIFFERENCE, not a sign.
///
/// The folded-difference rows measured on OpenJDK 25.0.3+9:
/// `"_".compareToIgnoreCase("a")` is `-2`, `"B".compareToIgnoreCase("a")`
/// is `1`, `"İ".compareToIgnoreCase("i")` is `0`, and
/// `"\u{A7D3}".compareToIgnoreCase("\u{A7D2}")` is `1`. The old body
/// returned `-1`/`0`/`1` from an `Ordering` over Rust-lowercased strings,
/// which is a wrong magnitude in the first row and a wrong SIGN in the
/// third and fourth.
#[test]
fn compare_to_ignore_case_returns_the_folded_difference() {
    // The fold each row depends on, in the JDK's upper-then-lower order.
    let folded = |c: u16| java_char_to_lower_case(java_char_to_upper_case(c));
    assert_eq!(
        i32::from(folded(b'_' as u16)) - i32::from(folded(b'a' as u16)),
        -2
    );
    assert_eq!(
        i32::from(folded(b'B' as u16)) - i32::from(folded(b'a' as u16)),
        1
    );
    // "İ" vs "i": the uppercases differ, the lowercases do not, so the
    // loop continues and equal lengths give 0.
    assert_ne!(
        java_char_to_upper_case(0x0130),
        java_char_to_upper_case(0x0069)
    );
    assert_eq!(folded(0x0130), folded(0x0069));
    // A7D3 vs A7D2: both fold to themselves, so the difference is 1.
    assert_eq!(i32::from(folded(0xA7D3)) - i32::from(folded(0xA7D2)), 1);
}

/// The `regionMatches` short circuit, pinned against the eight rows
/// measured on OpenJDK 25.0.3+9 with the receiver `"abc"`.
///
/// `true` means the JDK returned `false` without touching `other`; `false`
/// means it reached `other.length()`, which is a NullPointerException when
/// `other` is null. All eight used to answer plain `false` here.
#[test]
fn region_matches_dereferences_other_exactly_where_the_jdk_does() {
    let abc = 3i64;
    // Reaches other.length() -> NPE for a null `other`.
    assert!(!region_matches_short_circuits(abc, 0, 0, 1));
    assert!(!region_matches_short_circuits(abc, 0, 0, -1)); // negative len does NOT save it
    assert!(!region_matches_short_circuits(abc, 3, 0, 0)); // toffset == length() - len
    assert!(!region_matches_short_circuits(0, 0, 0, 0)); // "" receiver, still reached
    assert!(!region_matches_short_circuits(
        abc,
        0,
        0,
        i64::from(i32::MIN)
    ));
    // Decided by terms one to three -> `false`, `other` never read.
    assert!(region_matches_short_circuits(abc, -1, 0, 1)); // toffset < 0
    assert!(region_matches_short_circuits(abc, 0, -1, 1)); // ooffset < 0
    assert!(region_matches_short_circuits(abc, 99, 0, 1)); // toffset > len - len
    assert!(region_matches_short_circuits(abc, 0, 0, 4)); // 0 > 3 - 4
    assert!(region_matches_short_circuits(
        abc,
        i64::from(i32::MAX),
        0,
        1
    ));
    // The widening is load-bearing: with `i32` arithmetic
    // `length() - Integer.MIN_VALUE` wraps negative and the third term
    // would wrongly short-circuit.
    assert!(!region_matches_short_circuits(
        abc,
        0,
        0,
        i64::from(i32::MIN)
    ));
}

// -----------------------------------------------------------------------
// E18: String.{indexOf,lastIndexOf}(int ch[, int from])
//
// Written against a `&[u16]` rather than a heap String on purpose: a
// `NativeContext` mock cannot even express the interesting receivers,
// because `create_string(&str)` cannot hold an UNPAIRED surrogate and the
// lone-surrogate rows are half the contract. Every expectation below is a
// measured OpenJDK 25.0.3+9 answer (`scratchpad/e18/CharSearch.java`,
// `Idx3.java`) — not a re-derivation from Rust's `char`, which is the
// proxy-oracle trap E8 §4.2 warns about and which is exactly what
// `char::from_u32` was doing here.
// -----------------------------------------------------------------------

/// `"xзy𐐷z"` — a Cyrillic з (U+0437, the LOW HALF of U+10437 when masked),
/// then the real surrogate pair for U+10437 at index 3.
const MIXED: &[u16] = &[0x0078, 0x0437, 0x0079, 0xD801, 0xDC37, 0x007A];
/// `"abc"`, whose 'a' is `0x10061 & 0xFFFF`.
const ABC: &[u16] = &[0x0061, 0x0062, 0x0063];

fn idx(h: &[u16], ch: i32, from: i32) -> i32 {
    match code_point_needle(ch) {
        Some(n) => index_of_units_from(h, n.units(), from),
        None => -1,
    }
}

fn last_idx(h: &[u16], ch: i32, from: i64) -> i32 {
    match code_point_needle(ch) {
        Some(n) => last_index_of_units_from(h, n.units(), from),
        None => -1,
    }
}

#[test]
fn a_supplementary_needle_is_a_surrogate_pair_and_never_its_masked_low_half() {
    // THE row: `(ch & 0xFFFF)` finds 'a' at 0. HotSpot answers -1.
    assert_eq!(idx(ABC, 0x10061, 0), -1);
    assert_eq!(idx(ABC, 0x10061, -5), -1);
    assert_eq!(last_idx(ABC, 0x10061, 2), -1);
    assert_eq!(last_idx(ABC, 0x10061, i64::MAX), -1);
    // Same shape with a receiver that really does contain the low half.
    assert_eq!(idx(&[0x0437], 0x10437, 0), -1);
    assert_eq!(last_idx(&[0x0437], 0x10437, i64::MAX), -1);
    // …and the pair itself IS found, at the index of the HIGH surrogate.
    assert_eq!(idx(MIXED, 0x10437, 0), 3);
    assert_eq!(last_idx(MIXED, 0x10437, i64::MAX), 3);
}

#[test]
fn a_lone_surrogate_needle_is_an_ordinary_code_unit_scan() {
    // `char::from_u32` answers `None` for both of these, which is how
    // `lastIndexOf(int)` came to return -1 where HotSpot returns 3 and 4.
    assert_eq!(idx(MIXED, 0xD801, 0), 3);
    assert_eq!(idx(MIXED, 0xDC37, 0), 4);
    assert_eq!(last_idx(MIXED, 0xD801, i64::MAX), 3);
    assert_eq!(last_idx(MIXED, 0xDC37, i64::MAX), 4);
}

#[test]
fn the_gate_is_is_valid_code_point_and_not_a_narrowing_cast() {
    // `(char) -1` is 0xFFFF and this receiver holds 0xFFFF, yet HotSpot
    // answers -1: `Character.isValidCodePoint` is checked first.
    let ffff: &[u16] = &[0xFFFF, 0x0071];
    assert_eq!(idx(ffff, -1, 0), -1);
    assert_eq!(last_idx(ffff, -1, i64::MAX), -1);
    assert_eq!(idx(ffff, i32::MIN, 0), -1);
    // 0xFFFF is a NONCHARACTER but a valid code point, so it IS found.
    assert_eq!(idx(ffff, 0xFFFF, 0), 0);
    // Above the Unicode range, nothing matches.
    assert_eq!(idx(ABC, 0x110000, 0), -1);
    assert_eq!(idx(ABC, 0x1FFFFFF, 0), -1);
    assert_eq!(last_idx(ABC, 0x110000, i64::MAX), -1);
}

#[test]
fn from_index_clamps_forward_and_floors_backward() {
    assert_eq!(idx(MIXED, 0x10437, 3), 3);
    assert_eq!(idx(MIXED, 0x10437, 4), -1); // starts past the pair
    assert_eq!(idx(MIXED, 0x10437, 99), -1);
    assert_eq!(idx(MIXED, 0x10437, -5), 3); // negative clamps to 0
    assert_eq!(idx(ABC, 0x61, 99), -1);
    // Backward: `min(from, len - width)`, and a NEGATIVE from finds
    // nothing even when the needle is present — the JDK's loop counter
    // starts below zero and never runs.
    assert_eq!(last_idx(MIXED, 0x10437, 99), 3);
    assert_eq!(last_idx(MIXED, 0x10437, 4), 3);
    assert_eq!(last_idx(MIXED, 0x10437, 3), 3);
    assert_eq!(last_idx(MIXED, 0x10437, 2), -1); // width 2: start is 2, pair is at 3
    assert_eq!(last_idx(MIXED, 0x10437, -1), -1);
    assert_eq!(last_idx(ABC, 0x61, -1), -1);
    assert_eq!(last_idx(ABC, 0x61, 99), 0);
    assert_eq!(last_idx(&[], 0x61, 0), -1);
    // A two-unit needle in a two-unit haystack, started past index 0.
    assert_eq!(idx(&[0xD801, 0xDC37], 0x10437, 1), -1);
    assert_eq!(idx(&[0xD801, 0xDC37], 0x10437, 0), 0);
}

#[test]
fn the_empty_string_needle_keeps_its_own_contract() {
    // Not reachable from `code_point_needle` (which is never empty) but
    // shared with the String-needle overloads, whose measured answers are
    // `"abcabc".indexOf("", 99) == 6` and `indexOf("", -5) == 0`.
    let abcabc: &[u16] = &[0x61, 0x62, 0x63, 0x61, 0x62, 0x63];
    assert_eq!(index_of_units_from(abcabc, &[], 99), 6);
    assert_eq!(index_of_units_from(abcabc, &[], -5), 0);
    assert_eq!(index_of_units_from(abcabc, &[], 2), 2);
    assert_eq!(last_index_of_units_from(abcabc, &[], 99), 6);
    assert_eq!(last_index_of_units(abcabc, &[]), 6);
    // …and a needle longer than the haystack is a miss, not a panic.
    assert_eq!(index_of_units_from(&[0x61], &[0x61, 0x62], 0), -1);
    assert_eq!(
        last_index_of_units_from(&[0x61], &[0x61, 0x62], i64::MAX),
        -1
    );
}

// -----------------------------------------------------------------------
// String length
// -----------------------------------------------------------------------

#[test]
fn string_length_empty() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("");
    let r = native_string_length(&mut ctx, &[Value::Object(Some(s))]);
    assert_eq!(r.unwrap(), Some(Value::Int(0)));
}

#[test]
fn string_length_hello() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("hello");
    let r = native_string_length(&mut ctx, &[Value::Object(Some(s))]);
    assert_eq!(r.unwrap(), Some(Value::Int(5)));
}

#[test]
fn string_length_null_returns_zero() {
    let mut ctx = mock_ctx();
    let r = native_string_length(&mut ctx, &[Value::Object(None)]);
    assert_eq!(r.unwrap(), Some(Value::Int(0)));
}

// -----------------------------------------------------------------------
// String charAt
// -----------------------------------------------------------------------

#[test]
fn string_char_at_valid() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("abc");
    let r = native_string_char_at(&mut ctx, &[Value::Object(Some(s)), Value::Int(1)]);
    assert_eq!(r.unwrap(), Some(Value::Int('b' as i32)));
}

#[test]
fn string_char_at_out_of_bounds() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("abc");
    let r = native_string_char_at(&mut ctx, &[Value::Object(Some(s)), Value::Int(5)]);
    assert!(r.is_err());
}

#[test]
fn string_char_at_negative_index() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("abc");
    let r = native_string_char_at(&mut ctx, &[Value::Object(Some(s)), Value::Int(-1)]);
    assert!(r.is_err());
}

// -----------------------------------------------------------------------
// T2.2.6: String.indexOf(int ch, int fromIndex) / lastIndexOf(II)I
// -----------------------------------------------------------------------

#[test]
fn t2_string_index_of_from_finds_char_after_start() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("abcabc");
    let r = native_string_index_of_from(
        &mut ctx,
        &[
            Value::Object(Some(s)),
            Value::Int('b' as i32),
            Value::Int(2),
        ],
    );
    assert_eq!(r.unwrap(), Some(Value::Int(4)));
}

#[test]
fn t2_string_index_of_from_clamps_negative_start() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("abc");
    let r = native_string_index_of_from(
        &mut ctx,
        &[
            Value::Object(Some(s)),
            Value::Int('a' as i32),
            Value::Int(-100),
        ],
    );
    assert_eq!(r.unwrap(), Some(Value::Int(0)));
}

#[test]
fn t2_string_index_of_from_beyond_end_returns_minus_one() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("abc");
    let r = native_string_index_of_from(
        &mut ctx,
        &[
            Value::Object(Some(s)),
            Value::Int('a' as i32),
            Value::Int(10),
        ],
    );
    assert_eq!(r.unwrap(), Some(Value::Int(-1)));
}

#[test]
fn t2_string_index_of_from_no_match() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("abcdef");
    let r = native_string_index_of_from(
        &mut ctx,
        &[
            Value::Object(Some(s)),
            Value::Int('z' as i32),
            Value::Int(0),
        ],
    );
    assert_eq!(r.unwrap(), Some(Value::Int(-1)));
}

#[test]
fn t2_string_last_index_of_from_finds_char_before_start() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("abcabc");
    let r = native_string_last_index_of_from(
        &mut ctx,
        &[
            Value::Object(Some(s)),
            Value::Int('b' as i32),
            Value::Int(4),
        ],
    );
    assert_eq!(r.unwrap(), Some(Value::Int(4)));
}

#[test]
fn t2_string_last_index_of_from_stops_at_from_index() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("abcabc");
    // fromIndex=2 should find the first 'b' at index 1, not the second
    // at index 4.
    let r = native_string_last_index_of_from(
        &mut ctx,
        &[
            Value::Object(Some(s)),
            Value::Int('b' as i32),
            Value::Int(2),
        ],
    );
    assert_eq!(r.unwrap(), Some(Value::Int(1)));
}

#[test]
fn t2_string_last_index_of_from_negative_returns_minus_one() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("abc");
    let r = native_string_last_index_of_from(
        &mut ctx,
        &[
            Value::Object(Some(s)),
            Value::Int('a' as i32),
            Value::Int(-1),
        ],
    );
    assert_eq!(r.unwrap(), Some(Value::Int(-1)));
}

#[test]
fn t2_string_last_index_of_from_empty_string() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("");
    let r = native_string_last_index_of_from(
        &mut ctx,
        &[
            Value::Object(Some(s)),
            Value::Int('a' as i32),
            Value::Int(0),
        ],
    );
    assert_eq!(r.unwrap(), Some(Value::Int(-1)));
}

// -----------------------------------------------------------------------
// String equals
// -----------------------------------------------------------------------

#[test]
fn string_equals_same_content() {
    let mut ctx = mock_ctx();
    let a = ctx.create_string("hello");
    let b = ctx.create_string("hello");
    let r = native_string_equals(&mut ctx, &[Value::Object(Some(a)), Value::Object(Some(b))]);
    assert_eq!(r.unwrap(), Some(Value::Int(1)));
}

#[test]
fn string_equals_different_content() {
    let mut ctx = mock_ctx();
    let a = ctx.create_string("hello");
    let b = ctx.create_string("world");
    let r = native_string_equals(&mut ctx, &[Value::Object(Some(a)), Value::Object(Some(b))]);
    assert_eq!(r.unwrap(), Some(Value::Int(0)));
}

#[test]
fn string_equals_null_arg() {
    let mut ctx = mock_ctx();
    let a = ctx.create_string("hello");
    let r = native_string_equals(&mut ctx, &[Value::Object(Some(a)), Value::Object(None)]);
    assert_eq!(r.unwrap(), Some(Value::Int(0)));
}

// -----------------------------------------------------------------------
// String indexOf (char)
// -----------------------------------------------------------------------

#[test]
fn string_index_of_found() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("hello");
    let r = native_string_index_of(&mut ctx, &[Value::Object(Some(s)), Value::Int('l' as i32)]);
    assert_eq!(r.unwrap(), Some(Value::Int(2)));
}

#[test]
fn string_index_of_not_found() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("hello");
    let r = native_string_index_of(&mut ctx, &[Value::Object(Some(s)), Value::Int('z' as i32)]);
    assert_eq!(r.unwrap(), Some(Value::Int(-1)));
}

// -----------------------------------------------------------------------
// String contains, startsWith, endsWith
// -----------------------------------------------------------------------

#[test]
fn string_contains_true() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("hello world");
    let sub = ctx.create_string("world");
    let r = native_string_contains(
        &mut ctx,
        &[Value::Object(Some(s)), Value::Object(Some(sub))],
    );
    assert_eq!(r.unwrap(), Some(Value::Int(1)));
}

#[test]
fn string_contains_false() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("hello");
    let sub = ctx.create_string("xyz");
    let r = native_string_contains(
        &mut ctx,
        &[Value::Object(Some(s)), Value::Object(Some(sub))],
    );
    assert_eq!(r.unwrap(), Some(Value::Int(0)));
}

#[test]
fn string_starts_with_true() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("hello world");
    let prefix = ctx.create_string("hello");
    let r = native_string_starts_with(
        &mut ctx,
        &[Value::Object(Some(s)), Value::Object(Some(prefix))],
    );
    assert_eq!(r.unwrap(), Some(Value::Int(1)));
}

#[test]
fn string_ends_with_true() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("hello world");
    let suffix = ctx.create_string("world");
    let r = native_string_ends_with(
        &mut ctx,
        &[Value::Object(Some(s)), Value::Object(Some(suffix))],
    );
    assert_eq!(r.unwrap(), Some(Value::Int(1)));
}

// -----------------------------------------------------------------------
// String case conversion and trim
// -----------------------------------------------------------------------

#[test]
fn string_to_upper_case() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("hello");
    let r = native_string_to_upper_case(&mut ctx, &[Value::Object(Some(s))]);
    let obj = match r.unwrap() {
        Some(Value::Object(Some(o))) => o,
        other => panic!("expected Object, got {other:?}"),
    };
    assert_eq!(ctx.read_string(obj).unwrap(), "HELLO");
}

#[test]
fn string_to_lower_case() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("HELLO");
    let r = native_string_to_lower_case(&mut ctx, &[Value::Object(Some(s))]);
    let obj = match r.unwrap() {
        Some(Value::Object(Some(o))) => o,
        other => panic!("expected Object, got {other:?}"),
    };
    assert_eq!(ctx.read_string(obj).unwrap(), "hello");
}

/// E18. The JIT's door and the interpreter's door must answer the same
/// null `Locale`.
///
/// This is the whole shape of the bug: `jit_string_to_lower_case` called
/// `string_case_impl` one layer BELOW the null check, so the same source
/// line threw while it was interpreted and returned the default-locale
/// string once it tiered up. A tier-dependent answer is invisible to any
/// test that runs the method only a few times, which is why the assertion
/// is written against the two ENTRY POINTS rather than against the
/// behaviour of a warm loop.
///
/// Measured on OpenJDK 25.0.3+9: `"AbC".toLowerCase()` is `"abc"`,
/// `"AbC".toLowerCase((Locale) null)` throws NullPointerException, and it
/// still throws on the 200 000th warm iteration — HotSpot's answer does
/// not depend on its tier either.
#[test]
fn the_jit_lower_case_door_enforces_the_same_null_locale_as_the_native() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("AbC");

    // ABSENT Locale slot — the no-argument overload. Answers, does not
    // throw. (The JIT has no such call: its one bound descriptor,
    // `StringLatin1.toLowerCase(Ljava/lang/String;[BLjava/util/Locale;)`,
    // always carries the slot.)
    let absent = native_string_to_lower_case(&mut ctx, &[Value::Object(Some(s))]);
    match absent {
        Ok(Some(Value::Object(Some(o)))) => assert_eq!(ctx.read_string(o).unwrap(), "abc"),
        other => panic!("toLowerCase() must answer \"abc\", got {other:?}"),
    }

    // PRESENT-and-null Locale — both doors must throw.
    let interpreted =
        native_string_to_lower_case(&mut ctx, &[Value::Object(Some(s)), Value::Object(None)]);
    assert!(
        interpreted.is_err(),
        "the interpreted native must throw for toLowerCase((Locale) null)"
    );
    let jit = jit_string_to_lower_case(&mut ctx, s, None);
    assert!(
        jit.is_err(),
        "the JIT direct-call door must throw for the SAME call the interpreter throws for — \
         it took `string_case_impl` directly and answered the default-locale string"
    );
}

#[test]
fn string_trim_whitespace() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("  hello  ");
    let r = native_string_trim(&mut ctx, &[Value::Object(Some(s))]);
    let obj = match r.unwrap() {
        Some(Value::Object(Some(o))) => o,
        other => panic!("expected Object, got {other:?}"),
    };
    assert_eq!(ctx.read_string(obj).unwrap(), "hello");
}

// -----------------------------------------------------------------------
// String isEmpty, isBlank
// -----------------------------------------------------------------------

#[test]
fn string_is_empty_true() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("");
    let r = native_string_is_empty(&mut ctx, &[Value::Object(Some(s))]);
    assert_eq!(r.unwrap(), Some(Value::Int(1)));
}

#[test]
fn string_is_empty_false() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("a");
    let r = native_string_is_empty(&mut ctx, &[Value::Object(Some(s))]);
    assert_eq!(r.unwrap(), Some(Value::Int(0)));
}

// -----------------------------------------------------------------------
// String valueOf
// -----------------------------------------------------------------------

#[test]
fn string_value_of_int() {
    let mut ctx = mock_ctx();
    let r = native_string_value_of_int(&mut ctx, &[Value::Int(42)]);
    let obj = match r.unwrap() {
        Some(Value::Object(Some(o))) => o,
        other => panic!("expected Object, got {other:?}"),
    };
    assert_eq!(ctx.read_string(obj).unwrap(), "42");
}

#[test]
fn string_value_of_long() {
    let mut ctx = mock_ctx();
    let r = native_string_value_of_long(&mut ctx, &[Value::Long(123456789)]);
    let obj = match r.unwrap() {
        Some(Value::Object(Some(o))) => o,
        other => panic!("expected Object, got {other:?}"),
    };
    assert_eq!(ctx.read_string(obj).unwrap(), "123456789");
}

#[test]
fn string_value_of_boolean_true() {
    let mut ctx = mock_ctx();
    let r = native_string_value_of_boolean(&mut ctx, &[Value::Int(1)]);
    let obj = match r.unwrap() {
        Some(Value::Object(Some(o))) => o,
        other => panic!("expected Object, got {other:?}"),
    };
    assert_eq!(ctx.read_string(obj).unwrap(), "true");
}

// -----------------------------------------------------------------------
// String concat
// -----------------------------------------------------------------------

#[test]
fn string_concat_basic() {
    let mut ctx = mock_ctx();
    let a = ctx.create_string("hello ");
    let b = ctx.create_string("world");
    let r = native_string_concat(&mut ctx, &[Value::Object(Some(a)), Value::Object(Some(b))]);
    let obj = match r.unwrap() {
        Some(Value::Object(Some(o))) => o,
        other => panic!("expected Object, got {other:?}"),
    };
    assert_eq!(ctx.read_string(obj).unwrap(), "hello world");
}

// -----------------------------------------------------------------------
// String hashCode
// -----------------------------------------------------------------------

#[test]
fn string_hash_code_consistent() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("hello");
    let r1 = native_string_hash_code(&mut ctx, &[Value::Object(Some(s))]);
    let r2 = native_string_hash_code(&mut ctx, &[Value::Object(Some(s))]);
    assert_eq!(r1.unwrap(), r2.unwrap());
}

#[test]
fn string_hash_code_empty_is_zero() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("");
    let r = native_string_hash_code(&mut ctx, &[Value::Object(Some(s))]);
    assert_eq!(r.unwrap(), Some(Value::Int(0)));
}

/// A `String` that cannot answer "which layout am I" must not answer it.
///
/// `native_string_hash_code` latches the hash field slot (per VM since
/// gc-common w15-e; in a process-wide `OnceLock` before), from whichever
/// `String` it happens to hash first. That is fine for a receiver with a
/// readable `value` array and wrong for one without: `string_char_array` returns `None` for a
/// `String` allocated but not yet initialised, and folding `None` into
/// slot 1 — as this did — points every later call at `coder` on the JDK 25
/// layout. The read side returns `1` as the hash of every UTF-16 string;
/// the write side stores the hash INTO `coder`, silently corrupting the
/// encoding flag of every string the process hashes afterwards.
///
/// The `OnceLock` makes the whole-native version of this untestable — one
/// latch per test binary — so the decision lives in a pure function and the
/// test is on that. `None => None` is the entire fix; the two `Some` arms
/// are here so a rewrite cannot quietly swap the layouts.
#[test]
fn hash_slot_is_never_guessed_from_an_unreadable_string() {
    use cratonvm_types::ArrayElementType;

    assert_eq!(
        hash_slot_for(None),
        None,
        "a String with a null `value` carries no layout evidence; latching a \
         guess from it points the whole process at `coder` (slot 1) and makes \
         every later hash write corrupt the string it hashed"
    );
    assert_eq!(
        hash_slot_for(Some(ArrayElementType::Byte)),
        Some(2),
        "compact JDK 9+ layout: value:[B, coder:B, hash:I, hashIsZero:Z"
    );
    assert_eq!(
        hash_slot_for(Some(ArrayElementType::Boolean)),
        Some(2),
        "boolean[] is this VM's byte[] alias, so it is the compact layout too"
    );
    assert_eq!(
        hash_slot_for(Some(ArrayElementType::Char)),
        Some(1),
        "legacy synthetic-stub layout: value:[C, hash:I"
    );
}

// -----------------------------------------------------------------------
// Registration
// -----------------------------------------------------------------------

#[test]
fn register_string_builder_natives_does_not_panic() {
    let mut registry = NativeMethodRegistry::new();
    register_string_builder_natives(&mut registry, "java/lang/StringBuilder");
    assert!(registry.len() > 20);
}

// -----------------------------------------------------------------------
// C36: append(CharSequence, int, int) native interception
// -----------------------------------------------------------------------
//
// These tests verify that the append(CharSequence)/append(CharSequence, int, int)
// natives take effect on a StringBuilder initialised via native_sb_init_default
// and correctly update field 1 (count) — the field layout where real JDK
// bytecode would otherwise misread slot 2 as `count` and trigger huge array
// allocations inside `Formatter.format`.

fn make_sb(ctx: &mut dyn NativeContext) -> cratonvm_types::ObjectRef {
    let cid = ctx
        .ensure_class_initialized("java/lang/StringBuilder")
        .unwrap();
    // Allocate enough slots to match real JDK layout width (value/coder/count ≈ 3).
    let sb = ctx.alloc_object(cid, 4);
    native_sb_init_default(ctx, &[Value::Object(Some(sb))]).unwrap();
    sb
}

#[test]
fn sb_append_charsequence_full_string() {
    let mut ctx = mock_ctx();
    let sb = make_sb(&mut ctx);
    let s = ctx.create_string("hello");
    let r = native_sb_append_charsequence(
        &mut ctx,
        &[Value::Object(Some(sb)), Value::Object(Some(s))],
    );
    assert!(matches!(r.unwrap(), Some(Value::Object(Some(_)))));
    let out_r = native_sb_to_string(&mut ctx, &[Value::Object(Some(sb))]).unwrap();
    let Some(Value::Object(Some(out_obj))) = out_r else {
        panic!()
    };
    assert_eq!(ctx.read_string(out_obj).unwrap(), "hello");
}

#[test]
fn sb_append_charsequence_null_appends_null_literal() {
    let mut ctx = mock_ctx();
    let sb = make_sb(&mut ctx);
    let r = native_sb_append_charsequence(
        &mut ctx,
        &[Value::Object(Some(sb)), Value::Object(None)],
    );
    assert!(matches!(r.unwrap(), Some(Value::Object(Some(_)))));
    let out_r = native_sb_to_string(&mut ctx, &[Value::Object(Some(sb))]).unwrap();
    let Some(Value::Object(Some(out_obj))) = out_r else {
        panic!()
    };
    assert_eq!(ctx.read_string(out_obj).unwrap(), "null");
}

#[test]
fn sb_append_charsequence_off_len_slice() {
    let mut ctx = mock_ctx();
    let sb = make_sb(&mut ctx);
    let s = ctx.create_string("abcdefg");
    let r = native_sb_append_charsequence_off_len(
        &mut ctx,
        &[
            Value::Object(Some(sb)),
            Value::Object(Some(s)),
            Value::Int(2),
            Value::Int(5),
        ],
    );
    assert!(matches!(r.unwrap(), Some(Value::Object(Some(_)))));
    let out_r = native_sb_to_string(&mut ctx, &[Value::Object(Some(sb))]).unwrap();
    let Some(Value::Object(Some(out_obj))) = out_r else {
        panic!()
    };
    assert_eq!(ctx.read_string(out_obj).unwrap(), "cde");
}

/// The pin this test carries was REVERSED on 2026-08-12.
///
/// It used to assert that `[-1, 100)` over `"abc"` clamps to `[0, 3)` and
/// appends `"abc"`, which pinned the clamp W7-3 listed as a defect and
/// declined to reverse. JDK 25's body is `checkRange(start, end,
/// s.length())` and every out-of-range end of that window throws
/// `IndexOutOfBoundsException` — the sibling `append(char[], int, int)`
/// has raised it since W7-3. A clamp is not a lenient success, it is a
/// SHORT append reported as a complete one, so the builder ends up holding
/// text the caller never asked for.
///
/// Both polarities are asserted here, because a check that only rejects
/// would pass on a native that rejects everything.
/// docs/feature-designs/native-dispatch-architecture.md
#[test]
fn sb_append_charsequence_off_len_rejects_an_out_of_range_window() {
    let mut ctx = mock_ctx();
    let sb = make_sb(&mut ctx);
    let s = ctx.create_string("abc");
    for (start, end) in [(-1, 3), (0, 100), (-1, 100), (2, 1)] {
        let e = native_sb_append_charsequence_off_len(
            &mut ctx,
            &[
                Value::Object(Some(sb)),
                Value::Object(Some(s)),
                Value::Int(start),
                Value::Int(end),
            ],
        )
        .expect_err(&format!("[{start}, {end}) over \"abc\" must be refused"));
        // `checkRange`, not `checkRangeSIOOBE`: the javadoc names the plain
        // IndexOutOfBoundsException for this overload, and a caller
        // catching SIOOBE specifically must NOT see it.
        assert_eq!(err_kind(&ctx, &e), "ioobe", "[{start}, {end})");
    }
    // The builder is untouched by every refusal above — a rejected append
    // must not be a partial one.
    let out_r = native_sb_to_string(&mut ctx, &[Value::Object(Some(sb))]).unwrap();
    let Some(Value::Object(Some(out_obj))) = out_r else {
        panic!()
    };
    assert_eq!(ctx.read_string(out_obj).unwrap(), "");
    // The positive half: the in-range boundary window still appends.
    let r = native_sb_append_charsequence_off_len(
        &mut ctx,
        &[
            Value::Object(Some(sb)),
            Value::Object(Some(s)),
            Value::Int(0),
            Value::Int(3),
        ],
    );
    assert!(matches!(r.unwrap(), Some(Value::Object(Some(_)))));
    let out_r = native_sb_to_string(&mut ctx, &[Value::Object(Some(sb))]).unwrap();
    let Some(Value::Object(Some(out_obj))) = out_r else {
        panic!()
    };
    assert_eq!(ctx.read_string(out_obj).unwrap(), "abc");
}

/// `s == null` becomes the four-character `"null"` BEFORE the range check,
/// so the window is checked against 4 rather than against the caller's
/// numbers — `append(null, 0, 9)` throws, `append(null, 1, 3)` appends
/// `"ul"`. Getting the ORDER wrong here reads as a harmless difference and
/// is not: it decides whether a null sequence can produce an exception at
/// all.
#[test]
fn sb_append_charsequence_off_len_null_is_the_null_literal_then_checked() {
    let mut ctx = mock_ctx();
    let sb = make_sb(&mut ctx);
    let e = native_sb_append_charsequence_off_len(
        &mut ctx,
        &[
            Value::Object(Some(sb)),
            Value::Object(None),
            Value::Int(0),
            Value::Int(9),
        ],
    )
    .expect_err("[0, 9) over the \"null\" literal must be refused");
    assert_eq!(err_kind(&ctx, &e), "ioobe");
    let r = native_sb_append_charsequence_off_len(
        &mut ctx,
        &[
            Value::Object(Some(sb)),
            Value::Object(None),
            Value::Int(1),
            Value::Int(3),
        ],
    );
    assert!(matches!(r.unwrap(), Some(Value::Object(Some(_)))));
    let out_r = native_sb_to_string(&mut ctx, &[Value::Object(Some(sb))]).unwrap();
    let Some(Value::Object(Some(out_obj))) = out_r else {
        panic!()
    };
    assert_eq!(ctx.read_string(out_obj).unwrap(), "ul");
}

/// `appendCodePoint` admits every BMP code point INCLUDING lone surrogates
/// (JDK: `isBmpCodePoint` is `cp >>> 16 == 0`, so `Character.toChars` — the
/// only thrower — is never reached for them), encodes a supplementary one
/// as a surrogate pair, and REFUSES anything outside `0..=0x10FFFF`.
///
/// The refusal is the half that regressed: the old body truncated an
/// invalid code point to `cp as u16`, writing a character the caller never
/// named. The lone-surrogate case is asserted alongside it because that is
/// the input the truncation was justified by, and a fix that "tightened"
/// this into `char::from_u32` would break it.
#[test]
fn sb_append_code_point_admits_surrogates_and_refuses_non_code_points() {
    let mut ctx = mock_ctx();
    let sb = make_sb(&mut ctx);
    for cp in [0x41, 0x1_F600, 0xD800, 0xDFFF, 0x10_FFFF] {
        native_sb_append_code_point(&mut ctx, &[Value::Object(Some(sb)), Value::Int(cp)])
            .unwrap_or_else(|_| panic!("appendCodePoint(0x{cp:X}) must be admitted"));
    }
    // 1 + 2 + 1 + 1 + 2 code units.
    let out_r = native_sb_to_string(&mut ctx, &[Value::Object(Some(sb))]).unwrap();
    let Some(Value::Object(Some(out_obj))) = out_r else {
        panic!()
    };
    let text = ctx.read_string(out_obj).unwrap();
    assert_eq!(text.encode_utf16().count(), 7, "got {text:?}");

    for cp in [-1, 0x11_0000, i32::MIN, i32::MAX] {
        let r =
            native_sb_append_code_point(&mut ctx, &[Value::Object(Some(sb)), Value::Int(cp)]);
        let refused = matches!(
            r,
            Err(cratonvm_types::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(
                    cratonvm_types::error::RuntimeError::IllegalArgumentException { .. }
                )
            ))
        );
        assert!(
            refused,
            "appendCodePoint(0x{cp:X}) must raise IllegalArgumentException"
        );
    }
}

/// The four scalar `insert` overloads that had no native at all. Each
/// value is chosen so a wrong renderer cannot produce it by accident: a
/// long past `int` range, a `float`/`double` pair that `Float.toString` and
/// `Double.toString` spell differently from Rust's `Display` for other
/// inputs, and `checkOffset` on the far side.
#[test]
fn sb_scalar_insert_overloads_render_and_check_the_offset() {
    for (args_tail, expected) in [
        (Value::Int(1), "atruec"),
        (Value::Long(1_234_567_890_123), "a1234567890123c"),
        (Value::Float(1.5), "a1.5c"),
        (Value::Double(2.25), "a2.25c"),
    ] {
        let mut ctx = mock_ctx();
        let sb = make_sb_with(&mut ctx, "ac");
        let args = [Value::Object(Some(sb)), Value::Int(1), args_tail];
        let r = match args_tail {
            Value::Int(_) => native_sb_insert_boolean(&mut ctx, &args),
            Value::Long(_) => native_sb_insert_long(&mut ctx, &args),
            Value::Float(_) => native_sb_insert_float(&mut ctx, &args),
            _ => native_sb_insert_double(&mut ctx, &args),
        };
        assert!(matches!(r.unwrap(), Some(Value::Object(Some(_)))));
        let out_r = native_sb_to_string(&mut ctx, &[Value::Object(Some(sb))]).unwrap();
        let Some(Value::Object(Some(out_obj))) = out_r else {
            panic!()
        };
        assert_eq!(ctx.read_string(out_obj).unwrap(), expected);
    }

    // `checkOffset(offset, count)` — the same check the six overloads that
    // already had natives run. An offset past the end must throw, not
    // append at the end.
    let mut ctx = mock_ctx();
    let sb = make_sb_with(&mut ctx, "ab");
    let e = native_sb_insert_long(
        &mut ctx,
        &[Value::Object(Some(sb)), Value::Int(99), Value::Long(1)],
    )
    .expect_err("insert(99, 1L) on a 2-char builder must be refused");
    assert_eq!(err_kind(&ctx, &e), "sioobe");
}

#[test]
fn sb_append_charsequence_off_len_empty_slice() {
    let mut ctx = mock_ctx();
    let sb = make_sb(&mut ctx);
    let s = ctx.create_string("abc");
    let r = native_sb_append_charsequence_off_len(
        &mut ctx,
        &[
            Value::Object(Some(sb)),
            Value::Object(Some(s)),
            Value::Int(1),
            Value::Int(1),
        ],
    );
    assert!(matches!(r.unwrap(), Some(Value::Object(Some(_)))));
    let out_r = native_sb_to_string(&mut ctx, &[Value::Object(Some(sb))]).unwrap();
    let Some(Value::Object(Some(out_obj))) = out_r else {
        panic!()
    };
    assert_eq!(ctx.read_string(out_obj).unwrap(), "");
}

#[test]
fn sb_append_charsequence_registration_includes_cs_variants() {
    let mut registry = NativeMethodRegistry::new();
    register_string_builder_natives(&mut registry, "java/lang/StringBuilder");
    assert!(registry
        .find(
            "java/lang/StringBuilder",
            "append",
            "(Ljava/lang/CharSequence;II)Ljava/lang/StringBuilder;",
        )
        .is_some());
    assert!(registry
        .find(
            "java/lang/StringBuilder",
            "append",
            "(Ljava/lang/CharSequence;)Ljava/lang/StringBuilder;",
        )
        .is_some());
}

#[test]
fn sb_append_charsequence_off_len_registered_for_abstract_sb() {
    let mut registry = NativeMethodRegistry::new();
    register_string_builder_natives(&mut registry, "java/lang/AbstractStringBuilder");
    // Real JDK dispatch names the AbstractStringBuilder return type.
    assert!(registry
        .find(
            "java/lang/AbstractStringBuilder",
            "append",
            "(Ljava/lang/CharSequence;II)Ljava/lang/AbstractStringBuilder;",
        )
        .is_some());
}

#[test]
fn sb_abstract_builder_registration_includes_layout_sensitive_append_variants() {
    let mut registry = NativeMethodRegistry::new();
    register_string_builder_natives(&mut registry, "java/lang/AbstractStringBuilder");
    for (name, desc) in [
        (
            "append",
            "(Ljava/lang/String;)Ljava/lang/AbstractStringBuilder;",
        ),
        (
            "append",
            "(Ljava/lang/Object;)Ljava/lang/AbstractStringBuilder;",
        ),
        ("append", "([C)Ljava/lang/AbstractStringBuilder;"),
        ("append", "([CII)Ljava/lang/AbstractStringBuilder;"),
        ("append", "(I)Ljava/lang/AbstractStringBuilder;"),
        ("appendCodePoint", "(I)Ljava/lang/AbstractStringBuilder;"),
        ("repeat", "(II)Ljava/lang/AbstractStringBuilder;"),
        (
            "repeat",
            "(Ljava/lang/CharSequence;I)Ljava/lang/AbstractStringBuilder;",
        ),
    ] {
        assert!(
            registry
                .find("java/lang/AbstractStringBuilder", name, desc)
                .is_some(),
            "missing native for {name}{desc}"
        );
    }
}

#[test]
fn sb_appendable_bridge_registration_includes_charsequence_variants() {
    let mut registry = NativeMethodRegistry::new();
    register_string_builder_natives(&mut registry, "java/lang/StringBuilder");
    assert!(registry
        .find(
            "java/lang/StringBuilder",
            "append",
            "(C)Ljava/lang/Appendable;"
        )
        .is_some());
    assert!(registry
        .find(
            "java/lang/StringBuilder",
            "append",
            "(Ljava/lang/CharSequence;)Ljava/lang/Appendable;",
        )
        .is_some());
    assert!(registry
        .find(
            "java/lang/StringBuilder",
            "append",
            "(Ljava/lang/CharSequence;II)Ljava/lang/Appendable;",
        )
        .is_some());
}

// -----------------------------------------------------------------------
// StringUTF16.isBigEndian — used during <clinit> to pick a byte order
// for the compact-string byte[] layout. CratonVM always stores compact
// UTF-16 strings little-endian (see `create_java_string` and the decoders
// in this file), so this must return `false` to keep the real-JDK
// StringUTF16 bytecode consistent with CratonVM's native string code. On
// the x86-64 target this VM runs on, that also matches the host order.
// -----------------------------------------------------------------------

// FIX: was `..._returns_true` asserting Int(1). CratonVM is little-endian,
// so isBigEndian() must be false; align expectation with host endianness
// (the same model as `..._ignores_extra_args`).
#[test]
fn t18_k6_string_utf16_is_big_endian_returns_false() {
    let mut ctx = mock_ctx();
    let result = native_string_utf16_is_big_endian(&mut ctx, &[])
        .unwrap()
        .unwrap();
    // Little-endian on x86-64: isBigEndian() is false.
    let expected_int = if cfg!(target_endian = "big") { 1 } else { 0 };
    assert_eq!(result, Value::Int(expected_int));
}

#[test]
fn string_utf16_natives_are_registered() {
    let mut registry = NativeMethodRegistry::new();
    register_string_utf16_natives(&mut registry);
    assert!(registry
        .find("java/lang/StringUTF16", "isBigEndian", "()Z")
        .is_some());
    assert!(registry
        .find("java/lang/StringUTF16", "getChars", "([BII[CI)V")
        .is_some());
}

#[test]
fn string_utf16_get_chars_copies_little_endian_code_units() {
    let mut ctx = mock_ctx();
    let source = ctx.new_array(ArrayElementType::Byte, 6);
    // CratonVM compact UTF-16 storage is little-endian: A, omega, B.
    for (index, byte) in [0x41, 0x00, 0xA9, 0x03, 0x42, 0x00].into_iter().enumerate() {
        ctx.set_array_element(source, index, Value::Int(byte));
    }
    let dst = ctx.new_array(ArrayElementType::Char, 2);
    assert_eq!(
        native_string_utf16_get_chars(
            &mut ctx,
            &[
                Value::Object(Some(source)),
                Value::Int(1),
                Value::Int(3),
                Value::Object(Some(dst)),
                Value::Int(0),
            ],
        )
        .unwrap(),
        None
    );
    assert_eq!(ctx.get_array_element(dst, 0), Value::Int(0x03A9));
    assert_eq!(ctx.get_array_element(dst, 1), Value::Int(0x0042));
}

#[test]
fn string_utf16_get_chars_empty_range_does_not_dereference_arrays() {
    let mut ctx = mock_ctx();
    assert_eq!(
        native_string_utf16_get_chars(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Int(3),
                Value::Int(3),
                Value::Object(None),
                Value::Int(-1),
            ],
        )
        .unwrap(),
        None
    );
}

#[test]
fn string_utf16_is_big_endian_ignores_extra_args() {
    // The Java verifier guarantees correct arity at the call site, but
    // our impl should not panic if called with unexpected trailing args.
    let mut ctx = mock_ctx();
    let r = native_string_utf16_is_big_endian(&mut ctx, &[Value::Int(0), Value::Int(42)])
        .unwrap()
        .unwrap();
    let expected_int = if cfg!(target_endian = "big") { 1 } else { 0 };
    assert_eq!(r, Value::Int(expected_int));
}

// -----------------------------------------------------------------------
// String bounds checks — must report StringIndexOutOfBoundsException,
// never ArrayIndexOutOfBoundsException. The two are siblings, so
// `catch (StringIndexOutOfBoundsException)` does not see an AIOOBE.
// -----------------------------------------------------------------------

/// Name the exception class a native raised, whichever of the two shapes
/// it used.
///
/// A native can fail two ways and the class matters in both: a
/// `RuntimeError` the VM maps to a class later, or an already-materialised
/// throwable (`ExceptionThrown`), which is what
/// `crate::preconditions::throw_out_of_bounds` produces when it has to
/// build the class an application-supplied exception formatter asked for.
/// Classifying only the first shape would report `other-failed` for the
/// second, so a test asserting `"sioobe"` would fail on a *correct* answer
/// and — worse — a test asserting anything else would pass on a wrong one.
fn err_kind(
    ctx: &dyn NativeContext,
    e: &cratonvm_types::error::MethodCallFailed,
) -> &'static str {
    match e {
        cratonvm_types::error::MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::Runtime(re),
        ) => match re {
            cratonvm_types::error::RuntimeError::StringIndexOutOfBoundsException { .. } => {
                "sioobe"
            }
            cratonvm_types::error::RuntimeError::ArrayIndexOutOfBoundsException { .. } => {
                "aioobe"
            }
            cratonvm_types::error::RuntimeError::IndexOutOfBoundsException { .. } => "ioobe",
            _ => "other-runtime",
        },
        cratonvm_types::error::MethodCallFailed::ExceptionThrown(obj) => {
            let class_id = ctx.class_id_of_object(*obj);
            match ctx.class_name_of_id(class_id).unwrap_or_default().as_str() {
                "java/lang/StringIndexOutOfBoundsException" => "sioobe",
                "java/lang/ArrayIndexOutOfBoundsException" => "aioobe",
                "java/lang/IndexOutOfBoundsException" => "ioobe",
                _ => "other-thrown",
            }
        }
        _ => "other-failed",
    }
}

#[test]
fn bounds_off_count_accepts_an_in_range_window() {
    assert_eq!(bounds_off_count_violation(2, 3, 10), None);
    // Empty-string edge case: BC `PKCS12$Mappings` reaches this during
    // `Provider.put` for keys whose substring extraction falls into a
    // zero-length window.
    assert_eq!(bounds_off_count_violation(0, 0, 0), None);
    assert_eq!(bounds_off_count_violation(0, 10, 10), None);
}

#[test]
fn bounds_off_count_names_the_offending_argument() {
    // The JDK's SIOOBE message convention: `offset` when it is the bad
    // one, otherwise `count`.
    assert_eq!(bounds_off_count_violation(-1, 2, 5), Some(-1));
    assert_eq!(bounds_off_count_violation(0, -2, 5), Some(-2));
    assert_eq!(bounds_off_count_violation(0, 6, 5), Some(6));
}

#[test]
fn bounds_off_count_widens_before_adding() {
    // (offset=i32::MAX, count=1, length=i32::MAX) is out of bounds; without
    // the i64 widening the sum wraps negative and slips past the check.
    assert_eq!(bounds_off_count_violation(i32::MAX, 1, i32::MAX), Some(1));
}

/// The inverse of the old `f4_check_bounds_natives_registered_on_string`.
///
/// `String.checkBoundsBeginEnd` / `checkBoundsOffCount` are no longer
/// intercepted: their real bytecode hands `Preconditions` the
/// `SIOOBE_FORMATTER`, and `crate::preconditions` honours it. Re-adding an
/// interceptor here would mean the underlying override had regressed —
/// fix that instead, because a native here reaches only the `String`
/// callers and leaves NIO's on the generic path.
#[test]
fn string_bounds_helpers_are_left_to_their_bytecode() {
    let mut registry = NativeMethodRegistry::new();
    register_string_utf16_natives(&mut registry);
    assert!(
        registry
            .find("java/lang/String", "checkBoundsBeginEnd", "(III)V")
            .is_none(),
        "checkBoundsBeginEnd is the real bytecode's job now — see crate::preconditions"
    );
    assert!(
        registry
            .find("java/lang/String", "checkBoundsOffCount", "(III)I")
            .is_none(),
        "checkBoundsOffCount is the real bytecode's job now — see crate::preconditions"
    );
}

#[test]
fn f4_code_point_at_negative_throws_sioobe_not_aioobe() {
    // Audit: native_string_code_point_at used to throw AIOOBE on out-of-range.
    // Spec mandates SIOOBE so callers' `catch (StringIndexOutOfBoundsException)`
    // handlers fire correctly.
    let mut ctx = mock_ctx();
    let s = ctx.create_string("hi");
    let err = native_string_code_point_at(&mut ctx, &[Value::Object(Some(s)), Value::Int(-1)])
        .unwrap_err();
    assert_eq!(err_kind(&ctx, &err), "sioobe");
}

#[test]
fn f4_code_point_at_too_large_throws_sioobe_not_aioobe() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("hi");
    let err = native_string_code_point_at(&mut ctx, &[Value::Object(Some(s)), Value::Int(99)])
        .unwrap_err();
    assert_eq!(err_kind(&ctx, &err), "sioobe");
}

/// `codePointAt` reads one or two code units, and every answer must be the
/// one the full decode gives -- LATIN-1, UTF-16 BMP, a supplementary pair
/// from either half, and a lone surrogate at the end.
#[test]
fn code_point_at_per_unit_read_matches_the_full_decode() {
    let mut ctx = mock_ctx();
    for text in [
        "plain ascii",
        "caf\u{e9} \u{3b1}\u{3b2}",
        "a\u{1F600}b\u{10FFFF}",
    ] {
        let s = ctx.create_string(text);
        let units = read_string_chars(&ctx, s);
        assert_eq!(units, text.encode_utf16().collect::<Vec<_>>());
        for i in 0..units.len() {
            let expected = char::decode_utf16(units[i..].iter().copied())
                .next()
                .map(|r| r.map_or(units[i] as i32, |c| c as i32))
                .unwrap();
            let got = native_string_code_point_at(
                &mut ctx,
                &[Value::Object(Some(s)), Value::Int(i as i32)],
            )
            .unwrap();
            assert_eq!(got, Some(Value::Int(expected)), "{text:?} at {i}");
        }
    }
}

// -----------------------------------------------------------------------
// AbstractStringBuilder.getChars(srcBegin, srcEnd, dst, dstBegin) — must
// validate BOTH the source window (SIOOBE) and the destination window
// (NPE on null dst, AIOOBE on negative dstBegin / overrun) before copying,
// instead of silently dropping out-of-bounds writes.
// -----------------------------------------------------------------------

/// Build a StringBuilder pre-loaded with `text` for getChars tests.
fn make_sb_with(ctx: &mut dyn NativeContext, text: &str) -> cratonvm_types::ObjectRef {
    let sb = make_sb(ctx);
    let chars: Vec<u16> = text.encode_utf16().collect();
    let buf = ctx.new_array(ArrayElementType::Char, chars.len().max(1));
    for (i, &c) in chars.iter().enumerate() {
        ctx.set_array_element(buf, i, Value::Int(i32::from(c)));
    }
    ctx.set_field(sb, 0, Value::Object(Some(buf)));
    // `make_sb` allocates 4 slots (mimicking the real JDK
    // value/coder/count layout width), so the count belongs in slot 2,
    // not slot 1 -- go through `sb_set_count` rather than poking a
    // slot directly so this stays correct regardless of the mock's
    // allocated width. A raw `set_field(sb, 1, ...)` here only wrote
    // the slot `sb_state` reads as a *fallback*, which a 4-slot object
    // never falls back to (slot 2 already holds a valid Int from
    // `make_sb`'s `native_sb_init_default` call) -- every
    // `sb_get_chars`/`sb_get_value`/`sb_get_coder` test using this
    // helper silently saw count=0 regardless of `text`.
    sb_set_count(ctx, sb, chars.len() as i32);
    sb
}

#[test]
fn sb_get_chars_valid_copy() {
    let mut ctx = mock_ctx();
    let sb = make_sb_with(&mut ctx, "hello");
    let dst = ctx.new_array(ArrayElementType::Char, 8);
    // Copy "ell" (indices 1..4) into dst starting at offset 2.
    let r = native_sb_get_chars(
        &mut ctx,
        &[
            Value::Object(Some(sb)),
            Value::Int(1),
            Value::Int(4),
            Value::Object(Some(dst)),
            Value::Int(2),
        ],
    );
    assert!(r.unwrap().is_none());
    let read = |i: usize| match ctx.get_array_element(dst, i) {
        Value::Int(v) => v as u16,
        _ => 0,
    };
    assert_eq!(read(2), u16::from(b'e'));
    assert_eq!(read(3), u16::from(b'l'));
    assert_eq!(read(4), u16::from(b'l'));
    // Untouched slots stay zero.
    assert_eq!(read(0), 0);
    assert_eq!(read(5), 0);
}

#[test]
fn sb_get_chars_null_dst_throws_npe() {
    let mut ctx = mock_ctx();
    let sb = make_sb_with(&mut ctx, "abc");
    let err = native_sb_get_chars(
        &mut ctx,
        &[
            Value::Object(Some(sb)),
            Value::Int(0),
            Value::Int(3),
            Value::Object(None),
            Value::Int(0),
        ],
    )
    .unwrap_err();
    assert!(matches!(
        err,
        cratonvm_types::error::MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::Runtime(
                cratonvm_types::error::RuntimeError::NullPointerException { .. }
            )
        )
    ));
}

/// The detail message of an `IndexOutOfBoundsException`-family failure.
///
/// The exact wording separates `Preconditions`' `checkFromToIndex` shape
/// (`Range [a, b) out of bounds for length n`) from the `checkIndex` shape,
/// which is how a range check that reports the wrong WINDOW is caught even
/// when it reports the right class.
fn err_message(e: &cratonvm_types::error::MethodCallFailed) -> Option<String> {
    match e {
        cratonvm_types::error::MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::Runtime(re),
        ) => match re {
            cratonvm_types::error::RuntimeError::IndexOutOfBoundsException { message }
            | cratonvm_types::error::RuntimeError::StringIndexOutOfBoundsException {
                message,
                ..
            }
            | cratonvm_types::error::RuntimeError::ArrayIndexOutOfBoundsException {
                message,
                ..
            } => message.clone(),
            _ => None,
        },
        _ => None,
    }
}

// `AbstractStringBuilder.getChars` throws THREE different classes depending
// on which argument is wrong, and the choice is not the intuitive one. Its
// two range checks are one line apart in the JDK and pass DIFFERENT
// formatters — `SIOOBE_FORMATTER` for the source window, `IOOBE_FORMATTER`
// for the destination — so the destination case is the PLAIN
// `IndexOutOfBoundsException`, the superclass of both the
// `ArrayIndexOutOfBoundsException` the element stores suggest and the
// `StringIndexOutOfBoundsException` its own source-side neighbour throws.
//
// These assert through `err_kind`, which discriminates all three, because a
// `matches!` on `IndexOutOfBoundsException` alone cannot: the two wrong
// answers are SUBCLASSES, so any assertion phrased on the supertype passes
// on the broken code. MEASURED against jdk-25.0.3.9; see
// `docs/feature-designs/native-dispatch-architecture.md`.

#[test]
fn sb_get_chars_too_small_dst_throws_the_plain_ioobe_not_a_subclass() {
    let mut ctx = mock_ctx();
    let sb = make_sb_with(&mut ctx, "abcde");
    let dst = ctx.new_array(ArrayElementType::Char, 2);
    // `RJdkBridge1`'s `sbidx-step=getChars into a too-small array`, verbatim.
    let err = native_sb_get_chars(
        &mut ctx,
        &[
            Value::Object(Some(sb)),
            Value::Int(0),
            Value::Int(3),
            Value::Object(Some(dst)),
            Value::Int(0),
        ],
    )
    .unwrap_err();
    assert_eq!(err_kind(&ctx, &err), "ioobe");
    // The window named is the DESTINATION one, `[dstBegin, dstBegin + n)`
    // against `dst.length` — not the source window `[0, 3)` against 5.
    assert_eq!(
        err_message(&err).as_deref(),
        Some("Range [0, 3) out of bounds for length 2")
    );
}

#[test]
fn sb_get_chars_negative_dst_begin_throws_the_plain_ioobe() {
    let mut ctx = mock_ctx();
    let sb = make_sb_with(&mut ctx, "abc");
    let dst = ctx.new_array(ArrayElementType::Char, 8);
    let err = native_sb_get_chars(
        &mut ctx,
        &[
            Value::Object(Some(sb)),
            Value::Int(0),
            Value::Int(3),
            Value::Object(Some(dst)),
            Value::Int(-1),
        ],
    )
    .unwrap_err();
    assert_eq!(err_kind(&ctx, &err), "ioobe");
    assert_eq!(
        err_message(&err).as_deref(),
        Some("Range [-1, 2) out of bounds for length 8")
    );
}

#[test]
fn sb_get_chars_dst_overrun_throws_the_plain_ioobe() {
    let mut ctx = mock_ctx();
    let sb = make_sb_with(&mut ctx, "abcde");
    let dst = ctx.new_array(ArrayElementType::Char, 4);
    // Copying 5 chars starting at offset 2 would write through index 6 > len 4.
    let err = native_sb_get_chars(
        &mut ctx,
        &[
            Value::Object(Some(sb)),
            Value::Int(0),
            Value::Int(5),
            Value::Object(Some(dst)),
            Value::Int(2),
        ],
    )
    .unwrap_err();
    assert_eq!(err_kind(&ctx, &err), "ioobe");
    assert_eq!(
        err_message(&err).as_deref(),
        Some("Range [2, 7) out of bounds for length 4")
    );
    // The out-of-bounds store must NOT have silently written anything past
    // the array; in-range slots remain at their zero default.
    for i in 0..ctx.array_length(dst) {
        assert_eq!(ctx.get_array_element(dst, i), Value::Int(0));
    }
}

#[test]
fn sb_get_chars_bad_src_range_beats_a_null_destination() {
    let mut ctx = mock_ctx();
    let sb = make_sb_with(&mut ctx, "abcde");
    // The source check runs BEFORE `dst` is dereferenced, so `srcBegin >
    // srcEnd` with a null `dst` is a StringIndexOutOfBoundsException and
    // NOT the NullPointerException that checking the argument first would
    // give. Ordering, not just class choice.
    let err = native_sb_get_chars(
        &mut ctx,
        &[
            Value::Object(Some(sb)),
            Value::Int(3),
            Value::Int(1),
            Value::Object(None),
            Value::Int(0),
        ],
    )
    .unwrap_err();
    assert_eq!(err_kind(&ctx, &err), "sioobe");
    assert_eq!(
        err_message(&err).as_deref(),
        Some("Range [3, 1) out of bounds for length 5")
    );
}

#[test]
fn sb_get_chars_null_dst_throws_npe_even_when_nothing_would_be_copied() {
    let mut ctx = mock_ctx();
    let sb = make_sb_with(&mut ctx, "abcde");
    // `dst.length` is read unconditionally once the source window is
    // accepted, so a zero-length copy into a null array still throws.
    let err = native_sb_get_chars(
        &mut ctx,
        &[
            Value::Object(Some(sb)),
            Value::Int(0),
            Value::Int(0),
            Value::Object(None),
            Value::Int(0),
        ],
    )
    .unwrap_err();
    assert!(matches!(
        err,
        cratonvm_types::error::MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::Runtime(
                cratonvm_types::error::RuntimeError::NullPointerException { .. }
            )
        )
    ));
}

#[test]
fn sb_get_chars_bad_src_range_throws_sioobe() {
    let mut ctx = mock_ctx();
    let sb = make_sb_with(&mut ctx, "abc");
    let dst = ctx.new_array(ArrayElementType::Char, 8);
    // srcEnd (9) exceeds the builder length (3) -> SIOOBE, not AIOOBE.
    let err = native_sb_get_chars(
        &mut ctx,
        &[
            Value::Object(Some(sb)),
            Value::Int(0),
            Value::Int(9),
            Value::Object(Some(dst)),
            Value::Int(0),
        ],
    )
    .unwrap_err();
    assert_eq!(err_kind(&ctx, &err), "sioobe");
}

#[test]
fn sb_get_chars_empty_window_at_dst_end_is_valid() {
    let mut ctx = mock_ctx();
    let sb = make_sb_with(&mut ctx, "abc");
    let dst = ctx.new_array(ArrayElementType::Char, 4);
    // Zero-length copy with dstBegin == dst.length is exactly in bounds.
    let r = native_sb_get_chars(
        &mut ctx,
        &[
            Value::Object(Some(sb)),
            Value::Int(1),
            Value::Int(1),
            Value::Object(Some(dst)),
            Value::Int(4),
        ],
    );
    assert!(r.unwrap().is_none());
}

// `codePointCount` is the ONE range check in this class that the JDK gives a
// null formatter (`Preconditions.checkFromToIndex(begin, end, count,
// null)`), so it yields the PLAIN `IndexOutOfBoundsException` while its
// `delete`/`replace`/`substring` neighbours, checked by the same shared
// helper here, yield `StringIndexOutOfBoundsException`. The paired
// neighbour assertions below exist so that "fixing" this by changing the
// shared helper fails loudly instead of silently regressing four methods.

#[test]
fn sb_code_point_count_bad_range_throws_the_plain_ioobe() {
    for (begin, end, want_msg) in [
        (0, 6, "Range [0, 6) out of bounds for length 5"),
        (-1, 3, "Range [-1, 3) out of bounds for length 5"),
        (3, 1, "Range [3, 1) out of bounds for length 5"),
    ] {
        let mut ctx = mock_ctx();
        let sb = make_sb_with(&mut ctx, "abcde");
        let err = native_sb_code_point_count(
            &mut ctx,
            &[Value::Object(Some(sb)), Value::Int(begin), Value::Int(end)],
        )
        .unwrap_err();
        assert_eq!(
            err_kind(&ctx, &err),
            "ioobe",
            "codePointCount({begin}, {end})"
        );
        assert_eq!(err_message(&err).as_deref(), Some(want_msg));
    }
}

#[test]
fn sb_substring_and_delete_keep_the_string_subclass_that_code_point_count_drops() {
    let mut ctx = mock_ctx();
    let sb = make_sb_with(&mut ctx, "abcde");
    let err = native_sb_substring_range(
        &mut ctx,
        &[Value::Object(Some(sb)), Value::Int(3), Value::Int(1)],
    )
    .unwrap_err();
    assert_eq!(err_kind(&ctx, &err), "sioobe");

    let sb = make_sb_with(&mut ctx, "abcde");
    let err = native_sb_delete(
        &mut ctx,
        &[Value::Object(Some(sb)), Value::Int(3), Value::Int(1)],
    )
    .unwrap_err();
    assert_eq!(err_kind(&ctx, &err), "sioobe");
}


#[test]
fn sb_code_point_count_valid_window_still_counts_surrogate_pairs() {
    let mut ctx = mock_ctx();
    // "a" + U+10437 (a surrogate PAIR) + "b" = 4 units, 3 code points.
    let sb = make_sb_with(&mut ctx, "a\u{10437}b");
    let r = native_sb_code_point_count(
        &mut ctx,
        &[Value::Object(Some(sb)), Value::Int(0), Value::Int(4)],
    )
    .unwrap();
    assert_eq!(r, Some(Value::Int(3)));
}

#[test]
fn sb_init_capacity_negative_throws_negative_array_size_not_a_clamp() {
    // `AbstractStringBuilder(int)` is just `new byte[capacity]`, so a
    // negative capacity throws NegativeArraySizeException carrying the raw
    // size. Clamping to zero built a usable empty builder instead.
    //
    // Deliberately a DIFFERENT class from `setLength(-1)`'s
    // StringIndexOutOfBoundsException — the two negative-length paths in
    // this class do not agree, and `RJdkBridge1` asserts both in a row.
    for size in [-1_i32, -7] {
        let mut ctx = mock_ctx();
        let cid = ctx
            .ensure_class_initialized("java/lang/StringBuilder")
            .unwrap();
        let sb = ctx.alloc_object(cid, 4);
        let err =
            native_sb_init_capacity(&mut ctx, &[Value::Object(Some(sb)), Value::Int(size)])
                .unwrap_err();
        assert!(
            matches!(
                err,
                cratonvm_types::error::MethodCallFailed::InternalError(
                    cratonvm_types::error::VmError::Runtime(
                        cratonvm_types::error::RuntimeError::NegativeArraySizeException {
                            size: s
                        }
                    )
                ) if s == size
            ),
            "new StringBuilder({size}) must throw NegativeArraySizeException({size})"
        );
    }
}

#[test]
fn sb_init_capacity_zero_and_positive_still_build_an_empty_builder() {
    for size in [0_i32, 5] {
        let mut ctx = mock_ctx();
        let cid = ctx
            .ensure_class_initialized("java/lang/StringBuilder")
            .unwrap();
        let sb = ctx.alloc_object(cid, 4);
        native_sb_init_capacity(&mut ctx, &[Value::Object(Some(sb)), Value::Int(size)])
            .unwrap();
        // A capacity is not a length: the builder starts empty either way.
        let len = native_sb_length(&mut ctx, &[Value::Object(Some(sb))]).unwrap();
        assert_eq!(
            len,
            Some(Value::Int(0)),
            "new StringBuilder({size}).length()"
        );
    }
}

// -----------------------------------------------------------------------
// BUG-TC0622: AbstractStringBuilder.getValue()[B / getCoder()B — synthetic
// char[] buffer must present a compact byte[]+coder view matching CratonVM's
// own String layout, so real-JDK String.nonSyncContentEquals computes right.
// -----------------------------------------------------------------------

fn read_bytes(ctx: &dyn NativeContext, arr: cratonvm_types::ObjectRef) -> Vec<u8> {
    (0..ctx.array_length(arr))
        .map(|i| match ctx.get_array_element(arr, i) {
            Value::Int(v) => v as u8,
            _ => 0,
        })
        .collect()
}

#[test]
fn sb_get_coder_latin1_for_ascii() {
    let mut ctx = mock_ctx();
    let sb = make_sb_with(&mut ctx, "http://localhost:8080");
    let r = native_sb_get_coder(&mut ctx, &[Value::Object(Some(sb))]).unwrap();
    assert_eq!(r, Some(Value::Int(0)), "all-ASCII builder must be LATIN1");
}

#[test]
fn sb_get_coder_utf16_when_non_latin1_present() {
    let mut ctx = mock_ctx();
    let sb = make_sb_with(&mut ctx, "caf\u{00e9}\u{4e2d}"); // contains U+4E2D > 0xFF
    let r = native_sb_get_coder(&mut ctx, &[Value::Object(Some(sb))]).unwrap();
    assert_eq!(r, Some(Value::Int(1)), "char > 0xFF must force UTF16");
}

#[test]
fn sb_get_value_latin1_one_byte_per_char() {
    let mut ctx = mock_ctx();
    let sb = make_sb_with(&mut ctx, "AbZ");
    let r = native_sb_get_value(&mut ctx, &[Value::Object(Some(sb))]).unwrap();
    let Some(Value::Object(Some(arr))) = r else {
        panic!("expected byte[]")
    };
    assert_eq!(read_bytes(&ctx, arr), vec![b'A', b'b', b'Z']);
}

#[test]
fn sb_get_value_utf16_little_endian_pairs() {
    let mut ctx = mock_ctx();
    let sb = make_sb_with(&mut ctx, "A\u{4e2d}"); // 'A'=0x0041, U+4E2D
    let r = native_sb_get_value(&mut ctx, &[Value::Object(Some(sb))]).unwrap();
    let Some(Value::Object(Some(arr))) = r else {
        panic!("expected byte[]")
    };
    // Little-endian: low byte first. 0x0041 -> [0x41,0x00]; 0x4E2D -> [0x2D,0x4E].
    assert_eq!(read_bytes(&ctx, arr), vec![0x41, 0x00, 0x2D, 0x4E]);
}

#[test]
fn sb_get_value_empty_builder_is_empty_array() {
    let mut ctx = mock_ctx();
    let sb = make_sb(&mut ctx);
    let r = native_sb_get_value(&mut ctx, &[Value::Object(Some(sb))]).unwrap();
    let Some(Value::Object(Some(arr))) = r else {
        panic!("expected byte[]")
    };
    assert_eq!(ctx.array_length(arr), 0);
}

#[test]
fn sb_get_value_getcoder_registered_for_abstract_sb() {
    let mut registry = NativeMethodRegistry::new();
    register_string_builder_natives(&mut registry, "java/lang/AbstractStringBuilder");
    assert!(registry
        .find("java/lang/AbstractStringBuilder", "getValue", "()[B")
        .is_some());
    assert!(registry
        .find("java/lang/AbstractStringBuilder", "getCoder", "()B")
        .is_some());
}

/// STR16-2: the builder-constructor shadows yield to bytecode only on a
/// real image, with the switch armed, and where the builders are the
/// JDK's own: `--real-jdk`, or `--compatible` with the
/// `jdk-string-builder-bodies` alignment (strings round, `str16`). Every
/// other combination keeps the native (`--compatible`'s natively-mutated
/// builders never write `maybeLatin1`, and the synthetic image's builders
/// are `char[]`-backed, which is what DF05 is about).
#[test]
fn str16_builder_ctor_yields_only_where_the_builders_are_the_jdks() {
    use cratonvm_types::compat::CompatibilityMode::{Compatible, JdkOnly};
    assert!(string_builder_ctor_yields_to_bytecode(
        JdkOnly, true, false, true
    ));
    assert!(string_builder_ctor_yields_to_bytecode(
        JdkOnly, true, true, true
    ));
    assert!(string_builder_ctor_yields_to_bytecode(
        Compatible, true, true, true
    ));
    for (mode, real_jdk, aligned, flag_on) in [
        (JdkOnly, true, true, false),
        (JdkOnly, false, true, true),
        (Compatible, true, false, true),
        (Compatible, false, true, true),
        (Compatible, true, true, false),
    ] {
        assert!(
            !string_builder_ctor_yields_to_bytecode(mode, real_jdk, aligned, flag_on),
            "{mode:?} real_jdk={real_jdk} aligned={aligned} flag={flag_on} must keep the native",
        );
    }
}

/// str17: both growth rules against a transcription of JDK 25's
/// `AbstractStringBuilder.newCapacity` / `ArraysSupport.newLength` (plain
/// arm), in characters: the same-coder `ensureCapacitySameCoder` and the
/// inflating `ensureCapacityNewCoder` (LATIN1 -> UTF16), whose preferred
/// growth is computed over the OLD one-byte-per-char length.
#[test]
fn str17_builder_growth_follows_the_jdk_for_both_coders() {
    // `newCapacity(value, newCoder, min)` with `value.length = old << coder`.
    fn jdk(old: usize, coder: u32, new_coder: u32, min: usize) -> usize {
        if min <= old {
            return old;
        }
        let old_length = old << coder;
        let new_length = min << new_coder;
        let growth = new_length - old_length;
        let pref = old_length + (2 << new_coder);
        (old_length + growth.max(pref)) >> new_coder
    }
    for old in [0usize, 1, 2, 3, 7, 16, 34, 100] {
        for min in [0usize, 1, 2, 3, 4, 9, 17, 35, 64, 250] {
            assert_eq!(
                sb_grow_units(old, min),
                jdk(old, 0, 0, min),
                "latin1 old={old} min={min}"
            );
            assert_eq!(
                sb_grow_units(old, min),
                jdk(old, 1, 1, min),
                "utf16 old={old} min={min}"
            );
            assert_eq!(
                sb_inflate_grow_units(old, min),
                jdk(old, 0, 1, min),
                "inflate old={old} min={min}"
            );
        }
    }
    // The measured case: capacity 1 holding "n", then 'Ď'.
    assert_eq!(sb_inflate_grow_units(1, 2), 3);
}

/// str16 (`HotSpotAlignment::JdkStringBuilderBodies`): `--compatible`
/// over a real JDK registers none of the builder `SyntheticStub` rows, so
/// the JDK's bodies run as under `--real-jdk`; with the row off (or on the
/// synthetic image) every row is registered as before. A `Bridge` row on
/// the same class is kept.
#[test]
fn str16_compatible_real_jdk_drops_the_builder_stubs_with_the_row() {
    use cratonvm_types::compat::CompatibilityMode::Compatible;
    use cratonvm_types::{HotSpotAlignment, HotSpotAlignments};
    let build = |real_jdk: bool, row: bool| {
        let mut registry = NativeMethodRegistry::new();
        registry.set_compatibility_mode(Compatible);
        registry.set_real_jdk(real_jdk);
        registry.set_hotspot_alignments(
            HotSpotAlignments::defaults().with(HotSpotAlignment::JdkStringBuilderBodies, row),
        );
        register_string_builder_natives(&mut registry, "java/lang/StringBuilder");
        register_string_builder_natives(&mut registry, "java/lang/AbstractStringBuilder");
        registry.register(
            "java/lang/AbstractStringBuilder",
            "toString",
            "()Ljava/lang/String;",
            native_sb_init_default,
            cratonvm_native_api::NativeKind::Bridge,
        );
        registry
    };
    let append = |r: &NativeMethodRegistry| {
        r.find(
            "java/lang/StringBuilder",
            "append",
            "(I)Ljava/lang/StringBuilder;",
        )
        .is_some()
    };
    let bridge = |r: &NativeMethodRegistry| {
        r.find(
            "java/lang/AbstractStringBuilder",
            "toString",
            "()Ljava/lang/String;",
        )
        .is_some()
    };
    let dropped = build(true, true);
    assert!(
        !append(&dropped),
        "real JDK + row: the stub is not registered"
    );
    assert!(bridge(&dropped), "a Bridge row is kept");
    assert!(append(&build(true, false)), "row off: registered as before");
    assert!(
        append(&build(false, true)),
        "synthetic image: registered as before"
    );
}

/// STR16-4: a null builder is an NPE with the JDK constructor's own text
/// (`asb.getValue()` is its first dereference), never an empty String.
#[test]
fn str16_asb_ctor_null_builder_throws_the_jdk_npe() {
    if !string_builder_ctor_null_npe_enabled() {
        return; // kill switch armed in this environment
    }
    let mut ctx = mock_ctx();
    let this = ctx.create_string("placeholder");
    let err = native_string_init_abstract_string_builder(
        &mut ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(None),
            Value::Object(None),
        ],
    )
    .unwrap_err();
    match err {
        cratonvm_types::error::MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::Runtime(
                cratonvm_types::error::RuntimeError::NullPointerException { message },
            ),
        ) => assert_eq!(
            message.as_deref(),
            Some(
                "Cannot invoke \"java.lang.AbstractStringBuilder.getValue()\" because \"asb\" is null"
            ),
        ),
        other => panic!("expected the JDK NPE, got {other:?}"),
    }
}

#[test]
fn string_replace_unchanged_returns_receiver_identity() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("io.quarkus.Benchmark");
    let same_char = native_string_replace(
        &mut ctx,
        &[
            Value::Object(Some(s)),
            Value::Int('.' as i32),
            Value::Int('.' as i32),
        ],
    )
    .unwrap();
    assert_eq!(same_char, Some(Value::Object(Some(s))));

    let absent_char = native_string_replace(
        &mut ctx,
        &[
            Value::Object(Some(s)),
            Value::Int('/' as i32),
            Value::Int('_' as i32),
        ],
    )
    .unwrap();
    assert_eq!(absent_char, Some(Value::Object(Some(s))));

    let dot = ctx.create_string(".");
    let slash = ctx.create_string("/");
    let under = ctx.create_string("_");
    let same_cs = native_string_replace_charseq(
        &mut ctx,
        &[
            Value::Object(Some(s)),
            Value::Object(Some(dot)),
            Value::Object(Some(dot)),
        ],
    )
    .unwrap();
    assert_eq!(same_cs, Some(Value::Object(Some(s))));

    let absent_cs = native_string_replace_charseq(
        &mut ctx,
        &[
            Value::Object(Some(s)),
            Value::Object(Some(slash)),
            Value::Object(Some(under)),
        ],
    )
    .unwrap();
    assert_eq!(absent_cs, Some(Value::Object(Some(s))));
}
