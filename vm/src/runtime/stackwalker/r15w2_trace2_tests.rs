use super::r13_trace3_standin_tests::{add_class, java, native, pool};
use super::*;

const CNFE: &str = "java/lang/ClassNotFoundException";
const ONE_ARG: &str = "(Ljava/lang/String;)Ljava/lang/Class;";
const RESOLVE: &str = "(Ljava/lang/String;Z)Ljava/lang/Class;";

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

struct Loaders {
    store: ClassStore,
    /// JDK 25 `ClassLoaders$AppClassLoader`: no `loadClass` of its own.
    app: ClassId,
    /// JDK 17 / 21 shape: `loadClass(String, boolean)` calling
    /// `super.loadClass` at pc 3 (line 190).
    old_app: ClassId,
    /// Inherits the base `loadClass(String, boolean)` (calls `findClass`).
    plain: ClassId,
    /// Overrides the one-argument `loadClass`.
    one_arg: ClassId,
    /// `loadClass(String, boolean)` with two `new ClassNotFoundException`.
    two_raises: ClassId,
}

/// JDK-25-shaped: `ClassLoader.loadClass(String)` is `loadClass(name,
/// false)` (invokevirtual at pc 3, line 490); `BuiltinClassLoader.loadClass
/// (String, boolean)` calls `loadClassOrNull` and throws at pc 16 (line 580).
fn loaders() -> Loaders {
    let mut store = ClassStore::new();
    let object = add_class(&mut store, "java/lang/Object", None, pool(&[]), vec![]);
    let class_loader = add_class(
        &mut store,
        "java/lang/ClassLoader",
        Some(object),
        pool(&[
            ("java/lang/ClassLoader", "loadClass", RESOLVE),
            ("java/lang/ClassLoader", "findClass", ONE_ARG),
        ]),
        vec![
            // aload_0 aload_1 iconst_0 invokevirtual(loadClass RESOLVE) areturn
            java(
                "loadClass",
                ONE_ARG,
                vec![0x2a, 0x2b, 0x03, 0xb6, 0, 6, 0xb0],
                &[(0, 490)],
            ),
            // aload_0 aload_1 invokevirtual(findClass) areturn
            java(
                "loadClass",
                RESOLVE,
                vec![0x2a, 0x2b, 0xb6, 0, 12, 0xb0],
                &[(0, 520)],
            ),
        ],
    );
    let builtin = add_class(
        &mut store,
        "jdk/internal/loader/BuiltinClassLoader",
        Some(class_loader),
        pool(&[
            (
                "jdk/internal/loader/BuiltinClassLoader",
                "loadClassOrNull",
                RESOLVE,
            ),
            (CNFE, "<init>", "(Ljava/lang/String;)V"),
        ]),
        vec![java(
            "loadClass",
            RESOLVE,
            // 0 aload_0 1 aload_1 2 iload_2 3 invokevirtual(loadClassOrNull)
            // 6 astore_3 7 aload_3 8 ifnonnull(+12) 11 new 14 dup
            // 15 aload_1 16 invokespecial(CNFE.<init>) 19 athrow
            // 20 aload_3 21 areturn
            vec![
                0x2a, 0x2b, 0x1c, 0xb6, 0, 6, 0x4e, 0x2d, 0xc7, 0, 12, 0xbb, 0, 8, 0x59, 0x2b,
                0xb7, 0, 12, 0xbf, 0x2d, 0xb0,
            ],
            &[(0, 575), (7, 579), (11, 580), (20, 581)],
        )],
    );
    let app = add_class(
        &mut store,
        "jdk/internal/loader/ClassLoaders$AppClassLoader",
        Some(builtin),
        pool(&[]),
        vec![],
    );
    let old_app = add_class(
        &mut store,
        "probe/OldAppLoader",
        Some(builtin),
        pool(&[(
            "jdk/internal/loader/BuiltinClassLoader",
            "loadClass",
            RESOLVE,
        )]),
        // aload_0 aload_1 iload_2 invokespecial(super.loadClass) areturn
        vec![java(
            "loadClass",
            RESOLVE,
            vec![0x2a, 0x2b, 0x1c, 0xb7, 0, 6, 0xb0],
            &[(0, 190)],
        )],
    );
    let plain = add_class(
        &mut store,
        "probe/PlainLoader",
        Some(class_loader),
        pool(&[]),
        vec![],
    );
    let one_arg = add_class(
        &mut store,
        "probe/OneArgLoader",
        Some(builtin),
        pool(&[]),
        vec![java("loadClass", ONE_ARG, vec![0x01, 0xb0], &[(0, 30)])],
    );
    let two_raises = add_class(
        &mut store,
        "probe/TwoRaises",
        Some(class_loader),
        pool(&[(CNFE, "<init>", "(Ljava/lang/String;)V")]),
        // twice: new dup aload_1 invokespecial(CNFE.<init>) athrow
        vec![java(
            "loadClass",
            RESOLVE,
            vec![
                0xbb, 0, 2, 0x59, 0x2b, 0xb7, 0, 6, 0xbf, 0xbb, 0, 2, 0x59, 0x2b, 0xb7, 0, 6,
                0xbf,
            ],
            &[(0, 40), (9, 41)],
        )],
    );
    Loaders {
        store,
        app,
        old_app,
        plain,
        one_arg,
        two_raises,
    }
}

/// HotSpot 25's two frames for the application loader (no override of its
/// own), and JDK 17's three (its `super.loadClass` hop in the middle).
#[test]
fn a_served_builtin_loader_miss_gets_its_loader_frames() {
    let fx = loaders();
    let base = row("java/lang/ClassLoader.loadClass", 490, 3);
    let raise = row("jdk/internal/loader/BuiltinClassLoader.loadClass", 580, 16);
    assert_eq!(
        rows(served_loader_miss_frames(&fx.store, fx.app, CNFE)),
        Some(vec![base.clone(), raise.clone()])
    );
    assert_eq!(
        rows(served_loader_miss_frames(&fx.store, fx.old_app, CNFE)),
        Some(vec![
            base,
            row("probe/OldAppLoader.loadClass", 190, 3),
            raise
        ])
    );
}

/// Another throwable, a loader whose chain ends without one
/// `ClassNotFoundException` site, or one overriding the one-argument
/// `loadClass`: nothing (fail-closed).
#[test]
fn other_throwables_and_shapes_get_no_loader_frames() {
    let fx = loaders();
    for throwable in [
        "java/lang/NoClassDefFoundError",
        "java/lang/NullPointerException",
    ] {
        assert!(served_loader_miss_frames(&fx.store, fx.app, throwable).is_none());
    }
    for loader in [fx.plain, fx.one_arg, fx.two_raises] {
        assert!(served_loader_miss_frames(&fx.store, loader, CNFE).is_none());
    }
}

/// `LockSupport.park(Object)` calls `VirtualThreads.park()` (pc 0, a class
/// not loaded here, no row) and `U.park(false, 0L)` (pc 6, line 371).
/// `probe/Main.main()V` calls `LockSupport.park(Object)` at bci 1 and
/// `Unsafe.park` at bci 7.
fn parks() -> (ClassStore, ClassId) {
    let mut store = ClassStore::new();
    let object = add_class(&mut store, "java/lang/Object", None, pool(&[]), vec![]);
    add_class(
        &mut store,
        STANDIN_UNSAFE_CLASS,
        Some(object),
        pool(&[]),
        vec![native("park", "(ZJ)V")],
    );
    add_class(
        &mut store,
        STANDIN_LOCK_SUPPORT_CLASS,
        Some(object),
        pool(&[
            (STANDIN_UNSAFE_CLASS, "park", "(ZJ)V"),
            ("jdk/internal/misc/VirtualThreads", "park", "()V"),
        ]),
        vec![java(
            "park",
            "(Ljava/lang/Object;)V",
            // invokestatic(VirtualThreads.park) aconst_null iconst_0 lconst_0
            // invokevirtual(Unsafe.park) return
            vec![0xb8, 0, 12, 0x01, 0x03, 0x09, 0xb6, 0, 6, 0xb1],
            &[(0, 369), (3, 371)],
        )],
    );
    let main = add_class(
        &mut store,
        "probe/Main",
        Some(object),
        pool(&[
            (STANDIN_LOCK_SUPPORT_CLASS, "park", "(Ljava/lang/Object;)V"),
            (STANDIN_UNSAFE_CLASS, "park", "(ZJ)V"),
        ]),
        vec![java(
            "main",
            "()V",
            // aconst_null invokestatic(LockSupport.park) aconst_null iconst_0
            // lconst_0 invokevirtual(Unsafe.park) return
            vec![0x01, 0xb8, 0, 6, 0x01, 0x03, 0x09, 0xb6, 0, 12, 0xb1],
            &[(0, 10), (4, 11)],
        )],
    );
    (store, main)
}

fn published(main: ClassId, bci: i32) -> StackTraceEntry {
    StackTraceEntry {
        class_name: Arc::from("probe/Main"),
        method_name: Arc::from("main"),
        method_descriptor: None,
        source_file: None,
        line_number: LINE_NUMBER_UNKNOWN,
        byte_code_index: bci,
        class_id: Some(main),
        method_index: None,
    }
}

/// TR5-5: a thread parked in a served `LockSupport.park` gets
/// `LockSupport.park` at its `U.park` call and `Unsafe.park (Native
/// Method)`; one parked at `Unsafe.park` itself the native frame. A bound
/// virtual thread takes only the second.
#[test]
fn a_parked_threads_stack_gets_the_park_frames() {
    let (store, main) = parks();
    let mut resolve =
        |_: ClassId, name: &str| store.iter().find(|c| &*c.name == name).map(|c| c.id);
    let native_park = row("jdk/internal/misc/Unsafe.park", LINE_NUMBER_NATIVE, -1);
    let as_frames = |v: &Vec<StackTraceEntry>| -> Vec<BacktraceFrame> {
        v.iter().cloned().map(BacktraceFrame::Entry).collect()
    };
    let through_lock_support =
        parked_thread_standin_frames(&store, &published(main, 1), &mut resolve);
    assert_eq!(
        rows(through_lock_support.as_ref().map(as_frames)),
        Some(vec![
            row("java/util/concurrent/locks/LockSupport.park", 371, 6),
            native_park.clone(),
        ])
    );
    assert!(through_lock_support
        .as_deref()
        .is_some_and(|f| !parked_frames_fit_virtual_thread(f)));
    let direct = parked_thread_standin_frames(&store, &published(main, 7), &mut resolve);
    assert_eq!(
        rows(direct.as_ref().map(as_frames)),
        Some(vec![native_park])
    );
    assert!(direct
        .as_deref()
        .is_some_and(parked_frames_fit_virtual_thread));
}

/// The park rows are parked-only: no throwable screen admits them.
#[test]
fn a_throwable_at_a_park_gets_no_park_frames() {
    let (store, main) = parks();
    let mut resolve =
        |_: ClassId, name: &str| store.iter().find(|c| &*c.name == name).map(|c| c.id);
    for bci in [1, 7] {
        let mut at = published(main, bci);
        at.method_descriptor = Some(Arc::from("()V"));
        let at = BacktraceFrame::Entry(at);
        for throwable in [
            "java/lang/InterruptedException",
            "java/lang/IllegalMonitorStateException",
            "java/lang/NullPointerException",
        ] {
            assert!(
                native_standin_frames(&store, &at, throwable, &mut resolve).is_none(),
                "{bci} {throwable}"
            );
        }
    }
}
