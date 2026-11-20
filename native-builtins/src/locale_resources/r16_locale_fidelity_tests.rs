use super::*;

fn chain(lang: &str, script: &str, country: &str) -> Vec<String> {
    cldr_candidate_locales(lang, script, country)
        .iter()
        .map(CldrCandidate::bundle_suffix)
        .collect()
}

#[test]
fn plain_locales_truncate() {
    assert_eq!(chain("de", "", "DE"), vec!["_de_DE", "_de", ""]);
    assert_eq!(chain("en", "", "US"), vec!["_en_US", "_en", ""]);
    assert_eq!(chain("", "", ""), vec![""]);
}

#[test]
fn norwegian_bokmal_reads_the_no_bundles() {
    // nb_NO -> [nb_NO, no_NO, nb, no, ROOT] from Control; `no-NO`'s
    // irregular parent `no` then truncates the tail to [no, ROOT].
    assert_eq!(chain("nb", "", "NO"), vec!["_nb_NO", "_no_NO", "_no", ""]);
    assert_eq!(chain("nn", "", "NO"), vec!["_nn_NO", "_nn", "_no", ""]);
}

#[test]
fn chinese_gets_its_implicit_script_and_hant_parents_to_root() {
    assert_eq!(chain("zh", "", "TW"), vec!["_zh_Hant_TW", "_zh_Hant", ""]);
    assert_eq!(
        chain("zh", "Hant", "HK"),
        vec!["_zh_Hant_HK", "_zh_Hant", ""]
    );
    assert_eq!(
        chain("zh", "", "CN"),
        vec!["_zh_Hans_CN", "_zh_Hans", "_zh_CN", "_zh", ""]
    );
}

#[test]
fn script_and_irregular_parents() {
    assert_eq!(
        chain("sr", "Latn", "RS"),
        vec!["_sr_Latn_RS", "_sr_Latn", ""]
    );
    assert_eq!(chain("en", "", "GB"), vec!["_en_GB", "_en_001", "_en", ""]);
    assert_eq!(
        chain("en", "", "DE"),
        vec!["_en_DE", "_en_150", "_en_001", "_en", ""]
    );
    assert_eq!(chain("es", "", "MX"), vec!["_es_MX", "_es_419", "_es", ""]);
    assert_eq!(chain("pt", "", "AO"), vec!["_pt_AO", "_pt_PT", "_pt", ""]);
}

/// `getParentLocale` binary-searches each child list, so an unsorted row
/// would silently lose parents.
#[test]
fn parent_rows_are_sorted() {
    for (parent, kids) in CLDR_PARENT_LOCALES {
        assert!(
            kids.windows(2).all(|w| w[0] < w[1]),
            "children of {parent:?} are not sorted"
        );
    }
}

#[test]
fn non_format_char_skips_bidi_marks() {
    assert_eq!(find_non_format_char("\u{200E}-", '-' as u16), '-' as u16);
    assert_eq!(find_non_format_char("\u{061C}-", '-' as u16), '-' as u16);
    assert_eq!(
        find_non_format_char("\u{066A}\u{061C}", '%' as u16),
        0x066A,
        "a leading non-format char wins"
    );
    assert_eq!(find_non_format_char("\u{200F}", '%' as u16), '%' as u16);
    assert_eq!(find_non_format_char("", '%' as u16), '%' as u16);
    assert!(!is_java_format_char('-' as u16));
    assert!(is_java_format_char(0xFEFF));
}
