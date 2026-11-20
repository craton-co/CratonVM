use super::{apply_supersede_epoch, supersede_epoch_bump_needed, SupersedeOutcome};
use crate::config::VmConfig;
use crate::vm::SharedVm;

/// A first publish leaves another site's `Jit` entry current; a real
/// replacement still stales it.
#[test]
fn a_first_publish_keeps_other_jit_entries_and_a_replacement_evicts_them() {
    use SupersedeOutcome::{Changed, FirstPublish};
    let shared = SharedVm::new(VmConfig::default());
    // Another call site's `Jit` invoke-cache entry, filled before both
    // publishes.
    let other_site = shared.jit.supersede_gate();
    let epoch = shared.jit.supersede_epoch();

    // Stamp moved by exactly this publish: nothing else of the name.
    let bumped = apply_supersede_epoch(&shared, FirstPublish, Some(41), Some(42), true);
    assert!(!bumped, "a first publish replaced nothing");
    assert_eq!(shared.jit.supersede_epoch(), epoch);
    assert!(!other_site.is_stale(), "the other site's entry stays current");

    let bumped = apply_supersede_epoch(&shared, Changed, Some(42), Some(43), true);
    assert!(bumped, "a replacement must invalidate");
    assert!(other_site.is_stale(), "an entry that may hold the replaced body re-resolves");
}

/// Every case that is not provably a lone first publish bumps.
#[test]
fn only_a_lone_first_publish_skips_the_bump() {
    use SupersedeOutcome::{Changed, FirstPublish, Unchanged};
    assert!(!supersede_epoch_bump_needed(FirstPublish, Some(7), Some(8), true));
    // Another publication of the name in the compile's window.
    assert!(supersede_epoch_bump_needed(FirstPublish, Some(7), Some(9), true));
    // A flush or a redefinition can move the stamp any amount, including
    // backwards through wrapping.
    assert!(supersede_epoch_bump_needed(FirstPublish, Some(7), Some(7), true));
    assert!(supersede_epoch_bump_needed(FirstPublish, None, Some(8), true));
    assert!(supersede_epoch_bump_needed(FirstPublish, Some(7), None, true));
    assert!(supersede_epoch_bump_needed(Unchanged, Some(7), Some(8), true));
    assert!(supersede_epoch_bump_needed(Changed, Some(7), Some(8), true));
    // Wrapping arithmetic: one step across the top is still one step.
    assert!(!supersede_epoch_bump_needed(FirstPublish, Some(u64::MAX), Some(0), true));
}

/// `=0` restores the unconditional bump.
#[test]
fn the_switch_off_bumps_every_publish() {
    for outcome in [
        SupersedeOutcome::FirstPublish,
        SupersedeOutcome::Unchanged,
        SupersedeOutcome::Changed,
    ] {
        assert!(supersede_epoch_bump_needed(outcome, Some(7), Some(8), false));
    }
}

