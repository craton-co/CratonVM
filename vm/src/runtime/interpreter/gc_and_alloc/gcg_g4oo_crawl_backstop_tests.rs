use super::*;

const CAP: usize = 64 << 20;

/// The backstop applies only where the HotSpot default turned the limit
/// off, and only under its switch; an explicit limit keeps its meaning.
#[test]
fn gcg_g4oo_the_backstop_replaces_only_the_hotspot_default_zero() {
    let d = GC_OVERHEAD_LIMIT_CYCLES;
    let b = d | OVERHEAD_CRAWL_BACKSTOP_ONLY;
    assert_eq!(overhead_limit_word_for(None, None, true, true), b);
    assert_eq!(overhead_limit_word_for(None, Some(true), true, true), b);
    assert_eq!(overhead_limit_word_for(None, Some(false), true, true), b);
    assert_eq!(
        overhead_limit_word_for(None, None, true, false),
        0,
        "`=0`: the g2a answer"
    );
    assert_eq!(
        overhead_limit_word_for(None, None, false, false),
        d,
        "historical limit"
    );
    assert_eq!(overhead_limit_word_for(None, Some(false), false, false), 0);
    assert_eq!(overhead_limit_word_for(Some("0"), None, true, false), 0);
    assert_eq!(overhead_limit_word_for(Some("3"), None, true, false), 3);
    assert_eq!(b & !OVERHEAD_CRAWL_BACKSTOP_ONLY, d);
}

/// The crawl counts; a sliver with progress, a falling live set, a heap
/// with room or no window does not.
#[test]
fn gcg_g4oo_only_a_progress_starved_cycle_feeds_the_backstop() {
    let live = 60 << 20;
    let w = |allocated: u64, prev: usize, after: usize| {
        Some(MutatorProgress {
            allocated,
            prev_live_after: prev,
            live_after: after,
        })
    };
    assert!(gc_cycle_is_progress_starved(
        CAP,
        true,
        w(8_192, live, live + 8_192)
    ));
    assert!(!gc_cycle_is_progress_starved(
        CAP,
        false,
        w(8_192, live, live)
    ));
    assert!(!gc_cycle_is_progress_starved(
        CAP,
        true,
        w((CAP / 50) as u64 + 1, live, live)
    ));
    assert!(!gc_cycle_is_progress_starved(
        CAP,
        true,
        w(8_192, live, live - CAP / 50 - 1)
    ));
    assert!(!gc_cycle_is_progress_starved(CAP, true, None));
}



