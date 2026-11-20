use super::{classify_helper_window_refusal as classify, HelperWindowRefusal as R};

/// gch h3/fd (d9c step 1): one reason per failing gate, in the pass's
/// order; a window every gate passes is no refusal.
#[test]
fn gch_h3fd_each_refusing_gate_names_its_reason() {
    assert_eq!(
        classify(true, true, true, true, true),
        None,
        "a pinned window"
    );
    assert_eq!(classify(false, true, true, true, true), Some(R::PinOff));
    assert_eq!(
        classify(false, false, false, false, false),
        Some(R::PinOff),
        "first gate wins"
    );
    assert_eq!(
        classify(true, false, true, false, true),
        Some(R::ShadowUnpublished)
    );
    assert_eq!(
        classify(true, false, true, false, false),
        Some(R::ShadowUnpublished)
    );
    assert_eq!(
        classify(true, false, true, true, false),
        Some(R::ShadowBandless)
    );
    assert_eq!(
        classify(true, false, true, true, true),
        Some(R::ShadowUntrusted)
    );
    assert_eq!(
        classify(true, false, false, true, true),
        Some(R::ShadowUntrusted),
        "shadow before band"
    );
    assert_eq!(
        classify(true, true, false, true, true),
        Some(R::BandIncomplete)
    );
}

/// Every reason has its own token (the host greps count by token).
#[test]
fn gch_h3fd_refusal_tokens_are_distinct() {
    let all = [
        R::BandIncomplete,
        R::ShadowUnpublished,
        R::ShadowBandless,
        R::ShadowUntrusted,
        R::PinOff,
        R::Unreadable,
    ];
    let mut tokens: Vec<&str> = all.iter().map(|r| r.token()).collect();
    tokens.sort_unstable();
    tokens.dedup();
    assert_eq!(tokens.len(), all.len());    }