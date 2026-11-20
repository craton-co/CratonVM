use super::r13_trace3_standin_tests::{add_class, java, native, pool};
use super::*;

const ICCE: &str = "java/lang/IncompatibleClassChangeError";
const NCDFE: &str = "java/lang/NoClassDefFoundError";
const NSME: &str = "java/lang/NoSuchMethodError";

/// A JDK-25-shaped `java/lang/Class` whose `probe()V` calls, at bci 1 / 5
/// / 9, `this.getDeclaringClass0()`, `this.getDeclaredClasses0()` and
/// `this.getSimpleBinaryName0()` (private natives, as in JDK 25), and the
/// `LinkageError` family.
fn fixture() -> (ClassStore, ClassId) {
    let mut store = ClassStore::new();
    let object = add_class(&mut store, "java/lang/Object", None, pool(&[]), vec![]);
    let class = add_class(
        &mut store,
        "java/lang/Class",
        Some(object),
        pool(&[
            ("java/lang/Class", "getDeclaringClass0", "()Ljava/lang/Class;"),
            ("java/lang/Class", "getDeclaredClasses0", "()[Ljava/lang/Class;"),
            ("java/lang/Class", "getSimpleBinaryName0", "()Ljava/lang/String;"),
        ]),
        vec![
            java(
                "probe",
                "()V",
                vec![0x2a, 0xb6, 0, 6, 0x2a, 0xb6, 0, 12, 0x2a, 0xb6, 0, 18, 0xb1],
                &[(0, 1)],
            ),
            native("getDeclaringClass0", "()Ljava/lang/Class;"),
            native("getDeclaredClasses0", "()[Ljava/lang/Class;"),
            native("getSimpleBinaryName0", "()Ljava/lang/String;"),
        ],
    );
    let throwable = add_class(
        &mut store,
        "java/lang/Throwable",
        Some(object),
        pool(&[]),
        vec![],
    );
    let error = add_class(&mut store, "java/lang/Error", Some(throwable), pool(&[]), vec![]);
    let linkage = add_class(
        &mut store,
        "java/lang/LinkageError",
        Some(error),
        pool(&[]),
        vec![],
    );
    let icce = add_class(&mut store, ICCE, Some(linkage), pool(&[]), vec![]);
    add_class(&mut store, NCDFE, Some(linkage), pool(&[]), vec![]);
    add_class(&mut store, NSME, Some(icce), pool(&[]), vec![]);
    (store, class)
}

/// The `(Native Method)` frame listed on top of a `throwable` captured
/// with `probe` standing at `bci`, as `Class.method`.
fn leaf(store: &ClassStore, class: ClassId, bci: i32, throwable: &str) -> Option<String> {
    let tclass = store
        .iter()
        .find(|c| &*c.name == throwable)
        .expect("throwable class");
    let at = BacktraceFrame::Entry(StackTraceEntry {
        class_name: Arc::from("java/lang/Class"),
        method_name: Arc::from("probe"),
        method_descriptor: Some(Arc::from("()V")),
        source_file: None,
        line_number: LINE_NUMBER_UNKNOWN,
        byte_code_index: bci,
        class_id: Some(class),
        method_index: None,
    });
    let mut resolve =
        |_: ClassId, name: &str| store.iter().find(|c| &*c.name == name).map(|c| c.id);
    native_leaf_frame(store, &at, tclass, &mut resolve).map(|frame| {
        let e = frame.to_entry();
        assert_eq!(e.line_number, LINE_NUMBER_NATIVE);
        format!("{}.{}", e.class_name, e.method_name)
    })
}

/// HotSpot 25: `check_for_inner_class`'s ICCE and `klass_at`'s NCDFE are
/// raised inside `getDeclaringClass0` / `getDeclaredClasses0`, whose
/// `(Native Method)` frame tops the trace (`J27Mh2Rows`
/// `mh717-2-simple-name`).
#[test]
fn nesting_natives_list_their_frame_for_their_linkage_errors() {
    let (store, class) = fixture();
    for throwable in [ICCE, NCDFE] {
        assert_eq!(
            leaf(&store, class, 1, throwable).as_deref(),
            Some("java/lang/Class.getDeclaringClass0"),
            "{throwable}"
        );
        assert_eq!(
            leaf(&store, class, 5, throwable).as_deref(),
            Some("java/lang/Class.getDeclaredClasses0"),
            "{throwable}"
        );
    }
}

/// An ICCE subclass neither native raises stays the invoke's; and
/// `getSimpleBinaryName0` (`find_inner_classes_attr`, no check) is not a
/// nesting native.
#[test]
fn other_linkage_errors_and_natives_keep_the_caller_on_top() {
    let (store, class) = fixture();
    assert_eq!(leaf(&store, class, 1, NSME), None);
    assert_eq!(leaf(&store, class, 9, ICCE), None);
    assert_eq!(leaf(&store, class, 9, NCDFE), None);
}
