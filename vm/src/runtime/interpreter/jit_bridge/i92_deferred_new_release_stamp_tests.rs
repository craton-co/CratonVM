use super::{deferred_new_release_sweep_due, note_new_site_settle_event};
use crate::classloading::ClassId;
use crate::vm::SharedVm;

/// A release that settles nothing skips the sweep. Asked a few times:
/// another test's VM whose layout domain shares this VM's `StoreEpochs`
/// slot may define a class in between, which moves the stamp once.
fn quiet(shared: &SharedVm) -> bool {
    (0..64).any(|_| !deferred_new_release_sweep_due(shared))
}

/// Interpreter round i4 wave 92, lane L7: with the release stamp armed
/// (set on this VM directly, so the test pins no default), a release
/// sweeps once per moved stamp: nothing held -> no; a new retry, a link /
/// init completion, a resolution-epoch bump -> one sweep each; a release
/// with nothing moved -> no.
#[test]
fn i92_a_release_sweeps_only_after_its_stamp_moved() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let _ = shared.jit.bridge.deferred_new_release_stamp_switch.set(true);
    assert!(
        !deferred_new_release_sweep_due(&shared),
        "nothing held: nothing to sweep"
    );
    shared.jit.bridge.deferred_new_retries.note_bail(
        ClassId::new(0),
        "i92/Held",
        "m",
        "()V",
        &[(1, 2)],
    );
    assert!(deferred_new_release_sweep_due(&shared), "a new retry");
    assert!(quiet(&shared), "nothing moved");
    note_new_site_settle_event(&shared);
    assert!(deferred_new_release_sweep_due(&shared), "a link / init");
    assert!(quiet(&shared), "nothing moved");
    crate::runtime::interpreter::constants::bump_resolution_epoch_in(&shared);
    assert!(deferred_new_release_sweep_due(&shared), "a resolution bump");
    assert!(quiet(&shared), "nothing moved");
}
