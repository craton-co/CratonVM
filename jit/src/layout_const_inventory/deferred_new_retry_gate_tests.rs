// `pub(crate)` in `jfr_compile_decision`, so not reachable through the
// crate root's glob re-export; see the file preamble.
use crate::jfr_compile_decision::{note_deferred_new_bail, take_deferred_new_retry};

/// The memo map and its held-count are process-global, so these tests
/// cannot run concurrently with each other: `cargo test` runs them on
/// separate threads, and a count asserted as `before + 1` is only stable
/// if nothing else is arming or spending at the same time.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}


/// One-shot is one-shot: a granted retry is not re-grantable.
#[test]
fn a_spent_retry_is_not_granted_twice() {
    let _serial = serial();
    let (c, m, d) = ("T$Once", "run", "()V");
    note_deferred_new_bail(c, m, d, &[(1, 2)]);
    let now = |_: u32, _: u16| true;
    assert!(take_deferred_new_retry(c, m, d, &now));
    assert!(!take_deferred_new_retry(c, m, d, &now));
}


/// A memo with no recorded sites cannot answer the question, so it grants —
/// the historical behaviour, not a silent refusal.
#[test]
fn a_memo_without_sites_grants_as_before() {
    let _serial = serial();
    let (c, m, d) = ("T$Unknown", "run", "()V");
    note_deferred_new_bail(c, m, d, &[]);
    let never = |_: u32, _: u16| false;
    assert!(take_deferred_new_retry(c, m, d, &never));
}

/// An un-armed method has no retry to take.
#[test]
fn a_method_that_never_bailed_has_no_retry() {
    let _serial = serial();
    let now = |_: u32, _: u16| true;
    assert!(!take_deferred_new_retry("T$Never", "run", "()V", &now));
}


/// The sweep's fast path is this counter, so it has to actually track the
/// arm/spend pair — a counter stuck at zero silently disables the sweep,
/// and one that never decrements makes every class definition do work.
#[test]
fn the_held_count_tracks_arming_and_spending() {
    let _serial = serial();
    let (c, m, d) = ("T$Counted", "run", "()V");
    let before = super::held_deferred_new_count();
    super::note_deferred_new_bail(c, m, d, &[(5, 6)]);
    assert_eq!(super::held_deferred_new_count(), before + 1);
    let now = |_: u32, _: u16| true;
    assert!(take_deferred_new_retry(c, m, d, &now));
    assert_eq!(super::held_deferred_new_count(), before);    }