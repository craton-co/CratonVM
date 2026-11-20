use super::r13_trace3_standin_tests::{add_class, java, pool};
use super::*;

const FIELD_DESCRIPTOR: &str = "(Ljava/lang/String;)Ljava/lang/reflect/Field;";
const CTOR_DESCRIPTOR: &str = "([Ljava/lang/Class;)Ljava/lang/reflect/Constructor;";

/// A `java/lang/Class` whose `getField` raises `NoSuchFieldException` at
/// pc 5 (line 2068), `getDeclaredField` at two sites, `getConstructor`
/// calls `getConstructor0` at pc 3 (line 2199) and `getConstructor0`
/// raises `NoSuchMethodException` at pc 5 (line 3187); and a
/// `probe/Main.main()V` calling `getField` at bci 1, `getConstructor` at
/// bci 6 and `getDeclaredField` at bci 11.
fn fixture() -> (ClassStore, ClassId) {
    let mut store = ClassStore::new();
    let object = add_class(&mut store, "java/lang/Object", None, pool(&[]), Vec::new());
    // Methodref k at 6k + 6, its class at 6k + 2.
    let class_pool = pool(&[
        (NO_SUCH_FIELD_EXCEPTION, "<init>", "(Ljava/lang/String;)V"),
        (NO_SUCH_METHOD_EXCEPTION, "<init>", "(Ljava/lang/String;)V"),
        GET_CONSTRUCTOR0,
    ]);
    let raise_field = vec![0xbb, 0, 2, 0x59, 0x01, 0xb7, 0, 6, 0xbf];
    let mut twice = raise_field.clone();
    twice.extend_from_slice(&raise_field);
    add_class(
        &mut store,
        MEMBER_LOOKUP_CLASS,
        Some(object),
        class_pool,
        vec![
            java("getField", FIELD_DESCRIPTOR, raise_field, &[(0, 2068)]),
            java("getDeclaredField", FIELD_DESCRIPTOR, twice, &[(0, 2382)]),
            java(
                "getConstructor",
                CTOR_DESCRIPTOR,
                vec![0x2a, 0x2b, 0x03, 0xb6, 0, 18, 0xb0],
                &[(0, 2199)],
            ),
            java(
                GET_CONSTRUCTOR0.1,
                GET_CONSTRUCTOR0.2,
                vec![0xbb, 0, 8, 0x59, 0x01, 0xb7, 0, 12, 0xbf],
                &[(0, 3187)],
            ),
        ],
    );
    let main = add_class(
        &mut store,
        "probe/Main",
        Some(object),
        pool(&[
            (MEMBER_LOOKUP_CLASS, "getField", FIELD_DESCRIPTOR),
            (MEMBER_LOOKUP_CLASS, "getConstructor", CTOR_DESCRIPTOR),
            (MEMBER_LOOKUP_CLASS, "getDeclaredField", FIELD_DESCRIPTOR),
        ]),
        vec![java(
            "main",
            "()V",
            vec![
                0x01, 0xb6, 0, 6, 0x57, 0x01, 0xb6, 0, 12, 0x57, 0x01, 0xb6, 0, 18, 0x57, 0xb1,
            ],
            &[(0, 3)],
        )],
    );
    (store, main)
}

fn expand(store: &ClassStore, main: ClassId, bci: i32, throwable: &str) -> Option<Vec<String>> {
    let at = BacktraceFrame::Entry(StackTraceEntry {
        class_name: Arc::from("probe/Main"),
        method_name: Arc::from("main"),
        method_descriptor: Some(Arc::from("()V")),
        source_file: None,
        line_number: LINE_NUMBER_UNKNOWN,
        byte_code_index: bci,
        class_id: Some(main),
        method_index: None,
    });
    let mut resolve =
        |_: ClassId, name: &str| store.iter().find(|c| &*c.name == name).map(|c| c.id);
    member_lookup_standin_frames(store, &at, throwable, &mut resolve).map(|frames| {
        frames
            .iter()
            .map(|f| {
                let e = f.to_entry();
                format!("{}:{}@{}", e.method_name, e.line_number, e.byte_code_index)
            })
            .collect()
    })
}


