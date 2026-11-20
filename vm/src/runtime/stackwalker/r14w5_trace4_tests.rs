use super::r13_trace3_standin_tests::{add_class, java, native, pool};
use super::*;
use cratonvm_reader::class_access_flags::MethodAccessFlags;

const INTERRUPTED: &str = "java/lang/InterruptedException";

struct Fixture {
    store: ClassStore,
    main: ClassId,
}

fn with_flags(mut method: ClassFileMethod, flags: MethodAccessFlags) -> ClassFileMethod {
    method.access_flags = flags;
    method
}

/// JDK-25-shaped `Object` (`wait()` -> `wait(long)`, whose `wait0` calls
/// sit at pc 2 -- the virtual-thread branch, line 383 -- and pc 7, line
/// 389; native `clone`, final native `notify`, non-final native
/// `hashCode`), `System.arraycopy`, a signature-polymorphic
/// `MethodHandle.invokeExact`, the throwable classes the screens name, and
/// `probe/Main.main()V` calling, at bci:
/// 0 `System.arraycopy`, 3 `super.clone()`, 6 `this.notify()`,
/// 9 `this.hashCode()`, 12 `RuntimeException.<init>` then `athrow`,
/// 16 `MethodHandle.invokeExact`, 19 `helper()` (Java), 22 `this.wait()`.
fn fixture() -> Fixture {
    let mut store = ClassStore::new();
    let object = add_class(
        &mut store,
        "java/lang/Object",
        None,
        pool(&[
            ("java/lang/Object", "wait0", "(J)V"),
            ("java/lang/Object", "wait", "(J)V"),
        ]),
        vec![
            java(
                "wait",
                "()V",
                vec![0x2a, 0x09, 0xb6, 0, 12, 0xb1],
                &[(0, 351)],
            ),
            java(
                "wait",
                "(J)V",
                vec![0x2a, 0x1f, 0xb7, 0, 6, 0x2a, 0x1f, 0xb7, 0, 6, 0xb1],
                &[(0, 383), (5, 389)],
            ),
            native("wait0", "(J)V"),
            with_flags(
                native("clone", "()Ljava/lang/Object;"),
                MethodAccessFlags::PROTECTED | MethodAccessFlags::NATIVE,
            ),
            with_flags(
                native("notify", "()V"),
                MethodAccessFlags::PUBLIC
                    | MethodAccessFlags::FINAL
                    | MethodAccessFlags::NATIVE,
            ),
            with_flags(
                native("hashCode", "()I"),
                MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            ),
        ],
    );
    add_class(
        &mut store,
        "java/lang/System",
        Some(object),
        pool(&[]),
        vec![with_flags(
            native("arraycopy", "(Ljava/lang/Object;ILjava/lang/Object;II)V"),
            MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC | MethodAccessFlags::NATIVE,
        )],
    );
    add_class(
        &mut store,
        "java/lang/invoke/MethodHandle",
        Some(object),
        pool(&[]),
        vec![with_flags(
            native("invokeExact", "([Ljava/lang/Object;)Ljava/lang/Object;"),
            MethodAccessFlags::PUBLIC | MethodAccessFlags::FINAL | MethodAccessFlags::NATIVE,
        )],
    );
    let throwable = add_class(
        &mut store,
        "java/lang/Throwable",
        Some(object),
        pool(&[]),
        vec![],
    );
    let exception = add_class(
        &mut store,
        "java/lang/Exception",
        Some(throwable),
        pool(&[]),
        vec![],
    );
    let runtime = add_class(
        &mut store,
        "java/lang/RuntimeException",
        Some(exception),
        pool(&[]),
        vec![],
    );
    let error = add_class(
        &mut store,
        "java/lang/Error",
        Some(throwable),
        pool(&[]),
        vec![],
    );
    let linkage = add_class(
        &mut store,
        "java/lang/LinkageError",
        Some(error),
        pool(&[]),
        vec![],
    );
    let icce = add_class(
        &mut store,
        "java/lang/IncompatibleClassChangeError",
        Some(linkage),
        pool(&[]),
        vec![],
    );
    add_class(
        &mut store,
        "java/lang/reflect/Array",
        Some(object),
        pool(&[(
            "java/lang/reflect/Array",
            "newArray",
            "(Ljava/lang/Class;I)Ljava/lang/Object;",
        )]),
        vec![
            // newInstance(c, n) { return newArray(c, n); } -- the call at pc 2.
            java(
                "newInstance",
                "(Ljava/lang/Class;I)Ljava/lang/Object;",
                vec![0x2a, 0x1b, 0xb8, 0, 6, 0xb0],
                &[(0, 76)],
            ),
            with_flags(
                native("newArray", "(Ljava/lang/Class;I)Ljava/lang/Object;"),
                MethodAccessFlags::PRIVATE
                    | MethodAccessFlags::STATIC
                    | MethodAccessFlags::NATIVE,
            ),
        ],
    );
    for (name, superclass) in [
        ("java/lang/NegativeArraySizeException", runtime),
        ("java/lang/IllegalArgumentException", runtime),
        ("java/lang/NoSuchMethodError", icce),
        ("java/lang/UnsatisfiedLinkError", linkage),
        ("java/lang/ExceptionInInitializerError", linkage),
        ("java/lang/ClassFormatError", linkage),
        ("java/lang/StackOverflowError", error),
        ("java/lang/NullPointerException", runtime),
        ("java/lang/ArrayIndexOutOfBoundsException", runtime),
        ("java/lang/IllegalMonitorStateException", runtime),
        ("java/lang/CloneNotSupportedException", exception),
        (INTERRUPTED, exception),
    ] {
        add_class(&mut store, name, Some(superclass), pool(&[]), vec![]);
    }
    let main = add_class(
        &mut store,
        "probe/Main",
        Some(object),
        pool(&[
            (
                "java/lang/System",
                "arraycopy",
                "(Ljava/lang/Object;ILjava/lang/Object;II)V",
            ),
            ("java/lang/Object", "clone", "()Ljava/lang/Object;"),
            ("probe/Main", "notify", "()V"),
            ("probe/Main", "hashCode", "()I"),
            ("java/lang/RuntimeException", "<init>", "()V"),
            (
                "java/lang/invoke/MethodHandle",
                "invokeExact",
                "([Ljava/lang/Object;)Ljava/lang/Object;",
            ),
            ("probe/Main", "helper", "()V"),
            ("probe/Main", "wait", "()V"),
            (
                "java/lang/reflect/Array",
                "newInstance",
                "(Ljava/lang/Class;I)Ljava/lang/Object;",
            ),
        ]),
        vec![
            java(
                "main",
                "()V",
                vec![
                    0xb8, 0, 6, // 0: arraycopy
                    0xb7, 0, 12, // 3: super.clone()
                    0xb6, 0, 18, // 6: notify()
                    0xb6, 0, 24, // 9: hashCode()
                    0xb7, 0, 30, 0xbf, // 12: RuntimeException.<init>, athrow
                    0xb6, 0, 36, // 16: invokeExact
                    0xb8, 0, 42, // 19: helper()
                    0xb6, 0, 48, // 22: wait()
                    0xb8, 0, 54, // 25: Array.newInstance
                    0xb1,
                ],
                &[
                    (0, 3),
                    (3, 4),
                    (6, 5),
                    (9, 6),
                    (12, 7),
                    (16, 8),
                    (19, 9),
                    (22, 10),
                    (25, 11),
                ],
            ),
            java("helper", "()V", vec![0xb1], &[(0, 20)]),
        ],
    );
    Fixture { store, main }
}

fn at(fx: &Fixture, bci: i32) -> BacktraceFrame {
    BacktraceFrame::Entry(StackTraceEntry {
        class_name: Arc::from("probe/Main"),
        method_name: Arc::from("main"),
        method_descriptor: Some(Arc::from("()V")),
        source_file: None,
        line_number: LINE_NUMBER_UNKNOWN,
        byte_code_index: bci,
        class_id: Some(fx.main),
        method_index: None,
    })
}

fn describe(frame: &BacktraceFrame) -> (String, i32, i32) {
    let e = frame.to_entry();
    (
        format!("{}.{}", e.class_name, e.method_name),
        e.line_number,
        e.byte_code_index,
    )
}

/// The leaf frame for a throwable of class `throwable` raised with the
/// innermost frame at `bci`.
fn leaf(fx: &Fixture, bci: i32, throwable: &str) -> Option<(String, i32, i32)> {
    let store = &fx.store;
    let tclass = store
        .iter()
        .find(|c| &*c.name == throwable)
        .expect("throwable class");
    let mut resolve =
        |_: ClassId, name: &str| store.iter().find(|c| &*c.name == name).map(|c| c.id);
    native_leaf_frame(store, &at(fx, bci), tclass, &mut resolve).map(|f| describe(&f))
}

fn native_row(frame: &str) -> Option<(String, i32, i32)> {
    Some((frame.to_string(), LINE_NUMBER_NATIVE, -1))
}

/// T3-1: a static native's own throwables, a null argument's NPE
/// included, get its `(Native Method)` frame.
#[test]
fn a_static_native_gets_its_frame_for_what_it_raises() {
    let fx = fixture();
    for throwable in [
        "java/lang/ArrayIndexOutOfBoundsException",
        "java/lang/NullPointerException",
        "java/lang/UnsatisfiedLinkError",
        "java/lang/ClassFormatError",
    ] {
        assert_eq!(
            leaf(&fx, 0, throwable),
            native_row("java/lang/System.arraycopy"),
            "{throwable}"
        );
    }
}

/// What the invoke raises BEFORE entering keeps the caller on top.
#[test]
fn what_the_call_raises_before_entering_gets_no_frame() {
    let fx = fixture();
    for throwable in [
        "java/lang/NoSuchMethodError",
        "java/lang/ExceptionInInitializerError",
        "java/lang/StackOverflowError",
    ] {
        assert_eq!(leaf(&fx, 0, throwable), None, "{throwable}");
    }
    // A null receiver: every non-static call.
    assert_eq!(leaf(&fx, 6, "java/lang/NullPointerException"), None);
    assert_eq!(leaf(&fx, 3, "java/lang/NullPointerException"), None);
}

/// `super.clone()` and a final virtual native enter the native; a
/// non-final virtual native may have dispatched elsewhere.
#[test]
fn only_calls_that_enter_the_native_get_its_frame() {
    let fx = fixture();
    assert_eq!(
        leaf(&fx, 3, "java/lang/CloneNotSupportedException"),
        native_row("java/lang/Object.clone")
    );
    assert_eq!(
        leaf(&fx, 6, "java/lang/IllegalMonitorStateException"),
        native_row("java/lang/Object.notify")
    );
    assert_eq!(leaf(&fx, 9, "java/lang/IllegalMonitorStateException"), None);
}

/// Constructors, signature-polymorphic natives, Java methods and
/// non-calls get nothing.
#[test]
fn no_frame_for_what_is_not_a_native_call() {
    let fx = fixture();
    let any = "java/lang/IllegalMonitorStateException";
    for bci in [12, 15, 16, 19, -1, 99] {
        assert_eq!(leaf(&fx, bci, any), None, "bci {bci}");
    }
}

/// The lock-free screen: a call, except an `invokespecial` followed by
/// `athrow` (`throw new X()` standing at `X.<init>`).
#[test]
fn the_lock_free_screen_skips_throw_new() {
    let code = [
        0xb8, 0, 6, 0xb7, 0, 12, 0xb6, 0, 18, 0xb7, 0, 30, 0xbf, 0x00,
    ];
    assert!(code_may_call_native_at(&code, 0));
    assert!(code_may_call_native_at(&code, 3));
    assert!(code_may_call_native_at(&code, 6));
    assert!(!code_may_call_native_at(&code, 9));
    assert!(!code_may_call_native_at(&code, 12));
    assert!(!code_may_call_native_at(&code, 13));
    assert!(!code_may_call_native_at(&code, 40));
}

/// Item 5: on a virtual thread an interrupted registered `wait()` leaves
/// `wait(long)` from its FIRST `wait0` call (the virtual-thread branch,
/// line 383); a platform thread from its last (389). Nothing chains from
/// a call that is not an `Object.wait` row.
#[test]
fn a_virtual_thread_waits_from_the_virtual_branch() {
    let fx = fixture();
    let store = &fx.store;
    let mut resolve =
        |_: ClassId, name: &str| store.iter().find(|c| &*c.name == name).map(|c| c.id);
    let rows = |frames: Option<Vec<BacktraceFrame>>| {
        frames.map(|v| v.iter().map(describe).collect::<Vec<_>>())
    };
    let row = |frame: &str, line: i32, bci: i32| (frame.to_string(), line, bci);
    let site = at(&fx, 22);
    assert_eq!(
        rows(virtual_thread_standin_frames(
            store,
            &site,
            INTERRUPTED,
            &mut resolve
        )),
        Some(vec![
            row("java/lang/Object.wait", 351, 2),
            row("java/lang/Object.wait", 383, 2),
            row("java/lang/Object.wait0", LINE_NUMBER_NATIVE, -1),
        ])
    );
    assert_eq!(
        rows(native_standin_frames(
            store,
            &site,
            INTERRUPTED,
            &mut resolve
        )),
        Some(vec![
            row("java/lang/Object.wait", 351, 2),
            row("java/lang/Object.wait", 389, 7),
            row("java/lang/Object.wait0", LINE_NUMBER_NATIVE, -1),
        ])
    );
    let arraycopy = at(&fx, 0);
    assert!(
        virtual_thread_standin_frames(store, &arraycopy, INTERRUPTED, &mut resolve).is_none()
    );
}

/// A registered `Array.newInstance(Class, int)` that throws gets
/// HotSpot's `newArray` leaf and `newInstance` frame, on any thread; an
/// NPE standing at a call that names no entry row gets nothing.
#[test]
fn a_served_array_new_instance_gets_its_two_frames() {
    let fx = fixture();
    let store = &fx.store;
    let mut resolve =
        |_: ClassId, name: &str| store.iter().find(|c| &*c.name == name).map(|c| c.id);
    let expected = Some(vec![
        ("java/lang/reflect/Array.newInstance".to_string(), 76, 2),
        (
            "java/lang/reflect/Array.newArray".to_string(),
            LINE_NUMBER_NATIVE,
            -1,
        ),
    ]);
    let site = at(&fx, 25);
    for throwable in [
        "java/lang/NegativeArraySizeException",
        "java/lang/IllegalArgumentException",
        "java/lang/NullPointerException",
    ] {
        let got = native_standin_frames(store, &site, throwable, &mut resolve)
            .map(|v| v.iter().map(describe).collect::<Vec<_>>());
        assert_eq!(got, expected, "{throwable}");
        let got = virtual_thread_standin_frames(store, &site, throwable, &mut resolve)
            .map(|v| v.iter().map(describe).collect::<Vec<_>>());
        assert_eq!(got, expected, "virtual {throwable}");
    }
    assert!(native_standin_frames(store, &site, INTERRUPTED, &mut resolve).is_none());
    let npe = "java/lang/NullPointerException";
    assert!(native_standin_frames(store, &at(&fx, 6), npe, &mut resolve).is_none());
    // `newInstance` itself is Java: the one-frame rule has nothing to add.
    assert_eq!(leaf(&fx, 25, "java/lang/NegativeArraySizeException"), None);
}
