use super::r13_trace3_standin_tests::{add_class, java, native, pool};
use super::*;

const INTERRUPTED: &str = "java/lang/InterruptedException";

fn entry(
    class_id: ClassId,
    class: &str,
    name: &str,
    descriptor: &str,
    bci: i32,
) -> StackTraceEntry {
    StackTraceEntry {
        class_name: Arc::from(class),
        method_name: Arc::from(name),
        method_descriptor: Some(Arc::from(descriptor)),
        source_file: None,
        line_number: LINE_NUMBER_UNKNOWN,
        byte_code_index: bci,
        class_id: Some(class_id),
        method_index: None,
    }
}

/// `Thread.join()` is `join(0L)` (pc 2, line 1901). JDK-25-shaped
/// (`jdk17 == false`): `join(long)` calls `java/lang/Thread.wait(J)V` in
/// its timed loop first (pc 2, `lload_1`, line 1881), then in its
/// `wait(0)` loop (pc 7, `lconst_0`, line 1887); Java `Object.wait(long)`
/// ends in `wait0` (last call pc 7, line 389). JDK-17-shaped: the `wait(0)`
/// loop first (pc 2, line 1301), the timed `wait(delay)` second (pc 7,
/// line 1308), `Object.wait(long)` native. `probe/Main.main()V` calls
/// `join()` at bci 1 and `join(long)` at bci 6.
fn fixture(jdk17: bool) -> (ClassStore, ClassId) {
    let mut store = ClassStore::new();
    let object = if jdk17 {
        add_class(
            &mut store,
            "java/lang/Object",
            None,
            pool(&[]),
            vec![native("wait", "(J)V")],
        )
    } else {
        add_class(
            &mut store,
            "java/lang/Object",
            None,
            pool(&[("java/lang/Object", "wait0", "(J)V")]),
            vec![
                java(
                    "wait",
                    "(J)V",
                    vec![0x2a, 0x1f, 0xb7, 0, 6, 0x2a, 0x1f, 0xb7, 0, 6, 0xb1],
                    &[(0, 383), (5, 389)],
                ),
                native("wait0", "(J)V"),
            ],
        )
    };
    let join_long = if jdk17 {
        java(
            "join",
            "(J)V",
            vec![0x2a, 0x09, 0xb6, 0, 12, 0x2a, 0x1f, 0xb6, 0, 12, 0xb1],
            &[(0, 1301), (5, 1308)],
        )
    } else {
        java(
            "join",
            "(J)V",
            vec![0x2a, 0x1f, 0xb6, 0, 12, 0x2a, 0x09, 0xb6, 0, 12, 0xb1],
            &[(0, 1881), (5, 1887)],
        )
    };
    add_class(
        &mut store,
        "java/lang/Thread",
        Some(object),
        pool(&[
            ("java/lang/Thread", "join", "(J)V"),
            ("java/lang/Thread", "wait", "(J)V"),
        ]),
        vec![
            java(
                "join",
                "()V",
                vec![0x2a, 0x09, 0xb6, 0, 6, 0xb1],
                &[(0, 1901)],
            ),
            join_long,
            // `join(long, int)`: `join(millis)` at pc 2, line 1950.
            java(
                "join",
                "(JI)V",
                vec![0x2a, 0x1f, 0xb6, 0, 6, 0xb1],
                &[(0, 1950)],
            ),
        ],
    );
    let main = add_class(
        &mut store,
        "probe/Main",
        Some(object),
        pool(&[
            ("java/lang/Thread", "join", "()V"),
            ("java/lang/Thread", "join", "(J)V"),
            ("java/lang/Thread", "join", "(JI)V"),
        ]),
        vec![java(
            "main",
            "()V",
            vec![
                0x2a, 0xb6, 0, 6, 0x2a, 0x09, 0xb6, 0, 12, 0x2a, 0x09, 0x03, 0xb6, 0, 18, 0xb1,
            ],
            &[(0, 3), (4, 4), (9, 5)],
        )],
    );
    (store, main)
}

fn rows(frames: Option<Vec<BacktraceFrame>>) -> Option<Vec<(String, i32, i32)>> {
    frames.map(|frames| {
        frames
            .iter()
            .map(|f| {
                let e = f.to_entry();
                let method = format!("{}.{}", e.class_name, e.method_name);
                (method, e.line_number, e.byte_code_index)
            })
            .collect()
    })
}

fn row(method: &str, line: i32, bci: i32) -> (String, i32, i32) {
    (method.to_string(), line, bci)
}

/// TR5-3: a served direct `join(long)` interrupted with a positive
/// timeout leaves from the timed loop's `wait(delay)`, `join(0)` from the
/// `wait(0)` loop -- on both JDK shapes, whichever loop comes first.
#[test]
fn a_timed_join_leaves_from_the_wait_its_timeout_names() {
    let wait25 = || {
        vec![
            row("java/lang/Object.wait", 389, 7),
            row("java/lang/Object.wait0", LINE_NUMBER_NATIVE, -1),
        ]
    };
    let wait17 = || vec![row("java/lang/Object.wait", LINE_NUMBER_NATIVE, -1)];
    let join = |line, bci| row("java/lang/Thread.join", line, bci);
    for (jdk17, timed_row, zero_row, wait) in [
        (false, join(1881, 2), join(1887, 7), wait25()),
        (true, join(1308, 7), join(1301, 2), wait17()),
    ] {
        let (store, main) = fixture(jdk17);
        let store = &store;
        let mut resolve =
            |_: ClassId, name: &str| store.iter().find(|c| &*c.name == name).map(|c| c.id);
        let direct = BacktraceFrame::Entry(entry(main, "probe/Main", "main", "()V", 6));
        let mut expected = vec![timed_row.clone()];
        expected.extend(wait.clone());
        let got = timed_join_standin_frames(store, &direct, true, &mut resolve);
        assert_eq!(rows(got), Some(expected), "jdk17 {jdk17} timed");
        // `join(long, int)` hands the rounded millis to `join(long)`.
        let nanos = BacktraceFrame::Entry(entry(main, "probe/Main", "main", "()V", 12));
        let mut expected = vec![join(1950, 2), timed_row];
        expected.extend(wait.clone());
        let got = timed_join_standin_frames(store, &nanos, true, &mut resolve);
        assert_eq!(rows(got), Some(expected), "jdk17 {jdk17} join(long, int)");
        let mut expected = vec![zero_row];
        expected.extend(wait);
        let got = timed_join_standin_frames(store, &direct, false, &mut resolve);
        assert_eq!(rows(got), Some(expected), "jdk17 {jdk17} join(0)");
        // The untimed `join()` call is the census's, not the hint's.
        let untimed = BacktraceFrame::Entry(entry(main, "probe/Main", "main", "()V", 1));
        assert!(timed_join_standin_frames(store, &untimed, true, &mut resolve).is_none());
        // Without the hint, a direct `join(long)` still gets nothing.
        assert!(native_standin_frames(store, &direct, INTERRUPTED, &mut resolve).is_none());
    }
}

/// The `join()` -> `join(0L)` hop picks the `wait(0)` loop by its
/// `lconst_0` argument: JDK 25 as before (it is also the last call), and
/// JDK 17, whose last call is the TIMED `wait(delay)` the old rule picked.
#[test]
fn a_registered_join_takes_the_wait_zero_loop_on_both_shapes() {
    for (jdk17, hop, wait) in [
        (
            false,
            row("java/lang/Thread.join", 1887, 7),
            vec![
                row("java/lang/Object.wait", 389, 7),
                row("java/lang/Object.wait0", LINE_NUMBER_NATIVE, -1),
            ],
        ),
        (
            true,
            row("java/lang/Thread.join", 1301, 2),
            vec![row("java/lang/Object.wait", LINE_NUMBER_NATIVE, -1)],
        ),
    ] {
        let (store, main) = fixture(jdk17);
        let store = &store;
        let mut resolve =
            |_: ClassId, name: &str| store.iter().find(|c| &*c.name == name).map(|c| c.id);
        let untimed = BacktraceFrame::Entry(entry(main, "probe/Main", "main", "()V", 1));
        let mut expected = vec![row("java/lang/Thread.join", 1901, 2), hop];
        expected.extend(wait);
        let got = native_standin_frames(store, &untimed, INTERRUPTED, &mut resolve);
        assert_eq!(rows(got), Some(expected), "jdk17 {jdk17}");
    }
}

/// TR5-2: the memo answers only for its own key -- the redefinition
/// count, the thread's class and the outermost frame's class, name and
/// descriptor -- and remembers a "no frame" answer too.
#[test]
fn the_thread_run_memo_answers_only_its_own_key() {
    let (thread, task, other) = (ClassId::new(7), ClassId::new(8), ClassId::new(9));
    let run = BacktraceFrame::Entry(entry(task, "p/Task", "run", "()V", 0));
    let thread_run =
        BacktraceFrame::Entry(entry(ClassId::new(3), "java/lang/Thread", "run", "()V", 4));
    let memo = ThreadRunBottomMemo::new(5, thread, &run, Some(thread_run)).expect("memoisable");
    let bci = |hit: Option<Option<&BacktraceFrame>>| {
        hit.map(|answer| answer.map(|f| f.to_entry().byte_code_index))
    };
    assert_eq!(bci(memo.lookup(5, thread, &run)), Some(Some(4)));
    assert_eq!(bci(memo.lookup(6, thread, &run)), None);
    assert_eq!(bci(memo.lookup(5, other, &run)), None);
    for miss in [
        entry(other, "p/Task", "run", "()V", 0),
        entry(task, "p/Task", "call", "()V", 0),
        entry(task, "p/Task", "run", "(I)V", 0),
    ] {
        assert_eq!(
            bci(memo.lookup(5, thread, &BacktraceFrame::Entry(miss))),
            None
        );
    }
    let none = ThreadRunBottomMemo::new(5, thread, &run, None).expect("memoisable");
    assert!(matches!(none.lookup(5, thread, &run), Some(None)));
    // Round 15 wave 3 (lane trace3): one copy unless told otherwise.
    assert_eq!(memo.copies(), 1);
    let nested = ThreadRunBottomMemo::new(5, thread, &run, None).map(|m| m.with_copies(2));
    assert_eq!(nested.map(|m| m.copies()), Some(2));
    let zero = ThreadRunBottomMemo::new(5, thread, &run, None).map(|m| m.with_copies(0));
    assert_eq!(zero.map(|m| m.copies()), Some(1));
    // A frame without a class id is never memoised.
    let anonymous = BacktraceFrame::Inlined {
        label: Arc::from("p/Task.run:()V"),
        class_id: 0,
        bci: 0,
        cp_stamp: None,
        hidden: None,
    };
    assert!(ThreadRunBottomMemo::new(5, thread, &anonymous, None).is_none());
    assert!(none.lookup(5, thread, &anonymous).is_none());
}
