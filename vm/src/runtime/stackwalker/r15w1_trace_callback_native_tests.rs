use super::r13_trace3_standin_tests::{add_class, java, native, pool};
use super::*;

const FOR_NAME_1: &str = "(Ljava/lang/String;)Ljava/lang/Class;";
const FOR_NAME_ADAPTER: &str = "(Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/Class;";
const FOR_NAME_3: &str = "(Ljava/lang/String;ZLjava/lang/ClassLoader;)Ljava/lang/Class;";
const FOR_NAME_0: &str =
    "(Ljava/lang/String;ZLjava/lang/ClassLoader;Ljava/lang/Class;)Ljava/lang/Class;";

/// JDK-25-shaped `java.lang.Class`: `forName(String)` calls the private
/// adapter `forName(String, Class)` at pc 2 (line 543; twice when
/// `two_calls`), which calls `forName0` at pc 4 (line 562);
/// `forName(String, boolean, ClassLoader)` calls `forName0` at pc 4 (line
/// 600); `forName0` is native. `probe/Main.main()V` calls, at bci:
/// 0 `forName(String)`, 3 `forName0`, 6 `forName(String, boolean,
/// ClassLoader)`, 9 `helper()`.
fn fixture(two_calls: bool) -> (ClassStore, ClassId) {
    let mut store = ClassStore::new();
    let object = add_class(&mut store, "java/lang/Object", None, pool(&[]), vec![]);
    let for_name_1 = if two_calls {
        // aload_0 aconst_null invokestatic(adapter) pop aload_0
        // aconst_null invokestatic(adapter) areturn
        vec![0x2a, 0x01, 0xb8, 0, 6, 0x57, 0x2a, 0x01, 0xb8, 0, 6, 0xb0]
    } else {
        // aload_0 aconst_null invokestatic(adapter) areturn
        vec![0x2a, 0x01, 0xb8, 0, 6, 0xb0]
    };
    add_class(
        &mut store,
        "java/lang/Class",
        Some(object),
        pool(&[
            ("java/lang/Class", "forName", FOR_NAME_ADAPTER),
            ("java/lang/Class", "forName0", FOR_NAME_0),
        ]),
        vec![
            java("forName", FOR_NAME_1, for_name_1, &[(0, 543)]),
            // aload_0 iconst_1 aconst_null aload_1 invokestatic(forName0) areturn
            java(
                "forName",
                FOR_NAME_ADAPTER,
                vec![0x2a, 0x04, 0x01, 0x2b, 0xb8, 0, 12, 0xb0],
                &[(0, 560), (4, 562)],
            ),
            // aload_0 iload_1 aload_2 aconst_null invokestatic(forName0) areturn
            java(
                "forName",
                FOR_NAME_3,
                vec![0x2a, 0x1b, 0x2c, 0x01, 0xb8, 0, 12, 0xb0],
                &[(0, 600)],
            ),
            native("forName0", FOR_NAME_0),
        ],
    );
    let main = add_class(
        &mut store,
        "probe/Main",
        Some(object),
        pool(&[
            ("java/lang/Class", "forName", FOR_NAME_1),
            ("java/lang/Class", "forName0", FOR_NAME_0),
            ("java/lang/Class", "forName", FOR_NAME_3),
            ("probe/Main", "helper", "()V"),
        ]),
        vec![
            java(
                "main",
                "()V",
                vec![0xb8, 0, 6, 0xb8, 0, 12, 0xb8, 0, 18, 0xb8, 0, 24, 0xb1],
                &[(0, 3), (3, 4), (6, 5), (9, 6)],
            ),
            java("helper", "()V", vec![0xb1], &[(0, 20)]),
        ],
    );
    (store, main)
}

fn at(main: ClassId, bci: i32) -> BacktraceFrame {
    BacktraceFrame::Entry(StackTraceEntry {
        class_name: Arc::from("probe/Main"),
        method_name: Arc::from("main"),
        method_descriptor: Some(Arc::from("()V")),
        source_file: None,
        line_number: LINE_NUMBER_UNKNOWN,
        byte_code_index: bci,
        class_id: Some(main),
        method_index: None,
    })
}

fn callee(class_name: &str, method_name: &str) -> BacktraceFrame {
    BacktraceFrame::Entry(StackTraceEntry {
        class_name: Arc::from(class_name),
        method_name: Arc::from(method_name),
        method_descriptor: None,
        source_file: None,
        line_number: LINE_NUMBER_UNKNOWN,
        byte_code_index: 0,
        class_id: None,
        method_index: None,
    })
}

fn rows(frames: Option<Vec<BacktraceFrame>>) -> Option<Vec<(String, i32, i32)>> {
    frames.map(|v| {
        v.iter()
            .map(|f| {
                let e = f.to_entry();
                (
                    format!("{}.{}", e.class_name, e.method_name),
                    e.line_number,
                    e.byte_code_index,
                )
            })
            .collect()
    })
}

fn row(frame: &str, line: i32, bci: i32) -> (String, i32, i32) {
    (frame.to_string(), line, bci)
}

fn native_leaf() -> (String, i32, i32) {
    row("java/lang/Class.forName0", LINE_NUMBER_NATIVE, -1)
}

/// TR6-1: a loader `loadClass` (or a `<clinit>`) above a caller standing
/// at a census call gets `forName0 (Native Method)` below it, and a served
/// Java `forName` its own frames, each at its call of the next.
#[test]
fn a_callback_native_gets_its_frames_in_the_middle() {
    let (store, main) = fixture(false);
    let mut resolve =
        |_: ClassId, name: &str| store.iter().find(|c| &*c.name == name).map(|c| c.id);
    let load = callee("java/lang/ClassLoader", "loadClass");
    let clinit = callee("probe/Boom", "<clinit>");
    let served_one_arg = Some(vec![
        row("java/lang/Class.forName", 543, 2),
        row("java/lang/Class.forName", 562, 4),
        native_leaf(),
    ]);
    assert_eq!(
        rows(callback_native_middle_frames(
            &store,
            &at(main, 0),
            &load,
            &mut resolve
        )),
        served_one_arg
    );
    assert_eq!(
        rows(callback_native_middle_frames(
            &store,
            &at(main, 0),
            &clinit,
            &mut resolve
        )),
        served_one_arg
    );
    assert_eq!(
        rows(callback_native_middle_frames(
            &store,
            &at(main, 3),
            &load,
            &mut resolve
        )),
        Some(vec![native_leaf()])
    );
    assert_eq!(
        rows(callback_native_middle_frames(
            &store,
            &at(main, 6),
            &load,
            &mut resolve
        )),
        Some(vec![row("java/lang/Class.forName", 600, 4), native_leaf()])
    );
}

/// Not a census call, not a callee the native runs, or a hop with two
/// calls of the next: nothing (fail-closed).
#[test]
fn other_pairs_and_ambiguous_hops_get_nothing() {
    let (store, main) = fixture(false);
    let mut resolve =
        |_: ClassId, name: &str| store.iter().find(|c| &*c.name == name).map(|c| c.id);
    let load = callee("java/lang/ClassLoader", "loadClass");
    assert!(callback_native_middle_frames(&store, &at(main, 9), &load, &mut resolve).is_none());
    assert!(
        callback_native_middle_frames(&store, &at(main, 12), &load, &mut resolve).is_none()
    );
    let run = callee("probe/Task", "run");
    assert!(!may_need_callback_native_middle_frames(&at(main, 0), &run));
    assert!(callback_native_middle_frames(&store, &at(main, 0), &run, &mut resolve).is_none());
    let (store, main) = fixture(true);
    let mut resolve =
        |_: ClassId, name: &str| store.iter().find(|c| &*c.name == name).map(|c| c.id);
    assert!(callback_native_middle_frames(&store, &at(main, 0), &load, &mut resolve).is_none());
    // The native itself has no hop to be ambiguous about.
    assert_eq!(
        rows(callback_native_middle_frames(
            &store,
            &at(main, 3),
            &load,
            &mut resolve
        )),
        Some(vec![native_leaf()])
    );
}

/// The innermost form: every throwable the served call raises but
/// `StackOverflowError`.
#[test]
fn a_served_for_name_gets_its_frames_on_top() {
    let (store, main) = fixture(false);
    let mut resolve =
        |_: ClassId, name: &str| store.iter().find(|c| &*c.name == name).map(|c| c.id);
    for throwable in [
        "java/lang/ClassNotFoundException",
        "java/lang/NullPointerException",
        "java/lang/ExceptionInInitializerError",
        "java/lang/NoClassDefFoundError",
    ] {
        assert_eq!(
            rows(callback_native_leaf_frames(
                &store,
                &at(main, 0),
                throwable,
                &mut resolve
            )),
            Some(vec![
                row("java/lang/Class.forName", 543, 2),
                row("java/lang/Class.forName", 562, 4),
                native_leaf(),
            ]),
            "{throwable}"
        );
        assert_eq!(
            rows(callback_native_leaf_frames(
                &store,
                &at(main, 3),
                throwable,
                &mut resolve
            )),
            Some(vec![native_leaf()]),
            "{throwable}"
        );
    }
    let overflow = "java/lang/StackOverflowError";
    assert!(
        callback_native_leaf_frames(&store, &at(main, 0), overflow, &mut resolve).is_none()
    );
    let cnfe = "java/lang/ClassNotFoundException";
    assert!(callback_native_leaf_frames(&store, &at(main, 9), cnfe, &mut resolve).is_none());
}
