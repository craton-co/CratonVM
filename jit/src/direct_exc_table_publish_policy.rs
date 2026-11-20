use super::*;

/// The default is ON as of 2026-08-21, and `=0` is what restores the ban.
///
/// Pinned because the flip is NOT justified by a workload win — it is
/// justified by the static door otherwise being 11x worse than the virtual
/// door for the same callee — so the next person to read the census could
/// reasonably conclude it should go back. If they do, it should be a
/// decision that edits this test, not a silent polarity slip.
#[test]
fn the_default_is_on_and_zero_restores_the_ban() {
    assert!(
        direct_call_exc_table_publish_decision(None, SpIcDeoptCheck::On),
        "unset must mean ON since 2026-08-21",
    );
    for off in ["0", "false", "off"] {
        assert!(
            !direct_call_exc_table_publish_decision(Some(off), SpIcDeoptCheck::On),
            "`{off}` must restore the ban",
        );
    }
    for on in ["1", "true", ""] {
        assert!(
            direct_call_exc_table_publish_decision(Some(on), SpIcDeoptCheck::On),
            "`{on}` must not restore the ban",
        );
    }
}

/// The interlock outranks the flag, in BOTH directions.
///
/// Publishing a direct `CALL` while the `i64::MIN` sentinel check is
/// suppressed is the unsound state — the callee's own handler would never
/// run — and the MIC sibling's page records a throughput reading that was
/// mistaken for a pass because of exactly this
/// (`207.04 ns/op` with the interlock holding, `24.12` without it). The
/// default flipping must not turn the interlock into a knob.
#[test]
fn a_suppressed_sentinel_check_forces_the_ban_back_on() {
    for check in [SpIcDeoptCheck::Off, SpIcDeoptCheck::SkipVoid] {
        assert!(
            !direct_call_exc_table_publish_decision(None, check),
            "{check:?} must force the ban on even with the flag unset",
        );
        assert!(
            !direct_call_exc_table_publish_decision(Some("1"), check),
            "{check:?} must force the ban on even when the flag asks for it",
        );
    }
}
