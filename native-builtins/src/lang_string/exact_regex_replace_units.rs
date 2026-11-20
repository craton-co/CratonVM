#[allow(unused_imports)]
use super::*;

/// [`exact_regex_replace_with`] for the scratch path
/// ([`string_regex_scratch_enabled`]): the same walk ([`java_find_iter`]'s
/// empty-match step) and the same output, but over the caller's ASCII `text`
/// units (which ARE `s`'s bytes) instead of a re-encoded copy, and with one
/// reused `CaptureLocations` instead of a fresh `Captures` per match -- the
/// per-match allocation was the largest leaf of the `replaceAll` row on n23-w3.
pub fn exact_regex_replace_units(
    x: &ExactRegex,
    re: &regex::Regex,
    text: &[u16],
    s: &str,
    repl: &[u16],
    first_only: bool,
) -> Option<Option<Vec<u16>>> {
    debug_assert_eq!(text.len(), s.len());
    let group_count = re.captures_len().checked_sub(1)?;
    let parts = parse_java_replacement(repl, group_count)?;
    if x.quantified_group
        && parts
            .iter()
            .any(|p| matches!(p, JavaReplPart::Group(g) if *g > 0))
    {
        return None;
    }
    let mut locs = re.capture_locations();
    let mut out: Vec<u16> = Vec::new();
    let mut append_pos = 0usize;
    let mut found = false;
    let (mut first, mut last): (Option<usize>, usize) = (None, 0);
    loop {
        let mut next = last;
        if first == Some(last) {
            next += 1;
        }
        if next > s.len() {
            break;
        }
        let Some(m) = re.captures_read_at(&mut locs, s, next) else {
            break;
        };
        first = Some(m.start());
        last = m.end();
        if !found {
            out.reserve(text.len());
        }
        found = true;
        out.extend_from_slice(&text[append_pos..m.start()]);
        for part in &parts {
            match part {
                JavaReplPart::Lit(u) => out.extend_from_slice(u),
                JavaReplPart::Group(g) => {
                    if let Some((gs, ge)) = locs.get(*g) {
                        out.extend_from_slice(&text[gs..ge]);
                    }
                }
            }
        }
        append_pos = m.end();
        if first_only {
            break;
        }
    }
    if !found {
        return Some(None);
    }
    out.extend_from_slice(&text[append_pos..]);
    Some(Some(out))
}

/// `Pattern.split(CharSequence, int, false)` (JDK 25) for an [`ExactRegex`]
/// over ASCII `s`, as `(start, end)` unit ranges; `[(0, len)]` is the JDK's
/// `return new String[] {input.toString()}`.
pub fn exact_regex_split(x: &ExactRegex, s: &str, limit: i32) -> Option<Vec<(usize, usize)>> {
    let re = exact_std_regex(&x.rust)?;
    exact_regex_split_with(&re, s, limit)
}

/// [`exact_regex_split`] over an already compiled pattern (`re`).
pub fn exact_regex_split_with(re: &regex::Regex, s: &str, limit: i32) -> Option<Vec<(usize, usize)>> {
    let len = s.len();
    let limited = limit > 0;
    let mut count = 0i32;
    let mut index = 0usize;
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for caps in java_find_iter(re, s) {
        let m = caps.get(0)?;
        if !limited || count < limit - 1 {
            if index == 0 && m.start() == 0 && m.end() == 0 {
                // No empty leading substring for a zero-width match at 0.
                continue;
            }
            ranges.push((index, m.start()));
            index = m.end();
            count += 1;
        } else if count == limit - 1 {
            ranges.push((index, len));
            index = m.end();
            count += 1;
        } else {
            break; // nothing below changes once the limit is reached
        }
    }
    if index == 0 {
        return Some(vec![(0, len)]);
    }
    if !limited || count < limit {
        ranges.push((index, len));
    }
    if limit == 0 {
        while ranges.last().is_some_and(|&(a, b)| a == b) {
            ranges.pop();
        }
    }
    Some(ranges)
}

/// A `String[]` of `text` ranges with the JDK's identities: the whole range
/// is the receiver itself (`substring(0, length())` / `input.toString()`),
/// an empty range is the `""` literal (`StringLatin1.newString` answers `""`
/// for length 0), anything else a fresh String.
pub fn string_array_from_ranges(
    ctx: &mut dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    text: &[u16],
    ranges: &[(usize, usize)],
) -> MethodCallResult {
    let string_class_id = match ctx.ensure_class_initialized("java/lang/String") {
        Ok(id) => id,
        Err(_) => crate::util_concurrent_ext::refused_class(ctx, "java/lang/String", 8)?,
    };
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let arr_obj = scope.new_ref_array(string_class_id, ranges.len());
    let arr_h = scope.root(arr_obj);
    for (i, &(a, b)) in ranges.iter().enumerate() {
        let part = if a == 0 && b == text.len() {
            scope.get(&this_h)
        } else if a == b {
            scope.create_string("")
        } else {
            string_native_result_from_units(&mut *scope, &text[a..b], true)?
        };
        let arr = scope.get(&arr_h);
        scope.set_array_element(arr, i, Value::Object(Some(part)));
    }
    let arr = scope.get(&arr_h);
    Ok(Some(Value::Object(Some(arr))))
}

/// Run the JDK's own body of `java/lang/String.<name><desc>`; `None` when no
/// body could run (a non-Runtime internal error, or no value), so the caller
/// keeps its pre-wave-10 path.
pub fn string_regex_decline(
    ctx: &mut dyn NativeContext,
    name: &str,
    desc: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    match ctx.invoke_special_bytecode_only("java/lang/String", name, desc, args) {
        Ok(None) => None,
        Err(MethodCallFailed::InternalError(e))
            if !matches!(e, cratonvm_types::error::VmError::Runtime(_)) =>
        {
            None
        }
        other => Some(other),
    }
}

/// The wave-10 front of a regex String native: exact, else declined, else
/// `None` (the legacy body runs).
pub fn string_regex_front(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    op: StringRegexOp,
    name: &str,
    desc: &str,
) -> Option<MethodCallResult> {
    if !string_regex_exact_enabled() {
        return None;
    }
    if let Some(r) = string_regex_exact(ctx, args, op) {
        return Some(r);
    }
    string_regex_decline(ctx, name, desc, args)
}

pub fn native_string_replace_all(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(r) = string_regex_front(
        ctx,
        args,
        StringRegexOp::ReplaceAll,
        "replaceAll",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
    ) {
        return r;
    }
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Err(regex_arg_npe("receiver").into()),
    };
    let pattern = match args.get(1) {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => return Err(regex_arg_npe("regex").into()),
    };
    let replacement = match args.get(2) {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => return Err(regex_arg_npe("replacement").into()),
    };
    let s = ctx.read_string(this).unwrap_or_default();
    // A pattern this engine cannot compile is a `PatternSyntaxException`, not a
    // licence to do something else. This used to fall through to a LITERAL
    // `str::replace` of the pattern TEXT, so `"Hello, World".replaceAll("[",
    // "x")` returned the input unchanged where HotSpot throws — a silently
    // wrong answer produced by the error path of a fast path.
    let re = compile_java_regex(&pattern, 0)?;
    let result = re
        .replace_all_java(&s, replacement.as_str())
        .map_err(crate::regex_matcher::no_group_error)?;
    Ok(Some(Value::Object(Some(
        ctx.create_string_uninterned(&result),
    ))))
}

pub fn native_string_replace_first(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(r) = string_regex_front(
        ctx,
        args,
        StringRegexOp::ReplaceFirst,
        "replaceFirst",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
    ) {
        return r;
    }
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Err(regex_arg_npe("receiver").into()),
    };
    let pattern = match args.get(1) {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => return Err(regex_arg_npe("regex").into()),
    };
    let replacement = match args.get(2) {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => return Err(regex_arg_npe("replacement").into()),
    };
    let s = ctx.read_string(this).unwrap_or_default();
    let re = compile_java_regex(&pattern, 0)?;
    let result = re
        .replace_first_java(&s, replacement.as_str())
        .map_err(crate::regex_matcher::no_group_error)?;
    Ok(Some(Value::Object(Some(
        ctx.create_string_uninterned(&result),
    ))))
}

pub fn native_string_matches(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(r) = string_regex_front(
        ctx,
        args,
        StringRegexOp::Matches,
        "matches",
        "(Ljava/lang/String;)Z",
    ) {
        return r;
    }
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Err(regex_arg_npe("receiver").into()),
    };
    let pattern = match args.get(1) {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => return Err(regex_arg_npe("regex").into()),
    };
    let s = ctx.read_string(this).unwrap_or_default();
    // Same rule as the two above. The old fallback compared the subject to the
    // pattern TEXT, so `"Hello, World".matches("[")` answered `false` — a
    // plausible verdict for a question that should have raised.
    let re = compile_java_regex(&pattern, 0)?;
    let anchored = format!("^(?:{})$", re.as_str());
    let matched = match crate::compile_anchored_cached(&anchored) {
        Some(full) => full.is_match(&s),
        None => re.is_match(&s),
    };
    Ok(Some(Value::Int(if matched { 1 } else { 0 })))
}

/// `String.equalsIgnoreCase(String)`.
///
/// **The null argument is `false`, NOT a throw** — measured on
/// OpenJDK 25.0.3+9, and the JDK source says why:
/// `(anotherString != null) && ...`. This is one of the two rows in the whole
/// `String` null sweep that is specified to answer rather than raise (the other
/// is `equals`), which is why the sweep had to be a measurement per method.
///
/// # The comparison rule, which was not the JDK's
///
/// The old body was `a.to_lowercase() == b.to_lowercase()` — Rust's FULL
/// (SpecialCasing) mappings over whole strings. The JDK is
/// `regionMatches(true, 0, other, 0, length())` after a length check, i.e. the
/// per-code-unit `StringUTF16.regionMatchesCI` rule already transcribed in
/// [`code_unit_eq_ignore_case`]. The two disagree, measured:
///
/// ```text
///                                        HotSpot 25   to_lowercase() gave
/// "İ".equalsIgnoreCase("i")         true         false   (full mapping is i + U+0307)
/// "ꟓ".equalsIgnoreCase("꟒")    false        true    (Rust pairs them, the JDK does not)
/// "ꟕ".equalsIgnoreCase("꟔")    false        true
/// "꟏".equalsIgnoreCase("꟎")    false        true
/// "ꟑ".equalsIgnoreCase("Ꟑ")    true         true    <- control: a REAL pair
/// "K".equalsIgnoreCase("k")         true         true
/// "ß".equalsIgnoreCase("ss")        false        false
/// ```
///
/// The doc on `case_map::JDK_UNMAPPED_CASE_RUNS` already claimed
/// `equalsIgnoreCase` among the four answers it had measured and corrected.
/// The claim was true of the measurement and false of the code: the fix went
/// into `java_char_to_upper_case`, which this method never called.
/// `[1 of 10 callsites]` — the helper existed and one caller used it.
pub fn native_string_equals_ignore_case(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        // Specified, not swallowed: `equalsIgnoreCase(null)` IS `false`.
        _ => return Ok(Some(Value::Int(0))),
    };
    if this.as_ptr() == other.as_ptr() {
        return Ok(Some(Value::Int(1)));
    }
    // The thread-local scratches, not `read_string_chars`: this method is on
    // the hot path for HTTP header matching and must not start allocating two
    // Vecs per call to gain its correctness.
    // nio21 w8 L5: `regionMatches(true, 0, other, 0, length())`'s coder
    // dispatch, so a UTF16/UTF16 pair folds surrogate PAIRS by code point
    // (`"𐐨".equalsIgnoreCase("𐐀")` is `true` on HotSpot 25).
    // nio21 w9 L5: the coders are read from the Strings (W8L5-2).
    let ctx_ref: &dyn NativeContext = &*ctx;
    let equal = with_two_string_chars_scratches(ctx_ref, this, other, |a, b| {
        a.len() == b.len()
            && region_matches_ci_units(
                a,
                string_coder_is_latin1(ctx_ref, this, a),
                0,
                b,
                string_coder_is_latin1(ctx_ref, other, b),
                0,
                a.len(),
            )
    });
    Ok(Some(Value::Int(if equal { 1 } else { 0 })))
}

/// `String.compareToIgnoreCase(String)`.
///
/// Unlike its `equalsIgnoreCase` sibling, a null argument here THROWS —
/// measured on OpenJDK 25.0.3+9 (`Cannot read field "value" because "s2" is
/// null`), because `CASE_INSENSITIVE_ORDER.compare` dereferences both operands
/// with no guard. The two methods sit next to each other, take the same
/// parameter type, and have opposite null contracts; this VM answered `0` —
/// "equal" — for the null.
///
/// # The magnitude, not just the sign
///
/// The old body lower-cased both strings with Rust's full mapping and returned
/// `-1`/`0`/`1` from an `Ordering`. The JDK's `StringLatin1.compareToCI` /
/// `StringUTF16.compareToCI` return the DIFFERENCE of the two folded code
/// units, and the fold is the same upper-then-lower composition as
/// [`code_unit_eq_ignore_case`]:
///
/// ```text
/// c1 == c2                       -> keep going
/// u1 = toUpper(c1), u2 = toUpper(c2); u1 == u2  -> keep going
/// l1 = toLower(u1), l2 = toLower(u2); l1 == l2  -> keep going
/// otherwise                      -> return l1 - l2
/// exhausted                      -> return len1 - len2
/// ```
///
/// Measured rows the old shape got wrong: `"_".compareToIgnoreCase("a")` is
/// `-2` (the fold leaves `_` alone and lowercases `A` back to `a`, so it is
/// `0x5F - 0x61`) and was `-1`; `"İ".compareToIgnoreCase("i")` is `0` and
/// was non-zero; `"ꟓ".compareToIgnoreCase("꟒")` is `1` and was `0`.
/// A comparator only needs the sign, but `compareToIgnoreCase` is a public
/// method whose javadoc specifies the value, and the first two rows are sign
/// errors anyway.
pub fn native_string_compare_to_ignore_case(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(string_arg_npe(
                "Cannot read field \"value\" because \"s2\" is null",
            ))
        }
    };
    let ctx_ref: &dyn NativeContext = &*ctx;
    let result = with_two_string_chars_scratches(ctx_ref, this, other, |a, b| {
        // nio21 w8 L5: `CaseInsensitiveComparator.compare`'s coder dispatch.
        // UTF16/UTF16 is `StringUTF16.compareToCI` (code-point fold, the
        // difference of the folded CODE POINTS); any LATIN1 side is the
        // per-code-unit loop below (`compareToCI{,_UTF16,_Latin1}`).
        // nio21 w9 L5: the coders are read from the Strings (W8L5-2), and
        // the per-unit fold is the JDK table (`CiUnitFold`).
        if !string_coder_is_latin1(ctx_ref, this, a)
            && !string_coder_is_latin1(ctx_ref, other, b)
        {
            return utf16_compare_to_ci_impl(a, 0, a.len(), b, 0, b.len());
        }
        let mut fold = CiUnitFold::new();
        for (&c1, &c2) in a.iter().zip(b.iter()) {
            let d = fold.diff(c1, c2);
            if d != 0 {
                return d;
            }
        }
        a.len() as i32 - b.len() as i32
    });
    Ok(Some(Value::Int(result)))
}

pub fn native_string_last_index_of_char(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let ch = match args.get(1) {
        Some(Value::Int(c)) => *c,
        _ => return Ok(Some(Value::Int(-1))),
    };
    // bug nb-lang-string: lastIndexOf is defined over UTF-16 code UNITS, not
    // Unicode code points. The old `rfind` + `chars().count()` returned a
    // code-point index, off by the count of preceding supplementary chars.
    //
    // E18: the `char::from_u32(ch)` + `encode_utf16` that replaced it is the
    // ONE member of this family that got the surrogate pair right, and it is
    // still not the JDK's rule — `char::from_u32` rejects the surrogate code
    // points, so a lone-surrogate `ch` returned `-1` where HotSpot finds it
    // (`"xзy𐐷z".lastIndexOf(0xDC37)` is `4`). [`code_point_needle`] is the
    // rule; this native and its three siblings now share it.
    let Some(needle) = code_point_needle(ch) else {
        return Ok(Some(Value::Int(-1)));
    };
    let result = with_string_chars_scratch(ctx, this, |buf| {
        last_index_of_units_from(buf, needle.units(), i64::MAX)
    });
    Ok(Some(Value::Int(result)))
}

/// The UTF-16 code units that `String.{indexOf,lastIndexOf}(int ch[, int])`
/// actually searches for — `None` when no sequence of code units can match.
///
/// # E18. Three copies of this rule, and the two that answered were wrong
///
/// The JDK does **not** narrow `ch` to a code unit. `String.indexOf(int)` is
///
/// ```text
/// isLatin1() ? StringLatin1.indexOf(value, ch, …)   // if (!canEncode(ch)) return -1;
///            : StringUTF16 .indexOf(value, ch, …)   // !isValidCodePoint  -> -1
///                                                   //  ch <  0x10000     -> scan for (char) ch
///                                                   //  ch >= 0x10000     -> scan for the PAIR
/// ```
///
/// so a supplementary `ch` is matched as a surrogate *pair* and never as its
/// low half. `native_string_index_of`, `native_string_index_of_from` and
/// `native_string_last_index_of_from` all wrote `(ch & 0xFFFF) as u16`, which
/// finds the masked half; `native_string_last_index_of_char` instead wrote
/// `char::from_u32(ch)` + `encode_utf16`, which gets the pair right but drops
/// every LONE SURROGATE `ch` on the floor (`char::from_u32(0xDC37)` is `None`).
/// One JVMS rule, four implementations counting the JIT's, four different sets
/// of wrong answers. This is now the only place the rule is written down.
///
/// Measured on OpenJDK 25.0.3+9 (`scratchpad/e18/CharSearch.java`,
/// `Idx3.java`), receiver `"xзy𐐷z"` unless noted:
///
/// ```text
/// "abc".indexOf(0x10061)          -1   masking finds 'a' at 0        <- the KEY row
/// "з".indexOf(0x10437)       -1   masking finds it at 0
/// mixed.indexOf(0x10437)           3   the pair, not the low half
/// mixed.indexOf(0xD801)            3   a lone HIGH surrogate is a plain unit scan
/// mixed.indexOf(0xDC37)            4   a lone LOW surrogate too — char::from_u32 says None
/// "￿q".indexOf(0xFFFF)        0   0xFFFF is a valid (non-)character
/// "￿q".indexOf(-1)           -1   NOT (char)(-1) == 0xFFFF: isValidCodePoint gates first
/// "abc".indexOf(0x110000)         -1
/// "abc".lastIndexOf(0x10061)      -1                                 <- the KEY row again
/// ```
///
/// The `-1` row is the one that shows the rule is `isValidCodePoint`, not a
/// narrowing cast: `(char) -1` is `0xFFFF`, the receiver holds `0xFFFF`, and
/// HotSpot still answers `-1`.
pub fn code_point_needle(ch: i32) -> Option<CharNeedle> {
    if !(0..=0x10_FFFF).contains(&ch) {
        // `Character.isValidCodePoint(ch)` — checked before any narrowing.
        return None;
    }
    if ch <= 0xFFFF {
        // A BMP code point, INCLUDING an unpaired surrogate value: the JDK
        // scans for the single code unit and so must this.
        return Some(CharNeedle {
            units: [ch as u16, 0],
            len: 1,
        });
    }
    let off = ch - 0x1_0000;
    Some(CharNeedle {
        units: [
            (0xD800 + (off >> 10)) as u16,
            (0xDC00 + (off & 0x3FF)) as u16,
        ],
        len: 2,
    })
}

/// One or two UTF-16 code units — the output of [`code_point_needle`].
#[derive(Clone, Copy)]
pub struct CharNeedle {
    pub units: [u16; 2],
    pub len: usize,
}

impl CharNeedle {
    pub fn units(&self) -> &[u16] {
        &self.units[..self.len]
    }
}

/// Forward scan for `needle` in `haystack`, both in UTF-16 code units, starting
/// at `max(from, 0)`.
///
/// The empty-needle answer is `clamp(from, 0, haystack.len())`, which is the
/// JDK's — measured: `"abcabc".indexOf("", 99)` is `6` and `indexOf("", -5)` is
/// `0`. Shared by the code-point family and the `String`-needle family; the four
/// hand-rolled copies of this loop it replaces disagreed only in their bounds
/// arithmetic, which is the part worth having in one place.
pub fn index_of_units_from(haystack: &[u16], needle: &[u16], from: i32) -> i32 {
    if needle.is_empty() {
        return from.clamp(0, haystack.len() as i32);
    }
    if needle.len() > haystack.len() {
        return -1;
    }
    let max_start = haystack.len() - needle.len();
    let mut i = from.max(0) as usize;
    // Use AVX-512BW as a first-unit filter for complete 32-code-unit blocks.
    // Candidate verification remains scalar, keeping all UTF-16 code-unit and
    // lone-surrogate behavior exact; the scalar tail handles partial blocks.
    #[cfg(target_arch = "x86_64")]
    if i <= max_start
        && max_start - i >= 31
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_AVX512")
        && std::is_x86_feature_detected!("avx512f")
        && std::is_x86_feature_detected!("avx512bw")
    {
        let mut base = i;
        while base <= max_start && max_start - base >= 31 {
            // SAFETY: feature detection verifies AVX-512F/BW and this condition
            // proves that all 32 u16 elements are inside the haystack.
            let mask = unsafe { avx512_u16_equal_mask(haystack.as_ptr().add(base), needle[0]) };
            let mut candidates = mask;
            while candidates != 0 {
                let lane = candidates.trailing_zeros() as usize;
                let at = base + lane;
                if haystack[at..at + needle.len()] == *needle {
                    return at as i32;
                }
                candidates &= candidates - 1;
            }
            base += 32;
        }
        i = base;
    }
    while i <= max_start {
        if &haystack[i..i + needle.len()] == needle {
            return i as i32;
        }
        i += 1;
    }
    -1
}

/// Backward scan for `needle` in `haystack`, starting at
/// `min(from, haystack.len() - needle.len())`.
///
/// That subtraction is the JDK's own: `StringUTF16.lastIndexOfChar` starts at
/// `min(fromIndex, length - 1)` and `lastIndexOfSupplementary` at
/// `min(fromIndex, length - 2)`, which is the same expression once the needle
/// width is a parameter rather than a hard-coded 1 or 2.
///
/// `from` is `i64` so that a NEGATIVE `from` finds nothing rather than
/// saturating to a `usize`: the JDK's loop counter simply starts below zero and
/// never runs. Measured — `"abc".lastIndexOf('a', -1)` is `-1`, and
/// `mixed.lastIndexOf(0x10437, -1)` is `-1` even though the pair is present.
pub fn last_index_of_units_from(haystack: &[u16], needle: &[u16], from: i64) -> i32 {
    if needle.is_empty() {
        return haystack.len() as i32;
    }
    if needle.len() > haystack.len() {
        return -1;
    }
    let mut i = from.min((haystack.len() - needle.len()) as i64);
    // Scan complete 32-candidate blocks from right to left. The highest set
    // mask bit is verified first, preserving lastIndexOf's nearest-match rule.
    #[cfg(target_arch = "x86_64")]
    if i >= 31
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_AVX512")
        && std::is_x86_feature_detected!("avx512f")
        && std::is_x86_feature_detected!("avx512bw")
    {
        while i >= 31 {
            let base = (i - 31) as usize;
            // SAFETY: feature detection verifies AVX-512F/BW and i >= 31
            // guarantees that the 32-unit load stays within the array.
            let mut candidates =
                unsafe { avx512_u16_equal_mask(haystack.as_ptr().add(base), needle[0]) };
            while candidates != 0 {
                let lane = 31 - candidates.leading_zeros() as usize;
                let at = base + lane;
                if haystack[at..at + needle.len()] == *needle {
                    return at as i32;
                }
                candidates &= !(1u32 << lane);
            }
            if base == 0 {
                return -1;
            }
            i = base as i64 - 1;
        }
    }
    while i >= 0 {
        let at = i as usize;
        if &haystack[at..at + needle.len()] == needle {
            return i as i32;
        }
        i -= 1;
    }
    -1
}

/// Helper: last index (in UTF-16 code units) of `needle` within `haystack`.
/// Returns -1 when not found. An empty needle returns `haystack.len()` to
/// match `String.lastIndexOf("")` semantics used by the str variant.
pub fn last_index_of_units(haystack: &[u16], needle: &[u16]) -> i32 {
    // "no `fromIndex`" is "start as far right as the needle fits", which is
    // what [`last_index_of_units_from`] does with a `from` it cannot exceed.
    last_index_of_units_from(haystack, needle, i64::MAX)
}

pub fn native_string_last_index_of_str(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let needle = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        // See `native_string_index_of_str`: `-1` is indistinguishable from a
        // legitimate miss. HotSpot throws.
        _ => {
            return Err(string_arg_npe(
                "Cannot read field \"value\" because \"tgtStr\" is null",
            ))
        }
    };
    // bug nb-lang-string: lastIndexOf(String) must return a UTF-16 code-UNIT
    // index. The old `rfind` + `chars().count()` returned a code-point index,
    // off by preceding supplementary chars. Search over u16 code units.
    // nio21 w9 L5: read losslessly too. Through `read_string` every unpaired
    // surrogate on either side was U+FFFD, so `"\uD801x".lastIndexOf("\uDC00")`
    // found the HIGH half at 0 (both decode to U+FFFD) where HotSpot answers -1.
    let result = with_two_string_chars_scratches(ctx, this, needle, |s_units, needle_units| {
        last_index_of_units(s_units, needle_units)
    });
    Ok(Some(Value::Int(result)))
}

/// `String.getChars(int srcBegin, int srcEnd, char[] dst, int dstBegin)`.
///
/// # Three checks, in the JDK's order, none of which this body had
///
/// The old version silently clamped `srcEnd` to the receiver's length, ignored
/// a negative `srcBegin`, and treated a null `dst` as a NO-OP that returned
/// normally. `getChars` is the JDK's bulk copy out of a `String`, so a caller
/// that mis-sizes its buffer got a partly-filled array and no signal at all.
///
/// Measured on OpenJDK 25.0.3+9 with the receiver `"abc"`:
///
/// ```text
/// getChars(0, 9, null, 0)          StringIndexOutOfBoundsException: Range [0, 9) out of bounds for length 3
/// getChars(2, 1, null, 0)          StringIndexOutOfBoundsException: Range [2, 1) out of bounds for length 3
/// getChars(-1, 1, null, 0)         StringIndexOutOfBoundsException: Range [-1, 1) out of bounds for length 3
/// getChars(0, 1, null, 0)          NullPointerException: Cannot read the array length because "dst" is null
/// getChars(0, 1, null, -1)         NullPointerException                       <- null beats dstBegin
/// getChars(0, 1, new char[4], -1)  StringIndexOutOfBoundsException: Range [-1, -1 + 1) out of bounds for length 4
/// getChars(0, 3, new char[4], 2)   StringIndexOutOfBoundsException: Range [2, 2 + 3) out of bounds for length 4
/// getChars(0, 0, new char[0], 0)   (returns normally)
/// ```
///
/// So the order is: `checkBoundsBeginEnd(srcBegin, srcEnd, length())` first —
/// it fires even when `dst` is null — then the null check, then
/// `checkBoundsOffCount(dstBegin, srcEnd - srcBegin, dst.length)`. Both range
/// failures are `StringIndexOutOfBoundsException`, not
/// `ArrayIndexOutOfBoundsException`, including the one about the destination
/// ARRAY: `String.getChars` does its own checking before the `arraycopy`.
pub fn native_string_get_chars(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let src_begin = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let src_end = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    // bug nb-lang-string: getChars indexes over UTF-16 code UNITS, not Unicode
    // code points. The old `s.chars()` (one entry per code point) put
    // supplementary chars in a single slot and shifted every later index,
    // corrupting the dest array.
    //
    // `read_string_chars`, not `ctx.read_string(..).encode_utf16()`: the range
    // check below is against `length()`, so it must read the `value` array
    // through the SAME path `String.length()` does. Going via a Rust `String`
    // also substitutes U+FFFD for every unpaired surrogate, which this method
    // must copy out intact.
    let units = read_string_chars(ctx, this);
    let length = units.len() as i32;
    // Check one: the SOURCE range, before `dst` is looked at.
    if src_begin < 0 || src_begin > src_end || src_end > length {
        return Err(
            cratonvm_types::error::RuntimeError::sioobe_range(src_begin, src_end, length).into(),
        );
    }
    // Check two: the destination reference.
    let dst = match args.get(3) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(string_arg_npe(
                "Cannot read the array length because \"dst\" is null",
            ))
        }
    };
    let dst_begin = match args.get(4) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    // Check three: the destination range, `checkBoundsOffCount` style.
    let count = src_end - src_begin;
    let dst_len = ctx.array_length(dst) as i32;
    if bounds_off_count_violation(dst_begin, count, dst_len).is_some() {
        return Err(cratonvm_types::error::RuntimeError::sioobe_range_size(
            dst_begin, count, dst_len,
        )
        .into());
    }
    let src_begin = src_begin as usize;
    let dst_begin = dst_begin as usize;
    for (i, &cu) in units
        .iter()
        .enumerate()
        .take(src_end as usize)
        .skip(src_begin)
    {
        ctx.set_array_element(dst, dst_begin + (i - src_begin), Value::Int(cu as i32));
    }
    Ok(None)
}

/// The `String.strip*` family's shared walk.
///
/// W7-95a. `strip`/`stripLeading`/`stripTrailing` are specified in terms of
/// `Character.isWhitespace`, which is not `char::is_whitespace` — see
/// [`java_char_is_whitespace`] for the eight code points that disagree and for
/// the measurement. Measured, code units in / code units out: `strip()` of
/// `[2007, 0078, 2007]` is `[8199, 120, 8199]` on HotSpot 25.0.3+9 and was
/// `[120]` here (`U+2007` FIGURE SPACE is a non-breaking space, which Java
/// deliberately does not treat as whitespace); `strip()` of
/// `[001C, 0078, 001C]` is `[120]` there and was `[28, 120, 28]` here.
///
/// Java strips by code POINT, so the walk skips a whole surrogate pair — but
/// no supplementary code point is whitespace, so the pair never strips and
/// walking by code unit gives the same answer. Reading units rather than a
/// `str` also keeps an unpaired surrogate out of the U+FFFD substitution.
pub fn string_strip_range(units: &[u16], leading: bool, trailing: bool) -> (usize, usize) {
    let mut start = 0;
    let mut end = units.len();
    if leading {
        while start < end && java_char_is_whitespace(units[start]) {
            start += 1;
        }
    }
    if trailing {
        while end > start && java_char_is_whitespace(units[end - 1]) {
            end -= 1;
        }
    }
    (start, end)
}

pub fn native_string_strip_impl(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    leading: bool,
    trailing: bool,
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let units = read_string_chars(ctx, this);
    let (start, end) = string_strip_range(&units, leading, trailing);
    // nio21 w9 L5: lossless for a lone surrogate.
    let result = string_native_result_from_units(ctx, &units[start..end], false)?;
    Ok(Some(Value::Object(Some(result))))
}

pub fn native_string_strip(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_string_strip_impl(ctx, args, true, true)
}

pub fn native_string_strip_leading(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_string_strip_impl(ctx, args, true, false)
}

pub fn native_string_strip_trailing(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_string_strip_impl(ctx, args, false, true)
}

/// `String.copyValueOf(char[])` — and, registered under the same body,
/// `String.valueOf(char[])`.
///
/// Both are `new String(data)`, which dereferences the array for its length.
/// Measured on OpenJDK 25.0.3+9: `String.copyValueOf(null)`,
/// `String.valueOf((char[]) null)`, `new String((char[]) null)` and
/// `new String((char[]) null, 0, 1)` all throw NullPointerException. This body
/// answered `""` — and `""` is a legal result of this method, so nothing
/// downstream could tell the difference.
///
/// `String.valueOf((Object) null)` is the row that does NOT throw: it is
/// specified as the four-character string `"null"`, and
/// `native_string_value_of_object` already answers that.
pub fn native_string_copy_value_of(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Static method: args[0] = char[]
    let arr = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(string_arg_npe(
                "Cannot read the array length because \"value\" is null",
            ))
        }
    };
    // JIT r20 wave 12, lane qStr (correctness, unconditional): read the
    // UTF-16 units and build the String from them. The `char::from_u32` walk
    // this replaces DROPPED every surrogate unit (paired or lone), so
    // `String.valueOf(new char[] {'x', '\uD83D', '\uDE00'})` answered "x"
    // where HotSpot answers the three units. `create_string_from_units` is a
    // fresh, unpooled String (no literal identity), as `new String(char[])`.
    let len = ctx.array_length(arr);
    let mut units = vec![0u16; len];
    let written = ctx.read_char_array_into(arr, 0, &mut units);
    units.truncate(written);
    Ok(Some(Value::Object(Some(
        ctx.create_string_from_units(&units),
    ))))
}

// ---------------------------------------------------------------------------
// Phase 14 Step 3: String extras
// ---------------------------------------------------------------------------



/// The failing path of a `java/lang/String` code-point native: hand the call to
/// the JDK's own body (`invoke_special_bytecode_only`, which never re-enters
/// this native), so the exception is raised by `Preconditions` /
/// `Character.offsetByCodePoints` with HotSpot's frames
/// (`Preconditions$1.apply` ... `String.checkIndex`, `String.codePointAt`)
/// instead of a throwable whose trace starts at the caller. Only the throw
/// changes: the in-range fast path stays the native's, and the decision to
/// fail is still the native's own check, so the JDK body runs only for a call
/// HotSpot also fails (if it answered instead, its answer is HotSpot's).
///
/// `fallback` is the native's own error, kept when the switch is off or when
/// the loaded `java/lang/String` does not declare the method itself (the
/// synthetic class library, the unit-test mocks): there is no body to run.
pub fn string_error_from_jdk_body(
    ctx: &mut dyn NativeContext,
    method: &str,
    descriptor: &str,
    args: &[Value],
    fallback: MethodCallFailed,
) -> MethodCallResult {
    let declares = ctx
        .class_id_by_name("java/lang/String")
        .is_some_and(|cid| ctx.class_declares_method(cid, method, descriptor));
    if !declares {
        return Err(fallback);
    }
    ctx.invoke_special_bytecode_only("java/lang/String", method, descriptor, args)
}

/// `offsetByCodePoints`' one failure (a null-message IOOBE on HotSpot, raised
/// by `String.offsetByCodePoints` / `Character.offsetByCodePoints`), through
/// [`string_error_from_jdk_body`].
pub fn offset_by_code_points_error(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    string_error_from_jdk_body(
        ctx,
        "offsetByCodePoints",
        "(II)I",
        args,
        cratonvm_types::error::RuntimeError::ioobe_no_message().into(),
    )
}

pub fn native_string_code_point_at(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    // Spec: `String.codePointAt(int index)` raises StringIndexOutOfBoundsException
    // (NOT ArrayIndexOutOfBoundsException) for `index < 0 || index >= length()`.
    // Both flavours subclass IndexOutOfBoundsException, but BouncyCastle and other
    // libraries `catch (StringIndexOutOfBoundsException)` specifically; throwing
    // AIOOBE here would silently bypass that handler and surface as an
    // unrelated runtime error in callers that expect SIOOBE.
    let index_i32 = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    // Code units straight from the String's own `value` array, NOT
    // `ctx.read_string(..).encode_utf16()`. W7-95a: that round trip goes
    // through a Rust `String`, a Rust `str` cannot hold an unpaired surrogate,
    // and `String::from_utf16_lossy` therefore replaces one with U+FFFD.
    // `"x\uD800y".codePointAt(1)` measured 65533 here against HotSpot
    // 25.0.3+9's 55296 — a 65533 in a code-point answer is the diagnosis, not
    // a coincidence.
    //
    // And ONE OR TWO units, not the whole string: this used to decode every
    // unit (`read_string_chars`) to answer about one, so every
    // `for (i = 0; i < n; i += charCount(codePointAt(i)))` loop was
    // quadratic. H2's `StringUtils.quoteIdentifierOrLiteral` is exactly that
    // loop and quotes every string `SCRIPT` writes: 5,000 chars took 664 ms
    // and 20,000 took 12.8 s here, against 30 ms for 100,000 on HotSpot, and
    // `TestCases.testReuseSpace` (100,000-char rows, one `SCRIPT` per
    // statement) never finished. `StringCodeUnits::at` applies
    // `decode_string_chars`'s per-unit rule, so every answer is unchanged.
    let units = StringCodeUnits::of(ctx, this);
    let len = units.len();
    if index_i32 < 0 || (index_i32 as usize) >= len {
        // Message as well as class: the real `codePointAt` reaches
        // `Preconditions.checkIndex(index, length, SIOOBE_FORMATTER)`.
        // JIT r23 w2 L6: thrown by the JDK body, with its frames.
        return string_error_from_jdk_body(
            ctx,
            "codePointAt",
            "(I)I",
            args,
            cratonvm_types::error::RuntimeError::sioobe_index(index_i32, len as i32).into(),
        );
    }
    let index = index_i32 as usize;
    let ch = units.at(ctx, index);
    // Check for surrogate pair
    if (0xD800..=0xDBFF).contains(&ch) && index + 1 < len {
        let low = units.at(ctx, index + 1);
        if (0xDC00..=0xDFFF).contains(&low) {
            let cp = 0x10000 + ((ch as i32 - 0xD800) << 10) + (low as i32 - 0xDC00);
            return Ok(Some(Value::Int(cp)));
        }
    }
    Ok(Some(Value::Int(ch as i32)))
}



/// The span (units, or code points walked) up to which the windowed reads win
/// over one bulk decode.
pub const STRING_CODE_POINT_WINDOW_MAX: i64 = 64;

/// `String.codePointCount(int, int)`.
///
/// W7-95a. Two defects, both from the same three lines: the bounds were cast
/// straight to `usize` (so a negative `beginIndex` became a huge index that
/// the `while` simply skipped) and `endIndex` was *clamped* to the length
/// instead of rejected. Every out-of-range call therefore returned a number
/// where HotSpot throws. Measured on 25.0.3+9:
///
/// ```text
/// "a<U+1F600>b".codePointCount(3, 1)   IndexOutOfBoundsException: Range [3, 1) out of bounds for length 4
/// "a<U+1F600>b".codePointCount(0, 9)   IndexOutOfBoundsException: Range [0, 9) out of bounds for length 4
/// "a<U+1F600>b".codePointCount(-1, 2)  IndexOutOfBoundsException: Range [-1, 2) out of bounds for length 4
/// ```
///
/// The thrown class is the SUPERCLASS `IndexOutOfBoundsException`, not
/// `StringIndexOutOfBoundsException`: this call site reaches `Preconditions`
/// without the `String` domain's exception formatter. `codePointAt` a few
/// lines above *does* get the SIOOBE formatter, so the two neighbours throw
/// different classes on purpose — a `catch (StringIndexOutOfBoundsException)`
/// around this one would not fire on HotSpot either.
pub fn native_string_code_point_count(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let begin_i32 = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let end_i32 = match args.get(2) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    // JIT r23 wave 2, lane L5 (`CRATONVM_STRING_CODE_POINT_WINDOW`): a short
    // in-range span reads its own units, not the whole String. Anything else
    // (a throw, a long span) takes the bulk decode below.
    {
        let units = StringCodeUnits::of(ctx, this);
        let (b, e) = (i64::from(begin_i32), i64::from(end_i32));
        let length = units.len() as i64; // Cast: an array-bounded length
        if 0 <= b && b <= e && e <= length && e - b <= STRING_CODE_POINT_WINDOW_MAX {
            let (begin, end) = (b as usize, e as usize); // Cast: checked range
            let mut count = 0;
            let mut i = begin;
            while i < end {
                let ch = units.at(ctx, i);
                i += 1;
                if (0xD800..=0xDBFF).contains(&ch)
                    && i < end
                    && (0xDC00..=0xDFFF).contains(&units.at(ctx, i))
                {
                    i += 1;
                }
                count += 1;
            }
            return Ok(Some(Value::Int(count)));
        }
    }
    // `read_string_chars` for the surrogate reason spelled out in
    // `native_string_code_point_at`: a lossy UTF-8 round trip turns the
    // unpaired surrogates this method exists to count into U+FFFD.
    let chars = read_string_chars(ctx, this);
    let length = chars.len() as i32;
    if begin_i32 < 0 || begin_i32 > end_i32 || end_i32 > length {
        // JIT r23 w2 L6: thrown by the JDK body, with its frames.
        return string_error_from_jdk_body(
            ctx,
            "codePointCount",
            "(II)I",
            args,
            cratonvm_types::error::RuntimeError::ioobe(
                cratonvm_types::error::out_of_bounds_message::check_from_to_index(
                    i64::from(begin_i32),
                    i64::from(end_i32),
                    i64::from(length),
                ),
            )
            .into(),
        );
    }
    let begin = begin_i32 as usize;
    let end = end_i32 as usize;
    let mut count = 0;
    let mut i = begin;
    while i < end {
        let ch = chars[i];
        if (0xD800..=0xDBFF).contains(&ch) && i + 1 < end {
            let low = chars[i + 1];
            if (0xDC00..=0xDFFF).contains(&low) {
                i += 2;
                count += 1;
                continue;
            }
        }
        i += 1;
        count += 1;
    }
    Ok(Some(Value::Int(count)))
}

/// `String.offsetByCodePoints(int, int)`.
///
/// W7-95a. The old body was **VM-fatal**, not merely wrong. `index` was cast
/// to `usize`, so any `index > length()` survived; the backward branch then
/// did `pos -= 1` followed by an unguarded `chars[pos]`, and
/// `"a<U+1F600>b".offsetByCodePoints(10, -1)` indexed a 4-element `Vec` at 9.
/// A Rust panic is not a Java throwable — it terminates the VM, from one line
/// of ordinary application bytecode. `for _ in 0..(-code_point_offset)` was a
/// second one: negating `Integer.MIN_VALUE` overflows `i32`, which panics in
/// any debug-assertions profile.
///
/// Both branches now transcribe `Character.offsetByCodePoints`, whose loops
/// are bounded by `i < limit` / `i > start` and which reports a *shortfall*
/// (`x > 0` after the loop) as the throw. Measured on OpenJDK 25.0.3+9, all
/// four of these throw `IndexOutOfBoundsException` with a **null** message —
/// the plain superclass, no `Preconditions` text, unlike `codePointCount`'s
/// neighbouring `Range [a, b)` wording:
///
/// ```text
/// "a<U+1F600>b".offsetByCodePoints(0, 9)     IndexOutOfBoundsException  (ran off the end)
/// "a<U+1F600>b".offsetByCodePoints(0, -1)    IndexOutOfBoundsException  (ran off the start)
/// "a<U+1F600>b".offsetByCodePoints(10, -1)   IndexOutOfBoundsException  (index > length)
/// "a<U+1F600>b".offsetByCodePoints(-1, 1)    IndexOutOfBoundsException  (index < 0)
/// ```
///
/// while the in-range answers are `offsetByCodePoints(0, 2) == 3` and
/// `offsetByCodePoints(4, -2) == 1` — the surrogate pair counts as one step.
pub fn native_string_offset_by_code_points(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let index_i32 = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let code_point_offset = match args.get(2) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    // JIT r23 wave 2, lane L5 (`CRATONVM_STRING_CODE_POINT_WINDOW`): a short
    // walk reads its own units -- the same two arms as below, unit by unit.
    if i64::from(code_point_offset).abs() <= STRING_CODE_POINT_WINDOW_MAX
    {
        let units = StringCodeUnits::of(ctx, this);
        let n = units.len();
        if index_i32 < 0 || index_i32 as usize > n {
            return offset_by_code_points_error(ctx, args);
        }
        let mut pos = index_i32 as usize; // Cast: checked non-negative
        let high = |u: u16| (0xD800..=0xDBFF).contains(&u);
        let low = |u: u16| (0xDC00..=0xDFFF).contains(&u);
        let mut x = i64::from(code_point_offset).abs();
        if code_point_offset >= 0 {
            while x > 0 && pos < n {
                let ch = units.at(ctx, pos);
                pos += 1;
                if high(ch) && pos < n && low(units.at(ctx, pos)) {
                    pos += 1;
                }
                x -= 1;
            }
        } else {
            while x > 0 && pos > 0 {
                pos -= 1;
                if low(units.at(ctx, pos)) && pos > 0 && high(units.at(ctx, pos - 1)) {
                    pos -= 1;
                }
                x -= 1;
            }
        }
        if x > 0 {
            return offset_by_code_points_error(ctx, args);
        }
        return Ok(Some(Value::Int(pos as i32))); // Cast: pos <= length
    }
    // `read_string_chars`: see `native_string_code_point_at`.
    let chars = read_string_chars(ctx, this);
    let length = chars.len() as i32;
    // `String.offsetByCodePoints`'s own precondition. Note `index > length()`,
    // not `>=`: the position one past the last char is a legal starting point
    // for a backward walk.
    if index_i32 < 0 || index_i32 > length {
        return offset_by_code_points_error(ctx, args);
    }
    let mut pos = index_i32 as usize;
    if code_point_offset >= 0 {
        // `Character.offsetByCodePoints`, forward arm: `x` counts the steps
        // still owed, and a nonzero `x` at the end is the throw.
        let mut x = code_point_offset;
        while x > 0 && pos < chars.len() {
            let ch = chars[pos];
            pos += 1;
            if (0xD800..=0xDBFF).contains(&ch)
                && pos < chars.len()
                && (0xDC00..=0xDFFF).contains(&chars[pos])
            {
                pos += 1;
            }
            x -= 1;
        }
        if x > 0 {
            return offset_by_code_points_error(ctx, args);
        }
    } else {
        // Backward arm. `code_point_offset.unsigned_abs()` rather than
        // `-code_point_offset`: the latter panics on `Integer.MIN_VALUE`.
        let mut x = i64::from(code_point_offset.unsigned_abs());
        while x > 0 && pos > 0 {
            pos -= 1;
            if (0xDC00..=0xDFFF).contains(&chars[pos])
                && pos > 0
                && (0xD800..=0xDBFF).contains(&chars[pos - 1])
            {
                pos -= 1;
            }
            x -= 1;
        }
        if x > 0 {
            return offset_by_code_points_error(ctx, args);
        }
    }
    Ok(Some(Value::Int(pos as i32)))
}

/// The `[start, end)` code-unit ranges of `String.lines()` over `units`:
/// JDK 25 `LinesSpliterator` splits at `\n`, `\r` and `\r\n`, drops the
/// terminator, and yields no empty trailing line (`"a\n"` is one line, `""`
/// none, `"\n"` one empty line). Pure.
pub fn string_line_ranges(units: &[u16]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < units.len() {
        match units[i] {
            0x000A => {
                out.push((start, i));
                start = i + 1;
            }
            0x000D => {
                out.push((start, i));
                if i + 1 < units.len() && units[i + 1] == 0x000A {
                    i += 1;
                }
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if start < units.len() {
        out.push((start, units.len()));
    }
    out
}



pub fn native_string_lines(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    {
        let units = read_string_chars(ctx, this);
        let ranges = string_line_ranges(&units);
        // Each string can collect, moving the ones made before it: root each,
        // and read them all back only once the last allocation is behind us.
        // `make_stream_from_elements` pins the slice at entry.
        let mut scope = NativeHandleScope::new(ctx);
        let mut handles = Vec::with_capacity(ranges.len());
        for (start, end) in ranges {
            let line = sb_string_from_units(&mut *scope, &units[start..end])?;
            handles.push(scope.root(line));
        }
        let elements: Vec<Value> = handles
            .iter()
            .map(|h| Value::Object(Some(scope.get(h))))
            .collect();
        let r = cratonvm_native_collections::make_stream_from_elements(&mut *scope, &elements)?;
        // `LinesSpliterator.characteristics()`: ORDERED | IMMUTABLE | NONNULL.
        // Only on the synthetic carrier: under `--real-jdk` (reached there
        // only with `CRATONVM_STRICT_STRING_STREAM_BYTECODE=0`) the helper
        // answers a real `ReferencePipeline$Head`, whose slots are the JDK's
        // (round 17 wave 3, lane streams3).
        if let Some(Value::Object(Some(stream))) = r.filter(|v| match v {
            Value::Object(Some(s)) => {
                cratonvm_native_collections::is_synthetic_stream_object(&*scope, *s)
            }
            _ => false,
        }) {
            cratonvm_native_collections::record_source_spliterator_characteristics(
                &mut *scope,
                stream,
                0x510,
            );
        }
        Ok(r)
    }
}

pub fn native_string_indent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let n = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    // nio21 w10 L4: over code UNITS, with `String.lines()`'s terminators.
    // The old body iterated Rust's `str::lines()`, which splits on `\n` and
    // `\r\n` only, so `"a\rb".indent(0)` answered `"a\rb\n"` where HotSpot 25
    // answers `"a\nb\n"` (JDK `lines()` also ends a line at a lone `\r`), and
    // read the text through `read_string`, so an unpaired surrogate came back
    // as U+FFFD. JDK 25:
    //
    //   if (isEmpty()) return "";
    //   lines().map(n > 0 ? spaces + s : n == MIN_VALUE ? s.stripLeading()
    //       : n < 0 ? s.substring(Math.min(-n, s.indexOfNonWhitespace())) : s)
    //     .collect(joining("\n", "", "\n"))
    //
    // W7-95a (kept): the strip count is over `Character.isWhitespace`, capped
    // at the first non-whitespace unit (no supplementary code point is
    // whitespace, so units and code points agree there), and
    // `n.unsigned_abs()` makes `Integer.MIN_VALUE` "strip all of it" without
    // overflowing.
    let units = read_string_chars(ctx, this);
    let mut result: Vec<u16> = Vec::with_capacity(units.len() + 8);
    for line in java_lines_units(&units) {
        if n > 0 {
            result.extend(std::iter::repeat(0x20u16).take(n as usize));
            result.extend_from_slice(line);
        } else if n < 0 {
            let remove = n.unsigned_abs() as usize;
            let non_ws = line
                .iter()
                .position(|&u| !java_char_is_whitespace(u))
                .unwrap_or(line.len());
            result.extend_from_slice(&line[remove.min(non_ws)..]);
        } else {
            result.extend_from_slice(line);
        }
        result.push(0x0A);
    }
    // Only an empty receiver yields no line, and JDK 25 answers it with
    // `if (isEmpty()) return "";`, the literal (round 17 wave 4, lane sbempty,
    // `CRATONVM_STRING_LITERAL_IDENTITY`).
    let str_obj = if result.is_empty() && string_literal_identity_enabled(&*ctx) {
        ctx.intern_string("")
    } else {
        string_native_result_from_units(ctx, &result, false)?
    };
    Ok(Some(Value::Object(Some(str_obj))))
}

/// `String.lines()`'s split, over code units: a line ends at `\n`, `\r` or
/// `\r\n`; a trailing terminator does not start an empty last line.
pub fn java_lines_units(u: &[u16]) -> Vec<&[u16]> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < u.len() {
        if u[i] == 0x0A || u[i] == 0x0D {
            out.push(&u[start..i]);
            if u[i] == 0x0D && u.get(i + 1) == Some(&0x0A) {
                i += 1;
            }
            i += 1;
            start = i;
        } else {
            i += 1;
        }
    }
    if start < u.len() {
        out.push(&u[start..]);
    }
    out
}

pub fn native_string_transform(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let function =
        match args.get(1) {
            Some(Value::Object(Some(f))) => *f,
            // Returning the receiver made `s.transform(null)` behave like the
            // identity function — a plausible, silently wrong answer. `transform`
            // is `f.apply(this)`, so a null `f` is an NPE on OpenJDK 25.0.3+9.
            _ => return Err(string_arg_npe(
                "Cannot invoke \"java.util.function.Function.apply(Object)\" because \"f\" is null",
            )),
        };
    ctx.invoke_virtual(
        function,
        "apply",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(this))],
    )
}

// ---------------------------------------------------------------------------
// java.util.Formatter's NEGATIVE surface: the refusals, and the flags that
// change a value rather than decorate it.
// ---------------------------------------------------------------------------
//
// `probes/ShadowDifferentialProbe` measured five refusals against HotSpot 25
// on 2026-08-12 and CratonVM answered `no-throw` to every one of them
// (`docs/feature-designs/native-dispatch-architecture.md`). A
// formatter that never refuses is the shape W2-7 named: the negative half of
// the API answering "here you go".
//
// The type matters as much as the throw. `java.util.Formatter` specifies a
// DISTINCT `IllegalFormatException` subclass per failure, and callers catch
// the subclass — a mistyped refusal walks past the `catch` that was written
// for it and lands in one that was not. So each fault below carries the
// class the javadoc names, and is constructed through that class's REAL
// constructor rather than by allocating a Throwable and stuffing a message
// into slot 0: all six override `getMessage()` and never set
// `Throwable.detailMessage` (the same trap that kept
// `java/util/regex/PatternSyntaxException` out of the synthetic-exception
// bridge list in `lib.rs`).

/// A `java.util.Formatter` refusal, as the class the javadoc names for it.
///
/// Every variant's payload is exactly its JDK constructor's arguments, so
/// [`fmt_raise`] is a straight translation with no message reconstruction.
///
/// The variants cover every member of the `java.util.IllegalFormatException`
/// family that `Formatter`'s own parser and printers can actually reach on
/// JDK 25. Two members of the family are deliberately absent:
///
/// * `UnknownFormatFlagsException` is UNREACHABLE from `Formatter`. Its only
///   throw site is `Flags.parse(char)`'s `default` arm, and the only caller of
///   `Flags.parse` is `FormatSpecifier.flags(s, start, end)` over exactly the
///   run of characters `FormatSpecifierParser.parseFlag` accepted — and that
///   accepts precisely the eight characters `Flags.parse(char)` recognises.
///   The `default` arm is dead for every format string; only a hand-built
///   caller of the package-private `Flags` could reach it. Modelling it here
///   would be a variant no input can produce.
/// * `FormatterClosedException` is not in the family at all — it extends
///   `IllegalStateException`, not `IllegalFormatException`, and belongs to
///   `Formatter`'s lifecycle rather than to its format strings.
pub enum FmtFault {
    /// `%q` — "Conversion = 'q'". The conversion character is not one the
    /// javadoc's table defines.
    UnknownConversion(String),
    /// `%s %s` with one argument — "Format specifier '%s'". Carries the
    /// specifier's own text, which is what the JDK's message quotes.
    MissingArgument(String),
    /// `%d` of a String — "d != java.lang.String". Carries the conversion
    /// character and the argument's class.
    WrongType(char, cratonvm_types::ClassId),
    /// `%-08d` — "Flags = '-0'". Two flags that contradict each other; the
    /// message lists the specifier's WHOLE flag set.
    IllegalFlags(String),
    /// `%,x` — "Conversion = x, Flags = ,". A flag that is legal in general
    /// but not for this conversion; the message lists only the OFFENDING
    /// flags.
    FlagsMismatch(String, char),
    /// `%.2d` — "2". A precision on a conversion that has no fractional part.
    IllegalPrecision(i32),
    /// `%5n` — "5". A width on a conversion that has no field to justify in.
    IllegalWidth(i32),
    /// `%-d` — "%-d". `'-'` and `'0'` are relative to a field width, so they
    /// are meaningless without one; the message is the specifier's own text.
    MissingWidth(String),
    /// `%--8d` — "Flags = '-'". The same flag given twice.
    DuplicateFlags(String),
    /// `%c` of `0x110000` — "Code point = 0x110000". An `int`/`short`/`byte`
    /// argument to `%c` that `Character.isValidCodePoint` rejects. The message
    /// is `String.format("Code point = %#x", c)`, so a NEGATIVE code point
    /// renders as its unsigned 32-bit hex ("Code point = 0xffffffff").
    IllegalCodePoint(i32),
    /// `%0$s` — "Illegal format argument index = 0". An explicit argument
    /// index that is not a positive `int`. `Integer.MIN_VALUE` is the JDK's
    /// sentinel for "the digits did not parse as an int at all", and it prints
    /// a different message; see [`FmtFault::message`].
    ArgumentIndex(i32),
}

impl FmtFault {
    /// The class the javadoc names, and its constructor.
    pub fn class_and_ctor(&self) -> (&'static str, &'static str) {
        match self {
            FmtFault::UnknownConversion(_) => (
                "java/util/UnknownFormatConversionException",
                "(Ljava/lang/String;)V",
            ),
            FmtFault::MissingArgument(_) => (
                "java/util/MissingFormatArgumentException",
                "(Ljava/lang/String;)V",
            ),
            FmtFault::WrongType(..) => (
                "java/util/IllegalFormatConversionException",
                "(CLjava/lang/Class;)V",
            ),
            FmtFault::IllegalFlags(_) => (
                "java/util/IllegalFormatFlagsException",
                "(Ljava/lang/String;)V",
            ),
            FmtFault::FlagsMismatch(..) => (
                "java/util/FormatFlagsConversionMismatchException",
                "(Ljava/lang/String;C)V",
            ),
            FmtFault::IllegalPrecision(_) => ("java/util/IllegalFormatPrecisionException", "(I)V"),
            FmtFault::IllegalWidth(_) => ("java/util/IllegalFormatWidthException", "(I)V"),
            FmtFault::MissingWidth(_) => (
                "java/util/MissingFormatWidthException",
                "(Ljava/lang/String;)V",
            ),
            FmtFault::DuplicateFlags(_) => (
                "java/util/DuplicateFormatFlagsException",
                "(Ljava/lang/String;)V",
            ),
            FmtFault::IllegalCodePoint(_) => ("java/util/IllegalFormatCodePointException", "(I)V"),
            // The one member of the family that is PACKAGE-PRIVATE: JDK 25
            // declares `final class IllegalFormatArgumentIndexException` with
            // a package-private constructor, so only `java.util` code can name
            // it. `getClass().getName()` still reports the full name, which is
            // what a differential transcript records, and `fmt_raise` falls
            // back to the base class if the construction is refused — so
            // asking for it costs nothing if this VM ever grows the access
            // check that HotSpot would apply to a non-`java.util` caller.
            FmtFault::ArgumentIndex(_) => ("java/util/IllegalFormatArgumentIndexException", "(I)V"),
        }
    }

    /// The message the JDK's overridden `getMessage()` would build. Used ONLY
    /// by the fallback below, where the specified class could not be
    /// constructed — the real objects build it themselves.
    pub fn message(&self, ctx: &dyn NativeContext) -> String {
        match self {
            FmtFault::UnknownConversion(s) => format!("Conversion = '{s}'"),
            FmtFault::MissingArgument(s) => format!("Format specifier '{s}'"),
            FmtFault::WrongType(c, cid) => format!(
                "{c} != {}",
                ctx.class_name_of_id(*cid)
                    .unwrap_or_default()
                    .class_text(&*ctx)
            ),
            FmtFault::IllegalFlags(f) => format!("Flags = '{f}'"),
            FmtFault::FlagsMismatch(f, c) => format!("Conversion = {c}, Flags = {f}"),
            FmtFault::IllegalPrecision(p) | FmtFault::IllegalWidth(p) => p.to_string(),
            FmtFault::MissingWidth(s) => s.clone(),
            FmtFault::DuplicateFlags(f) => format!("Flags = '{f}'"),
            // `String.format("Code point = %#x", c)` over an `int`: `%x`
            // renders a negative `int` as unsigned 32-bit, so the cast is part
            // of the message and not a convenience.
            FmtFault::IllegalCodePoint(c) => format!("Code point = {:#x}", *c as u32),
            FmtFault::ArgumentIndex(i) => {
                if *i == i32::MIN {
                    "Format argument index: (not representable as int)".to_string()
                } else {
                    format!("Illegal format argument index = {i}")
                }
            }
        }
    }
}

/// Is the named `java.util.IllegalFormatException` subclass available to be
/// thrown as itself?
///
/// This is the whole of W7-41. The predicate used to be
/// `ctx.class_id_by_name(name).is_some()`, and `class_id_by_name` is
/// `find_unique_class_by_name` — an index read over the ALREADY-LOADED
/// classes, with no loading of its own. Nothing in a normal program ever
/// touches `java.util.UnknownFormatConversionException` before the moment
/// `String.format` needs to throw it, so that predicate was false on every
/// first refusal and the fallback below ran instead: right message, wrong
/// class. `format.unknownConversion` was measured as
/// `java.lang.IllegalArgumentException: Conversion = 'q'` — the base class
/// carrying the message this file reconstructs — which is that fallback's
/// exact signature. See W7-40-differential-at-14.md.
///
/// So: ask the class loader, not the index. Two screens keep the load from
/// making things worse than the defect it fixes:
///
/// * `would_fabricate_synthetic_stub` is checked FIRST and is non-destructive.
///   A build with no real `java.util` exception hierarchy would answer the
///   load with a minted stub that has no `<init>` and does not extend
///   `IllegalArgumentException`; throwing that would turn a wrong-superclass
///   defect into an object no `catch (IllegalArgumentException)` can catch.
/// * after the load, the class must actually BE an `IllegalArgumentException`.
///   That is the invariant the fallback relies on (see [`fmt_raise`]), and it
///   is cheap to confirm rather than assume.
pub fn fmt_exception_class_available(ctx: &mut dyn NativeContext, class_name: &str) -> bool {
    let class_id = match ctx.class_id_by_name(class_name) {
        Some(id) => id,
        None => {
            if ctx.would_fabricate_synthetic_stub(class_name) {
                return false;
            }
            // The `Err` is dropped on purpose. A `ClassNotFoundException` from
            // this speculative load is not the answer `String.format` owes its
            // caller — the format refusal is — and CratonVM carries a thrown
            // exception in the return value rather than in thread state, so
            // there is nothing left pending to clear.
            if ctx.load_class(class_name).is_err() {
                return false;
            }
            match ctx.class_id_by_name(class_name) {
                Some(id) => id,
                None => return false,
            }
        }
    };
    match ctx.class_id_by_name("java/lang/IllegalArgumentException") {
        Some(base) => ctx.is_subclass(class_id, base),
        // ACCEPT on an unanswerable screen, do not refuse. `class_id_by_name`
        // is `find_unique_class_by_name`, which returns `None` for an
        // AMBIGUOUS name as well as an absent one — and refusing on that would
        // re-create the exact defect this function exists to fix, silently, on
        // whatever configuration defines `IllegalArgumentException` twice.
        // Nothing is lost by accepting: the stub screen above has already run,
        // so what is being accepted here is a real loaded class named by the
        // javadoc, which is a better answer than the base class either way.
        None => true,
    }
}

/// Turn a [`FmtFault`] into the thrown Java exception.
///
/// Constructed through the class's real `<init>` because every one of them
/// overrides `getMessage()` off its own fields — allocating the class and
/// setting a `detailMessage` would produce an object whose `getMessage()` is
/// still null.
///
/// The fallback is `IllegalArgumentException`, which is the SUPERCLASS of
/// `IllegalFormatException` and therefore never sends a caller down a `catch`
/// branch it did not ask for; it is reached only when the specified class is
/// genuinely unavailable (see [`fmt_exception_class_available`]), never to
/// make a diff go away.
pub fn fmt_raise(ctx: &mut dyn NativeContext, fault: &FmtFault) -> MethodCallFailed {
    let (class_name, ctor) = fault.class_and_ctor();
    if fmt_exception_class_available(ctx, class_name) {
        let ctor_args: Vec<Value> = match fault {
            FmtFault::UnknownConversion(s)
            | FmtFault::MissingArgument(s)
            | FmtFault::IllegalFlags(s)
            | FmtFault::MissingWidth(s)
            | FmtFault::DuplicateFlags(s) => {
                let obj = ctx.create_string(s);
                vec![Value::Object(Some(obj))]
            }
            FmtFault::WrongType(c, cid) => {
                let mirror = ctx.get_class_mirror(*cid);
                vec![Value::Int(*c as i32), Value::Object(Some(mirror))]
            }
            FmtFault::FlagsMismatch(f, c) => {
                let obj = ctx.create_string(f);
                vec![Value::Object(Some(obj)), Value::Int(*c as i32)]
            }
            FmtFault::IllegalPrecision(p)
            | FmtFault::IllegalWidth(p)
            | FmtFault::IllegalCodePoint(p)
            | FmtFault::ArgumentIndex(p) => vec![Value::Int(*p)],
        };
        match ctx.new_object_initialized(class_name, ctor, &ctor_args) {
            Ok(Some(Value::Object(Some(exc)))) => return MethodCallFailed::ExceptionThrown(exc),
            // An `InternalError` is a VM fault (heap exhaustion, a broken
            // class file) and must not be dressed up as a format refusal.
            // A thrown Java exception, though, means the constructor itself
            // refused — a missing or inaccessible `<init>` on a class that
            // passed the screens above — and the caller is owed the refusal it
            // asked for, in the base class the subclass would have extended.
            Err(err @ MethodCallFailed::InternalError(_)) => return err,
            Err(MethodCallFailed::ExceptionThrown(_)) | Ok(_) => {}
        }
    }
    cratonvm_types::error::RuntimeError::IllegalArgumentException {
        message: fault.message(ctx),
    }
    .into()
}

/// The flag characters `java.util.Formatter` accepts, in `Flags.toString`'s
/// canonical order — which is the order every flag-bearing message lists them
/// in, regardless of the order they were written in the format string
/// (`%+ d` reports "Flags = '+ '", `%-08d` reports "Flags = '-0'").
pub const FMT_FLAG_ORDER: &str = "-#+ 0,(<";

/// Every character `java.util.Formatter`'s package-private `DateTime.isValid`
/// admits after a `%t`/`%T` prefix, verbatim from its `switch` on JDK 25.
///
/// The list is the JDK's, not CratonVM's: this decides whether a field is an
/// `UnknownFormatConversionException` ("t" + the character), which is a
/// question about the SPECIFIER and must be answered the same way whether or
/// not this VM happens to implement the field. `format_temporal_field`
/// currently implements all 31 of them, so the two sets coincide today —
/// but they are separate questions and a future gap must not silently become
/// a mistyped refusal.
pub const FMT_DATETIME_FIELDS: &str = "HIklMNLQpsSTzZaAbBCdehjmyYrRcDF";

/// `CharSequence.length()` for a Rust `str` — the count of UTF-16 code units.
///
/// Every width decision `java.util.Formatter` makes is
/// `width - cs.length()` in `appendJustified`, and a Java `length()` counts
/// UTF-16 code units. Rust's `str::len()` counts BYTES, and reaching for it
/// here padded non-ASCII output short: `String.format("%5s|", "\u{00e9}")`
/// came back with three spaces where HotSpot 25 writes four (é is one code
/// unit but two UTF-8 bytes), and an astral character — four bytes, TWO code
/// units — lost two more. ASCII is the case where the two agree, which is why
/// every existing row passed.
pub fn fmt_utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// `s.substring(0, precision)` — the precision truncation every GENERAL
/// conversion applies, counted in UTF-16 code units like `String.substring`.
///
/// This is now a plain `Vec::truncate` on code UNITS, and that is the whole
/// point. It replaced a `char_indices` walk over a Rust `&str`, which got two
/// separate things wrong:
///
///   * Counting CODE POINTS is wrong even when nothing straddles the cut:
///     `%.3s` of `"a<U+1F600>b"` is `"a<U+1F600>"` on HotSpot (three code
///     UNITS) and was `"a<U+1F600>b"` here.
///   * When the cut lands INSIDE a surrogate pair the JDK keeps the LONE HIGH
///     surrogate, and a Rust `String` cannot hold one — so the walk had to stop
///     before the pair and came back one code unit short. Measured on HotSpot
///     25 (`String.format(Locale.ROOT, …)` of `"😀"`, U+1F600):
///
///     | format | HotSpot | units |
///     |---|---|---|
///     | `%.1s` | length 1 | `D83D` |
///     | `[%5.1s]` | length **7** | `005B 0020 0020 0020 0020 D83D 005D` |
///     | `%.1S` | length 1 | `D83D` |
///     | `%.2s` of `"\uD800\uD800\uD800"` | length 2 | `D800 D800` |
///
///     The `[%5.1s]` row is the one that shows why the truncation and the
///     justifier had to move to units TOGETHER: the pad is computed from the
///     truncated length, so a truncation that came back short also padded one
///     space too many.
pub fn fmt_truncate_units(units: &mut Vec<u16>, prec: usize) {
    units.truncate(prec);
}

/// A run of `n` copies of the ASCII padding character `ch`, allocated
/// **fallibly**.
///
/// Every padding run in this file goes through here, and the reason is that a
/// width is caller data. `FormatSpecifier.width` refuses only a digit run that
/// overflows an `int`, so `%2000000000d` is a perfectly legal specifier that
/// asks for two billion spaces. `str::repeat` reaches `handle_alloc_error` when
/// that allocation fails, and a Rust abort is not a Java throwable: HotSpot 25
/// answers the same format string with a **catchable**
/// `OutOfMemoryError: Java heap space` (measured at `-Xmx256m`; at `-Xmx1g` the
/// same VM SERVES `%100000000d` and hands back a 100-million-character String,
/// which is why this is a fallible allocation and not a fixed cap — a cap would
/// refuse a width HotSpot answers).
///
/// `try_reserve_exact` is the same primitive [`native_string_repeat`] uses for
/// the same reason, one screen away in this file.
pub fn fmt_repeat(ch: char, n: usize) -> Result<String, MethodCallFailed> {
    let bytes = n.checked_mul(ch.len_utf8()).ok_or_else(fmt_out_of_memory)?;
    let mut s = String::new();
    if s.try_reserve_exact(bytes).is_err() {
        return Err(fmt_out_of_memory());
    }
    for _ in 0..n {
        s.push(ch);
    }
    Ok(s)
}

/// Ask the allocator for `n` bytes and give them straight back — the same
/// question [`fmt_repeat`] asks, for the callers that then build the run
/// themselves.
///
/// The PRECISION is the second unbounded axis and it behaves exactly like the
/// width. Measured on HotSpot 25 at `-Xmx256m`, all with a `Double` argument:
///
/// ```text
/// %.2000000000f   OutOfMemoryError: Java heap space
/// %.2000000000e   OutOfMemoryError: Java heap space
/// %.2000000000a   OutOfMemoryError: Java heap space
/// %.1000000f      SERVED — a 1,000,002-character String
/// ```
///
/// so, again, a catchable throwable and not a fixed cap. `fmt_render_fixed`,
/// `fmt_render_scientific` and `fmt_hex_pad` each push one character per
/// fraction digit into a growing `String`, which is a `handle_alloc_error`
/// abort at two billion; this probe runs once, at the single point in
/// `format_arg_full` where a precision reaches any of them, so none of their
/// signatures has to become fallible for the abort to stop being one.
pub fn fmt_reserve_probe(n: usize) -> Result<(), MethodCallFailed> {
    let mut s = String::new();
    if s.try_reserve_exact(n).is_err() {
        return Err(fmt_out_of_memory());
    }
    Ok(())
}

/// The `OutOfMemoryError` HotSpot 25 raises when a format's own width or
/// precision outruns the heap, message included (measured).
pub fn fmt_out_of_memory() -> MethodCallFailed {
    cratonvm_types::error::RuntimeError::OutOfMemoryError {
        message: "Java heap space".to_string(),
    }
    .into()
}

/// `java.util.Formatter.appendJustified` — **the** width justifier, for every
/// conversion in the table.
///
/// It used to be one of two. This body served `%t`/`%T` and the null path,
/// while `format_arg_full` carried a second, hand-inlined copy of the same
/// `width - cs.length()` for everything else; the two had already disagreed
/// once (the second counted UTF-8 bytes where this one counts UTF-16 code
/// units) and were fixed in parallel rather than merged. The survivor is this
/// one, and it counts code units through [`fmt_utf16_len`] — checked by
/// reading the body below, which has no `str::len` in it and no second
/// definition anywhere: `grep -n 'fmt_pad_to_width(' lang_string.rs` now
/// reaches every justification decision the file makes.
///
/// The zero-padding `format_arg_full` still does is NOT this function and was
/// not merged into it: the JDK's `trailingZeros` runs *inside* the numeric
/// printers, before `appendJustified` ever sees the buffer, and by the time it
/// has run the buffer is already `width` units long so this pads nothing.
///
/// Fallible for the reason [`fmt_repeat`] documents.
///
/// Takes and returns UTF-16 code UNITS. It used to take a `String`, and every
/// caller's output had already been through a lossy `str` round trip by the
/// time it arrived — so the one function that is *defined* as counting code
/// units was the one place a lone surrogate could no longer be present to be
/// counted. `out.len()` is now the count directly, which is also why
/// [`fmt_utf16_len`] no longer appears in this body: it survives only for the
/// `String`-shaped numeric decoration in `format_arg_full`, whose output is
/// ASCII by construction.
pub fn fmt_pad_to_width(
    out: Vec<u16>,
    flags: &str,
    width: Option<usize>,
) -> Result<Vec<u16>, MethodCallFailed> {
    match width {
        Some(w) if out.len() < w => {
            let pad = fmt_repeat(' ', w - out.len())?;
            let pad: Vec<u16> = pad.encode_utf16().collect();
            let mut joined = Vec::new();
            if joined
                .try_reserve_exact(out.len().saturating_add(pad.len()))
                .is_err()
            {
                return Err(fmt_out_of_memory());
            }
            if flags.contains('-') {
                joined.extend_from_slice(&out);
                joined.extend_from_slice(&pad);
            } else {
                joined.extend_from_slice(&pad);
                joined.extend_from_slice(&out);
            }
            Ok(joined)
        }
        _ => Ok(out),
    }
}

/// [`fmt_pad_to_width`] for a caller that still holds `str`-shaped output.
///
/// Deliberately a one-line adapter and NOT a second justifier: the width rule
/// has exactly one body, for the reason [`fmt_pad_to_width`]'s own doc records
/// (it used to be one of two, and the two had already disagreed once).
pub fn fmt_pad_str_to_width(
    out: &str,
    flags: &str,
    width: Option<usize>,
) -> Result<Vec<u16>, MethodCallFailed> {
    fmt_pad_to_width(out.encode_utf16().collect(), flags, width)
}

/// [`fmt_upper_case`] on code UNITS.
///
/// `toUpperCaseWithLocale` is defined on a `String`, and the mapping tables
/// `case_map` owns are indexed by scalar value — neither can say anything about
/// an unpaired surrogate, and the JDK's own `String.toUpperCase` leaves one
/// alone. So the units are split on the surrogates that cannot participate:
/// each well-formed run is case-mapped as text, and each lone surrogate is
/// carried through verbatim. That keeps the two length-changing properties this
/// family gets wrong:
///
///   * the mapping may GROW the text (`%S` of `"ß"` is `"SS"`), which is
///     why this runs BEFORE the justifier — `String.format(ROOT, "%5S", "ß")`
///     is `"   SS"`, five units, measured on HotSpot 25; and
///   * a lone surrogate is one unit before and one unit after.
pub fn fmt_upper_case_units(ctx: &mut dyn NativeContext, units: &[u16], locale: FmtLocale) -> Vec<u16> {
    if !has_unpaired_surrogate(units) {
        let text = String::from_utf16_lossy(units);
        return fmt_upper_case(ctx, &text, locale).encode_utf16().collect();
    }
    let mut out: Vec<u16> = Vec::with_capacity(units.len());
    let mut run: Vec<u16> = Vec::new();
    let mut i = 0usize;
    while i < units.len() {
        let u = units[i];
        let paired = (0xD800..=0xDBFF).contains(&u)
            && units
                .get(i + 1)
                .is_some_and(|n| (0xDC00..=0xDFFF).contains(n));
        if (0xD800..=0xDFFF).contains(&u) && !paired {
            if !run.is_empty() {
                let text = String::from_utf16_lossy(&run);
                out.extend(fmt_upper_case(ctx, &text, locale).encode_utf16());
                run.clear();
            }
            out.push(u);
            i += 1;
            continue;
        }
        run.push(u);
        i += 1;
        if paired {
            run.push(units[i]);
            i += 1;
        }
    }
    if !run.is_empty() {
        let text = String::from_utf16_lossy(&run);
        out.extend(fmt_upper_case(ctx, &text, locale).encode_utf16());
    }
    out
}

/// The word `java.util.Formatter`'s printers substitute for a **null**
/// argument, before any decoration.
///
/// Every printer in the table opens with
/// `if (arg == null) { print(fmt, "null", l); return; }` — except
/// `printBoolean`, which reaches the same shared printer with
/// `Boolean.toString(false)`. That one-conversion exception is the whole of
/// this function, and it exists so the rule is written once: `format_arg_full`
/// (which then applies the precision, the upper-caser and the justifier, as
/// `print(Formatter, String, Locale)` does) and [`format_arg`]'s own null arm
/// now read the SAME table rather than each carrying a copy of it.
pub fn fmt_null_text(spec: char) -> &'static str {
    if spec == 'b' || spec == 'B' {
        "false"
    } else {
        "null"
    }
}

/// `FormatSpecifier.toUpperCaseWithLocale` — the general family's upper-caser,
/// which is **locale-sensitive**.
///
/// ```java
/// s.toUpperCase(Objects.requireNonNullElse(l, Locale.getDefault(FORMAT)))
/// ```
///
/// This used to be a bare `str::to_uppercase`, which is Rust's
/// locale-independent mapping. Measured on HotSpot 25:
///
/// | | `Locale.ROOT` | `Locale.forLanguageTag("tr")` |
/// |---|---|---|
/// | `%S` of `"i"` | `I` | `U+0130` |
/// | `%C` of `Character.valueOf('i')` | `I` | `U+0130` |
/// | `%TA` of a Monday | `MON` | `PAZARTES` + `U+0130` |
///
/// and, with `Locale.getDefault(FORMAT)` forced to `tr`, an EXPLICIT-null
/// locale (`String.format((Locale) null, "%S", "i")`) is `U+0130` as well —
/// which is why the two non-`Some` [`FmtLocale`] arms both resolve the default
/// here and neither takes the root mapping. (That is the opposite of
/// [`fmt_symbols_for`], where an explicit null really does mean "no
/// localization"; the two questions are asked of the same `Locale` argument and
/// answered differently by the JDK, so they are answered differently here.)
///
/// The rules themselves are NOT reimplemented: `case_map` already owns the
/// `tr`/`az`/`lt` port of `ConditionalSpecialCasing`, `String.toUpperCase`
/// already calls it, and this is its third caller. `case_map::to_upper_case`
/// early-outs to `jdk_to_uppercase` for every other language, so the ROOT path
/// gains only the JDK-version-skew correction that `String.toUpperCase` has
/// had since W7-95a.
///
/// Resolving the language runs no bytecode — `locale_language_for_case_mapping`
/// reads `baseLocale.language` off the `Locale` object, or a `OnceLock`'d
/// `user.language` for the default — so there is no re-entrancy latch here and
/// none is needed.
pub fn fmt_upper_case(ctx: &mut dyn NativeContext, s: &str, locale: FmtLocale) -> String {
    // gen r4w3/rooting: the `Locale` is read back through its pin — a raw
    // copy taken at entry is stale after any earlier conversion's upcall.
    let given = match locale {
        FmtLocale::Given(Some(l)) => Some(l.current(&*ctx)),
        FmtLocale::Given(None) | FmtLocale::DefaultFormat => None,
    };
    let lang = crate::locale_language_for_case_mapping(ctx, given);
    crate::case_map::to_upper_case(s, &lang)
}

/// The LOWER-case twin of [`fmt_upper_case`], for `%tp`'s AM/PM marker — the
/// only place `java.util.Formatter` lower-cases anything.
///
/// ```java
/// String[] ampm = { "AM", "PM" };
/// if (l != null && l != Locale.US) { ampm = DateFormatSymbols.getInstance(l).getAmPmStrings(); }
/// sb.append(ampm[…].toLowerCase(Objects.requireNonNullElse(l, Locale.getDefault(FORMAT))));
/// ```
///
/// Identical in both of `printDateTime`'s printers, and the locale rule is the
/// upper-caser's: an explicit `null` takes the DEFAULT locale here, not
/// `Locale.US`, even though the STRING it is applied to is the English literal
/// in that case. That is why the two `FmtLocale` non-`Some` arms collapse
/// together below exactly as they do in [`fmt_upper_case`].
///
/// # This is a family convergence, and NO measured row moves
///
/// The body it replaces was `str::to_lowercase`, Rust's locale-independent
/// mapping — the exact mirror of the defect `W8-F4-1` N3 recorded on the UPPER
/// side and fixed there by routing through `case_map`. F28-1 §5.4 flagged the
/// twin without a live row and said so. I looked for one and there is none:
///
///  * Swept all **1158** locales this JDK reports available. For every one, and
///    for both of its `getAmPmStrings()` entries, `s.toLowerCase(locale)`
///    equals `s.toLowerCase(Locale.ROOT)` — **zero** divergent rows. Repeated
///    for the English literal `"AM"`/`"PM"` against every locale as the
///    case-mapping locale (the explicit-null and `Locale.US` paths): zero
///    again. The `tr`/`az`/`lt` rules need a dotted/dotless `I` or a
///    combining-dot sequence, and no AM/PM string in CLDR carries one.
///  * The other half of the change — `str::to_lowercase` → `jdk_to_lowercase`
///    — can only differ on [`crate::case_map::JDK_UNMAPPED_CASE_RUNS`],
///    which is `U+A7CE`, `U+A7CF`, `U+A7D2`..`U+A7D5`. No AM/PM string is in
///    Latin Extended-D.
///
/// It is landed anyway because the alternative is leaving one JDK rule with two
/// implementations, one of them fixed — this codebase's most common defect
/// shape, and the reason `%TA` under `tr` was wrong for as long as it was.
/// Whichever locale eventually grows a dotted `I` in its AM/PM data, this side
/// will already be right.
pub fn fmt_lower_case(ctx: &mut dyn NativeContext, s: &str, locale: FmtLocale) -> String {
    // gen r4w3/rooting: current `Locale` address, as in `fmt_upper_case`.
    let given = match locale {
        FmtLocale::Given(Some(l)) => Some(l.current(&*ctx)),
        FmtLocale::Given(None) | FmtLocale::DefaultFormat => None,
    };
    let lang = crate::locale_language_for_case_mapping(ctx, given);
    crate::case_map::to_lower_case(s, &lang)
}

/// `printString`'s first line: `if (arg instanceof Formattable)
/// { ((Formattable) arg).formatTo(fmt, flags, width, precision); }`.
///
/// `Ok(None)` means "not a `Formattable`" (or a VM with no usable
/// `java.util.Formatter`), and the caller falls through to `String.valueOf`.
/// `Ok(Some(units))` is the callee's output VERBATIM — the conversion applies
/// no precision, no upper-casing and no width on top of it, because `formatTo`
/// was handed all three and owns the decision. Measured on HotSpot 25 with a
/// `Formattable` whose `formatTo` writes `"x\uD800y"`: `%s`, `%S` and `%-10s`
/// all answer `length() == 3`, units `0078 D800 0079` — the `%S` row proving
/// the upper-caser does not run (an `x` that survived as `x`) and the `%-10s`
/// row proving the justifier does not; one whose `formatTo` writes `"😀"` under
/// `%.1s` answers TWO units, `D83D DE00`, so the precision does not truncate
/// either.
///
/// The output is read as UTF-16 code **units**. `formatTo` writes through a
/// `StringBuilder`, and `ctx.read_string` on that `StringBuilder`'s
/// `toString()` is a UTF-8 round trip that turns every unpaired surrogate into
/// U+FFFD — measured above, HotSpot keeps `D800`. This was the last general
/// conversion route still lossy after the rest of the pipeline moved to units,
/// and the loss was at THIS read, not downstream: the caller already returns
/// these units unchanged.
///
/// Measured on HotSpot 25 (a `Formattable` whose `formatTo` prints its three
/// arguments):
///
/// | format | flags | width | precision |
/// |---|---|---|---|
/// | `%s` | 0 | -1 | -1 |
/// | `%S` | 2 | -1 | -1 |
/// | `%#s` | 4 | -1 | -1 |
/// | `%-10s` | 1 | 10 | -1 |
/// | `%#-10.3S` | 7 | 10 | 3 |
/// | `%s %<s` (second) | **256** | -1 | -1 |
///
/// The first three bits are `java.util.FormattableFlags`' public constants
/// (`LEFT_JUSTIFY`/`UPPERCASE`/`ALTERNATE`); 256 is `Flags.PREVIOUS`, the
/// package-private `'<'` bit, which reaches a `Formattable` because
/// `checkGeneral` never removes it. Absent width and precision are `-1`, not
/// `0`. Also measured: `"[%10s]"` of a `Formattable` that writes nothing is
/// `"[]"` (the width is NOT applied afterwards), a `formatTo` that throws
/// propagates the exception out of `String.format`, and `%b`/`%h` of the same
/// object do NOT dispatch (`printBoolean` and `printHashCode` have no such
/// branch) — hence the `spec == 's'` guard at the call site.
///
/// **Why the interface is looked up but never loaded.** A `class_id_by_name`
/// miss is a conclusive "no" here: for `obj` to implement `Formattable`, the
/// interface must already have been resolved when `obj`'s own class was
/// linked, and `is_subclass` could not answer through an unloaded interface
/// anyway. So the hot `%s` path pays one index read and never a speculative
/// class load — unlike [`fmt_exception_class_available`], whose class genuinely
/// may not be loaded yet at the moment it is needed.
pub fn fmt_formattable_dispatch(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    flags: &str,
    width: Option<usize>,
    precision: Option<usize>,
    uppercase_conversion: bool,
    locale: FmtLocale,
) -> Result<Option<Vec<u16>>, MethodCallFailed> {
    let Some(iface) = ctx.class_id_by_name("java/util/Formattable") else {
        return Ok(None);
    };
    let cid = ctx.class_id_of_object(obj);
    if cid != iface && !ctx.is_subclass(cid, iface) {
        return Ok(None);
    }

    // `java.util.FormattableFlags`, and `Flags.PREVIOUS` for the `'<'` bit.
    let mut bits = 0i32;
    if flags.contains('-') {
        bits |= 1;
    }
    if uppercase_conversion {
        bits |= 2;
    }
    if flags.contains('#') {
        bits |= 4;
    }
    if flags.contains('<') {
        bits |= 256;
    }
    let w = width.map_or(-1i32, |v| v as i32);
    let p = precision.map_or(-1i32, |v| v as i32);

    // GC: everything below allocates and runs bytecode, so every reference
    // held across a call is pinned and re-derived. `pin_base` is the FIRST
    // handle, so one `unpin_native_roots(pin_base)` releases the whole batch.
    let pin_base = ctx.pin_native_root(obj);

    // The `Locale` the callee's own `Formatter` must carry:
    // `if (fmt.locale() != l) fmt = new Formatter(fmt.out(), l);`. An EXPLICIT
    // null locale is handed on as null (measured: the callee sees
    // `formatter.locale() == null`), which is why `Given(None)` is not folded
    // into the default arm the way [`fmt_upper_case`] folds it.
    let locale_ref = match locale {
        // gen r4w3/rooting: the pinned `Locale`'s current address.
        FmtLocale::Given(l) => l.map(|l| l.current(&*ctx)),
        // The no-`Locale` overload formats against `Locale.getDefault(FORMAT)`;
        // this VM keeps a single default `Locale`, which is what
        // `fmt_symbols_for`'s `DefaultFormat` arm resolves through the JDK as
        // well. A failure here is not worth failing the format over — the
        // callee then sees a null locale, which is the old behaviour of every
        // no-`Locale` surface in this file.
        FmtLocale::DefaultFormat => match ctx.invoke(
            "java/util/Locale",
            "getDefault",
            "()Ljava/util/Locale;",
            &[],
        ) {
            Ok(Some(Value::Object(Some(l)))) => Some(l),
            _ => None,
        },
    };
    let locale_pin = locale_ref.map(|l| ctx.pin_native_root(l));

    let sb = match ctx.new_object_initialized("java/lang/StringBuilder", "()V", &[]) {
        Ok(Some(Value::Object(Some(o)))) => o,
        // No usable `StringBuilder` means no `Appendable` to hand the callee.
        // Decline rather than refuse: the caller then prints `toString()`,
        // which is this conversion's behaviour before this function existed.
        _ => {
            ctx.unpin_native_roots(pin_base);
            return Ok(None);
        }
    };
    let sb_pin = ctx.pin_native_root(sb);
    let locale_ref = match (locale_ref, locale_pin) {
        (Some(l), Some(h)) => Some(ctx.read_native_pin(h, l)),
        _ => None,
    };
    let formatter = match ctx.new_object_initialized(
        "java/util/Formatter",
        "(Ljava/lang/Appendable;Ljava/util/Locale;)V",
        &[Value::Object(Some(sb)), Value::Object(locale_ref)],
    ) {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => {
            ctx.unpin_native_roots(pin_base);
            return Ok(None);
        }
    };
    let fmt_pin = ctx.pin_native_root(formatter);

    let obj = ctx.read_native_pin(pin_base, obj);
    let formatter = ctx.read_native_pin(fmt_pin, formatter);
    let call = ctx.invoke_virtual(
        obj,
        "formatTo",
        "(Ljava/util/Formatter;III)V",
        &[
            Value::Object(Some(formatter)),
            Value::Int(bits),
            Value::Int(w),
            Value::Int(p),
        ],
    );
    let sb = ctx.read_native_pin(sb_pin, sb);
    if let Err(err) = call {
        // "one that throws must propagate" — `formatTo` is declared to throw
        // and HotSpot lets an `IllegalStateException` out of `String.format`
        // untouched. Unpin first; the exception is the caller's answer.
        ctx.unpin_native_roots(pin_base);
        return Err(err);
    }
    // `read_string_chars`, NOT `ctx.read_string(..)`: see the surrogate rows in
    // this function's doc. The `StringBuilder` holds whatever `formatTo` wrote,
    // including a lone surrogate, and the UTF-8 round trip is where it died.
    let out = match ctx.invoke_virtual(sb, "toString", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => read_string_chars(ctx, s),
        Ok(_) => Vec::new(),
        Err(err) => {
            ctx.unpin_native_roots(pin_base);
            return Err(err);
        }
    };
    ctx.unpin_native_roots(pin_base);
    Ok(Some(out))
}

/// Render a flag set in `Flags.toString`'s canonical order.
pub fn fmt_flags_string(flags: &str) -> String {
    FMT_FLAG_ORDER
        .chars()
        .filter(|f| flags.contains(*f))
        .collect()
}

/// Rebuild a specifier's own text, which is what `FormatSpecifier.toString`
/// produces and what the `MissingFormatArgument` / `MissingFormatWidth`
/// messages quote.
pub fn fmt_spec_text(
    explicit_index: Option<usize>,
    flags: &str,
    width: Option<usize>,
    precision: Option<usize>,
    conversion: char,
) -> String {
    // `FormatSpecifier.toString` writes the FLAGS before the argument index,
    // and writes them in `Flags.toString`'s canonical order rather than the
    // order they were typed — `%#-s` reports itself as `%-#s`. The '<' flag
    // is one of them, so it survives here as written.
    let mut s = String::from("%");
    s.push_str(&fmt_flags_string(flags));
    if let Some(i) = explicit_index {
        s.push_str(&format!("{}$", i + 1));
    }
    if let Some(w) = width {
        s.push_str(&w.to_string());
    }
    if let Some(p) = precision {
        s.push('.');
        s.push_str(&p.to_string());
    }
    s.push(conversion);
    s
}

/// `java.util.Formatter`'s `checkGeneral`/`checkCharacter`/`checkInteger`/
/// `checkFloat`/`checkNumeric`, in their JDK order — the order decides WHICH
/// exception a doubly-illegal specifier gets.
///
/// The rules are the javadoc's, quoted where they are not obvious:
///
/// * numeric conversions — "If the `'-'` or `'0'` flags are given, then the
///   width is required" and `'+'` with `' '`, or `'-'` with `'0'`, is
///   "an illegal combination of flags".
/// * `'d'` — "If the `'#'` flag is given then a
///   FormatFlagsConversionMismatchException will be thrown."
/// * `'o'`/`'x'`/`'X'` — likewise for `','`, and (from `print(long, Locale)`)
///   for `'('`, `' '` and `'+'`, none of which a two's-complement rendering
///   has anywhere to put.
/// * every integral conversion — "If a precision is provided then an
///   IllegalFormatPrecisionException will be thrown", and the same for
///   `'c'`/`'C'`.
/// * `'e'`/`'E'` — grouping is a mismatch; `'a'`/`'A'` — grouping and
///   parentheses both are; `'g'`/`'G'` — `'#'` is.
pub fn fmt_check_spec(
    explicit_index: Option<usize>,
    flags: &str,
    width: Option<usize>,
    precision: Option<usize>,
    conversion: char,
) -> Result<(), FmtFault> {
    let spec_text = || fmt_spec_text(explicit_index, flags, width, precision, conversion);
    let mismatch = |bad: &str| {
        let offending: String = fmt_flags_string(flags)
            .chars()
            .filter(|c| bad.contains(*c))
            .collect();
        if offending.is_empty() {
            Ok(())
        } else {
            // `failMismatch` reports the LOWER-case conversion, because
            // `Conversion.isValid` folds `%X`/`%E`/`%S` down to `x`/`e`/`s`
            // and keeps the upper-casing in a separate internal flag. Only
            // `UnknownFormatConversionException` sees the character as typed,
            // since an unknown one is never folded — `%Q` really does say
            // "Conversion = 'Q'".
            Err(FmtFault::FlagsMismatch(
                offending,
                conversion.to_ascii_lowercase(),
            ))
        }
    };
    // `IllegalFormatFlagsException` reports `Flags.toString(flags)` over the
    // WHOLE set, and that set carries the JDK's INTERNAL `UPPERCASE` flag —
    // added by `Conversion.isValid` for every upper-case conversion and
    // rendered as `'^'`, between `'-'` and `'#'`. So `%+ X` says
    // "Flags = '^+ '" where `%+ x` says "Flags = '+ '". `FormatSpecifier`'s
    // own `toString` explicitly REMOVES it again, which is why `spec_text`
    // does not carry it.
    let all_flags = || {
        let mut s = fmt_flags_string(flags);
        if conversion.is_ascii_uppercase() {
            s.insert(usize::from(s.starts_with('-')), '^');
        }
        s
    };
    // `checkNumeric`, shared by the integral and float families.
    let check_numeric = || -> Result<(), FmtFault> {
        if width.is_none() && (flags.contains('-') || flags.contains('0')) {
            return Err(FmtFault::MissingWidth(spec_text()));
        }
        if (flags.contains('+') && flags.contains(' '))
            || (flags.contains('-') && flags.contains('0'))
        {
            return Err(FmtFault::IllegalFlags(all_flags()));
        }
        Ok(())
    };

    match conversion {
        // General: 'b'/'B' 'h'/'H' 's'/'S'.
        'b' | 'B' | 'h' | 'H' | 's' | 'S' => {
            // `checkGeneral` rejects '#' up front for 'b'/'h' only; for
            // 's' the JDK gets there later, inside `printString`, which is
            // why `%#-s` reports the MISSING WIDTH and `%#-b` reports the
            // flag mismatch. Order is the whole of the difference.
            if matches!(conversion, 'b' | 'B' | 'h' | 'H') {
                mismatch("#")?;
            }
            if width.is_none() && flags.contains('-') {
                return Err(FmtFault::MissingWidth(spec_text()));
            }
            mismatch("+ 0,(")?;
            // `'#'` for 's'/'S' is NOT refused here, and the omission is the
            // point. `printString` opens with the `Formattable` dispatch and
            // only reaches `failMismatch(ALTERNATE, 's')` in its `else` — so
            // `%#s` of a `Formattable` is LEGAL and hands `ALTERNATE` to the
            // callee, while `%#s` of anything else still throws. Both halves
            // are measured on HotSpot 25 and both are in `format_arg_full`,
            // which is the only place the argument's class is known.
            //
            // Deferring it is visible in three other orderings, all measured:
            // `%#,s` is the ','  mismatch (this layer, before the argument),
            // `%#-s` is `MissingFormatWidthException: %-#s` (this layer), and
            // `%#s` with NO argument is
            // `MissingFormatArgumentException: Format specifier '%#s'` — the
            // argument fetch sits between the two layers, so hoisting the
            // refusal back up here would answer that last one wrongly.
        }
        'c' | 'C' => {
            if let Some(p) = precision {
                return Err(FmtFault::IllegalPrecision(p as i32));
            }
            mismatch("#+ 0,(")?;
            if width.is_none() && flags.contains('-') {
                return Err(FmtFault::MissingWidth(spec_text()));
            }
        }
        'd' | 'o' | 'x' | 'X' => {
            check_numeric()?;
            if let Some(p) = precision {
                return Err(FmtFault::IllegalPrecision(p as i32));
            }
            if conversion == 'd' {
                mismatch("#")?;
            } else {
                mismatch(",")?;
                // `print(long, Locale)`'s own `checkBadFlags(PARENTHESES |
                // LEADING_SPACE | PLUS)` is NOT here, deliberately: it runs
                // inside the printer, so it does not apply to a BigInteger
                // argument (whose printer has no such check) nor to a null one
                // (which never reaches a printer). It lives in
                // `format_arg_full`, where the argument's class is known.
            }
        }
        'e' | 'E' | 'f' | 'g' | 'G' | 'a' | 'A' => {
            check_numeric()?;
            match conversion {
                'a' | 'A' => mismatch("(,")?,
                'e' | 'E' => mismatch(",")?,
                'g' | 'G' => mismatch("#")?,
                _ => {}
            }
        }
        other => return Err(FmtFault::UnknownConversion(other.to_string())),
    }
    Ok(())
}

/// The four `java.text.DecimalFormatSymbols` characters
/// `java.util.Formatter` reads off a `Locale`.
///
/// The JDK's own
/// `getZero`/`getDecimalSeparator`/`getGroupingSeparator`/`getMinusSign`
/// helpers read exactly these four off
/// `DecimalFormatSymbols.getInstance(locale)`, and answer
/// `'0'`/`'.'`/`','`/`'-'` for a null locale.
///
/// # `minus` has exactly ONE consumer, and it is not a numeric conversion
///
/// F28-1 §5.3 recorded this symbol as missing and described its blast radius as
/// "`%tF`'s minus, and every negative `%d`/`%f`/`%e`/`%g`". **The second half of
/// that is wrong**, and the correction is why `minus` is affordable at all.
/// `getMinusSign` has a SINGLE call site in `java.base/java/util/Formatter.java`
/// (line 4635 of the JDK 25 source), inside the `TemporalAccessor` printer's
/// `ISO_STANDARD_DATE` arm. Every other negative number takes `leadingSign`,
/// which appends a bare ASCII `'-'` (or `'('`) and consults no locale at all.
///
/// Measured on HotSpot 25 under `lt-LT`, `et-EE`, `sl-SI`, `sv-SE` and `fi-FI`
/// — five of the 59 available locales whose `getMinusSign()` is U+2212 MINUS
/// SIGN — dumped as UTF-16 code units:
///
/// | conversion | argument | HotSpot 25 |
/// |---|---|---|
/// | `%d` | `-5` | `002D 0035` — ASCII |
/// | `%,d` | `-1234567` | `002D` … — ASCII |
/// | `%.2f` `%e` `%g` `%a` | `-1234.5` | `002D` … — ASCII |
/// | `%d` | `BigInteger("-5")` | `002D 0035` — ASCII |
/// | `%.2f` | `BigDecimal("-1.5")` | `002D` … — ASCII |
/// | `%tz` | `OffsetDateTime` at `-03` | `002D 0030 0033 0030 0030` — ASCII |
/// | `%tF` | `LocalDate.of(-44,3,15)` | **`2212`** `0030 0030 0034 0034 002D …` |
///
/// The last row is the whole of it: the leading year sign is localized and the
/// two `'-'` DATE SEPARATORS in the same field are not. So this symbol costs one
/// extra `getMinusSign` call per already-cached [`fmt_symbols_for`] resolution
/// and closes the row completely, rather than "one quarter of one row" against a
/// hot path — which is the trade F28 declined on a premise that had not been
/// measured.
#[derive(Clone, Copy)]
pub struct FmtSymbols {
    pub grouping: char,
    pub decimal: char,
    pub zero: char,
    /// `DecimalFormatSymbols.getMinusSign()`. Read by [`fmt_iso_year`] and by
    /// nothing else — see the type doc.
    pub minus: char,
}

impl Default for FmtSymbols {
    fn default() -> Self {
        FmtSymbols {
            grouping: ',',
            decimal: '.',
            zero: '0',
            // `getMinusSign(Locale)` is `locale == null ? '-' : …`, the same
            // shape as the other three. Measured: `String.format((Locale) null,
            // "%tF", LocalDate.of(-44,3,15))` is `-0044-03-15` with an ASCII
            // U+002D even on a host whose default locale's minus is U+2212.
            minus: '-',
        }
    }
}

/// Which `Locale` a `java.util.Formatter` call localizes against.
///
/// This exists because the `Option<Locale>` it replaced could not tell two
/// different requests apart, and `java.util.Formatter` answers them
/// differently:
///
/// * **The overload has no `Locale` parameter** — `String.format(String,
///   Object...)`, `String.formatted`, `PrintStream.printf(String, Object...)`,
///   `new Formatter()`. Every one of these is specified as formatting with
///   "the locale returned by `Locale.getDefault(Locale.Category.FORMAT)`".
/// * **The overload was given one, and it is `null`** —
///   `String.format((Locale) null, …)`. "If `l` is `null` then no localization
///   is applied", i.e. the root separators and ASCII digits.
///
/// Collapsing the two onto one `None` is the defect this type removes (W7-91
/// §5, and the last open `format` row of
/// `docs/feature-designs/native-dispatch-architecture.md`): the
/// no-`Locale` overload took the explicit-null branch and localized against
/// `Locale.ROOT` on every host. Invisible on a ROOT/en-US host, and wrong
/// everywhere else — separators, grouping and digits all diverge.
///
/// The distinction is resolved AT CONSUMPTION, in [`fmt_symbols_for`] and
/// [`fmt_date_name`], never by re-encoding one variant as the other. Those two
/// are one JDK rule implemented twice and they had already drifted: the date
/// half was reading `DateFormatSymbols.getInstance()` (the FORMAT default) for
/// the absent-locale case while the number half took the root constants.
#[derive(Clone, Copy)]
pub enum FmtLocale {
    /// The overload has no `Locale` parameter, so the JDK supplies
    /// `Locale.getDefault(Locale.Category.FORMAT)`.
    DefaultFormat,
    /// The overload carries an explicit `Locale` argument, possibly `null`.
    Given(Option<FmtLocaleRef>),
}

/// An explicit `Locale` argument together with the native pin that keeps it
/// reachable at its CURRENT address.
///
/// gen r4w3/rooting: the formatter runs bytecode between conversions
/// (`DecimalFormatSymbols.getInstance`, `toString`/`hashCode` upcalls,
/// `DateFormatSymbols`, `TimeZone.getDisplayName`), and a moving young
/// collection relocates the `Locale` like any other object. A bare `ObjectRef`
/// copied out of `args[0]` then names a vacated from-space address. `format_impl`
/// pins it once for the whole call; every consumer reads it back through
/// [`FmtLocaleRef::current`] rather than holding the raw reference. `pin` is
/// `None` only when `CRATONVM_NO_FORMAT_ARG_PIN` disables pinning, which is the
/// old (unpinned) behaviour.
#[derive(Clone, Copy)]
pub struct FmtLocaleRef {
    pub obj: cratonvm_types::ObjectRef,
    pub pin: Option<usize>,
}

impl FmtLocaleRef {
    /// The `Locale`'s current address — re-derived from the pin, so it is valid
    /// after any collection that ran since the formatter was entered.
    pub fn current(self, ctx: &dyn NativeContext) -> cratonvm_types::ObjectRef {
        match self.pin {
            Some(h) => ctx.read_native_pin(h, self.obj),
            None => self.obj,
        }
    }
}

thread_local! {
    /// Re-entrancy latch for [`fmt_symbols_for`].
    ///
    /// Resolving symbols runs real JDK bytecode (resource bundles, locale
    /// providers) which is free to call `String.format(Locale, …)` itself. It
    /// would then re-enter this native and ask for symbols again, on a locale
    /// whose symbols are still mid-construction. While the latch is set the
    /// inner call takes the root defaults, which is the same answer the JDK's
    /// own null-locale branch gives and cannot recurse.
    pub static FMT_SYMBOLS_RESOLVING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Read a `java.util.Locale`'s formatting symbols, or the root defaults for an
/// explicit null locale.
///
/// Goes through `DecimalFormatSymbols.getInstance(Locale)` rather than a
/// hard-coded table because that is what `java.util.Formatter` does, and
/// because the separators are not guessable: France's grouping separator is
/// U+202F NARROW NO-BREAK SPACE, not U+0020 — measured on HotSpot 25, and
/// CratonVM's own `DecimalFormatSymbols` was measured to answer the identical
/// three characters for ROOT/US/GERMANY/FRANCE, so this reads a real value
/// rather than reproducing one.
///
/// The three [`FmtLocale`] arms, and why each is what the JDK does:
///
/// * `Given(None)` — an explicit `null` `Locale`. The JDK's own
///   `Formatter.zero(Locale)` is `if ((l != null) && !l.equals(Locale.US)) {…}
///   return '0';`, so a null locale takes the three constants and runs no
///   bytecode at all. Unchanged, and it is also the arm every `new Formatter()`
///   currently arrives on (see the residual note below).
/// * `Given(Some(l))` — resolve `l`. Unchanged, deliberately: the
///   very common `String.format(Locale.ROOT, …)` must not become slower or
///   different because the no-`Locale` overload was fixed. The only fast path
///   ROOT ever had is the LAZINESS in [`format_impl`] — this function is not
///   reached at all unless a conversion actually localizes — and that is
///   untouched.
/// * `DefaultFormat` — the overload had no `Locale`. Resolves
///   through the NO-ARG `DecimalFormatSymbols.getInstance()`, whose body is
///   `getInstance(Locale.getDefault(Locale.Category.FORMAT))`. Asking the JDK
///   for the default rather than composing one here is what keeps this from
///   becoming a fourth opinion about what the default locale is — the same
///   argument [`fmt_date_name`] already makes, and the reason these two agree
///   again.
///
/// W7-34 left this arm undone and named the hazard: "resolving a default locale
/// on the no-locale path is the one place where the re-entrancy hazard is
/// worst — every internal `String.format` in the VM, including the logging
/// shims, goes through it." Two things bound it, and neither is new code.
thread_local! {
    pub static CACHED_FMT_SYMBOLS: std::cell::RefCell<Option<(usize, u64, usize, FmtSymbols)>> =
        const { std::cell::RefCell::new(None) };
}

/// First, LAZINESS: `format_impl` only calls this at a `%d`/`%f`/`%e`/`%g`
/// conversion, so an internal `String.format("{}: {}"-shaped %s format)` still
/// runs zero locale bytecode. Second, `FMT_SYMBOLS_RESOLVING`: an inner
/// `String.format` raised while the outer one is resolving answers the root
/// constants and cannot recurse. `fmt_date_name` has been taking exactly this
/// route on exactly this path since the `%t` name fields landed.
pub fn fmt_symbols_for(ctx: &mut dyn NativeContext, locale: FmtLocale) -> FmtSymbols {
    if let FmtLocale::Given(None) = locale {
        return FmtSymbols::default();
    }
    let locale_key = match locale {
        FmtLocale::DefaultFormat => 0usize,
        FmtLocale::Given(Some(l)) => l.obj.as_ptr() as usize,
        FmtLocale::Given(None) => return FmtSymbols::default(),
    };
    let vm = ctx.vm_identity();
    let epoch = ctx.gc_address_epoch();
    if let Some((c_vm, c_epoch, c_key, c_sym)) = CACHED_FMT_SYMBOLS.with(|c| *c.borrow()) {
        if c_vm == vm && c_epoch == epoch && c_key == locale_key {
            return c_sym;
        }
    }
    if FMT_SYMBOLS_RESOLVING.with(std::cell::Cell::get) {
        return FmtSymbols::default();
    }
    FMT_SYMBOLS_RESOLVING.with(|f| f.set(true));
    let resolved = (|| {
        let instance = match locale {
            FmtLocale::Given(Some(l)) => {
                // gen r4w3/rooting: current address through the pin.
                let l = l.current(&*ctx);
                ctx.invoke(
                    "java/text/DecimalFormatSymbols",
                    "getInstance",
                    "(Ljava/util/Locale;)Ljava/text/DecimalFormatSymbols;",
                    &[Value::Object(Some(l))],
                )
            }
            // `DefaultFormat`. `Given(None)` returned above, so this arm is
            // only ever the no-`Locale` overload.
            _ => ctx.invoke(
                "java/text/DecimalFormatSymbols",
                "getInstance",
                "()Ljava/text/DecimalFormatSymbols;",
                &[],
            ),
        };
        let dfs = match instance {
            Ok(Some(Value::Object(Some(o)))) => o,
            _ => return None,
        };
        // GC: the four reads below are `invoke_virtual`s, so each can collect
        // and relocate `dfs`. A receiver is rooted FOR the call it is passed to
        // and by nothing between them, so reads two through four were issued
        // against the address the first one saw. Pin it and re-derive per read.
        // Same defect, same file, same call as the varargs array the caller
        // pins -- see
        // `bug-generational-ntru-unpinned-jit-reference-20260821-FIXED.md`.
        let dfs_pin = if format_arg_pin_enabled() {
            Some(ctx.pin_native_root(dfs))
        } else {
            None
        };
        let read = |ctx: &mut dyn NativeContext, name: &str, fallback: char| -> char {
            let recv: cratonvm_types::ObjectRef = match dfs_pin {
                Some(h) => ctx.read_native_pin(h, dfs),
                None => dfs,
            };
            match ctx.invoke_virtual(recv, name, "()C", &[]) {
                Ok(Some(Value::Int(c))) => char::from_u32(c as u32).unwrap_or(fallback),
                _ => fallback,
            }
        };
        let syms = FmtSymbols {
            grouping: read(ctx, "getGroupingSeparator", ','),
            decimal: read(ctx, "getDecimalSeparator", '.'),
            zero: read(ctx, "getZeroDigit", '0'),
            // The fourth read. It is the same `()C` shape as the other three
            // and it is resolved eagerly with them rather than lazily at
            // [`fmt_iso_year`], because its only consumer sits inside
            // `format_temporal_field`, which has no `ctx` re-entrancy budget of
            // its own and is already handed the resolved `FmtSymbols`. 59 of
            // this JDK's 1158 available locales answer U+2212 here.
            minus: read(ctx, "getMinusSign", '-'),
        };
        if let Some(h) = dfs_pin {
            ctx.unpin_native_roots(h);
        }
        Some(syms)
    })();
    FMT_SYMBOLS_RESOLVING.with(|f| f.set(false));
    if let Some(syms) = resolved {
        CACHED_FMT_SYMBOLS.with(|c| {
            *c.borrow_mut() = Some((vm, epoch, locale_key, syms));
        });
        return syms;
    }
    FmtSymbols::default()
}

thread_local! {
    /// Re-entrancy latch for [`fmt_date_name`] — the `DateFormatSymbols` twin
    /// of [`FMT_SYMBOLS_RESOLVING`], and it exists for the same reason.
    ///
    /// `DateFormatSymbols.getInstance` runs real JDK bytecode: the locale
    /// provider chain, `ResourceBundle`, and (since W7-80) the CLDR bundle
    /// classes out of the JDK image. Any of that is free to call
    /// `String.format` itself — a `tracing`-style diagnostic or an exception
    /// message is enough — which re-enters this native and asks for the same
    /// locale's symbols while they are still mid-construction. While the latch
    /// is set the inner call answers `None` and the caller falls back to the
    /// English table, which is what the JDK's own null-locale branch prints
    /// and cannot recurse.
    pub static FMT_DATE_NAMES_RESOLVING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// One element of one `java.text.DateFormatSymbols` name array, for the
/// `%t`/`%T` conversions that render a NAME rather than a number.
///
/// `getter` is the no-arg `()[Ljava/lang/String;` accessor
/// `java.util.Formatter` itself calls — `getMonths`, `getShortMonths`,
/// `getWeekdays`, `getShortWeekdays` or `getAmPmStrings` — and `index` is the
/// index into it, in that array's own convention (months 0-based over 13
/// entries, weekdays 1-based over 8 with slot 0 unused).
///
/// `locale` is the call's [`FmtLocale`]. `Given(Some(l))` resolves
/// `DateFormatSymbols.getInstance(l)`; every other arm resolves through the
/// NO-ARG `DateFormatSymbols.getInstance()`, whose body is
/// `getInstance(Locale.getDefault(Locale.Category.FORMAT))` — i.e. exactly the
/// `Locale` a real `java.util.Formatter` built by `String.format(String,
/// Object...)` would be carrying. Asking the JDK for it rather than composing
/// one here is what keeps this from becoming a fourth opinion about what the
/// default locale is.
///
/// # `Given(None)` is `Locale.US`, which the English tables below already are
///
/// `Formatter.printDateTime` opens every name field with `Locale lt =
/// Objects.requireNonNullElse(l, Locale.US)`, so a literal
/// `String.format((Locale) null, "%tB", d)` renders ENGLISH. This arm used to
/// resolve the default FORMAT locale instead. MEASURED on HotSpot 25 with the
/// host default forced to a non-US locale — the case F22 could not reach and
/// recorded rather than guess at:
///
/// | expression | `-Duser.language=tr -Duser.country=TR` | `ru_RU` |
/// |---|---|---|
/// | `format((Locale) null, "%tb", d)` | `Nov` | `Nov` |
/// | `format("%tb", d)` (default) | `Kas` | `нояб.` |
/// | `format((Locale) null, "%tA", d)` | `Wednesday` | `Wednesday` |
/// | `format((Locale) null, "%tp", d)` | `am` | `am` |
/// | `format("%tp", d)` (default) | `öö` | `am` |
///
/// So `Given(None)` returns `None` HERE — before the latch, before any
/// bytecode — and the caller prints its English table. Those four tables ARE
/// `Locale.US`'s answers: `DateFormatSymbols.getInstance(Locale.US)` gives
/// `Jan..Dec`, `January..December`, `Sun..Sat`, `Sunday..Saturday` and
/// `{"AM","PM"}`, which is also why `printDateTime`'s own `AM_PM` arm skips
/// `DateFormatSymbols` entirely for a null (and for `Locale.US`) and uses the
/// literal `String[] ampm = { "AM", "PM" }`. Asking the JDK for US data would
/// be a slower route to the same characters, with a re-entrancy hazard the
/// early return does not have.
///
/// **This shape matches [`fmt_symbols_for`], deliberately.** That helper has
/// answered `Given(None)` with the root constants since it was written — the
/// JDK's `getGroupingSeparator(Locale)` is literally `locale == null ? ',' :
/// …` — and the two now give an explicit null locale ONE meaning across the
/// whole file. Measured: `format((Locale) null, "%,d", 1234567)` is
/// `1,234,567` on a `tr_TR` host where the default gives `1.234.567`.
/// [`fmt_upper_case`] is the deliberate exception and stays as it is:
/// `toUpperCaseWithLocale` is `s.toUpperCase(Objects.requireNonNullElse(l,
/// Locale.getDefault(FORMAT)))`, the DEFAULT — measured, `format((Locale)
/// null, "%S", "i")` is U+0130 on a `tr_TR` host, not `I`. Three sites, two
/// JDK rules, and the JDK does mean both.
///
/// # What this costs until the `Formatter` constructors are fixed
///
/// In this VM `Given(None)` is not only an explicit null: `new Formatter()`
/// and `new Formatter(Appendable)` reach `format` through natives in
/// `native-builtins/src/lib.rs` (and `register_formatter_natives` in the same
/// file) that write `null` into the receiver's locale slot, where the real
/// `java.util.Formatter()` constructor writes
/// `Locale.getDefault(Locale.Category.FORMAT)`; `format` then reads slot 1 and
/// arrives here as `Given(None)`. For THOSE the right answer is the default
/// locale, and this arm now gets them wrong. That is not a new trade — it is
/// the trade `fmt_symbols_for` has always made on the same path, so the change
/// makes the file self-consistent and moves the whole residual into ONE fix,
/// in the constructor rather than in three consumers. See W7-34's residuals,
/// which already carry that constructor row as open, and F28-1's NOMINATION.
///
/// `None` on any failure, and the caller then prints the English name it
/// printed before. Every failure mode is a legitimate one: synthetic-JDK mode
thread_local! {
    pub static CACHED_DATE_NAMES: std::cell::RefCell<Option<(usize, u64, usize, &'static str, Vec<String>)>> =
        const { std::cell::RefCell::new(None) };
}

/// has no `java.text.DateFormatSymbols.getInstance`, a jlinked image can be
/// missing `jdk.localedata`, and an entry can be the empty string (both arrays
/// carry one). The `Err` from `invoke` is dropped on purpose — the caller is
/// owed a formatted string, not this lookup's failure, and CratonVM carries a
/// thrown exception in the return value rather than in thread state, so
/// nothing is left pending to clear (the same argument `fmt_raise` makes for
/// its speculative `load_class`).
pub fn fmt_date_name(
    ctx: &mut dyn NativeContext,
    locale: FmtLocale,
    getter: &str,
    index: usize,
) -> Option<String> {
    // An explicit null `Locale` is `Locale.US`, and the caller's English table
    // IS `DateFormatSymbols.getInstance(Locale.US)` for all five getters — see
    // the measured rows above. Returning before the latch keeps this the one
    // arm that runs no bytecode at all, the same shape `fmt_symbols_for` uses
    // for the same request.
    if let FmtLocale::Given(None) = locale {
        return None;
    }
    let locale_key = match locale {
        FmtLocale::DefaultFormat => 0usize,
        FmtLocale::Given(Some(l)) => l.obj.as_ptr() as usize,
        FmtLocale::Given(None) => return None,
    };
    let vm = ctx.vm_identity();
    let epoch = ctx.gc_address_epoch();
    let cached = CACHED_DATE_NAMES.with(|c| {
        let borrow = c.borrow();
        if let Some((c_vm, c_epoch, c_key, c_getter, ref c_arr)) = *borrow {
            if c_vm == vm && c_epoch == epoch && c_key == locale_key && c_getter == getter {
                return Some(c_arr.get(index).filter(|s| !s.is_empty()).cloned());
            }
        }
        None
    });
    if let Some(hit) = cached {
        return hit;
    }
    if FMT_DATE_NAMES_RESOLVING.with(std::cell::Cell::get) {
        return None;
    }
    FMT_DATE_NAMES_RESOLVING.with(|f| f.set(true));
    let resolved = (|| {
        let instance = match locale {
            FmtLocale::Given(Some(l)) => {
                // gen r4w3/rooting: current address through the pin.
                let l = l.current(&*ctx);
                ctx.invoke(
                    "java/text/DateFormatSymbols",
                    "getInstance",
                    "(Ljava/util/Locale;)Ljava/text/DateFormatSymbols;",
                    &[Value::Object(Some(l))],
                )
            }
            // `Given(None)` returned above, so this arm is only ever the
            // no-`Locale` overload.
            _ => ctx.invoke(
                "java/text/DateFormatSymbols",
                "getInstance",
                "()Ljava/text/DateFormatSymbols;",
                &[],
            ),
        };
        let dfs: cratonvm_types::ObjectRef = match instance {
            Ok(Some(Value::Object(Some(o)))) => o,
            _ => return None,
        };
        let arr: cratonvm_types::ObjectRef =
            match ctx.invoke_virtual(dfs, getter, "()[Ljava/lang/String;", &[]) {
                Ok(Some(Value::Object(Some(a)))) => a,
                _ => return None,
            };
        let len = ctx.array_length(arr);
        let mut strings = Vec::with_capacity(len);
        for i in 0..len {
            let elem = ctx.get_array_element(arr, i);
            let s = match elem {
                Value::Object(Some(o)) => ctx.read_string(o).unwrap_or_default(),
                _ => String::new(),
            };
            strings.push(s);
        }
        let static_getter = match getter {
            "getMonths" => Some("getMonths"),
            "getShortMonths" => Some("getShortMonths"),
            "getWeekdays" => Some("getWeekdays"),
            "getShortWeekdays" => Some("getShortWeekdays"),
            "getAmPmStrings" => Some("getAmPmStrings"),
            _ => None,
        };
        if let Some(sg) = static_getter {
            let result = strings.get(index).cloned();
            CACHED_DATE_NAMES.with(|c| {
                *c.borrow_mut() = Some((vm, epoch, locale_key, sg, strings));
            });
            return result;
        }
        strings.get(index).cloned()
    })();
    FMT_DATE_NAMES_RESOLVING.with(|f| f.set(false));
    resolved.filter(|s| !s.is_empty())
}

/// Append a date-name entry directly to `out` (as UTF-16 code units) without
/// allocating a `String` on cache hit, falling back to `fallback` when the
/// locale is explicit-null or the lookup returns nothing.
pub fn fmt_date_name_push_u16(
    ctx: &mut dyn NativeContext,
    locale: FmtLocale,
    getter: &str,
    index: usize,
    fallback: &str,
    out: &mut Vec<u16>,
) {
    if !matches!(locale, FmtLocale::Given(None)) {
        let locale_key = match locale {
            FmtLocale::DefaultFormat => 0usize,
            FmtLocale::Given(Some(l)) => l.obj.as_ptr() as usize,
            FmtLocale::Given(None) => 0usize,
        };
        let vm = ctx.vm_identity();
        let epoch = ctx.gc_address_epoch();
        let hit = CACHED_DATE_NAMES.with(|c| {
            let borrow = c.borrow();
            if let Some((c_vm, c_epoch, c_key, c_getter, ref c_arr)) = *borrow {
                if c_vm == vm && c_epoch == epoch && c_key == locale_key && c_getter == getter {
                    if let Some(s) = c_arr.get(index).filter(|s| !s.is_empty()) {
                        out.extend(s.encode_utf16());
                        return true;
                    }
                }
            }
            false
        });
        if hit {
            return;
        }
        if let Some(s) = fmt_date_name(ctx, locale, getter, index) {
            out.extend(s.encode_utf16());
            return;
        }
    }
    out.extend(fallback.encode_utf16());
}

/// Rewrite an ASCII numeric body into the locale's digits and separators.
///
/// Runs LAST, after grouping, signs and width padding, so everything upstream
/// keeps working in ASCII where a byte length and a character count agree —
/// U+202F is three UTF-8 bytes, and a width pad computed over it would be
/// short by two. The substitution is one character for one, so the padded
/// character count survives it unchanged.
///
/// Applied only to the conversions the JDK localizes (`%d %f %e %g`): `%x`,
/// `%o` and `%a` are documented as "No localization is applied", and a `%s`
/// argument's own digits are the caller's text, not a magnitude.
/// [`fmt_localize`]'s DIGIT half, for the `%t` family.
///
/// `localizedMagnitude` does two things — substitute the zero digit, and
/// substitute the separators — and the `%t` conversions only ever ask for the
/// first: a date/time field carries no grouping separator (its
/// `localizedMagnitude` calls pass `Flags.NONE` or `Flags.ZERO_PAD`, never
/// `Flags.GROUP`) and no decimal point. Mapping `','` and `'.'` here as well
/// would be wrong rather than merely unnecessary — `%tD` is `mm/dd/yy` and a
/// `DateFormatSymbols` month name may legitimately END in `'.'` (`ru`'s
/// `нояб.`).
///
/// So this is deliberately NOT a call to [`fmt_localize`] with the separators
/// zeroed out: the two functions answer different questions and share the one
/// mechanism that is genuinely common, the `zero - '0'` shift. Both are
/// one-character-for-one, which is what lets the width justifier keep counting
/// after them.
///
/// The `sym.zero == '0'` short circuit is the whole cost on every Latin-digit
/// locale, which is nearly all of them.
pub fn fmt_localize_digits(s: &str, sym: FmtSymbols) -> String {
    if sym.zero == '0' {
        return s.to_string();
    }
    let shift = sym.zero as u32 - '0' as u32;
    s.chars()
        .map(|c| {
            if c.is_ascii_digit() {
                char::from_u32(c as u32 + shift).unwrap_or(c)
            } else {
                c
            }
        })
        .collect()
}

pub fn fmt_localize(s: &str, sym: FmtSymbols) -> String {
    if sym.grouping == ',' && sym.decimal == '.' && sym.zero == '0' {
        return s.to_string();
    }
    let shift = sym.zero as u32 - '0' as u32;
    s.chars()
        .map(|c| match c {
            ',' => sym.grouping,
            '.' => sym.decimal,
            '0'..='9' => char::from_u32(c as u32 + shift).unwrap_or(c),
            other => other,
        })
        .collect()
}

// --- String.format (basic %s/%d/%f support) ---

/// `String.format(String, Object...)` — the overload with NO `Locale`.
///
/// [`FmtLocale::DefaultFormat`], not a null locale: `java.util.Formatter`
/// formats this overload against `Locale.getDefault(Locale.Category.FORMAT)`.
/// It used to pass `None` here, which [`fmt_symbols_for`] read as "no
/// localization" — so `String.format("%,.2f", 1234.5)` answered the ROOT
/// `1,234.50` on a German host where HotSpot answers `1.234,50`. Every
/// no-`Locale` surface funnels through here ([`native_string_formatted`], and
/// `PrintStream.printf`/`format` and `PrintWriter.printf`/`format` in
/// `native-builtins/src/lib.rs` and `logging_shims.rs`), so they all move
/// together and cannot drift.
pub fn native_string_format(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match format_impl(ctx, args, FmtLocale::DefaultFormat) {
        Ok(v) => Ok(v),
        Err(_) => ctx.invoke_special_bytecode_only(
            "java/lang/String",
            "format",
            "(Ljava/lang/String;[Ljava/lang/Object;)Ljava/lang/String;",
            args,
        ),
    }
}

/// The body of every `String.format` / `Formatter.format` overload.
///
/// `locale` says which `java.util.Locale` this call localizes against — see
/// [`FmtLocale`] for why the absent-parameter and explicit-null cases are not
/// the same request. It is resolved to [`FmtSymbols`] LAZILY, at the first
/// conversion that actually localizes, so the very common
/// `String.format(Locale.ROOT, "%s", x)` pays nothing for a locale it never
/// consults — and, since the fix above, neither does the no-`Locale` overload,
/// which is the same laziness doing the same job on a hotter path.
/// `CRATONVM_NO_FORMAT_ARG_PIN=1` restores the unpinned formatter, so the two
/// arms are one binary apart.
pub fn format_arg_pin_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        None::<std::ffi::OsString>.is_none()
    })
}

/// A null format string: every overload reaches `Formatter.parse(String s)`,
/// whose first statement is `s.length()`, so HotSpot 25's helpful message is
/// this one (both tiers; JIT round 20, lane huntIntr, `R20HuntIntrWrappers`
/// fmt.null). It used to be the VM-invented "String.format: format is null".
pub const FORMAT_NULL_NPE: &str = "Cannot invoke \"String.length()\" because \"s\" is null";

#[derive(Clone)]
pub enum FmtPlanSeg {
    Lit(String),
    Temporal { use_idx: usize, field: char },
}

pub struct CachedFmtPlan {
    pub vm: usize,
    pub epoch: u64,
    pub fmt_addr: usize,
    pub max_idx: usize,
    pub needs_digits: bool,
    pub segments: Vec<FmtPlanSeg>,
}

thread_local! {
    pub static CACHED_FMT_PLAN: std::cell::RefCell<Option<CachedFmtPlan>> =
        const { std::cell::RefCell::new(None) };
}

pub fn try_fast_temporal_format(
    ctx: &mut dyn NativeContext,
    fmt_obj: cratonvm_types::ObjectRef,
    arr_obj: cratonvm_types::ObjectRef,
    locale: FmtLocale,
) -> Option<cratonvm_types::ObjectRef> {
    if !matches!(locale, FmtLocale::DefaultFormat | FmtLocale::Given(None)) {
        return None;
    }
    let vm = ctx.vm_identity();
    let epoch = ctx.gc_address_epoch();
    let fmt_addr = fmt_obj.as_ptr() as usize;
    let long_cid = match CACHED_LONG_CID.with(std::cell::Cell::get) {
        Some((c_vm, cid)) if c_vm == vm => cid,
        _ => return None,
    };

    let mut ascii = FMT_ASCII_SCRATCH.with(std::cell::Cell::take);
    ascii.clear();

    let ok = CACHED_FMT_PLAN.with(|cell| {
        let borrow = cell.borrow();
        let plan = borrow.as_ref()?;
        if plan.vm != vm || plan.epoch != epoch || plan.fmt_addr != fmt_addr {
            return None;
        }
        let arr_len = ctx.array_length(arr_obj);
        if plan.max_idx >= arr_len {
            return None;
        }
        if matches!(locale, FmtLocale::DefaultFormat) && plan.needs_digits {
            let sym_ok = CACHED_FMT_SYMBOLS.with(|c| {
                matches!(*c.borrow(), Some((c_vm, c_epoch, 0, sym)) if c_vm == vm && c_epoch == epoch && sym.zero == '0')
            });
            if !sym_ok {
                return None;
            }
        }

        let mut last_idx: Option<usize> = None;
        let mut last_fields: (i64, i32, i32, i32, i32, i32) = (0, 0, 0, 0, 0, 0);

        for seg in &plan.segments {
            match seg {
                FmtPlanSeg::Lit(s) => ascii.push_str(s),
                FmtPlanSeg::Temporal { use_idx, field } => {
                    let (year, month, day, hour, minute, second) = if last_idx == Some(*use_idx) {
                        last_fields
                    } else {
                        let elem = match ctx.get_array_element(arr_obj, *use_idx) {
                            Value::Object(Some(o)) if ctx.class_id_of_object(o) == long_cid => o,
                            _ => return None,
                        };
                        let millis = match ctx.get_field(elem, 0) {
                            Value::Long(ms) => ms,
                            Value::Int(ms) => i64::from(ms),
                            _ => return None,
                        };
                        let offset_ms = CACHED_DEFAULT_ZONE_RULES.with(|c| {
                            let b = c.borrow();
                            if let Some((c_vm, c_epoch, ref rules)) = *b {
                                if c_vm == vm && c_epoch == epoch {
                                    return Some(crate::tzdb::legacy_offsets_ms_of(rules, millis).0);
                                }
                            }
                            None
                        })?;
                        let shifted = millis.saturating_add(i64::from(offset_ms));
                        let day_millis = 86_400_000i64;
                        let epoch_day = shifted.div_euclid(day_millis);
                        let mut tod = shifted.rem_euclid(day_millis);
                        let hour = (tod / 3_600_000) as i32;
                        tod %= 3_600_000;
                        let minute = (tod / 60_000) as i32;
                        tod %= 60_000;
                        let second = (tod / 1000) as i32;
                        let (y, m, d) = crate::julian_date::hybrid_from_epoch_day(epoch_day);
                        let f = (y as i64, m, d, hour, minute, second);
                        last_idx = Some(*use_idx);
                        last_fields = f;
                        f
                    };
                    let year_of_era = if year <= 0 { 1 - year } else { year };
                    let month_idx = (month.clamp(1, 12) - 1) as usize;
                    let push_2d = |out: &mut String, v: i32| {
                        out.push((b'0' + ((v / 10) % 10) as u8) as char);
                        out.push((b'0' + (v % 10) as u8) as char);
                    };
                    match field {
                        'H' => push_2d(&mut ascii, hour),
                        'M' => push_2d(&mut ascii, minute),
                        'S' => push_2d(&mut ascii, second),
                        'd' => push_2d(&mut ascii, day),
                        'm' => push_2d(&mut ascii, month),
                        'y' => push_2d(&mut ascii, (year_of_era % 100) as i32),
                        'C' => push_2d(&mut ascii, (year_of_era / 100) as i32),
                        'Y' => {
                            if (0..10_000).contains(&year_of_era) {
                                ascii.push((b'0' + ((year_of_era / 1000) % 10) as u8) as char);
                                ascii.push((b'0' + ((year_of_era / 100) % 10) as u8) as char);
                                ascii.push((b'0' + ((year_of_era / 10) % 10) as u8) as char);
                                ascii.push((b'0' + (year_of_era % 10) as u8) as char);
                            } else {
                                use std::fmt::Write;
                                let _ = write!(ascii, "{:04}", year_of_era);
                            }
                        }
                        'b' | 'h' => {
                            if matches!(locale, FmtLocale::Given(None)) {
                                ascii.push_str(MONTHS_ABBR_EN[month_idx]);
                            } else {
                                let hit = CACHED_DATE_NAMES.with(|c| {
                                    let b = c.borrow();
                                    if let Some((c_vm, c_epoch, 0, "getShortMonths", ref arr)) = *b {
                                        if c_vm == vm && c_epoch == epoch {
                                            if let Some(s) = arr.get(month_idx).filter(|s| !s.is_empty()) {
                                                ascii.push_str(s);
                                                return true;
                                            }
                                        }
                                    }
                                    false
                                });
                                if !hit {
                                    return None;
                                }
                            }
                        }
                        _ => return None,
                    }
                }
            }
        }
        Some(())
    });

    let res = if ok.is_some() {
        Some(ctx.create_string_uninterned_gc_safe(&ascii))
    } else {
        None
    };
    ascii.clear();
    FMT_ASCII_SCRATCH.with(|c| c.set(ascii));
    res
}

pub fn format_impl(ctx: &mut dyn NativeContext, args: &[Value], locale: FmtLocale) -> MethodCallResult {
    if let (Some(Value::Object(Some(fmt_obj))), Some(Value::Object(Some(arr_obj)))) =
        (args.first(), args.get(1))
    {
        if let Some(s) = try_fast_temporal_format(ctx, *fmt_obj, *arr_obj, locale) {
            return Ok(Some(Value::Object(Some(s))));
        }
    }
    // GC. `format_impl_pinned` RUNS BYTECODE: every numeric conversion resolves
    // `FmtSymbols` through `DecimalFormatSymbols.getInstance()`, which allocates
    // and can trigger a young collection, and `format_arg_full` dispatches
    // `hashCode`/`toString` on the argument. The format string and the varargs
    // array arrive as raw `ObjectRef`s in `args` and were held across all of it
    // with nothing rooting them, so a collection mid-format reclaimed the array
    // together with every box still inside it. The next conversion then read an
    // all-zero header at the argument's address -- `ClassId(0)`, which the class
    // manager names `java.lang.Object` -- and the formatter refused it as
    // `IllegalFormatConversionException: d != java.lang.Object`. See
    // `bug-generational-ntru-unpinned-jit-reference-20260821-FIXED.md`: the signature
    // reads like a lost JIT root and is neither JIT- nor relocation-related.
    //
    // Pin both for the duration, and re-derive the array from its handle before
    // every element read so a moving collection's new address is used. Same
    // idiom as `fmt_format_to` further up this file; `pin_base` is the FIRST
    // handle, so one `unpin_native_roots(pin_base)` releases the batch on every
    // return path -- which is why the body is a separate function rather than
    // an early-returning block.
    if format_arg_pin_enabled() {
        let fmt_obj = match args.first() {
            Some(Value::Object(Some(obj))) => *obj,
            _ => {
                return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                    message: Some(FORMAT_NULL_NPE.to_string()),
                }
                .into())
            }
        };
        let arr_obj = match args.get(1) {
            Some(Value::Object(Some(obj))) => Some(*obj),
            _ => None,
        };
        let pin_base = ctx.pin_native_root(fmt_obj);
        let arr_pin = arr_obj.map(|a| ctx.pin_native_root(a));
        // gen r4w3/rooting: the explicit `Locale` is held across every
        // conversion's upcalls too, so it is pinned with the batch (released by
        // the same `unpin_native_roots(pin_base)`) and every consumer re-reads
        // it through `FmtLocaleRef::current`.
        let locale = match locale {
            FmtLocale::Given(Some(l)) => FmtLocale::Given(Some(FmtLocaleRef {
                obj: l.obj,
                pin: Some(ctx.pin_native_root(l.obj)),
            })),
            other => other,
        };
        let out = format_impl_pinned(ctx, args, locale, arr_pin);
        ctx.unpin_native_roots(pin_base);
        return out;
    }
    format_impl_pinned(ctx, args, locale, None)
}

pub struct VecU16ScratchGuard {
    pub slot: &'static std::thread::LocalKey<std::cell::Cell<Vec<u16>>>,
    pub buf: Vec<u16>,
}

impl VecU16ScratchGuard {
    pub fn take(slot: &'static std::thread::LocalKey<std::cell::Cell<Vec<u16>>>) -> Self {
        let mut buf = slot.with(std::cell::Cell::take);
        buf.clear();
        Self { slot, buf }
    }
}

impl Drop for VecU16ScratchGuard {
    fn drop(&mut self) {
        self.buf.clear();
        self.slot.with(|c| c.set(std::mem::take(&mut self.buf)));
    }
}

thread_local! {
    pub static FMT_UNITS_SCRATCH: std::cell::Cell<Vec<u16>> = const { std::cell::Cell::new(Vec::new()) };
    pub static FMT_OUT_SCRATCH: std::cell::Cell<Vec<u16>> = const { std::cell::Cell::new(Vec::new()) };
    pub static FMT_ASCII_SCRATCH: std::cell::Cell<String> = const { std::cell::Cell::new(String::new()) };
}

#[inline]
pub fn parse_u16_digits_i32(units: &[u16]) -> Option<i32> {
    let mut acc: i32 = 0;
    for &u in units {
        let d = (u as i32).wrapping_sub(b'0' as i32);
        if !(0..=9).contains(&d) {
            return None;
        }
        acc = acc.checked_mul(10)?.checked_add(d)?;
    }
    Some(acc)
}

#[inline]
pub fn u16_is_ascii_digit(u: u16) -> bool {
    (u16::from(b'0')..=u16::from(b'9')).contains(&u)
}

#[inline]
pub fn u16_to_char(u: u16) -> char {
    char::from_u32(u32::from(u)).unwrap_or('\u{FFFD}')
}

pub const FLAG_MINUS: u8 = 1 << 0;
pub const FLAG_PLUS: u8 = 1 << 1;
pub const FLAG_ZERO: u8 = 1 << 2;
pub const FLAG_SPACE: u8 = 1 << 3;
pub const FLAG_ALT: u8 = 1 << 4;
pub const FLAG_PAREN: u8 = 1 << 5;
pub const FLAG_GROUP: u8 = 1 << 6;
pub const FLAG_PREV: u8 = 1 << 7;

#[inline]
pub fn flag_bit(ch: u8) -> Option<u8> {
    match ch {
        b'-' => Some(FLAG_MINUS),
        b'+' => Some(FLAG_PLUS),
        b'0' => Some(FLAG_ZERO),
        b' ' => Some(FLAG_SPACE),
        b'#' => Some(FLAG_ALT),
        b'(' => Some(FLAG_PAREN),
        b',' => Some(FLAG_GROUP),
        b'<' => Some(FLAG_PREV),
        _ => None,
    }
}

#[inline]
pub fn flags_bits_to_str(bits: u8, buf: &mut [u8; 8]) -> &str {
    if bits == 0 {
        return "";
    }
    // Order matches fmt_flags_string / canonical flag order if needed; only
    // non-zero when flags were actually present.
    let mut n = 0usize;
    for (bit, ch) in [
        (FLAG_MINUS, b'-'),
        (FLAG_PLUS, b'+'),
        (FLAG_ZERO, b'0'),
        (FLAG_SPACE, b' '),
        (FLAG_ALT, b'#'),
        (FLAG_PAREN, b'('),
        (FLAG_GROUP, b','),
        (FLAG_PREV, b'<'),
    ] {
        if (bits & bit) != 0 {
            buf[n] = ch;
            n += 1;
        }
    }
    // SAFETY: all bytes in buf[..n] are ASCII.
    unsafe { std::str::from_utf8_unchecked(&buf[..n]) }
}

pub fn format_impl_pinned(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    locale: FmtLocale,
    arr_pin: Option<usize>,
) -> MethodCallResult {
    // Static: args[0] = format String, args[1] = Object[] array
    let fmt_obj = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some(FORMAT_NULL_NPE.to_string()),
            }
            .into())
        }
    };
    let mut fmt_units_guard = VecU16ScratchGuard::take(&FMT_UNITS_SCRATCH);
    decode_string_chars(ctx, fmt_obj, &mut fmt_units_guard.buf);
    let fmt_units: &[u16] = &fmt_units_guard.buf;

    // Get the varargs array
    let arr_ref = match args.get(1) {
        Some(Value::Object(Some(obj))) => Some(*obj),
        _ => None,
    };

    let arr_len = arr_ref.map_or(0, |a| ctx.array_length(a));
    let mut symbols: Option<FmtSymbols> = None;

    let mut result_guard = VecU16ScratchGuard::take(&FMT_OUT_SCRATCH);
    let result = &mut result_guard.buf;
    result.reserve(fmt_units.len() + 32);
    let mut last_temporal: Option<(usize, ExtractedTemporal)> = None;
    let mut plan_segs: Option<Vec<FmtPlanSeg>> =
        if matches!(locale, FmtLocale::DefaultFormat | FmtLocale::Given(None))
            && fmt_units.iter().all(|&u| u <= 0x7F)
        {
            Some(Vec::with_capacity(8))
        } else {
            None
        };
    let mut plan_max_idx: usize = 0;
    let mut plan_needs_digits = false;
    let mut plan_lit_start: usize = 0;
    let mut i = 0;
    let mut arg_idx = 0;
    // Index used by the most recent conversion, for the `%<` relative-index
    // flag ("reuse the previous argument").
    let mut last_used_index: Option<usize> = None;

    while i < fmt_units.len() {
        if fmt_units[i] == u16::from(b'%') {
            if let Some(ref mut segs) = plan_segs {
                if result.len() > plan_lit_start {
                    let s: String = result[plan_lit_start..]
                        .iter()
                        .map(|&u| u as u8 as char)
                        .collect();
                    segs.push(FmtPlanSeg::Lit(s));
                }
            }
            if i + 1 >= fmt_units.len() {
                return Err(fmt_raise(
                    ctx,
                    &FmtFault::UnknownConversion("%".to_string()),
                ));
            }
            i += 1;
            let first_after_pct = u16_to_char(fmt_units[i]);
            // Check for %% and %n first
            if fmt_units[i] == u16::from(b'%') {
                plan_segs = None;
                result.push(u16::from(b'%'));
                i += 1;
                continue;
            }
            if fmt_units[i] == u16::from(b'n') {
                plan_segs = None;
                result.extend(fmt_line_separator_units());
                i += 1;
                continue;
            }

            let mut explicit_index: Option<usize> = None;
            let mut deferred_fault: Option<FmtFault> = None;
            {
                let mut j = i;
                while fmt_units.get(j).is_some_and(|&u| u16_is_ascii_digit(u)) {
                    j += 1;
                }
                if j > i && fmt_units.get(j) == Some(&u16::from(b'$')) {
                    match parse_u16_digits_i32(&fmt_units[i..j]) {
                        Some(n) if n >= 1 => explicit_index = Some(n as usize - 1),
                        Some(n) => deferred_fault = Some(FmtFault::ArgumentIndex(n)),
                        None => deferred_fault = Some(FmtFault::ArgumentIndex(i32::MIN)),
                    }
                    i = j + 1; // consume the digits and the '$'
                }
            }

            let mut flag_bits: u8 = 0;
            while let Some(&u) = fmt_units.get(i) {
                if u <= 0x7F {
                    if let Some(bit) = flag_bit(u as u8) {
                        if (flag_bits & bit) != 0 {
                            if deferred_fault.is_none() {
                                deferred_fault =
                                    Some(FmtFault::DuplicateFlags((u as u8 as char).to_string()));
                            }
                        } else {
                            flag_bits |= bit;
                        }
                        i += 1;
                        continue;
                    }
                }
                break;
            }
            let mut flags_buf = [0u8; 8];
            let flags = flags_bits_to_str(flag_bits, &mut flags_buf);

            let mut width: Option<usize> = None;
            let width_start = i;
            while fmt_units.get(i).is_some_and(|&u| u16_is_ascii_digit(u)) {
                i += 1;
            }
            if i > width_start {
                match parse_u16_digits_i32(&fmt_units[width_start..i]) {
                    Some(w) => width = Some(w as usize),
                    None => {
                        if deferred_fault.is_none() {
                            deferred_fault = Some(FmtFault::IllegalWidth(i32::MIN));
                        }
                    }
                }
            }

            let mut precision: Option<usize> = None;
            let mut malformed_precision = false;
            if fmt_units.get(i) == Some(&u16::from(b'.')) {
                i += 1;
                let prec_start = i;
                while fmt_units.get(i).is_some_and(|&u| u16_is_ascii_digit(u)) {
                    i += 1;
                }
                if i > prec_start {
                    match parse_u16_digits_i32(&fmt_units[prec_start..i]) {
                        Some(p) => precision = Some(p as usize),
                        None => {
                            if deferred_fault.is_none() {
                                deferred_fault = Some(FmtFault::IllegalPrecision(i32::MIN));
                            }
                        }
                    }
                } else {
                    malformed_precision = true;
                }
            }

            // Parse conversion character
            if malformed_precision {
                return Err(fmt_raise(
                    ctx,
                    &FmtFault::UnknownConversion(first_after_pct.to_string()),
                ));
            }
            if let Some(&spec_u) = fmt_units.get(i) {
                let spec = u16_to_char(spec_u);
                i += 1;
                // `FormatSpecifier`'s constructor validates in source order —
                // index, flags, width, precision — and every one of those comes
                // before `conversion()` and `check()`. So a specifier with two
                // faults reports the LEFTMOST, and this is where the ones the
                // scan deferred are raised: after the scan proved there IS a
                // conversion character, before any conversion-specific check.
                if let Some(fault) = deferred_fault {
                    return Err(fmt_raise(ctx, &fault));
                }
                match spec {
                    's' | 'S' | 'd' | 'f' | 'x' | 'X' | 'c' | 'C' | 'b' | 'B' | 'e' | 'E' | 'g'
                    | 'G' | 'o' | 'h' | 'H' | 'a' | 'A' => {
                        plan_segs = None;
                        // Select the argument this conversion consumes:
                        //   `%<x`  → reuse the previous conversion's index
                        //   `%N$x` → explicit 1-based index N
                        //   `%x`   → next ordinary index (advances the counter)
                        // Only ordinary conversions advance `arg_idx`, matching
                        // java.util.Formatter (explicit/relative specs do not).
                        //
                        // A `'<'` with NO previous conversion is a REFUSAL, not
                        // a fallback to argument 0. `Formatter.format`'s loop
                        // is `case -1 -> { if (last < 0 || …) throw new
                        // MissingFormatArgumentException(fs.toString()); … }`
                        // and `last` starts at -1. Measured on HotSpot
                        // 25.0.3+9: `String.format("%<s", "a")` is
                        // `MissingFormatArgumentException: Format specifier
                        // '%<s'`, and so is `String.format("%<tY", 0L)`. This
                        // answered args[0] instead, silently.
                        let relative_without_previous =
                            flags.contains('<') && last_used_index.is_none();
                        let use_idx = if flags.contains('<') {
                            last_used_index.unwrap_or(0)
                        } else if let Some(ei) = explicit_index {
                            ei
                        } else {
                            let cur = arg_idx;
                            arg_idx += 1;
                            cur
                        };
                        last_used_index = Some(use_idx);
                        // Flag/width/precision legality is decided BEFORE the
                        // argument is fetched, as it is in the JDK — the
                        // `FormatSpecifier` constructor runs every `check*`
                        // before `format` ever sees a value. So `%.2d` refuses
                        // even when no argument was supplied.
                        if let Err(fault) =
                            fmt_check_spec(explicit_index, &flags, width, precision, spec)
                        {
                            return Err(fmt_raise(ctx, &fault));
                        }
                        // ...and the missing PREVIOUS argument is raised here,
                        // with the same standing as a missing NEXT one: both
                        // are `format`'s own range test, which runs after every
                        // `FormatSpecifier` constructor has passed its checks.
                        // `%<0d` is therefore the flag mismatch, not this.
                        if relative_without_previous {
                            return Err(fmt_raise(
                                ctx,
                                &FmtFault::MissingArgument(fmt_spec_text(
                                    explicit_index,
                                    &flags,
                                    width,
                                    precision,
                                    spec,
                                )),
                            ));
                        }
                        // "If there are fewer arguments than format specifiers,
                        // the argument index is out of range ... a
                        // MissingFormatArgumentException is thrown." Answering
                        // the empty string instead let a caller's own
                        // arity-guard `catch` never fire, and silently shifted
                        // every later conversion's argument.
                        //
                        // A NULL varargs array is not the same thing: the JDK's
                        // `format` loop guards the range check with `args !=
                        // null` and then passes `null` for every conversion, so
                        // `String.format("%s", (Object[]) null)` is "null" and
                        // not a refusal.
                        let arg = match arr_ref {
                            None => Value::Object(None),
                            Some(a) if use_idx < arr_len => {
                                // gen r4w3/rooting: an EARLIER conversion's
                                // upcalls may have moved the array; read it
                                // through its pin, not the entry-time copy.
                                let a = match arr_pin {
                                    Some(h) => ctx.read_native_pin(h, a),
                                    None => a,
                                };
                                ctx.get_array_element(a, use_idx)
                            }
                            Some(_) => {
                                return Err(fmt_raise(
                                    ctx,
                                    &FmtFault::MissingArgument(fmt_spec_text(
                                        explicit_index,
                                        &flags,
                                        width,
                                        precision,
                                        spec,
                                    )),
                                ))
                            }
                        };
                        // Only a conversion that localizes needs the locale, so
                        // this is where the `DecimalFormatSymbols` lookup is
                        // paid for — never on a format string of plain `%s`.
                        let sym = if matches!(spec, 'd' | 'f' | 'e' | 'E' | 'g' | 'G') {
                            match symbols {
                                Some(s) => s,
                                None => {
                                    let s = fmt_symbols_for(ctx, locale);
                                    symbols = Some(s);
                                    s
                                }
                            }
                        } else {
                            FmtSymbols::default()
                        };
                        // `fmt_symbols_for` above may have run
                        // `DecimalFormatSymbols.getInstance()` -- real bytecode,
                        // real allocation, so a collection can have happened
                        // since `arg` was read. The array is pinned, so the
                        // element is still LIVE; re-read it through the handle
                        // so a moving collection's new address is used rather
                        // than the copy taken before the call.
                        let arg = match (arr_ref, arr_pin) {
                            (Some(a), Some(h)) if use_idx < arr_len => {
                                let a = ctx.read_native_pin(h, a);
                                ctx.get_array_element(a, use_idx)
                            }
                            _ => arg,
                        };
                        let text = format_arg_full(
                            ctx, &arg, spec, &flags, width, precision, sym, locale,
                        )?;
                        result.extend_from_slice(&text);
                    }
                    't' | 'T' => {
                        // Date/time conversion: 't'/'T' is a *prefix*, not a
                        // complete conversion — the next character selects the
                        // actual field (e.g. `%tb` = abbreviated month, `%tY` =
                        // 4-digit year). Real java.util.Formatter upper-cases
                        // the whole result when the prefix itself is 'T'.
                        let uppercase = spec == 'T';
                        // The prefix only IS a prefix when a conversion
                        // character follows: `FormatSpecifierParser.parse`
                        // requires `isConversion(c1)` before it consumes two
                        // characters, and otherwise falls back to reading the
                        // 't' itself as the conversion — which
                        // `Conversion.isValid` rejects. So `%t1` reports
                        // "Conversion = 't'", NOT an unknown date/time field,
                        // and neither does it consume the '1'.
                        let field = match fmt_units.get(i).map(|&u| u16_to_char(u)) {
                            Some(c) if c.is_ascii_alphabetic() || c == '%' => {
                                i += 1;
                                c
                            }
                            // A 't'/'T' with no usable field character after it
                            // never matches the JDK's specifier regex either, so
                            // it is the same truncated-specifier refusal: HotSpot
                            // 25 answers `UnknownFormatConversionException:
                            // Conversion = 't'` for `String.format("%t")`.
                            _ => {
                                return Err(fmt_raise(
                                    ctx,
                                    &FmtFault::UnknownConversion(spec.to_string()),
                                ))
                            }
                        };
                        // `FormatSpecifier.toString` for a date/time specifier
                        // re-emits the 't'/'T' prefix and upper-cases the field
                        // when the prefix was 'T' — `%-Ty` reports itself as
                        // "%-TY". It is what the `MissingFormatWidth` and
                        // `MissingFormatArgument` messages quote.
                        let dt_spec_text = || {
                            let mut s = String::from("%");
                            s.push_str(&fmt_flags_string(&flags));
                            if let Some(idx) = explicit_index {
                                s.push_str(&format!("{}$", idx + 1));
                            }
                            if let Some(w) = width {
                                s.push_str(&w.to_string());
                            }
                            if let Some(p) = precision {
                                s.push('.');
                                s.push_str(&p.to_string());
                            }
                            s.push(if uppercase { 'T' } else { 't' });
                            s.push(if uppercase {
                                field.to_ascii_uppercase()
                            } else {
                                field
                            });
                            s
                        };
                        // `checkDateTime`, in the JDK's order. None of it ran
                        // before: a `%t` specifier skipped every legality check
                        // the other conversions go through, so `%.2tY` formatted
                        // instead of refusing and `%,tY` was accepted outright.
                        if let Some(p) = precision {
                            return Err(fmt_raise(ctx, &FmtFault::IllegalPrecision(p as i32)));
                        }
                        if !FMT_DATETIME_FIELDS.contains(field) {
                            return Err(fmt_raise(
                                ctx,
                                &FmtFault::UnknownConversion(format!("t{field}")),
                            ));
                        }
                        if (flag_bits
                            & (FLAG_ALT
                                | FLAG_PLUS
                                | FLAG_SPACE
                                | FLAG_ZERO
                                | FLAG_GROUP
                                | FLAG_PAREN))
                            != 0
                        {
                            let offending: String = fmt_flags_string(flags)
                                .chars()
                                .filter(|c| "#+ 0,(".contains(*c))
                                .collect();
                            return Err(fmt_raise(
                                ctx,
                                &FmtFault::FlagsMismatch(offending, field),
                            ));
                        }
                        if width.is_none() && (flag_bits & FLAG_MINUS) != 0 {
                            return Err(fmt_raise(ctx, &FmtFault::MissingWidth(dt_spec_text())));
                        }
                        if (flag_bits & FLAG_PREV) != 0 && last_used_index.is_none() {
                            return Err(fmt_raise(ctx, &FmtFault::MissingArgument(dt_spec_text())));
                        }
                        let use_idx = if (flag_bits & FLAG_PREV) != 0 {
                            last_used_index.unwrap_or(0)
                        } else if let Some(ei) = explicit_index {
                            ei
                        } else {
                            let cur = arg_idx;
                            arg_idx += 1;
                            cur
                        };
                        last_used_index = Some(use_idx);
                        // Same argument-availability rule as the general
                        // conversions above, which this arm did not have: an
                        // absent argument APPENDED NOTHING and the format string
                        // came back silently short, where HotSpot raises
                        // `MissingFormatArgumentException`.
                        let elem = match arr_ref {
                            None => Value::Object(None),
                            Some(a) if use_idx < arr_len => {
                                // gen r4w3/rooting: read through the pin, as in
                                // the general-conversion arm above.
                                let a = match arr_pin {
                                    Some(h) => ctx.read_native_pin(h, a),
                                    None => a,
                                };
                                ctx.get_array_element(a, use_idx)
                            }
                            Some(_) => {
                                return Err(fmt_raise(
                                    ctx,
                                    &FmtFault::MissingArgument(dt_spec_text()),
                                ))
                            }
                        };
                        // The `Locale` goes through so the name fields can ask
                        // `DateFormatSymbols` for it.
                        //
                        // `FmtSymbols` goes through too, and shares the SAME
                        // per-call cache the numeric conversions use, because
                        // every number a `%t` field renders is a
                        // `localizedMagnitude` and takes the locale's zero
                        // digit (measured under `ar-EG`; see
                        // `fmt_localize_digits`). The seven pure-NAME fields
                        // still resolve nothing — a `%tb`-only format string
                        // runs no `DecimalFormatSymbols` bytecode — which is
                        // the same laziness the `'d'`/`'f'` arm above applies,
                        // one screen up, over the same cache.
                        //
                        // `uppercase` goes through too rather than being
                        // applied to the returned text. `printDateTime` is
                        // `appendJustified(a, toUpperCaseWithLocale(sb, l))` —
                        // the upper-caser runs INSIDE the width, and it is the
                        // locale-sensitive one (`%TA` of a Monday under `tr` is
                        // `PAZARTES` + U+0130 on HotSpot 25, measured). Doing it
                        // out here meant a mapping that changes length padded
                        // to the wrong field, and did it with Rust's
                        // locale-independent table.
                        let sym = if matches!(field, 'B' | 'b' | 'h' | 'A' | 'a' | 'p' | 'Z') {
                            FmtSymbols::default()
                        } else {
                            match symbols {
                                Some(s) => s,
                                None => {
                                    let s = fmt_symbols_for(ctx, locale);
                                    symbols = Some(s);
                                    s
                                }
                            }
                        };
                        // Same re-derivation as the general-conversion arm: the
                        // symbols lookup above can run `DecimalFormatSymbols`
                        // bytecode, so `elem` may name a pre-collection address.
                        // The array is pinned, so the element is still live.
                        let elem = match (arr_ref, arr_pin) {
                            (Some(a), Some(h)) if use_idx < arr_len => {
                                let a = ctx.read_native_pin(h, a);
                                ctx.get_array_element(a, use_idx)
                            }
                            _ => elem,
                        };
                        let pre = match last_temporal {
                            Some((idx, ext)) if idx == use_idx => Some(ext),
                            _ => None,
                        };
                        if let Some(ext) = format_temporal_field_into(
                            ctx, &elem, field, flags, width, sym, locale, uppercase, pre, result,
                        )? {
                            last_temporal = Some((use_idx, ext));
                        }
                        if let Some(ref mut segs) = plan_segs {
                            if !uppercase
                                && width.is_none()
                                && flags.is_empty()
                                && matches!(field, 'H' | 'M' | 'S' | 'd' | 'm' | 'y' | 'C' | 'Y' | 'b' | 'h')
                            {
                                segs.push(FmtPlanSeg::Temporal { use_idx, field });
                                plan_max_idx = plan_max_idx.max(use_idx);
                                if !matches!(field, 'b' | 'h') {
                                    plan_needs_digits = true;
                                }
                                plan_lit_start = result.len();
                            } else {
                                plan_segs = None;
                            }
                        }
                    }
                    // The literal-percent conversion, reached only when it
                    // carried flags or a width — a bare `%%` is short-circuited
                    // above. `checkText` admits `'-'` and nothing else ("The
                    // flags ... are the same as for the general conversions,
                    // except that only the '-' flag is allowed"), and `'-'`
                    // still needs a width.
                    '%' => {
                        if let Some(p) = precision {
                            return Err(fmt_raise(ctx, &FmtFault::IllegalPrecision(p as i32)));
                        }
                        if !flags.is_empty() && flags != "-" {
                            return Err(fmt_raise(
                                ctx,
                                &FmtFault::IllegalFlags(fmt_flags_string(&flags)),
                            ));
                        }
                        if flags == "-" && width.is_none() {
                            return Err(fmt_raise(
                                ctx,
                                &FmtFault::MissingWidth(fmt_spec_text(
                                    explicit_index,
                                    &flags,
                                    width,
                                    precision,
                                    '%',
                                )),
                            ));
                        }
                        // `fmt_repeat`, not `str::repeat`: `%2000000000%` is a
                        // legal specifier and a two-gigabyte allocation.
                        let pad = fmt_repeat(' ', width.unwrap_or(1).saturating_sub(1))?;
                        if flags == "-" {
                            result.push(u16::from(b'%'));
                            result.extend(pad.encode_utf16());
                        } else {
                            result.extend(pad.encode_utf16());
                            result.push(u16::from(b'%'));
                        }
                    }
                    // Likewise `%n`, reached only when decorated. It takes no
                    // width at all ("If the width is set, an
                    // IllegalFormatWidthException will be thrown") and no flags.
                    'n' => {
                        // `checkText` tests the PRECISION before it switches on
                        // the conversion, so it outranks both the width and the
                        // flag refusals: `%5.2n` is IllegalFormatPrecisionException
                        // on HotSpot 25, not IllegalFormatWidthException. This arm
                        // had no precision check at all, so `%.2n` emitted a line
                        // separator and formatted clean — the `'%'` arm two above
                        // has always had the same check, which is how a family
                        // written twice drifts in one member.
                        if let Some(p) = precision {
                            return Err(fmt_raise(ctx, &FmtFault::IllegalPrecision(p as i32)));
                        }
                        if let Some(w) = width {
                            return Err(fmt_raise(ctx, &FmtFault::IllegalWidth(w as i32)));
                        }
                        if !flags.is_empty() {
                            return Err(fmt_raise(
                                ctx,
                                &FmtFault::IllegalFlags(fmt_flags_string(&flags)),
                            ));
                        }
                        result.extend(fmt_line_separator_units());
                    }
                    // "If the conversion is not one of the conversions defined
                    // above, an UnknownFormatConversionException is thrown."
                    // Echoing the specifier back instead made every typo a
                    // silent pass-through, and the argument it should have
                    // consumed stayed queued for the NEXT conversion.
                    other => {
                        return Err(fmt_raise(
                            ctx,
                            &FmtFault::UnknownConversion(other.to_string()),
                        ))
                    }
                }
            } else {
                // Reached end of string after consuming flags/width/precision
                // with no conversion character — a truncated specifier.
                return Err(fmt_raise(
                    ctx,
                    &FmtFault::UnknownConversion(first_after_pct.to_string()),
                ));
            }
        } else {
            // The raw code UNIT, not `chars[i]`: `chars` is the parser's
            // ASCII-testable view and carries U+FFFD wherever the format string
            // held a surrogate.
            result.push(fmt_units[i]);
            i += 1;
        }
    }

    // [`sb_string_from_units`], this file's single lossless `String` writer. For
    // a well-formed result it still takes exactly the
    // `create_string_uninterned_gc_safe` path the `&str` build took, so no
    // allocation, interning or GC-safety property moves for output that has no
    // lone surrogate; only a result that actually carries one takes the units
    // path. An EMPTY result is the `""` literal: the JDK body ends in
    // `Formatter.toString()` -> `StringBuilder.toString()`, whose empty arm is
    // `return "";` (round 17 wave 4, lane sbempty, [`sb_new_string_from_units`]).
    if let Some(mut segs) = plan_segs {
        if result.len() > plan_lit_start {
            if result[plan_lit_start..].iter().all(|&u| u <= 0x7F) {
                let s: String = result[plan_lit_start..]
                    .iter()
                    .map(|&u| u as u8 as char)
                    .collect();
                segs.push(FmtPlanSeg::Lit(s));
            } else {
                segs.clear();
            }
        }
        if segs.iter().any(|s| matches!(s, FmtPlanSeg::Temporal { .. })) {
            let vm = ctx.vm_identity();
            let epoch = ctx.gc_address_epoch();
            let fmt_addr = fmt_obj.as_ptr() as usize;
            CACHED_FMT_PLAN.with(|c| {
                *c.borrow_mut() = Some(CachedFmtPlan {
                    vm,
                    epoch,
                    fmt_addr,
                    max_idx: plan_max_idx,
                    needs_digits: plan_needs_digits,
                    segments: segs,
                });
            });
        }
    }
    if !result.is_empty() && result.iter().all(|&u| u <= 0x7F) {
        let mut ascii = FMT_ASCII_SCRATCH.with(std::cell::Cell::take);
        ascii.clear();
        ascii.reserve(result.len());
        for &u in result.iter() {
            ascii.push(u as u8 as char);
        }
        let s = ctx.create_string_uninterned_gc_safe(&ascii);
        ascii.clear();
        FMT_ASCII_SCRATCH.with(|c| c.set(ascii));
        return Ok(Some(Value::Object(Some(s))));
    }
    let obj = sb_new_string_from_units(ctx, result)?;
    Ok(Some(Value::Object(Some(obj))))
}

/// `System.lineSeparator()` as code units — `%n`'s output, in one place so the
/// bare `%n` short-circuit and the decorated `%n` arm cannot drift apart on the
/// platform test. (They are ~440 lines apart and have already drifted once, on
/// the precision check; see the decorated arm.)
pub fn fmt_line_separator_units() -> Vec<u16> {
    if cfg!(windows) {
        vec![u16::from(b'\r'), u16::from(b'\n')]
    } else {
        vec![u16::from(b'\n')]
    }
}


pub fn temporal_is_leap_year(year: i32) -> bool {
    civil_date::is_leap_year(year)
}

pub fn temporal_days_in_month(year: i32, month: i32) -> i32 {
    civil_date::days_in_month(year, month)
}

/// Extract the wall-clock `(year, month[1-12], day, hour[0-23], minute,
/// second, nanosecond)` fields a `%t`/`%T` conversion needs from a
/// `Date`/`Calendar`/`Long`/`TemporalAccessor` argument.
///
/// CratonVM's Calendar/Date model (see `cal_to_epoch_millis`/
/// `cal_from_epoch_millis` in `phases_early.rs`) treats epoch millis as raw
/// wall-clock fields with no timezone shift applied anywhere in the VM — so
/// the same zero-offset breakdown is used here for `Date`/`Long`/`Calendar`
/// to stay consistent with the rest of the VM (and with how those values
/// were constructed in the first place, e.g. `Timestamp.valueOf`).
///
/// `field` is the `%t` field character, carried only so that an argument this
/// conversion cannot accept is refused as `IllegalFormatConversionException`
/// naming it — `Formatter.printDateTime`'s `else` arm is `failConversion(c,
/// arg)` and `c` for a date/time specifier is the FIELD, not the 't'.
/// The zone the `%t` fields were computed IN.
///
/// `java.util.Formatter.printDateTime` never formats an instant directly: it
/// builds `Calendar.getInstance(l == null ? Locale.US : l)` — which carries
/// `TimeZone.getDefault()` — and calls `setTimeInMillis`, so every field it then
/// reads is a LOCAL field. A `Calendar` argument is used as it stands and
/// carries its own zone.
///
/// This VM had no zone at all: `millis_to_fields` divided the epoch millis
/// directly, so every field was UTC and the `'z'`/`'Z'` arms answered a fixed
/// `"+0000"`/`"UTC"` that was *consistent with that*. The hard-coded zone name
/// was the visible half; the wrong HOUR, DAY, MONTH and YEAR were the quiet
/// half. Measured on HotSpot 25 for `new Date(1699999999000L)`:
///
/// | default zone | `%tc` | `%tZ` | `%tz` | `%tH` |
/// |---|---|---|---|---|
/// | `UTC` | `Tue Nov 14 22:13:19 UTC 2023` | `UTC` | `+0000` | `22` |
/// | `America/New_York` | `Tue Nov 14 17:13:19 EST 2023` | `EST` | `-0500` | `17` |
/// | `Asia/Kolkata` | `Wed Nov **15** 03:43:19 IST 2023` | `IST` | `+0530` | `03` |
/// | `Europe/Berlin` | `Tue Nov 14 23:13:19 CET 2023` | `CET` | `+0100` | `23` |
///
/// (the Kolkata row rolls the DAY over, which is why this could not be fixed by
/// naming the zone alone). Those `%tZ` values are the `Locale.US` display
/// names; under `Locale.ROOT` the same rows read `GMT-05:00`, `GMT+05:30` and
/// `GMT+01:00`, and in July — with DST in force — New York reads `EDT` /
/// `GMT-04:00` and Berlin `CEST` / `GMT+02:00`. All measured.
#[derive(Clone, Copy)]
pub struct FmtZone {
    /// `Calendar.ZONE_OFFSET + Calendar.DST_OFFSET` in milliseconds, i.e. what
    /// `TimeZone.getOffset(millis)` answers, at the instant being formatted.
    pub offset_ms: i32,
    /// `Calendar.DST_OFFSET != 0` — the `daylight` argument of
    /// `TimeZone.getDisplayName`, and the reason `getRawOffset()` is asked for
    /// as well as `getOffset(millis)`.
    pub dst: bool,
    /// False when no zone could be resolved — the `java.time` types that carry
    /// none, and any configuration where `java.util.TimeZone` could not be
    /// reached. The fields are then UTC and the `'z'`/`'Z'` arms keep their old
    /// fixed answers.
    ///
    /// **`known == false` is not the same question as "must refuse".** A
    /// `LocalDateTime` has no zone and HotSpot REFUSES `%tZ` on it; a VM that
    /// simply cannot reach `java.util.TimeZone` must keep printing `UTC`
    /// rather than start throwing. The refusal is decided by
    /// [`FmtSupport::zone`], which the degraded path leaves TRUE, and this
    /// flag only decides what a non-refusing `%tZ` prints.
    pub known: bool,
    /// The offset came off a `java.time` object rather than a
    /// `java.util.TimeZone`, so the `%tZ` NAME lookup goes back through the
    /// object ([`fmt_temporal_zone_name`]) and `dst` above is not meaningful —
    /// the daylight question is asked there, of the object's own zone.
    pub temporal: bool,
}

impl FmtZone {
    pub const NONE: FmtZone = FmtZone {
        offset_ms: 0,
        dst: false,
        known: false,
        temporal: false,
    };
}

/// Which `java.time.temporal.ChronoField`s the `%t` argument answers — the
/// question `Formatter.print(Formatter, TemporalAccessor, char, Locale)` asks
/// implicitly, by calling `t.get(field)` and turning the resulting
/// `DateTimeException` into `IllegalFormatConversionException(c,
/// t.getClass())`.
///
/// **This is a per-FIELD refusal, not a per-type one, and the brief that
/// commissioned it named only `%tZ`.** F22 measured that `Instant` and
/// `LocalDateTime` throw on `%tZ`; sweeping all 31 fields against all six
/// `java.time` sources on HotSpot 25 shows the refusal is much wider — an
/// `Instant` answers only `%tL`, `%tN`, `%ts` and `%tQ` and throws on the
/// other 27, and a `LocalDate` throws on every time field. Every one of those
/// was previously answered here with a FABRICATED value (`invoke_i32` returns
/// 0 for a method the class does not have), so `%tH` of an `Instant` printed
/// a plausible UTC hour where HotSpot refuses.
///
/// The flags are the field GROUPS the JDK's switch actually partitions
/// on, not a per-type table — a table would have to be re-derived for a
/// further source type, and this does not. Measured group membership (G2-1
/// swept 11 sources × all 31 fields on HotSpot 25.0.3+9, 2026-08-16):
///
/// | source | year | month | day | time | sub_second | instant | zone |
/// |---|---|---|---|---|---|---|---|
/// | `long` / `Long` / `Date` / `Calendar` | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
/// | `Instant` | | | | | ✓ | ✓ | |
/// | `LocalDate` | ✓ | ✓ | ✓ | | | | |
/// | `LocalTime` | | | | ✓ | ✓ | | |
/// | `LocalDateTime` | ✓ | ✓ | ✓ | ✓ | ✓ | | |
/// | `ZonedDateTime` / `OffsetDateTime` | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
/// | `OffsetTime` | | | | ✓ | ✓ | | ✓ |
/// | `Year` | ✓ | | | | | | |
/// | `YearMonth` | ✓ | ✓ | | | | | |
/// | `MonthDay` | | ✓ | ✓ | | | | |
/// | `Month` | | ✓ | | | | | |
///
/// The epoch-shaped sources are `ALL` because they do not take the
/// `TemporalAccessor` printer at all — `printDateTime` builds a `Calendar`,
/// whose printer has an arm for every field and refuses none of them.
///
/// **Why `date` is three flags and not one.** It was one until G2-1 swept the
/// partial `java.time` types. `Year` answers `%tY %ty %tC` and refuses `%tm`;
/// `YearMonth` answers those and `%tm %tB %tb %th` and refuses `%td`;
/// `MonthDay` answers the month and day fields and refuses `%tY`. A single
/// `date` flag cannot express any of the three, and — because `%tD`, `%tF` and
/// `%tc` each report the FIRST missing inner field — it cannot express their
/// refusal CHARACTERS either (measured: `%tF` of a `Year` is `m`, of a
/// `YearMonth` is `d`, of a `MonthDay` is `F`).
#[derive(Clone, Copy)]
pub struct FmtSupport {
    /// `YEAR_OF_ERA` — `%tY %ty %tC`, and the year slots of `%tD`/`%tF`/`%tc`.
    pub year: bool,
    /// `MONTH_OF_YEAR` — `%tm %tB %tb %th`, and the month slots of the three
    /// composites.
    pub month: bool,
    /// `DAY_OF_MONTH` — `%td %te`, and the day slots of the three composites.
    pub day: bool,
    /// `HOUR_OF_DAY`, `CLOCK_HOUR_OF_AMPM`, `MINUTE_OF_HOUR`,
    /// `SECOND_OF_MINUTE`, `AMPM_OF_DAY` — `%tH %tk %tI %tl %tM %tS %tp`, plus
    /// `%tR %tT %tr`.
    pub time: bool,
    /// `MILLI_OF_SECOND` / `NANO_OF_SECOND` — `%tL` and `%tN`. Separate from
    /// `time` because an `Instant` answers these two and no other clock field.
    pub sub_second: bool,
    /// `INSTANT_SECONDS` — `%ts`, and (with `sub_second`) `%tQ`.
    pub instant: bool,
    /// The zone query answers non-null and `OFFSET_SECONDS` is supported —
    /// `%tz` and `%tZ`. Left TRUE on the degraded `java.util.TimeZone` path so
    /// that a VM which cannot resolve a zone keeps PRINTING one rather than
    /// starting to refuse; see [`FmtZone::known`].
    pub zone: bool,
    /// **Which of `printDateTime`'s two printers this source takes**, not a
    /// support question — and it cannot be derived from the flags above,
    /// because a `ZonedDateTime` supports every group and still takes the
    /// OTHER printer.
    ///
    /// `java.util.Formatter.printDateTime` dispatches
    /// `long`/`Long`/`Date`/`Calendar` to `print(Formatter, Calendar, char,
    /// Locale)` and every `TemporalAccessor` to `print(Formatter,
    /// TemporalAccessor, char, Locale)`. The two are near-copies of each other
    /// and they disagree about exactly one conversion, `%tF`:
    ///
    /// ```text
    /// Calendar          case ISO_STANDARD_DATE:  print(YEAR_4) '-' print(MONTH) '-' print(DAY)
    /// TemporalAccessor  case ISO_STANDARD_DATE:  int year = t.get(YEAR);          // PROLEPTIC
    ///                                            if (year < 0) { getMinusSign(l); year = -year; }
    ///                                            else if (year > 9999) '+';
    ///                                            localizedMagnitude(year, ZERO_PAD, 4) …
    /// ```
    ///
    /// `YEAR_4` is `Calendar.YEAR`, which is ERA-RELATIVE and never negative, so
    /// the `Calendar` arm has no sign rule, no `'+'` rule and no localized minus
    /// at all. Measured on HotSpot 25, `Locale.ROOT`, `Asia/Kolkata`:
    ///
    /// | argument | `%tF` | `%tY` |
    /// |---|---|---|
    /// | `LocalDate.of(-44,3,15)` | `-0044-03-15` | `0045` |
    /// | `GregorianCalendar` ERA=BC YEAR=45 (the SAME instant) | **`0045-03-15`** | `0045` |
    /// | `new Date(-63549548877000L)` (the same instant) | **`0045-03-15`** | `0045` |
    /// | `LocalDate.of(12345,3,4)` | `+12345-03-04` | `12345` |
    /// | `GregorianCalendar` AD 12345 | **`12345-03-04`** | `12345` |
    ///
    /// The `%tY` column is identical down both printers, which is the point of
    /// §5.2's correction: see [`format_temporal_field`]'s `year_of_era`.
    pub calendar_printer: bool,
    /// G2-1 residual §4.1, closed. `DAY_OF_WEEK` (`%tA %ta`) answerable with
    /// **no** year, month or day at all — `java.time.DayOfWeek` is the one
    /// measured source that contradicts [`FmtSupport::full_date`]'s
    /// derivation (see there). `true` only for that source; every other
    /// constructor leaves it `false` and relies on `full_date()` instead, so
    /// `%tA`/`%ta` support is `full_date() || weekday_only` — see
    /// [`FmtSupport::weekday`].
    pub weekday_only: bool,
}

impl FmtSupport {
    /// The epoch-shaped sources, which take the `Calendar` printer and refuse
    /// nothing.
    pub const ALL: FmtSupport = FmtSupport {
        year: true,
        month: true,
        day: true,
        time: true,
        sub_second: true,
        instant: true,
        zone: true,
        calendar_printer: true,
        weekday_only: false,
    };

    /// `DAY_OF_WEEK` (`%tA %ta`) support: `full_date()`, OR the one measured
    /// exception, `weekday_only`. See [`FmtSupport::full_date`] for `%tj`
    /// (`DAY_OF_YEAR`), which stays tied to `full_date()` alone —
    /// `java.time.DayOfWeek` refuses it (measured) along with the other 29
    /// fields it does not answer.
    pub const fn weekday(self) -> bool {
        self.full_date() || self.weekday_only
    }

    /// `DAY_OF_WEEK` (`%tA %ta`) and `DAY_OF_YEAR` (`%tj`) — both DERIVED,
    /// because neither can be answered without a complete year/month/day and
    /// every source that has all three answers both.
    ///
    /// Measured over the 11 sources G2-1 swept: `LocalDate`, `LocalDateTime`,
    /// `ZonedDateTime`, `OffsetDateTime` and the four `ChronoLocalDate`s have
    /// y+m+d and answer `%tA %ta %tj`; `Instant`, `LocalTime`, `OffsetTime`,
    /// `Year`, `YearMonth`, `MonthDay` and `Month` each lack at least one of
    /// the three and refuse all three fields.
    ///
    /// **One measured source contradicts the derivation and is now decoded
    /// separately**: `java.time.DayOfWeek` answers `%tA`/`%ta` while having no
    /// year, month or day at all (and its `%tc` reports `b`, not the `a` this
    /// derivation alone would give). See [`FmtSupport::weekday`] and
    /// `extract_temporal_fields`'s `DayOfWeek` arm, which fabricates a
    /// reference `(year, month, day)` known to land on the requested weekday
    /// via [`day_of_week_sun0`] purely so `'A'`/`'a'` can read it — every
    /// other field stays refused because `year`/`month`/`day` are `false`.
    pub const fn full_date(self) -> bool {
        self.year && self.month && self.day
    }

    /// Whether the `(year, month, day)` this source decoded to came through
    /// `millis_to_fields`'s hybrid Julian/Gregorian calendar (pre-1582 dates
    /// Julian) rather than a `java.time` accessor's proleptic-Gregorian one —
    /// i.e. whether re-encoding those fields back to an epoch (`epoch_sec`,
    /// for `%ts`/`%tQ`) must go through the same hybrid calendar to round
    /// trip correctly.
    ///
    /// `calendar_printer` alone is not this flag: it is true for every
    /// `calendar_printer` source (`Long`/`Date`/`Calendar`, correctly hybrid)
    /// but also *false* for `java.time.Instant`, whose `(y, m, d)` are
    /// nonetheless hybrid-decoded (`extract_temporal_fields`'s `Instant` arm
    /// calls `millis_to_fields` directly) — `Instant` just has no printer that
    /// would read `%tY`/`%tD`/etc. off them, so `calendar_printer` was never
    /// set for it. `instant && !zone` catches exactly that case: the only
    /// other `instant`-capable, non-`calendar_printer` sources are
    /// `ZonedDateTime`/`OffsetDateTime`, and both carry their own zone
    /// (`zone: true`) where `Instant` does not.
    pub const fn hybrid_epoch(self) -> bool {
        self.calendar_printer || (self.instant && !self.zone)
    }
}

