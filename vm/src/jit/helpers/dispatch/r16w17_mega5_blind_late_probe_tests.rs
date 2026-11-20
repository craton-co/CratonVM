//! Round 16 wave 17, lane mega5: the late probe sets a blind key to probe
//! on its next call, once per memo generation, and the blind arm's count
//! does not see the mark.
use super::*;

const THRESHOLD: u32 = 500;
/// A fake site address: the memo is keyed by value, and a test `static`
/// would move the per-VM statics ratchet.
const SITE: usize = 0x5170_0000;

fn key(receiver: u32) -> (JitSiteKey, u32) {
    (jit_site_key(0x51_7e, SITE), receiver)
}

fn raw(key: (JitSiteKey, u32)) -> Option<u32> {
    VIRTUAL_DISPATCH_COUNTER.with(|dc| dc.borrow().get(&key).copied())
}

#[test]
fn a_key_is_set_to_probe_once_per_generation() {
    let k = key(0x9001);
    VIRTUAL_DISPATCH_COUNTER.with(|dc| dc.borrow_mut().insert(k, 3));
    assert!(blind_late_probe_rewind_counter(k, THRESHOLD));
    let set = raw(k).expect("entry");
    assert_eq!(set & !BLIND_LATE_PROBE_MARK, THRESHOLD - 1);
    // The blind arm's next increment reads exactly the threshold.
    assert_eq!(set.wrapping_add(1) & !BLIND_LATE_PROBE_MARK, THRESHOLD);
    // Marked: a second stand-down in the same generation does nothing.
    assert!(!blind_late_probe_rewind_counter(k, THRESHOLD));
    assert_eq!(raw(k), Some(set));
    // A publication clears the memo, and with it the mark.
    clear_publication_sensitive_site_memos();
    assert_eq!(raw(k), None);
    assert!(blind_late_probe_rewind_counter(k, THRESHOLD));
    clear_publication_sensitive_site_memos();
}

#[test]
fn a_key_never_counted_is_created_set_and_a_zero_threshold_is_inert() {
    let k = key(0x9002);
    clear_publication_sensitive_site_memos();
    assert!(blind_late_probe_rewind_counter(k, THRESHOLD));
    assert_eq!(
        raw(k).map(|r| r & !BLIND_LATE_PROBE_MARK),
        Some(THRESHOLD - 1)
    );
    let other = key(0x9003);
    assert!(!blind_late_probe_rewind_counter(other, 0));
    assert_eq!(raw(other), None);
    clear_publication_sensitive_site_memos();    }