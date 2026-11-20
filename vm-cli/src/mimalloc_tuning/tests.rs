use super::*;

#[test]
fn zr7rs_the_switch_reads_off_words_only() {
    assert!(!switch_is_off(None));
    for v in [&b"0"[..], b"off", b"OFF", b" false ", b"No"] {
        assert!(switch_is_off(Some(v)), "{v:?}");
    }
    for v in [&b"1"[..], b"on", b"", b"2"] {
        assert!(!switch_is_off(Some(v)), "{v:?}");
    }
}

#[test]
fn zr7rs_an_explicit_mimalloc_variable_wins_and_off_keeps_eager() {
    assert_eq!(decide(false, false, false, 2), LazyCommitOutcome::Off);
    assert_eq!(decide(false, false, true, 0), LazyCommitOutcome::Off);
    assert_eq!(decide(true, false, true, 2), LazyCommitOutcome::EnvWins);
    assert_eq!(decide(true, false, true, 0), LazyCommitOutcome::EnvWins);
    assert_eq!(decide(true, false, false, 0), LazyCommitOutcome::Default);
}

/// zr z10/mt: opt-in. Unset, blank and the off words leave mimalloc's
/// eager default; `1`, `on` and `force` ask for lazy commit.
#[test]
fn zr10mt_lazy_commit_is_opt_in() {
    assert!(!switch_is_on(None));
    for v in [&b""[..], b"  ", b"0", b"off", b"FALSE", b"no"] {
        assert!(!switch_is_on(Some(v)), "{v:?}");
    }
    for v in [&b"1"[..], b"on", b"true", b"force", b" Force "] {
        assert!(switch_is_on(Some(v)), "{v:?}");
    }
    // Unset reaches `decide` as not requested: eager, whatever the option.
    assert_eq!(
        decide(switch_is_on(None), false, false, 2),
        LazyCommitOutcome::Off
    );
    assert_eq!(
        decide(switch_is_on(Some(b"1")), false, false, 0),
        LazyCommitOutcome::Default
    );
}

/// zr z9/rv8: a non-zero option after the default is an explicit setting
/// mimalloc found under a spelling the probe missed (case-insensitive
/// match, legacy name), e.g. `mimalloc_eager_region_commit=2`: it wins.
/// The z8 hook forced it to 0 (`Late`); `force` keeps that for an A/B.
#[test]
fn zr9rv8_an_explicit_setting_the_probe_missed_is_not_overridden() {
    assert_eq!(decide(true, false, false, 2), LazyCommitOutcome::EnvWins);
    assert_eq!(decide(true, false, false, 1), LazyCommitOutcome::EnvWins);
    assert_eq!(decide(true, true, false, 2), LazyCommitOutcome::Late);
    assert!(switch_is_force(Some(b" Force ")));
    assert!(!switch_is_force(Some(b"1")));
    assert!(!switch_is_force(None));
    assert!(!switch_is_off(Some(b"force")), "`force` keeps the hook on");
}

/// boot-rss levers close: unset and blank install the 64 KiB cap; `0`,
/// the off words and a non-number keep mimalloc's own cap.
#[test]
fn brss_the_arena_cap_switch_parses() {
    assert_eq!(arena_max_object_kb(None), Some(DEFAULT_ARENA_MAX_OBJECT_KB));
    assert_eq!(
        arena_max_object_kb(Some(b" ")),
        Some(DEFAULT_ARENA_MAX_OBJECT_KB)
    );
    assert_eq!(arena_max_object_kb(Some(b"512")), Some(512));
    assert_eq!(arena_max_object_kb(Some(b" 4096 ")), Some(4096));
    for v in [&b"0"[..], b"off", b"No", b"2M", b"-1", b"99999999999999"] {
        assert_eq!(arena_max_object_kb(Some(v)), None, "{v:?}");
    }
}

/// The cap hook ran before `main` in this test binary (Linux with
/// mimalloc) and, unless the environment set the option, left the
/// default cap.
#[cfg(all(feature = "mimalloc", target_os = "linux"))]
#[test]
fn brss_the_arena_cap_hook_ran_before_main() {
    let o = CAP_OUTCOME.load(Ordering::Relaxed);
    assert_ne!(o, LazyCommitOutcome::NotRun as u8, "{}", summary_line());
    if o == LazyCommitOutcome::Default as u8 {
        assert_eq!(
            CAP_VALUE_AFTER.load(Ordering::Relaxed),
            arena_max_object_kb(
                None::<std::ffi::OsString>
                    .as_deref()
                    .map(|v| v.as_encoded_bytes())
            )
            .unwrap()
        );
    }
}

/// In this very test binary the hook ran before `main` (Linux / macOS
/// with mimalloc): its outcome is recorded and, when it installed the
/// lazy default, the option reads 0. Unset (the default since zr z10/mt)
/// records `Off` and leaves mimalloc's value.
#[cfg(all(feature = "mimalloc", not(windows)))]
#[test]
fn zr7rs_the_hook_ran_before_main() {
    let o = OUTCOME.load(Ordering::Relaxed);
    assert_ne!(o, LazyCommitOutcome::NotRun as u8, "{}", summary_line());
    if o == LazyCommitOutcome::Default as u8 {
        assert_eq!(VALUE_AFTER.load(Ordering::Relaxed), 0);
    }
}
