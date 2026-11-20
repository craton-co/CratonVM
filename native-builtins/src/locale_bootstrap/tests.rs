use super::{parse_posix_locale, split_language_tag};

fn split(tag: &str) -> (String, String, String, String) {
    split_language_tag(tag)
}

/// The script subtag survives, title-cased, and does not eat the region.
///
/// `zh-hant-CN` used to come back as `("zh", "", "CN", "")` — the parser
/// recognised the 4-letter script only well enough to *skip* it — so
/// `Locale.toString()` rendered `zh_CN` where HotSpot renders
/// `zh_CN_#Hant` (`TestAcceptLanguage.bug56848`).
#[test]
fn script_subtag_is_kept_and_title_cased() {
    assert_eq!(
        split("zh-hant-CN"),
        ("zh".into(), "Hant".into(), "CN".into(), String::new())
    );
    assert_eq!(
        split("zh-hans-TW"),
        ("zh".into(), "Hans".into(), "TW".into(), String::new())
    );
    // No region at all.
    assert_eq!(
        split("az-Cyrl"),
        ("az".into(), "Cyrl".into(), String::new(), String::new())
    );
    // Already title-cased input is left alone.
    assert_eq!(
        split("sr-Latn-RS"),
        ("sr".into(), "Latn".into(), "RS".into(), String::new())
    );
}

/// `und` is BCP-47 for "undetermined"; `Locale` renders it as an EMPTY
/// language, so `forLanguageTag("und-DE").toString()` is `_DE` — not
/// `und_DE`, which is what treating `und` as a language produced.
#[test]
fn undetermined_language_is_empty() {
    assert_eq!(
        split("und-DE"),
        (String::new(), String::new(), "DE".into(), String::new())
    );
}

/// A single-character subtag opens a BCP-47 extension (`-u-`, `-x-`).
/// The side table cannot hold extensions, so parsing stops there; the old
/// parser filed the singleton itself as the variant, producing locales
/// whose variant was literally `"u"` or `"x"`.
#[test]
fn extension_singleton_is_not_a_variant() {
    assert_eq!(
        split("en-US-u-ca-japanese"),
        ("en".into(), String::new(), "US".into(), String::new())
    );
    assert_eq!(
        split("zh-Hant-TW-x-java"),
        ("zh".into(), "Hant".into(), "TW".into(), String::new())
    );
}

/// Variants are 5-8 alphanumerics, or 4 characters starting with a digit,
/// and several may be present (`Locale` joins them with `_`).
#[test]
fn variants_follow_the_bcp47_shape() {
    assert_eq!(
        split("de-DE-1996"),
        ("de".into(), String::new(), "DE".into(), "1996".into())
    );
    assert_eq!(
        split("de-DE-POSIX"),
        ("de".into(), String::new(), "DE".into(), "POSIX".into())
    );
    assert_eq!(
        split("sl-IT-nedis-rozaj"),
        (
            "sl".into(),
            String::new(),
            "IT".into(),
            "nedis_rozaj".into()
        )
    );
    // A 3-digit region is legal (UN M.49); a 3-letter one is not.
    assert_eq!(
        split("es-419"),
        ("es".into(), String::new(), "419".into(), String::new())
    );
}

/// Plain tags, and the `_` separator CratonVM callers sometimes pass.
#[test]
fn simple_tags_are_unchanged() {
    assert_eq!(
        split("en"),
        ("en".into(), String::new(), String::new(), String::new())
    );
    assert_eq!(
        split("en-gb"),
        ("en".into(), String::new(), "GB".into(), String::new())
    );
    assert_eq!(
        split("fr_FR"),
        ("fr".into(), String::new(), "FR".into(), String::new())
    );
    assert_eq!(
        split(""),
        (String::new(), String::new(), String::new(), String::new())
    );
}

/// The POSIX `C`/`POSIX` locales, and no locale at all, are English/US.
///
/// This is the case a bare Linux shell (and ubuntu-latest CI) hits. The
/// HotSpot launcher's `java_props_md.c` maps them, so a real JVM reports
/// `Locale.getDefault().getLanguage() == "en"` — never `"C"`, which is what
/// CratonVM used to return by passing the environment value straight
/// through.
#[test]
fn posix_c_locale_maps_to_english() {
    for raw in ["C", "POSIX", "C.UTF-8", "POSIX.UTF-8", "c", "", "   "] {
        assert_eq!(
            parse_posix_locale(raw),
            ("en".to_string(), "US".to_string()),
            "raw = {raw:?}"
        );
    }
}

/// `language_COUNTRY.codeset@modifier` — both suffixes are dropped, in
/// either order, and the casing is normalised the way `Locale` does it.
#[test]
fn posix_locale_strips_codeset_and_modifier() {
    assert_eq!(
        parse_posix_locale("en_GB.UTF-8@euro"),
        ("en".to_string(), "GB".to_string())
    );
    assert_eq!(
        parse_posix_locale("de_DE.ISO-8859-1"),
        ("de".to_string(), "DE".to_string())
    );
    assert_eq!(
        parse_posix_locale("sr_RS@latin"),
        ("sr".to_string(), "RS".to_string())
    );
    assert_eq!(
        parse_posix_locale("EN_gb"),
        ("en".to_string(), "GB".to_string())
    );
    // A trailing variant is not part of the (language, country) answer.
    assert_eq!(
        parse_posix_locale("ca_ES_VALENCIA.UTF-8"),
        ("ca".to_string(), "ES".to_string())
    );
}

/// A bare language code keeps an empty country, as `new Locale("en")` does.
#[test]
fn posix_bare_language_has_empty_country() {
    assert_eq!(parse_posix_locale("en"), ("en".to_string(), String::new()));
    assert_eq!(
        parse_posix_locale("fr.UTF-8"),
        ("fr".to_string(), String::new())
    );
    // `en_` — a country that is present but empty.
    assert_eq!(parse_posix_locale("en_"), ("en".to_string(), String::new()));
}
