use super::r13_trace3_standin_tests::{add_class, java, native, pool};
use super::*;

const INTERRUPTED: &str = "java/lang/InterruptedException";

struct Fixture {
    store: ClassStore,
    main: ClassId,
}

/// JDK-25-shaped `Object.wait(long)` (its platform `wait0` at pc 7, line
/// 389) and `Thread.join()` / `join(long)`: `join()` is `join(0L)` (pc 2,
/// line 1901); `join(long)` calls `java/lang/Thread.wait(J)V` -- javac's
/// qualification -- at pc 2 (the timed loop, line 1881) and pc 7 (the
/// `wait(0)` loop, line 1887). `probe/Main.main()V` calls `join()` at bci
/// 1 and `join(long)` at bci 6.
fn fixture() -> Fixture {
    let mut store = ClassStore::new();
    let object = add_class(
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
    );
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
            java(
                "join",
                "(J)V",
                vec![0x2a, 0x1f, 0xb6, 0, 12, 0x2a, 0x09, 0xb6, 0, 12, 0xb1],
                &[(0, 1881), (5, 1887)],
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
        ]),
        vec![java(
            "main",
            "()V",
            vec![0x2a, 0xb6, 0, 6, 0x2a, 0x09, 0xb6, 0, 12, 0xb1],
            &[(0, 3), (4, 4)],
        )],
    );
    Fixture { store, main }
}

fn entry(class_id: ClassId, name: &str, descriptor: Option<&str>, bci: i32) -> StackTraceEntry {
    StackTraceEntry {
        class_name: Arc::from("probe/Main"),
        method_name: Arc::from(name),
        method_descriptor: descriptor.map(Arc::from),
        source_file: None,
        line_number: LINE_NUMBER_UNKNOWN,
        byte_code_index: bci,
        class_id: Some(class_id),
        method_index: None,
    }
}

fn rows(entries: &[StackTraceEntry]) -> Vec<(String, i32, i32)> {
    entries
        .iter()
        .map(|e| {
            (
                format!("{}.{}", e.class_name, e.method_name),
                e.line_number,
                e.byte_code_index,
            )
        })
        .collect()
}

fn joined() -> Vec<(String, i32, i32)> {
    vec![
        ("java/lang/Thread.join".to_string(), 1901, 2),
        ("java/lang/Thread.join".to_string(), 1887, 7),
        ("java/lang/Object.wait".to_string(), 389, 7),
        ("java/lang/Object.wait0".to_string(), LINE_NUMBER_NATIVE, -1),
    ]
}

/// Item 1 / T4-2: an interrupted registered `join()` gets HotSpot's four
/// JDK frames, through the `Thread`-qualified `wait(J)V` call; a direct
/// timed `join(long)` is not an entry and gets none.
#[test]
fn a_registered_join_gets_the_wait_chain_through_owner_resolution() {
    let fx = fixture();
    let store = &fx.store;
    let mut resolve =
        |_: ClassId, name: &str| store.iter().find(|c| &*c.name == name).map(|c| c.id);
    let untimed = BacktraceFrame::Entry(entry(fx.main, "main", Some("()V"), 1));
    let got = native_standin_frames(store, &untimed, INTERRUPTED, &mut resolve).map(|v| {
        let entries: Vec<StackTraceEntry> = v.iter().map(BacktraceFrame::to_entry).collect();
        rows(&entries)
    });
    assert_eq!(got, Some(joined()));
    let timed = BacktraceFrame::Entry(entry(fx.main, "main", Some("()V"), 6));
    assert!(native_standin_frames(store, &timed, INTERRUPTED, &mut resolve).is_none());
    // `join` raises no `IllegalMonitorStateException` from inside.
    let monitor = "java/lang/IllegalMonitorStateException";
    assert!(native_standin_frames(store, &untimed, monitor, &mut resolve).is_none());
}

/// T4-3: a descriptor-less published entry of a thread parked in the
/// registered `join()` gets the same frames; a timed join, or an entry
/// not standing at a call, gets none.
#[test]
fn a_parked_threads_published_stack_gets_the_leaf_frames() {
    let fx = fixture();
    let store = &fx.store;
    let mut resolve =
        |_: ClassId, name: &str| store.iter().find(|c| &*c.name == name).map(|c| c.id);
    let got =
        parked_thread_standin_frames(store, &entry(fx.main, "main", None, 1), &mut resolve);
    assert_eq!(got.map(|v| rows(&v)), Some(joined()));
    for bci in [6, 0, -1] {
        let parked = entry(fx.main, "main", None, bci);
        let got = parked_thread_standin_frames(store, &parked, &mut resolve);
        assert!(got.is_none(), "bci {bci}");
    }
    // A descriptor that names no method of that name answers nothing.
    let wrong = entry(fx.main, "main", Some("(I)V"), 1);
    assert!(parked_thread_standin_frames(store, &wrong, &mut resolve).is_none());
}

/// A parked joiner's published stack (`main` -> `join()` -> `join(long)`,
/// no descriptors, `join` overloaded) plus the appended `wait` leaf: each
/// overloaded entry gets the line of the call it stands at.
#[test]
fn published_overloads_get_their_lines_from_the_callee() {
    let fx = fixture();
    let id = |name: &str| fx.store.iter().find(|c| &*c.name == name).map(|c| c.id);
    let (thread, object) = (
        id("java/lang/Thread").unwrap(),
        id("java/lang/Object").unwrap(),
    );
    let mut trace = vec![
        entry(fx.main, "main", None, 1),
        entry(thread, "join", None, 2),
        entry(thread, "join", None, 7),
        entry(object, "wait", Some("(J)V"), 7),
    ];
    let resolved = resolve_published_overloads_by_callee(&fx.store, &mut trace);
    assert_eq!(resolved, 3);
    let lines: Vec<i32> = trace.iter().map(|e| e.line_number).collect();
    assert_eq!(lines, vec![3, 1901, 1887, LINE_NUMBER_UNKNOWN]);
    assert_eq!(trace[1].method_descriptor.as_deref(), Some("()V"));
    assert_eq!(trace[2].method_descriptor.as_deref(), Some("(J)V"));
    // A callee the entry does not call at its bci resolves nothing.
    let mut wrong = vec![
        entry(thread, "join", None, 2),
        entry(object, "sleep", None, 0),
    ];
    assert_eq!(
        resolve_published_overloads_by_callee(&fx.store, &mut wrong),
        0
    );
    assert_eq!(wrong[0].line_number, LINE_NUMBER_UNKNOWN);
}

fn frame(class_id: ClassId, name: &str, descriptor: &str) -> BacktraceFrame {
    BacktraceFrame::Entry(entry(class_id, name, Some(descriptor), 0))
}

/// The lambda screen keeps every implementation-method shape and refuses
/// the frames a started thread's task cannot have run from.
#[test]
fn the_lambda_screen_refuses_what_cannot_be_an_implementation_method() {
    let id = ClassId::new(7);
    for (name, descriptor) in [
        ("lambda$main$0", "()V"),
        ("lambda$main$1", "(ILjava/lang/String;)V"),
        ("main$lambda$0", "(Ljava/lang/Object;)V"),
        ("$anonfun$main$1", "(I)V"),
        ("serve", "()V"),
        ("size", "()I"),
        ("<init>", "()V"),
    ] {
        assert!(
            may_be_thread_run_lambda_frame(&frame(id, name, descriptor)),
            "{name}"
        );
    }
    for (name, descriptor) in [
        ("main", "([Ljava/lang/String;)V"),
        ("<clinit>", "()V"),
        ("loadClass", "(Ljava/lang/String;)Ljava/lang/Class;"),
        (
            "checkAndLoadMain",
            "(ZILjava/lang/String;)Ljava/lang/Class;",
        ),
    ] {
        assert!(
            !may_be_thread_run_lambda_frame(&frame(id, name, descriptor)),
            "{name}"
        );
    }
}

/// A `run()V` of the thread's own class is refused without the lock; a
/// task's `run`, or another method of the thread's class, is not.
#[test]
fn the_threads_own_run_is_refused_before_the_lock() {
    let own = ClassId::new(7);
    let task = ClassId::new(8);
    assert!(outermost_is_threads_own_run(&frame(own, "run", "()V"), own));
    assert!(!outermost_is_threads_own_run(
        &frame(task, "run", "()V"),
        own
    ));
    assert!(!outermost_is_threads_own_run(
        &frame(own, "lambda$new$0", "()V"),
        own
    ));
    assert!(!outermost_is_threads_own_run(
        &frame(own, "run", "(I)V"),
        own
    ));
}

#[test]
fn virtual_thread_classes_are_recognised_by_their_chain() {
    let mut store = ClassStore::new();
    let object = add_class(&mut store, "java/lang/Object", None, pool(&[]), vec![]);
    let thread = add_class(
        &mut store,
        "java/lang/Thread",
        Some(object),
        pool(&[]),
        vec![],
    );
    let base = add_class(
        &mut store,
        "java/lang/BaseVirtualThread",
        Some(thread),
        pool(&[]),
        vec![],
    );
    let vt = add_class(
        &mut store,
        "java/lang/VirtualThread",
        Some(base),
        pool(&[]),
        vec![],
    );
    let bound = add_class(
        &mut store,
        "java/lang/ThreadBuilders$BoundVirtualThread",
        Some(base),
        pool(&[]),
        vec![],
    );
    assert!(is_virtual_thread_class(&store, vt));
    assert!(is_virtual_thread_class(&store, bound));
    assert!(!is_virtual_thread_class(&store, thread));
    assert!(!is_virtual_thread_class(&store, object));
}
