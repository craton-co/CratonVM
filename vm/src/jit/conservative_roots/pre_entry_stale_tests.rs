use super::*;

fn with_record<R>(words: Vec<(usize, usize)>, f: impl FnOnce() -> R) -> R {
    let saved = PRE_ENTRY_STALE.with(|c| {
        let mut s = c.borrow_mut();
        let old = (s.recorded, std::mem::replace(&mut s.words, words));
        s.recorded = true;
        old
    });
    let r = f();
    PRE_ENTRY_STALE.with(|c| {
        let mut s = c.borrow_mut();
        s.recorded = saved.0;
        s.words = saved.1;
    });
    r
}

/// Only the exact inherited pair is skipped: the same word in another slot,
/// or another word in the same slot, is still a hit.
#[test]
fn only_an_unchanged_inherited_pair_is_skipped() {
    with_record(vec![(0x7000_1000, 0xdead_0b0b)], || {
        assert!(is_pre_entry_stale_word(0x7000_1000, 0xdead_0b0b));
        assert!(!is_pre_entry_stale_word(0x7000_1008, 0xdead_0b0b));
        assert!(!is_pre_entry_stale_word(0x7000_1000, 0xdead_0b0c));
    });
}

/// A thread that inherited nothing skips nothing.
#[test]
fn an_empty_record_skips_nothing() {
    with_record(Vec::new(), || {
        assert!(!is_pre_entry_stale_word(0x7000_1000, 0xdead_0b0b));
    });
}

/// The record is taken once per thread: a later entry does not re-record
/// (a word written after the first entry is never "inherited").
#[test]
fn the_record_is_taken_once_per_thread() {
    std::thread::spawn(|| {
        note_pre_entry_stale_words(current_stack_pointer());
        let first = PRE_ENTRY_STALE.with(|c| c.borrow().recorded);
        assert!(first);
        PRE_ENTRY_STALE.with(|c| c.borrow_mut().words.push((1, 2)));
        note_pre_entry_stale_words(current_stack_pointer());
        assert_eq!(PRE_ENTRY_STALE.with(|c| c.borrow().words.clone()), vec![(1, 2)]);
    })
    .join()
    .unwrap();
}
