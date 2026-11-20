use super::*;

/// The latch these tests exercise is a PROCESS-GLOBAL, and `cargo test` runs
/// the tests of one crate as threads of one process — so two tests that each
/// call [`reset_for_test`] race, and whichever resets second makes the other
/// read a state it did not set.
///
/// MEASURED on the build host, same tree, same binary:
/// `--test-threads=1` passed 3 of 3 and the default parallel harness passed
/// 2 of 3, failing in `the_default_licenses_the_yield` and
/// `a_fallback_mint_revokes_the_yield_permanently` together. That is an
/// intermittently red `types` gate for every lane, and it says nothing about
/// the VM.
///
/// The lock is the fix rather than merging the two tests, because they
/// assert different properties and a reader should be able to see which one
/// broke. `parking_lot` is not a dependency of `types`, and a poisoned
/// `std` mutex would turn one failure into a cascade, so the guard is taken
/// through `unwrap_or_else(|e| e.into_inner())`.
static LATCH: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The default licenses the yield. See the module header for why this is the
/// right default and what the previous, inert one was.
#[test]
fn the_default_licenses_the_yield() {
    let _serialised = LATCH.lock().unwrap_or_else(|e| e.into_inner());
    reset_for_test();
    assert!(!arraylist_classed_view_possible());
    assert_eq!(arraylist_view_fallback_count(), 0);
}

/// One-way: a fallback mint revokes the yield and nothing restores it.
#[test]
fn a_fallback_mint_revokes_the_yield_permanently() {
    let _serialised = LATCH.lock().unwrap_or_else(|e| e.into_inner());
    reset_for_test();
    note_arraylist_classed_view_minted();
    assert!(arraylist_classed_view_possible());
    assert_eq!(arraylist_view_fallback_count(), 1);
    note_arraylist_classed_view_minted();
    assert!(arraylist_classed_view_possible());
    assert_eq!(arraylist_view_fallback_count(), 2);
    reset_for_test();
}
