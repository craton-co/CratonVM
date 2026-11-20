use super::{pinned_plan_seeds_without_unnamed_watched, PIN_WORD_SLACK_ABOVE};

// `new WeakReference<>(new Object())` twice: [WR1 112][O1 16][WR2 112][O2 16].
const WR1: usize = 0x1000;
const O1: usize = 0x1070;
const WR2: usize = 0x1080;
const O2: usize = 0x10f0;
const OBJECTS: [(usize, usize); 4] = [(WR1, 0x70), (O1, 0x10), (WR2, 0x70), (O2, 0x10)];

fn seeds() -> Vec<usize> {
    OBJECTS.iter().map(|&(b, _)| b).collect()
}

/// The probe's shape: the compiled frame names both `WeakReference`s by
/// their bases; both referents are watched and no word names them, so
/// they stop being seeds and the two references stay seeds.
#[test]
fn gci_i30wy_unnamed_watched_referents_are_not_seeds() {
    let watched = [WR1, O1, WR2, O2];
    let got = pinned_plan_seeds_without_unnamed_watched(
        &[WR1, WR2],
        &OBJECTS,
        &seeds(),
        |b| watched.contains(&b),
        |_| false,
    );
    assert_eq!(got, Some(vec![WR1, WR2]));
}

/// Nothing watched, or every watched object named: the plan's seeds stand
/// (`None`, byte-identical to the pre-i30 plan).
#[test]
fn gci_i30wy_nothing_dropped_answers_none() {
    let none = pinned_plan_seeds_without_unnamed_watched(
        &[WR1],
        &OBJECTS,
        &seeds(),
        |_| false,
        |_| false,
    );
    assert_eq!(none, None);
    let named = pinned_plan_seeds_without_unnamed_watched(
        &[O1 + 8, O2],
        &OBJECTS,
        &seeds(),
        |b| b == O1 || b == O2,
        |_| false,
    );
    assert_eq!(named, None, "a word inside O1 and one at O2's base name both");
}

/// A word that is not an object start keeps gcg g1/y's base-minus-offset
/// reach: everything starting within the slack above it stays a seed.
#[test]
fn gci_i30wy_interior_word_keeps_the_slack_above_reach() {
    assert!(O2 - (WR1 + 8) <= PIN_WORD_SLACK_ABOVE);
    let got = pinned_plan_seeds_without_unnamed_watched(
        &[WR1 + 8],
        &OBJECTS,
        &seeds(),
        |b| b == O1 || b == O2,
        |_| false,
    );
    assert_eq!(got, None);
}

/// A word exactly at an object start names the object ending there only
/// when that object is an array (a one-past-the-end element cursor).
#[test]
fn gci_i30wy_one_past_end_names_only_an_array() {
    let plain = pinned_plan_seeds_without_unnamed_watched(
        &[WR2],
        &OBJECTS,
        &seeds(),
        |b| b == O1,
        |_| false,
    );
    assert_eq!(plain, Some(vec![WR1, WR2, O2]));
    let array = pinned_plan_seeds_without_unnamed_watched(
        &[WR2],
        &OBJECTS,
        &seeds(),
        |b| b == O1,
        |b| b == O1,
    );
    assert_eq!(array, None);
    // A word one past the plan's last object (not a start) names it.
    let tail = pinned_plan_seeds_without_unnamed_watched(
        &[O2 + 0x10],
        &OBJECTS,
        &seeds(),
        |b| b == O2,
        |_| false,
    );
    assert_eq!(tail, None);
}

/// gci i34/wy: under the array-only slack rule an interior word of the
/// first `WeakReference` (a field address) names that reference only; the
/// referents and the second reference it used to reach are not named.
#[test]
fn gci_i34wy_array_only_slack_does_not_name_plain_neighbours() {
    let watched = [WR1, O1, WR2, O2];
    let got = super::pinned_plan_seeds_without_unnamed_watched_by(
        &[WR1 + 16, WR2],
        &OBJECTS,
        &seeds(),
        |b| watched.contains(&b),
        |_| false,
        true,
    );
    assert_eq!(got, Some(vec![WR1, WR2]));
    // The i30 rule (fz's) still names all three from the interior word.
    let i30 = pinned_plan_seeds_without_unnamed_watched(
        &[WR1 + 16, WR2],
        &OBJECTS,
        &seeds(),
        |b| watched.contains(&b),
        |_| false,
    );
    assert_eq!(i30, None);
}

/// gci i34/wy: an ARRAY within the slack above any word -- a start word
/// included -- stays named (the biased element cursor gcg g1/y protects).
#[test]
fn gci_i34wy_array_only_slack_still_names_an_array_above() {
    let got = super::pinned_plan_seeds_without_unnamed_watched_by(
        &[WR1],
        &OBJECTS,
        &seeds(),
        |b| b == O1 || b == O2,
        |b| b == O1,
        true,
    );
    assert_eq!(got, Some(vec![WR1, O1, WR2]));
}

/// gci i35/wy: the i34b shape -- a pin word equal to the `inline`
/// referent's own base that the root vector does not hold -- no longer
/// makes it a seed; the same word with the referent rooted, an interior
/// word, and the rule switched off (`None`) all keep it.
#[test]
fn gci_i35wy_unrooted_start_word_does_not_seed_a_watched_object() {
    let watched = [WR1, O1, WR2, O2];
    let rooted = [WR1, WR2];
    let in_root = |b: usize| rooted.contains(&b);
    let run = |words: &[usize], need: Option<&dyn Fn(usize) -> bool>| {
        super::pinned_plan_seeds_without_unnamed_watched_rooted(
            words,
            &OBJECTS,
            &seeds(),
            |b| watched.contains(&b),
            |_| false,
            true,
            need,
        )
    };
    assert_eq!(run(&[WR1, O1, WR2], Some(&in_root)), Some(vec![WR1, WR2]));
    assert_eq!(run(&[WR1, O1, WR2], None), Some(vec![WR1, O1, WR2]));
    assert_eq!(run(&[WR1, O1 + 8, WR2], Some(&in_root)), Some(vec![WR1, O1, WR2]));
    let o1_rooted = |b: usize| b == O1 || rooted.contains(&b);
    assert_eq!(run(&[WR1, O1, WR2], Some(&o1_rooted)), Some(vec![WR1, O1, WR2]));
    // `WR1` and `WR2` are start words too: rooted, they stay seeds; unrooted
    // (nothing rooted) every watched start-named object is dropped.
    assert_eq!(run(&[WR1, O1, WR2], Some(&|_| false)), Some(Vec::new()));
}

/// gci i39/rv: a card walk-gap word -- an old-gen heap word the card scan
/// cannot trace, so the plan is its only root -- names a watched object's
/// START (a reference value always does). It must keep the object a seed
/// under the i35/wy start-word rule although the root vector does not hold
/// it; before the fix the object was dropped and, unreached, freed under
/// the gap word.
#[test]
fn gci_i39rv_a_card_gap_start_word_keeps_a_watched_object_a_seed() {
    let watched = [O1, O2];
    let roots = [WR1, WR2];
    // The gap value names O1's start; the ledger words name WR1 and WR2.
    let gap = [O1];
    let set = super::pinned_watched_start_rooted_set(roots.iter().copied(), &gap);
    assert!(set.contains(&O1) && set.contains(&WR1) && !set.contains(&O2));
    let in_root = |b: usize| set.contains(&b);
    let got = super::pinned_plan_seeds_without_unnamed_watched_rooted(
        &[WR1, WR2, O1],
        &OBJECTS,
        &seeds(),
        |b| watched.contains(&b),
        |_| false,
        true,
        Some(&in_root),
    );
    // O1 stays (gap-named), O2 (named by nothing) is dropped.
    assert_eq!(got, Some(vec![WR1, O1, WR2]));
    // The root vector alone (the i35 rule) dropped O1 as well.
    let roots_only = super::pinned_watched_start_rooted_set(roots.iter().copied(), &[]);
    let in_roots_only = |b: usize| roots_only.contains(&b);
    let i35 = super::pinned_plan_seeds_without_unnamed_watched_rooted(
        &[WR1, WR2, O1],
        &OBJECTS,
        &seeds(),
        |b| watched.contains(&b),
        |_| false,
        true,
        Some(&in_roots_only),
    );
    assert_eq!(i35, Some(vec![WR1, WR2]));
}

/// gci i39/rv: the door feeds the card walk-gap words into the start-word
/// set, in the plan and in its census. gcj j1/pn item 1: and, under its
/// switch, the ledger's layout-free / register / peer-band words.
#[test]
fn gci_i39rv_the_plan_and_its_census_count_gap_words() {
    let src = concat!(include_str!("../gen_heap/mod.rs"), "\n", include_str!("../gen_heap/young_from_guard.rs"), "\n", include_str!("../gen_heap/geometry_cap_destination.rs"), "\n", include_str!("../gen_heap/publish_young_commit_bits.rs"), "\n", include_str!("../gen_heap/set_resolved_compact_primitive_field.rs"), "\n", include_str!("../gen_heap/set_field_full.rs"), "\n", include_str!("../gen_heap/non_moving_young_cycle.rs"), "\n", include_str!("../gen_heap/collect_garbage_inner_with_pins.rs"), "\n", include_str!("../gen_heap/young_drain_blocked.rs"), "\n", include_str!("../gen_heap/sweep_young_non_moving.rs"), "\n", include_str!("../gen_heap/sweep_old_gen_non_moving.rs"), "\n", include_str!("../gen_heap/mark_young_to_old_refs_deferring.rs"), "\n", include_str!("../gen_heap/cell_corrupt_diag_enabled.rs"), "\n", include_str!("../gen_heap/note_rejected_old_mark_candidate.rs"), "\n", include_str!("../gen_heap/zero_run_verdict.rs")).replace("\r\n", "\n");
    // `rfind`: the door sits far below this test, whose own needles would
    // otherwise be the first match.
    let plan = src
        .rfind("let start_needs_root = true;")
        .expect("the plan's start-word rule");
    let tail = &src[plan..];
    let window = &tail[..tail
        .find("let dropped = pinned_plan_seeds_without_unnamed_watched_rooted(")
        .expect("the plan call")];
    assert!(
        window.contains("pinned_watched_start_rooted_set(")
            && window
                .contains("pinned_watched_start_rooted_words(&card_gap_roots.young_words)")
            && window
                .contains("start_rooted_words.extend_from_slice(&pinned_layout_free_words);")
            && window.contains("pinned_watched_start_layout_free_rooted_enabled()")
            && window.contains("&start_rooted_words,"),
        "{window}"
    );
    let census_call =
        &tail[..tail.find("if let Some(kept) = dropped \u{7b}").expect("the plan's end")];
    assert!(
        census_call
            .find("start_needs_root,")
            .is_some_and(|at| census_call[at..].trim_start_matches("start_needs_root,").trim_start().starts_with("&start_rooted_words,")),
        "{census_call}"
    );
    let census = src
        .find("    let counted_set =\n        pinned_watched_start_rooted_set(")
        .expect("the census uses the same set");
    assert!(census > 0);
}

/// The naming census reports how each object is named.
#[test]
fn gci_i34wy_word_names_report_the_rule() {
    let names = super::pinned_plan_word_names(&[WR1 + 16, O2 + 0x10], &OBJECTS, &|_| false, true);
    assert!(names.contains(&(WR1, WR1 + 16, super::PIN_NAMES_INSIDE)));
    assert!(names.contains(&(O2, O2 + 0x10, super::PIN_NAMES_INSIDE)));
    assert!(!names.iter().any(|&(_, _, how)| how == super::PIN_NAMES_SLACK));
    assert!(!names.iter().any(|&(_, _, how)| how == super::PIN_NAMES_ARRAY_END));
}

/// The exact-roots subset is filtered the same way.
#[test]
fn gci_i30wy_filters_an_exact_roots_subset() {
    let got = pinned_plan_seeds_without_unnamed_watched(
        &[WR1, WR2],
        &OBJECTS,
        &[WR1, O1, WR2],
        |b| b == O1,
        |_| false,
    );
    assert_eq!(got, Some(vec![WR1, WR2]));
}
