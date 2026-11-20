use super::*;

/// Nothing since the door's number: an empty answer. Entries after it: those
/// classes. A log that dropped entries newer than the number: `None` (the
/// door then treats every initialized holder as possibly late).
#[test]
fn the_log_names_the_classes_initialized_since_a_number() {
    let mut log = StaticInitLog::default();
    assert_eq!(log.initialized_since(0), Some(Vec::new()));
    log.push(7);
    let seq0 = log.seq;
    assert_eq!(log.initialized_since(seq0), Some(Vec::new()));
    log.push(8);
    log.push(9);
    assert_eq!(log.initialized_since(seq0), Some(vec![8, 9]));
    assert_eq!(log.initialized_since(0), Some(vec![7, 8, 9]));
    for id in 0..(STATIC_INIT_LOG_CAP as u32 + 5) {
        log.push(100 + id);
    }
    assert_eq!(log.recent.len(), STATIC_INIT_LOG_CAP, "bounded");
    assert_eq!(log.initialized_since(seq0), None, "the log no longer reaches back");
    let recent = log.seq - 2;
    assert_eq!(log.initialized_since(recent).map(|v| v.len()), Some(2));
}

/// INDY21-2: an init-owner cell is minted on ask (an unknown class gets
/// no address, but its cell is kept), names `1 +` the claimant after a
/// claim and `0` after the claim ends; hooks before any cell do nothing.
#[test]
fn an_owner_cell_follows_the_claim_hooks() {
    use std::sync::atomic::Ordering::SeqCst;
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let vm: &SharedVm = &shared;
    let id = ClassId::new(0x7_2102);
    clinit_owner_note(vm, id, Some(5));
    assert_eq!(vm.jit.bridge.clinit_owner_cell_count.load(SeqCst), 0);
    assert_eq!(clinit_owner_cell_for(vm, id), 0, "an unknown class: no address");
    let cell = vm
        .jit
        .bridge
        .clinit_owner_cells
        .lock()
        .expect("the cell map")
        .get(&id.as_u32())
        .copied()
        .expect("minted");
    assert_eq!(cell.load(SeqCst), 0);
    clinit_owner_note(vm, id, Some(5));
    assert_eq!(cell.load(SeqCst), 6, "1 + the claimant's thread id");
    clinit_owner_note(vm, id, None);
    assert_eq!(cell.load(SeqCst), 0, "the claim ended");
}

/// Review rev-vmjit item 2: only an INIT claim (state `Initializing`)
/// names its thread; a link-only claim (any other unfinished state with
/// `initializing_thread` set) and a finished class name nobody.
#[test]
fn only_an_init_claim_names_its_claimant() {
    use crate::classloading::ClassState;
    assert_eq!(clinit_init_claimant(ClassState::Initializing, Some(7)), Some(7));
    assert_eq!(clinit_init_claimant(ClassState::Initializing, None), None);
    for state in [
        ClassState::Loaded,
        ClassState::Verified,
        ClassState::Prepared,
        ClassState::Initialized,
        ClassState::InitializationError,
    ] {
        assert_eq!(clinit_init_claimant(state, Some(7)), None, "{state:?}");
    }
}
