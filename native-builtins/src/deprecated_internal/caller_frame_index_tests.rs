use super::*;
use cratonvm_native_api::StackTraceEntry;
use std::sync::Arc;

fn entries(names: &[&str]) -> Vec<StackTraceEntry> {
    names
        .iter()
        .map(|n| StackTraceEntry {
            class_name: Arc::from(*n),
            method_name: Arc::from("m"),
            method_descriptor: None,
            source_file: None,
            line_number: -1,
            byte_code_index: 0,
            class_id: None,
            method_index: None,
        })
        .collect()
}

fn idx(names: &[&str], whole: bool) -> Option<Option<usize>> {
    caller_frame_index(&entries(names), whole).ok()
}

/// The answer of the whole-trace walk this replaced, spelled out: the entry
/// below the top, skipping stream / Spliterator / ServiceLoader plumbing
/// outward, the outermost entry when the plumbing runs out.
fn whole_trace_answer(names: &[&str]) -> Option<usize> {
    let n = names.len();
    match n {
        0 => None,
        1 => Some(0),
        _ => {
            let mut i = n - 2;
            loop {
                let name = names[i];
                let plumbing = name.starts_with("java/util/stream/")
                    || name == "java/util/ServiceLoader"
                    || (name.starts_with("java/util/") && name.contains("Spliterator"));
                if !plumbing || i == 0 {
                    return Some(i);
                }
                i -= 1;
            }
        }
    }
}

#[test]
fn the_whole_trace_answers_as_before() {
    for names in [
        vec![],
        vec!["A"],
        vec!["A", "B"],
        vec!["A", "B", "C"],
        vec!["A", "java/util/ServiceLoader", "B"],
        vec![
            "A",
            "java/util/stream/ForEachOps$ForEachOp$OfRef",
            "java/util/ServiceLoader",
            "B",
        ],
        vec!["java/util/stream/X", "java/util/ServiceLoader", "B"],
        vec!["A", "java/util/Spliterators$X", "java/util/ServiceLoader"],
    ] {
        assert_eq!(
            idx(&names, true),
            Some(whole_trace_answer(&names)),
            "{names:?}"
        );
    }
}

/// A tail is asked for more exactly when the walk ends inside it or the
/// tail is too short to say what lies below it.
#[test]
fn a_tail_that_ends_inside_the_plumbing_asks_for_more() {
    assert!(caller_frame_index(&entries(&[]), false).is_err());
    assert!(caller_frame_index(&entries(&["A"]), false).is_err());
    assert_eq!(idx(&["A", "B"], false), Some(Some(0)));
    // The walk settles on a non-plumbing frame inside the tail.
    assert_eq!(idx(&["X", "A", "B"], false), Some(Some(1)));
    // Plumbing all the way to the top of the tail: more is needed, unless
    // that IS the whole trace.
    let names = ["java/util/stream/X", "java/util/ServiceLoader", "B"];
    assert!(caller_frame_index(&entries(&names), false).is_err());
    assert_eq!(idx(&names, true), Some(Some(0)));
}

/// For every whole trace and every tail length, a tail that answers answers
/// the whole trace's index (shifted into the tail); one that cannot settle
/// says so.
#[test]
fn every_tail_agrees_with_the_whole_trace() {
    let all = [
        "M",
        "A",
        "java/util/stream/S",
        "java/util/ServiceLoader",
        "java/util/stream/T",
        "B",
        "C",
    ];
    for len in 0..=all.len() {
        let whole = &all[..len];
        let want = whole_trace_answer(whole);
        for tail_len in 0..=len {
            let tail = &whole[len - tail_len..];
            let is_whole = tail_len == len;
            match caller_frame_index(&entries(tail), is_whole) {
                Ok(Some(i)) => {
                    assert_eq!(Some(len - tail_len + i), want, "{whole:?} tail {tail_len}")
                }
                Ok(None) => assert_eq!(want, None, "{whole:?} tail {tail_len}"),
                Err(NeedsWiderTail) => assert!(!is_whole, "{whole:?} tail {tail_len}"),
            }
        }
    }
}
