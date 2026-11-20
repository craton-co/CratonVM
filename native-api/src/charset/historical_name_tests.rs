use super::*;

/// Every canonical name `canonical_charset_name` can produce, paired with
/// what Temurin 25.0.3 reports from
/// `new InputStreamReader(in, cs).getEncoding()` (identical to
/// `OutputStreamWriter`'s — both go through `HistoricallyNamedCharset`).
///
/// Frozen as a transcript rather than a rule, because there is no rule: the
/// spellings are historic, and `GB2312 -> EUC_CN` cannot be derived from
/// anything. Regenerate with `probes/ReaderWriterLayoutProbe`'s method on
/// the host JDK if this ever has to move.
const HOST_JDK: &[(&str, &str)] = &[
    ("UTF-8", "UTF8"),
    ("UTF-16", "UTF-16"),
    ("UTF-16BE", "UnicodeBigUnmarked"),
    ("UTF-16LE", "UnicodeLittleUnmarked"),
    ("UTF-32", "UTF-32"),
    ("UTF-32BE", "UTF-32BE"),
    ("UTF-32LE", "UTF-32LE"),
    ("US-ASCII", "ASCII"),
    ("ISO-8859-1", "ISO8859_1"),
    ("ISO-8859-2", "ISO8859_2"),
    ("ISO-8859-3", "ISO8859_3"),
    ("ISO-8859-4", "ISO8859_4"),
    ("ISO-8859-5", "ISO8859_5"),
    ("ISO-8859-15", "ISO8859_15"),
    ("Shift_JIS", "SJIS"),
    ("EUC-JP", "EUC_JP"),
    ("ISO-2022-JP", "ISO2022JP"),
    ("Big5", "Big5"),
    ("EUC-KR", "EUC_KR"),
    ("GB2312", "EUC_CN"),
    ("GBK", "GBK"),
    ("GB18030", "GB18030"),
    ("windows-1252", "Cp1252"),
    ("windows-1251", "Cp1251"),
    ("windows-1250", "Cp1250"),
    ("KOI8-R", "KOI8_R"),
    ("KOI8-U", "KOI8_U"),
    ("IBM850", "Cp850"),
    ("IBM1047", "Cp1047"),
    ("IBM500", "Cp500"),
];

#[test]
fn historical_names_match_the_host_jdk_transcript() {
    let wrong: Vec<String> = HOST_JDK
        .iter()
        .filter(|(canon, want)| historical_charset_name(canon) != *want)
        .map(|(canon, want)| {
            format!(
                "  {canon}: got {:?}, JDK says {want:?}",
                historical_charset_name(canon)
            )
        })
        .collect();
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// Twenty-three of the thirty differ from the canonical name. Asserting the
/// COUNT keeps a table that quietly degenerates to the identity function —
/// which is the pre-fix behaviour, and passes every "unlisted name is
/// returned unchanged" test — from looking correct.
#[test]
fn most_charsets_do_not_report_their_canonical_name() {
    let differing = HOST_JDK
        .iter()
        .filter(|(canon, want)| canon != want)
        .count();
    assert_eq!(differing, 23);
}

#[test]
fn an_unlisted_name_is_returned_unchanged() {
    assert_eq!(
        historical_charset_name("x-craton-nonesuch"),
        "x-craton-nonesuch"
    );
}

/// The table's keys must be canonical names, or a lookup can never hit.
#[test]
fn every_key_is_a_canonical_name() {
    for (canon, _) in HOST_JDK {
        assert_eq!(
            canonical_charset_name(canon),
            Some(*canon),
            "{canon} is not what canonical_charset_name produces"
        );
    }
}
