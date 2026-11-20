use super::r13_trace3_standin_tests::{add_class, java, pool};
use super::*;
use cratonvm_native_api::MhWmteRaise;

const WMTE: &str = "java/lang/invoke/WrongMethodTypeException";
const INVOKERS: &str = "java/lang/invoke/Invokers";
const HANDLE: &str = "java/lang/invoke/MethodHandle";
const MT_ARG: &str = "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;";
const NEW_WMTE: &str = "(Ljava/lang/invoke/MethodType;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/WrongMethodTypeException;";
const CHECK_EXACT: &str = "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodType;)V";
const CHECK_GENERIC: &str = "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;";
const IWA_ARRAY: &str = "([Ljava/lang/Object;)Ljava/lang/Object;";
const IWA_LIST: &str = "(Ljava/util/List;)Ljava/lang/Object;";

type Row = (String, i32, i32);

fn rows(frames: Option<Vec<BacktraceFrame>>) -> Option<Vec<Row>> {
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

fn row(frame: &str, line: i32, bci: i32) -> Row {
    (frame.to_string(), line, bci)
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

/// `Invokers` (unless `without_invokers`) and `MethodHandle` with the
/// bodies' call / raise shapes, and a `probe/Main.main()V` calling, at
/// bci 0 / 3 / 6 / 9 / 12 / 15: `MethodHandle.invoke`,
/// `invokeWithArguments(Object[])`, `invokeWithArguments(List)`,
/// `asType`, `invokeExact` and `probe/Other.invoke`.
fn fixture(without_invokers: bool) -> (ClassStore, ClassId) {
    let mut store = ClassStore::new();
    let object = add_class(&mut store, "java/lang/Object", None, pool(&[]), vec![]);
    if !without_invokers {
        add_class(
            &mut store,
            INVOKERS,
            Some(object),
            pool(&[
                (INVOKERS, "newWrongMethodTypeException", NEW_WMTE),
                (WMTE, "<init>", "(Ljava/lang/String;)V"),
                (HANDLE, "asType", MT_ARG),
            ]),
            vec![
                // aconst_null aconst_null invokestatic(#6) athrow
                java(
                    "checkExactType",
                    CHECK_EXACT,
                    vec![0x01, 0x01, 0xb8, 0, 6, 0xbf],
                    &[(0, 531)],
                ),
                // new(#8) dup aconst_null invokespecial(#12) areturn
                java(
                    "newWrongMethodTypeException",
                    NEW_WMTE,
                    vec![0xbb, 0, 8, 0x59, 0x01, 0xb7, 0, 12, 0xb0],
                    &[(0, 522)],
                ),
                // aload_0 aload_1 invokevirtual(#18) areturn
                java(
                    "checkGenericType",
                    CHECK_GENERIC,
                    vec![0x2a, 0x2b, 0xb6, 0, 18, 0xb0],
                    &[(0, 541)],
                ),
            ],
        );
    }
    add_class(
        &mut store,
        HANDLE,
        Some(object),
        pool(&[
            (HANDLE, "setAsTypeCache", MT_ARG),
            (HANDLE, "asTypeUncached", MT_ARG),
            (WMTE, "<init>", "(Ljava/lang/String;)V"),
            (HANDLE, "asType", MT_ARG),
            (HANDLE, "invokeWithArguments", IWA_ARRAY),
        ]),
        vec![
            // aload_0 aload_1 invokevirtual(#6) areturn
            java(
                "asType",
                MT_ARG,
                vec![0x2a, 0x2b, 0xb6, 0, 6, 0xb0],
                &[(0, 873)],
            ),
            // aload_0 aload_1 invokevirtual(#12) areturn
            java(
                "setAsTypeCache",
                MT_ARG,
                vec![0x2a, 0x2b, 0xb6, 0, 12, 0xb0],
                &[(0, 900)],
            ),
            // new(#14) dup aconst_null invokespecial(#18) athrow
            java(
                "asTypeUncached",
                MT_ARG,
                vec![0xbb, 0, 14, 0x59, 0x01, 0xb7, 0, 18, 0xbf],
                &[(0, 915)],
            ),
            // aload_0 aconst_null invokevirtual(#24) areturn
            java(
                "invokeWithArguments",
                IWA_ARRAY,
                vec![0x2a, 0x01, 0xb6, 0, 24, 0xb0],
                &[(0, 735)],
            ),
            // aload_0 aconst_null invokevirtual(#30) areturn
            java(
                "invokeWithArguments",
                IWA_LIST,
                vec![0x2a, 0x01, 0xb6, 0, 30, 0xb0],
                &[(0, 760)],
            ),
        ],
    );
    let main = add_class(
        &mut store,
        "probe/Main",
        Some(object),
        pool(&[
            (HANDLE, "invoke", "(Ljava/lang/String;)I"),
            (HANDLE, "invokeWithArguments", IWA_ARRAY),
            (HANDLE, "invokeWithArguments", IWA_LIST),
            (HANDLE, "asType", MT_ARG),
            (HANDLE, "invokeExact", "(I)V"),
            ("probe/Other", "invoke", "()V"),
        ]),
        vec![java(
            "main",
            "()V",
            vec![
                0xb6, 0, 6, 0xb6, 0, 12, 0xb6, 0, 18, 0xb6, 0, 24, 0xb6, 0, 30, 0xb6, 0, 36,
                0xb1,
            ],
            &[(0, 10)],
        )],
    );
    (store, main)
}

fn as_type_core() -> Vec<Row> {
    vec![
        row("java/lang/invoke/MethodHandle.asType", 873, 2),
        row("java/lang/invoke/MethodHandle.setAsTypeCache", 900, 2),
        row("java/lang/invoke/MethodHandle.asTypeUncached", 915, 5),
    ]
}

/// `invokeExact`: `checkExactType` at its call of
/// `newWrongMethodTypeException`, which is at its constructor call.
#[test]
fn an_exact_refusal_lists_check_exact_type() {
    let (store, main) = fixture(false);
    let innermost = at(main, 12);
    assert_eq!(
        rows(mh_wmte_raise_frames(
            &store,
            MhWmteRaise::ExactType,
            Some(&innermost),
            WMTE
        )),
        Some(vec![
            row("java/lang/invoke/Invokers.checkExactType", 531, 2),
            row(
                "java/lang/invoke/Invokers.newWrongMethodTypeException",
                522,
                5
            ),
        ])
    );
}

/// `asType`'s three frames, and below them what the caller's call names:
/// `checkGenericType` for `invoke` / `invokeExact`, the
/// `invokeWithArguments` frame(s), nothing for `asType` itself, another
/// class's `invoke` or no innermost frame.
#[test]
fn an_as_type_refusal_lists_as_type_and_the_callers_door() {
    let (store, main) = fixture(false);
    let frames = |bci: Option<i32>| {
        let innermost = bci.map(|bci| at(main, bci));
        rows(mh_wmte_raise_frames(
            &store,
            MhWmteRaise::AsType,
            innermost.as_ref(),
            WMTE,
        ))
    };
    let generic = row("java/lang/invoke/Invokers.checkGenericType", 541, 2);
    let array = row("java/lang/invoke/MethodHandle.invokeWithArguments", 735, 2);
    let list = row("java/lang/invoke/MethodHandle.invokeWithArguments", 760, 2);
    let with = |below: Vec<Row>| {
        let mut out = below;
        out.extend(as_type_core());
        Some(out)
    };
    assert_eq!(frames(Some(0)), with(vec![generic.clone()]));
    assert_eq!(frames(Some(3)), with(vec![array.clone()]));
    assert_eq!(frames(Some(6)), with(vec![list, array]));
    assert_eq!(frames(Some(9)), with(vec![]));
    assert_eq!(frames(Some(12)), with(vec![generic]));
    assert_eq!(frames(Some(15)), with(vec![]));
    assert_eq!(frames(None), with(vec![]));
}

/// Another throwable gets nothing; without `Invokers` the exact chain is
/// not named at all and the `asType` chain keeps its core.
#[test]
fn other_throwables_and_missing_classes_fail_closed() {
    let (store, main) = fixture(false);
    let innermost = at(main, 0);
    for raise in [MhWmteRaise::ExactType, MhWmteRaise::AsType] {
        let cce = "java/lang/ClassCastException";
        assert!(mh_wmte_raise_frames(&store, raise, Some(&innermost), cce).is_none());
    }
    let (store, main) = fixture(true);
    let innermost = at(main, 0);
    let exact = mh_wmte_raise_frames(&store, MhWmteRaise::ExactType, Some(&innermost), WMTE);
    assert!(exact.is_none());
    assert_eq!(
        rows(mh_wmte_raise_frames(
            &store,
            MhWmteRaise::AsType,
            Some(&innermost),
            WMTE
        )),
        Some(as_type_core())
    );
}
