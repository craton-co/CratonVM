use super::*;

#[test]
fn zt26x21_pagesize_routes_like_hotspots_default_warning_output() {
    assert_eq!(LogTag::from_str("pagesize"), Ok(LogTag::Pagesize));
    assert_eq!(LogTag::Pagesize.to_string(), "pagesize");
    assert!(LogTag::all().contains(&LogTag::Pagesize));
    let parse = |spec: &str| UnifiedLogger::parse(spec).expect(spec);
    // `-Xlog:gc+init:file=...` (row `h7b_T1`): stdout keeps the default.
    let file_only = parse("gc+init:file=x.log::filecount=0");
    assert!(file_only.default_jfr_startup);
    assert!(file_only
        .level_for(&LogOutput::Stdout, LogTag::Pagesize)
        .is_none());
    let off = parse("pagesize=off");
    assert_eq!(
        off.level_for(&LogOutput::Stdout, LogTag::Pagesize),
        Some(LogLevel::Off)
    );
    assert!(!off.is_enabled(&[LogTag::Pagesize], LogLevel::Warning));
    assert!(!parse("disable").default_jfr_startup);
    assert!(parse("disable;pagesize:stderr")
        .is_enabled(&[LogTag::Pagesize], LogLevel::Warning));
    assert!(parse("all").is_enabled(&[LogTag::Pagesize], LogLevel::Warning));
    // hunt9 F3: the OS layer's lines, for G1 / Generational.
    let os = |thp, lp, anon, pages| os_large_page_warning(thp, lp, anon, pages);
    assert_eq!(os(true, false, Some("madvise"), false), None);
    assert_eq!(
        os(true, false, Some("never"), false),
        Some(THP_UNSUPPORTED_WARNING)
    );
    assert_eq!(
        os(false, true, Some("madvise"), false),
        Some(NO_LARGE_PAGES_WARNING)
    );
    assert_eq!(os(false, true, Some("madvise"), true), None);
    assert_eq!(os(false, false, Some("never"), false), None);
    // zki (item 5(c)): G1 / Serial's `Large Page Support:` value (host
    // `zki_hs1` a2 / a3 / a4 / a5 / b3 / b4 / c2 / c4).
    let g = |thp, lp, anon, pages| gc_init_large_pages_line(thp, lp, anon, pages);
    assert_eq!(
        g(true, false, Some("madvise"), false),
        "Enabled (Transparent)"
    );
    assert_eq!(g(true, true, Some("always"), true), "Enabled (Transparent)");
    assert_eq!(g(true, false, Some("never"), false), "Disabled");
    assert_eq!(g(false, true, Some("madvise"), false), "Disabled");
    assert_eq!(g(false, true, Some("madvise"), true), "Enabled (Explicit)");
    assert_eq!(g(false, false, Some("always"), true), "Disabled");
    assert_eq!(
        pagesize_warning_default_line(0.0034, NO_LARGE_PAGES_WARNING),
        "[0.003s][warning][pagesize] UseLargePages disabled, no large pages configured and available on the system."
    );
}
