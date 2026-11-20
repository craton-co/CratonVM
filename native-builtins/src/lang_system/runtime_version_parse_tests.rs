use super::parse_runtime_version_str;

/// Parse and render as `numbers|pre|build|optional` for terse assertions.
fn shape(s: &str) -> String {
    let p = parse_runtime_version_str(s).expect("parse");
    let numbers = p
        .numbers
        .iter()
        .map(i32::to_string)
        .collect::<Vec<_>>()
        .join(".");
    format!(
        "{numbers}|{}|{}|{}",
        p.pre.unwrap_or_default(),
        p.build.map(|b| b.to_string()).unwrap_or_default(),
        p.optional.unwrap_or_default(),
    )
}

/// Every shape the JDK's `$VNUM(-$PRE)?(\+$BUILD)?(-$OPT)?` grammar admits.
/// The expectations are `Runtime.Version.parse(s)`'s own field values on
/// HotSpot 25 (verified against a real JDK, not derived from this parser).
#[test]
fn parses_the_vstr_grammar() {
    assert_eq!(shape("25"), "25|||");
    assert_eq!(shape("25.0.3"), "25.0.3|||");
    // The two version strings this VM reports for itself.
    assert_eq!(shape("25.0.1+8"), "25.0.1||8|");
    assert_eq!(shape("25.0.1+8-LTS-27"), "25.0.1||8|LTS-27");
    // Temurin's, for a real-JDK cross-check.
    assert_eq!(shape("25.0.3+9-LTS"), "25.0.3||9|LTS");
    assert_eq!(shape("21-ea+3"), "21|ea|3|");
    assert_eq!(shape("17.0.2-ea+7-abc"), "17.0.2|ea|7|abc");
    // `+-$OPT` is the JDK's spelling for optional-without-build.
    assert_eq!(shape("17+-abc"), "17|||abc");
    // `-$PRE-$OPT` with no build at all.
    assert_eq!(shape("17-ea-abc"), "17|ea||abc");
}

#[test]
fn rejects_strings_with_no_usable_version_number() {
    assert!(parse_runtime_version_str("").is_none());
    assert!(parse_runtime_version_str("nonsense").is_none());
    // `1.8.0_392` is the pre-JEP-223 spelling; the JDK rejects it too.
    assert!(parse_runtime_version_str("1.8.0_392").is_none());
}
