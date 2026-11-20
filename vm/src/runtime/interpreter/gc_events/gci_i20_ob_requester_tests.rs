//! gci i20/ob (`gci-i19ob-jfr-gc-row-field-residuals-FIXED-20261017` item 1):
//! `jdk.ExecuteVMOperation.caller` names the collection's requester.
use super::*;
use crate::config::{GcAlgorithm, VmConfig};

fn vm() -> SharedVm {
    SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    })
}

/// A registry id with no entry has no Java tid: no requester, so the
/// row keeps `caller` 0 (and nothing panics on the lookup).
#[test]
fn gci_i20_ob_an_unknown_thread_is_no_requester() {
    let shared = vm();
    let id = crate::threading::jvm_thread::ThreadId(u64::MAX - 7);
    assert!(jfr_requester_of(&shared, id).is_none());
}

/// The owned requester lends the backend's caller record unchanged.
#[test]
fn gci_i20_ob_the_requester_lends_its_fields() {
    let who = JfrRequester {
        java_tid: 3,
        name: "main".to_string(),
        os_tid: 4242,
    };
    let c = who.as_caller();
    assert_eq!((c.java_tid, c.name, c.os_tid), (3, "main", 4242));
}

/// Every JFR collection row the reporter builds carries the field, the
/// plain finish names nobody, and the `_by` door resolves its requester
/// only while a backend recording runs on a reporting collection.
#[test]
fn gci_i20_ob_every_collection_row_carries_the_caller() {
    let src = include_str!("../gc_events.rs").replace("\r\n", "\n");
    // Every struct literal (this module spells its needles escaped, so
    // it is not one of them): the lines up to the first closing line.
    let needle = "&JfrCollection \u{7b}";
    let literals = src.matches(needle).count();
    assert!(literals >= 5, "the reporter's literals: {literals}");
    for (at, _) in src.match_indices(needle) {
        let body: Vec<&str> = src[at..]
            .lines()
            .skip(1)
            .take_while(|l| !l.trim_start().starts_with('\u{7d}'))
            .collect();
        let body = body.join("\n");
        assert!(
            body.contains("caller") || body.contains("..*c"),
            "a JfrCollection literal without `caller`: {}",
            &src[at..at + 40]
        );
    }
    let production = &src[..src.find("\nmod gci_i20_ob_requester_tests").expect("this module")];
    // The requester-less entry, while it exists, names nobody.
    if let Some(at) = src.find("\npub(super) fn gc_event_finish(") {
        let finish = &src[at..];
        let finish = &finish[..finish.find("\n\u{7d}\n").expect("its end")];
        assert!(finish.contains("gc_event_finish_with(shared, door, start, None)"));
    }
    let by = &src[src.find("\npub(super) fn gc_event_finish_by(").expect("by")..];
    let by = &by[..by.find("\n\u{7d}\n").expect("its end")];
    assert!(by.contains("any_recording()"));
    assert!(by.contains("s.events"));
    assert!(production.contains("caller: c.caller,"));
}
