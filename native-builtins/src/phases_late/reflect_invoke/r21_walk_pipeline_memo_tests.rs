use super::*;
use cratonvm_native_api::NativeClassAccess;

/// WALK21-6: the owners resolve once per VM (a second VM on the thread
/// resolves its own), and only once both classes are loaded.
#[test]
fn r21_walk_invoke_owners_are_per_vm_and_need_both_classes() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    ctx.set_vm_identity(0x2101);
    assert_eq!(p59_pipeline_invoke_owners(&mut ctx), None);
    let factory = ctx
        .ensure_class_initialized("java/lang/StackStreamFactory")
        .unwrap_or_else(|_| panic!("factory"));
    assert_eq!(p59_pipeline_invoke_owners(&mut ctx), None);
    let walker = ctx
        .ensure_class_initialized("java/lang/StackStreamFactory$AbstractStackWalker")
        .unwrap_or_else(|_| panic!("walker"));
    assert_eq!(p59_pipeline_invoke_owners(&mut ctx), Some((factory, walker)));
    // Cached for this VM: answered without asking again.
    assert_eq!(p59_pipeline_invoke_owners(&mut ctx), Some((factory, walker)));
    let mut other = crate::test_utils::MockNativeContext::new();
    other.set_vm_identity(0x2102);
    assert_eq!(p59_pipeline_invoke_owners(&mut other), None);
}

/// WALK21-10: the extension's prefix check compares class, method and
/// bci of every window frame against the whole list's first frames.
#[test]
fn r21_walk_window_prefix_check() {
    let entry = |method: &str, bci: i32| cratonvm_native_api::StackTraceEntry {
        class_name: std::sync::Arc::from("p/C"),
        method_name: std::sync::Arc::from(method),
        method_descriptor: None,
        source_file: None,
        line_number: -1,
        byte_code_index: bci,
        class_id: Some(ClassId::new(9)),
        method_index: None,
    };
    let whole = vec![entry("a", 1), entry("b", 2), entry("c", 3)];
    assert!(p59_walk_window_is_prefix(&whole[..2], &whole));
    assert!(p59_walk_window_is_prefix(&[], &whole));
    assert!(!p59_walk_window_is_prefix(&[entry("a", 1), entry("b", 9)], &whole));
    assert!(!p59_walk_window_is_prefix(&[entry("x", 1)], &whole));
    let mut longer = whole.clone();
    longer.push(entry("d", 4));
    assert!(!p59_walk_window_is_prefix(&longer, &whole));
    // The fallback's search: the window's only occurrence.
    let current = vec![
        entry("lambda", 7),
        entry("b", 2),
        entry("a", 1),
        entry("b", 2),
        entry("c", 3),
    ];
    assert_eq!(p59_walk_window_position(&[entry("a", 1), entry("b", 2)], &current, None), Ok(2));
    // Two occurrences and no anchor: refused, never the innermost.
    assert_eq!(
        p59_walk_window_position(&[entry("b", 2)], &current, None),
        Err(P59WindowMiss::Ambiguous)
    );
    // The anchored list's length places it (4 frames listed: from index 1;
    // 2 frames listed: from index 3).
    assert_eq!(p59_walk_window_position(&[entry("b", 2)], &current, Some(4)), Ok(1));
    assert_eq!(p59_walk_window_position(&[entry("b", 2)], &current, Some(2)), Ok(3));
    assert_eq!(
        p59_walk_window_position(&[entry("z", 0)], &current, None),
        Err(P59WindowMiss::Absent)
    );
    assert_eq!(p59_walk_window_position(&longer, &whole, None), Err(P59WindowMiss::Absent));
}

/// r21 review rev-w45-vm item 5: a nested re-entrant walk (its function
/// re-enters the walk's caller, which suspends at the same bci) puts a copy
/// of the window above the real one; recursion repeats it below. The
/// anchor's length picks the real one; without it the walk refuses.
#[test]
fn r21_walk_window_position_repeated_frames() {
    let entry = |method: &str, bci: i32| cratonvm_native_api::StackTraceEntry {
        class_name: std::sync::Arc::from("p/C"),
        method_name: std::sync::Arc::from(method),
        method_descriptor: None,
        source_file: None,
        line_number: -1,
        byte_code_index: bci,
        class_id: Some(ClassId::new(9)),
        method_index: None,
    };
    let window = vec![entry("caller", 5), entry("rec", 3), entry("rec", 3)];
    // Nested re-entry: the inner walk's copy of the window (0..3), the
    // outer function's frames, then the real window and its tail.
    let nested = vec![
        entry("caller", 5),
        entry("rec", 3),
        entry("rec", 3),
        entry("lambda", 8),
        entry("caller", 5),
        entry("rec", 3),
        entry("rec", 3),
        entry("main", 1),
    ];
    assert_eq!(
        p59_walk_window_position(&window, &nested, None),
        Err(P59WindowMiss::Ambiguous)
    );
    // The walk's list is the last four frames.
    assert_eq!(p59_walk_window_position(&window, &nested, Some(4)), Ok(4));
    // An anchor length the window does not fit is not trusted: the
    // occurrences decide, and two of them refuse.
    assert_eq!(
        p59_walk_window_position(&window, &nested, Some(5)),
        Err(P59WindowMiss::Ambiguous)
    );
    // Recursion repeating the window below the real one (overlapping
    // runs: rec, rec, rec): placed by the anchor only.
    let window = vec![entry("rec", 3), entry("rec", 3)];
    let recursive = vec![entry("rec", 3), entry("rec", 3), entry("rec", 3), entry("main", 1)];
    assert_eq!(
        p59_walk_window_position(&window, &recursive, None),
        Err(P59WindowMiss::Ambiguous)
    );
    assert_eq!(p59_walk_window_position(&window, &recursive, Some(4)), Ok(0));
    assert_eq!(p59_walk_window_position(&window, &recursive, Some(3)), Ok(1));
    // A single occurrence needs no anchor, and a fitting anchor agrees.
    let single = vec![entry("lambda", 8), entry("rec", 3), entry("rec", 3), entry("main", 1)];
    assert_eq!(p59_walk_window_position(&window, &single, None), Ok(1));
    assert_eq!(p59_walk_window_position(&window, &single, Some(3)), Ok(1));
    // A list longer than the capture is not a place.
    assert_eq!(p59_walk_window_position(&window, &single, Some(9)), Ok(1));
    // r21 review rev-w68-native item 6 (wave 10, lane walk): an empty
    // window (every frame of the first batch screened out) with no
    // anchored capture is the whole list from 0, as with the switch off,
    // never refused; an anchor that places it still decides.
    assert_eq!(p59_walk_window_position(&[], &single, None), Ok(0));
    assert_eq!(p59_walk_window_position(&[], &[], None), Ok(0));
    assert_eq!(p59_walk_window_position(&[], &single, Some(3)), Ok(1));
    assert_eq!(p59_walk_window_position(&[], &single, Some(9)), Ok(0));
}

/// WALK21-2: a layout row answers only its VM and buffer kind.
#[test]
fn r21_walk_pipeline_layout_rows_are_per_vm_and_buffer_kind() {
    let layout = P59PipelineLayout {
        traverser_cid: ClassId::new(11),
        traverser_slots: [1, 2, 3],
        buffer_cid: ClassId::new(12),
        buffer_slots: [4, 5, 6, 7, 8, 9],
    };
    p59_pipeline_layout_store(0x2103, false, layout);
    let hit = p59_pipeline_layout_cached(0x2103, false).expect("stored");
    assert_eq!(hit.traverser_cid, ClassId::new(11));
    assert_eq!(hit.buffer_slots, [4, 5, 6, 7, 8, 9]);
    assert!(p59_pipeline_layout_cached(0x2103, true).is_none());
    assert!(p59_pipeline_layout_cached(0x2104, false).is_none());
}
