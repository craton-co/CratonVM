//! JIT round 19 proposal wave 8, lane gc8 (GC719-2): a blocked thread's
//! window rows are matched to its published trace entry by entry, with at
//! most a native heading, or not at all.
use super::*;
use crate::native::registry::StackTraceEntry;

fn row(location: i64, frame: Option<usize>) -> OtherRow {
    OtherRow {
        info: FrameInfo {
            method: 0,
            location,
        },
        raw: BlockedRow {
            class_id: 0,
            method_id: 0,
            location,
            frame,
            runs_compiled: false,
            chain: None,
        },
    }
}

fn entry(class: &str, method: &str) -> StackTraceEntry {
    StackTraceEntry {
        class_name: Arc::from(class),
        method_name: Arc::from(method),
        method_descriptor: None,
        source_file: None,
        line_number: -1,
        byte_code_index: 0,
        class_id: None,
        method_index: None,
    }
}

fn name(class: &str, method: &str) -> Option<(String, Arc<str>)> {
    Some((class.to_string(), Arc::from(method)))
}

/// `hold` (compiled, holding a block) under `run` (interpreted), blocked
/// in the native `sleep0` the listing heads with.
#[test]
fn rows_match_the_trace_under_a_native_heading() {
    let rows = vec![row(-1, None), row(3, None), row(7, Some(0))];
    let names = vec![
        name("java.lang.Thread", "sleep0"),
        name("p.T", "hold"),
        name("p.T", "run"),
    ];
    let trace = vec![entry("p/T", "hold"), entry("p.T", "run")];
    assert_eq!(
        window_rows_for_trace(&names, &rows, &trace),
        Some(vec![1, 2])
    );
    // Equal lengths: one to one.
    let trace3 = vec![
        entry("java.lang.Thread", "sleep0"),
        entry("p.T", "hold"),
        entry("p.T", "run"),
    ];
    assert_eq!(
        window_rows_for_trace(&names, &rows, &trace3),
        Some(vec![0, 1, 2])
    );
}

#[test]
fn any_disagreement_refuses() {
    let rows = vec![row(-1, None), row(3, None), row(7, Some(0))];
    let names = vec![
        name("java.lang.Thread", "sleep0"),
        name("p.T", "hold"),
        name("p.T", "run"),
    ];
    // A method name differs.
    let trace = vec![entry("p.T", "other"), entry("p.T", "run")];
    assert_eq!(window_rows_for_trace(&names, &rows, &trace), None);
    // A class name differs.
    let trace = vec![entry("p.U", "hold"), entry("p.T", "run")];
    assert_eq!(window_rows_for_trace(&names, &rows, &trace), None);
    // The extra row is not a native heading.
    let rows2 = vec![row(5, Some(1)), row(3, None), row(7, Some(0))];
    let trace = vec![entry("p.T", "hold"), entry("p.T", "run")];
    assert_eq!(window_rows_for_trace(&names, &rows2, &trace), None);
    // Two rows more, or the trace longer than the listing.
    assert_eq!(window_rows_for_trace(&names, &rows, &trace[..1]), None);
    let long = vec![
        entry("x.X", "a"),
        entry("java.lang.Thread", "sleep0"),
        entry("p.T", "hold"),
        entry("p.T", "run"),
    ];
    assert_eq!(window_rows_for_trace(&names, &rows, &long), None);
    // A row that names no loaded method.
    let unnamed = vec![None, name("p.T", "hold"), name("p.T", "run")];
    let trace3 = vec![
        entry("java.lang.Thread", "sleep0"),
        entry("p.T", "hold"),
        entry("p.T", "run"),
    ];
    assert_eq!(window_rows_for_trace(&unnamed, &rows, &trace3), None);
}
