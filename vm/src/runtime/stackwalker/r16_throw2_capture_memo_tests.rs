use super::{
    same_standin_site, BacktraceFrame, StandinSiteMemo, ThrowableIsAMemo,
    STANDIN_SITE_MEMO_WAYS, THROWABLE_IS_A_MEMO_WAYS,
};
use crate::classloading::ClassId;
use crate::native::registry::StackTraceEntry;
use std::sync::Arc;

const LABEL: &str = "probe/Main.thrower:(I)V";

fn compiled(label: &str, owner: u32, bci: i32, cp_stamp: Option<u64>) -> BacktraceFrame {
    BacktraceFrame::Compiled {
        label: Arc::from(label),
        owner_class_id: owner,
        bci,
        cp_stamp,
    }
}

fn inlined(label: &str, class_id: u32, bci: u32, cp_stamp: Option<u64>) -> BacktraceFrame {
    BacktraceFrame::Inlined {
        label: Arc::from(label),
        class_id,
        bci,
        cp_stamp,
        hidden: None,
    }
}

fn entry() -> BacktraceFrame {
    BacktraceFrame::Entry(StackTraceEntry {
        class_name: Arc::from("probe/Main"),
        method_name: Arc::from("thrower"),
        method_descriptor: Some(Arc::from("(I)V")),
        source_file: None,
        line_number: super::LINE_NUMBER_UNKNOWN,
        byte_code_index: 4,
        class_id: Some(ClassId::new(7)),
        method_index: None,
    })
}

#[test]
fn every_site_field_takes_part() {
    let base = compiled(LABEL, 7, 4, Some(1));
    // A different `Arc` with the same text is the same site.
    assert!(same_standin_site(&base, &compiled(LABEL, 7, 4, Some(1))));
    assert!(!same_standin_site(
        &base,
        &compiled("probe/Main.thrower:(J)V", 7, 4, Some(1))
    ));
    assert!(!same_standin_site(&base, &compiled(LABEL, 8, 4, Some(1))));
    assert!(!same_standin_site(&base, &compiled(LABEL, 7, 5, Some(1))));
    assert!(!same_standin_site(&base, &compiled(LABEL, 7, 4, Some(2))));
    assert!(!same_standin_site(&base, &compiled(LABEL, 7, 4, None)));
    let level = inlined(LABEL, 7, 4, Some(1));
    assert!(same_standin_site(&level, &inlined(LABEL, 7, 4, Some(1))));
    assert!(!same_standin_site(&level, &inlined(LABEL, 0, 4, Some(1))));
    assert!(!same_standin_site(&level, &inlined(LABEL, 7, 6, Some(1))));
    // A compiled frame is never an inlined level of the same text, and an
    // entry is never a memoised site.
    assert!(!same_standin_site(&base, &level));
    assert!(!same_standin_site(&entry(), &entry()));
}

#[test]
fn a_site_memo_answers_only_its_own_key() {
    let site = compiled(LABEL, 7, 4, None);
    let throwable = ClassId::new(40);
    let mut memo = StandinSiteMemo::default();
    assert!(!memo.answers(0, throwable, &site));
    memo.remember(0, throwable, &site);
    assert!(memo.answers(0, throwable, &compiled(LABEL, 7, 4, None)));
    // Another throwable class, a redefinition since, another bci: no answer.
    assert!(!memo.answers(0, ClassId::new(41), &site));
    assert!(!memo.answers(1, throwable, &site));
    assert!(!memo.answers(0, throwable, &compiled(LABEL, 7, 9, None)));
    // An interpreted-shape or entry frame is never remembered.
    let mut other = StandinSiteMemo::default();
    other.remember(0, throwable, &entry());
    assert!(!other.answers(0, throwable, &entry()));
}

#[test]
fn a_site_memo_keeps_its_last_ways() {
    let throwable = ClassId::new(40);
    let mut memo = StandinSiteMemo::default();
    let sites: Vec<_> = (0..STANDIN_SITE_MEMO_WAYS as i32 + 1)
        .map(|bci| compiled(LABEL, 7, bci, None))
        .collect();
    for site in &sites {
        memo.remember(0, throwable, site);
        // Remembering a remembered site takes no way.
        memo.remember(0, throwable, site);
    }
    // The oldest way was overwritten; the rest still answer.
    assert!(!memo.answers(0, throwable, &sites[0]));
    for site in &sites[1..] {
        assert!(memo.answers(0, throwable, site));
    }
}

#[test]
fn an_is_a_memo_answers_only_its_own_pair() {
    let (throwable, holder) = (ClassId::new(40), ClassId::new(12));
    let mut memo = ThrowableIsAMemo::default();
    assert_eq!(memo.lookup(0, throwable, holder), None);
    memo.remember(0, throwable, holder, true);
    memo.remember(0, throwable, ClassId::new(13), false);
    assert_eq!(memo.lookup(0, throwable, holder), Some(true));
    assert_eq!(memo.lookup(0, throwable, ClassId::new(13)), Some(false));
    assert_eq!(memo.lookup(0, ClassId::new(41), holder), None);
    assert_eq!(memo.lookup(1, throwable, holder), None);
    // A pair already remembered keeps its first answer and takes no way.
    memo.remember(0, throwable, holder, false);
    assert_eq!(memo.lookup(0, throwable, holder), Some(true));
    for h in 0..THROWABLE_IS_A_MEMO_WAYS as u32 {
        memo.remember(0, ClassId::new(100), ClassId::new(h), h % 2 == 0);
    }
    assert_eq!(memo.lookup(0, throwable, holder), None);
    assert_eq!(
        memo.lookup(0, ClassId::new(100), ClassId::new(2)),
        Some(true)
    );
    assert_eq!(
        memo.lookup(0, ClassId::new(100), ClassId::new(3)),
        Some(false)
    );
}
