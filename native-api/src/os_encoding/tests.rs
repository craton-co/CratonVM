use super::*;

#[test]
fn native_encoding_is_stable_and_non_empty() {
    let a = native_encoding();
    assert!(!a.is_empty());
    assert_eq!(a, native_encoding(), "cached value must not drift");
}

#[cfg(unix)]
#[test]
fn unix_streams_all_answer_the_locale_codeset() {
    // The measured HotSpot behaviour: on Unix the three stream encodings
    // and the native encoding are ONE value, tty or not.
    for s in [StdStream::In, StdStream::Out, StdStream::Err] {
        assert_eq!(stream_encoding(s), native_encoding());
    }
}

#[cfg(unix)]
#[test]
fn unix_codeset_is_read_after_setlocale() {
    // Not a tautology: the test host runs under `LANG=C.UTF-8`, and without
    // the `setlocale(LC_CTYPE, "")` in `detect_native_encoding` the process
    // is still in the "C" locale, where `nl_langinfo(CODESET)` answers
    // `ANSI_X3.4-1968`. So this asserts the setlocale call is there.
    //
    // Skipped when the environment really does ask for a C/POSIX locale,
    // where `ANSI_X3.4-1968` is the RIGHT answer, and when either kill
    // switch is pinning the answer.
    if pinned("CRATONVM_NATIVE_ENCODING").is_some() {
        return;
    }
    // `pinned`, not `std::env::var`. The rule
    // `tools/flag-census/check-surface.sh` enforces has no test carve-out
    // and should not grow one: every environment read in a core runtime
    // crate enters through `flags::runtime_var`, which routes DECLARED VM
    // flags to the immutable snapshot and everything else -- these locale
    // names included -- to a live read. So the behaviour here is
    // unchanged, and the rule stays a rule rather than a rule with an
    // exception that the next reader has to check they are inside.
    let asks_for_c = ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .find_map(|k| pinned(k))
        .is_none_or(|v| v == "C" || v == "POSIX");
    if asks_for_c {
        return;
    }
    assert_ne!(
        native_encoding(),
        "ANSI_X3.4-1968",
        "codeset was read before setlocale()"
    );
}

#[cfg(windows)]
#[test]
fn acp_spelling_is_uppercase_cp_and_console_spelling_is_lowercase() {
    // The two rows that are MEASURED against HotSpot on a 1251 host —
    // `Cp1251` from the ANSI code page, `cp866` from a `chcp 866` console —
    // plus the band rule around them.
    assert_eq!(windows_acp_name(1251), "Cp1251");
    assert_eq!(windows_console_name(866), "cp866");
    assert_eq!(windows_console_name(932), "ms932");
    assert_eq!(windows_console_name(950), "ms950");
    assert_eq!(windows_console_name(1251), "cp1251");
    assert_eq!(windows_acp_name(936), "GBK");
}

#[cfg(windows)]
#[test]
fn the_measured_redirection_matrix() {
    // Every row MEASURED 2026-09-02 against HotSpot 25.0.3+9 on a host with
    // `GetACP()==1251`, console at `chcp 866`, one command run five ways.
    // The `NUL` rows are the whole reason this is a `GetFileType` test and
    // not a `GetConsoleMode` one: `NUL` is a character device, and the
    // rule that shipped on 2026-09-01 answered `Cp1251` for them where
    // HotSpot answers `cp866`.
    const ACP: u32 = 1251;
    const CONSOLE: u32 = 866;
    // (nothing redirected) — a console, FILE_TYPE_CHAR
    assert_eq!(windows_stream_encoding_for(CONSOLE, true, ACP), "cp866");
    // `1> NUL` — still FILE_TYPE_CHAR, and HotSpot still says cp866
    assert_eq!(windows_stream_encoding_for(CONSOLE, true, ACP), "cp866");
    // `1> file.txt` — FILE_TYPE_DISK
    assert_eq!(windows_stream_encoding_for(CONSOLE, false, ACP), "Cp1251");
    // `| more` — FILE_TYPE_PIPE
    assert_eq!(windows_stream_encoding_for(CONSOLE, false, ACP), "Cp1251");
    // No console attached at all: `GetConsoleCP` answers 0.
    assert_eq!(windows_stream_encoding_for(0, true, ACP), "Cp1251");
    // A UTF-8 console, which must not be spelled `cp65001`.
    assert_eq!(windows_stream_encoding_for(65001, true, ACP), "UTF-8");
}

#[cfg(windows)]
#[test]
fn code_page_65001_is_utf8_by_both_spellings() {
    // MEASURED on a `chcp 65001` console: HotSpot answers `UTF-8`. The
    // generic console rule would say `cp65001`, and there is no
    // `sun.nio.cs.CP65001` for `Charset.forName` to find — so this row is
    // the difference between a resolvable charset and a throw at bootstrap.
    assert_eq!(windows_console_name(65001), "UTF-8");
    assert_eq!(windows_acp_name(65001), "UTF-8");
    // No console at all: both callers fall back to the ANSI code page, and
    // `0` is what `GetACP` cannot return but `GetConsoleCP` can.
    assert_eq!(windows_acp_name(0), "UTF-8");
}
