use super::r13_trace3_standin_tests::{add_class, java, native, pool};
use super::*;

const CNFE: &str = "java/lang/ClassNotFoundException";
const ONE_ARG: &str = "(Ljava/lang/String;)Ljava/lang/Class;";
const RESOLVE: &str = "(Ljava/lang/String;Z)Ljava/lang/Class;";
const FOR_NAME_3: &str = "(Ljava/lang/String;ZLjava/lang/ClassLoader;)Ljava/lang/Class;";
const FOR_NAME_0: &str =
    "(Ljava/lang/String;ZLjava/lang/ClassLoader;Ljava/lang/Class;)Ljava/lang/Class;";

struct Fixture {
    store: ClassStore,
    main: ClassId,
    url: ClassId,
    /// Inherits everything from `URLClassLoader` (`new URLClassLoader(..) {}`).
    plain_sub: ClassId,
    /// Inherits everything from `ClassLoader` (`new ClassLoader(null) {}`).
    plain_loader: ClassId,
    /// Overrides `loadClass(String, boolean)`, calling `super.loadClass`.
    own_load: ClassId,
}

/// JDK-25-shaped `ClassLoader` (`loadClass(String)` calls `loadClass(name,
/// false)` at pc 3, line 490; `loadClass(String, boolean)` calls `findClass`
/// at pc 9, line 557), `URLClassLoader.findClass` raising its plain
/// `ClassNotFoundException` at pc 15 (line 366), `Class.forName(String,
/// boolean, ClassLoader)` calling `forName0` at pc 4 (line 547).
/// `probe/Main.main()V` calls, at bci: 0 `URLClassLoader.loadClass(String)`,
/// 3 `ClassLoader.loadClass(String, boolean)`, 6 `helper()`, 9
/// `Class.forName(String, boolean, ClassLoader)`, 12
/// `OwnLoadLoader.loadClass(String)`, 15 `invokespecial
/// URLClassLoader.loadClass(String)`, 18 `URLClassLoader.findClass`.
fn fixture() -> Fixture {
    let mut store = ClassStore::new();
    let object = add_class(&mut store, "java/lang/Object", None, pool(&[]), vec![]);
    let class_loader = add_class(
        &mut store,
        "java/lang/ClassLoader",
        Some(object),
        pool(&[
            ("java/lang/ClassLoader", "loadClass", RESOLVE),
            ("java/lang/ClassLoader", "findClass", ONE_ARG),
            (CNFE, "<init>", "(Ljava/lang/String;)V"),
        ]),
        vec![
            java(
                "loadClass",
                ONE_ARG,
                vec![0x2a, 0x2b, 0x03, 0xb6, 0, 6, 0xb0],
                &[(0, 490)],
            ),
            java(
                "loadClass",
                RESOLVE,
                vec![
                    0x2a, 0x2b, 0x03, 0xb6, 0, 6, 0x57, 0x2a, 0x2b, 0xb6, 0, 12, 0xb0,
                ],
                &[(0, 545), (7, 557)],
            ),
            // 0 new 3 dup 4 aload_1 5 invokespecial(CNFE.<init>) 8 athrow
            java(
                "findClass",
                ONE_ARG,
                vec![0xbb, 0, 14, 0x59, 0x2b, 0xb7, 0, 18, 0xbf],
                &[(0, 1220)],
            ),
        ],
    );
    let secure = add_class(
        &mut store,
        "java/security/SecureClassLoader",
        Some(class_loader),
        pool(&[]),
        vec![],
    );
    let url = add_class(
        &mut store,
        "java/net/URLClassLoader",
        Some(secure),
        pool(&[
            (CNFE, "<init>", "(Ljava/lang/String;Ljava/lang/Throwable;)V"),
            (CNFE, "<init>", "(Ljava/lang/String;)V"),
        ]),
        vec![java(
            "findClass",
            ONE_ARG,
            vec![
                0xbb, 0, 2, 0x59, 0x2b, 0x01, 0xb7, 0, 6, 0xbf, 0xbb, 0, 2, 0x59, 0x2b, 0xb7,
                0, 12, 0xbf,
            ],
            &[(0, 360), (10, 366)],
        )],
    );
    let plain_sub = add_class(
        &mut store,
        "probe/PlainUrlSub",
        Some(url),
        pool(&[]),
        vec![],
    );
    let plain_loader = add_class(
        &mut store,
        "probe/PlainLoader",
        Some(class_loader),
        pool(&[]),
        vec![],
    );
    let own_load = add_class(
        &mut store,
        "probe/OwnLoadLoader",
        Some(url),
        pool(&[("java/net/URLClassLoader", "loadClass", RESOLVE)]),
        // aload_0 aload_1 iload_2 invokespecial(super.loadClass) areturn
        vec![java(
            "loadClass",
            RESOLVE,
            vec![0x2a, 0x2b, 0x1c, 0xb7, 0, 6, 0xb0],
            &[(0, 70)],
        )],
    );
    add_class(
        &mut store,
        "java/lang/Class",
        Some(object),
        pool(&[("java/lang/Class", "forName0", FOR_NAME_0)]),
        vec![
            // aload_0 iload_1 aload_2 aconst_null invokestatic(forName0) areturn
            java(
                "forName",
                FOR_NAME_3,
                vec![0x2a, 0x1b, 0x2c, 0x01, 0xb8, 0, 6, 0xb0],
                &[(0, 547)],
            ),
            native("forName0", FOR_NAME_0),
        ],
    );
    let main = add_class(
        &mut store,
        "probe/Main",
        Some(object),
        pool(&[
            ("java/net/URLClassLoader", "loadClass", ONE_ARG),
            ("java/lang/ClassLoader", "loadClass", RESOLVE),
            ("probe/Main", "helper", "()V"),
            ("java/lang/Class", "forName", FOR_NAME_3),
            ("probe/OwnLoadLoader", "loadClass", ONE_ARG),
            ("java/net/URLClassLoader", "findClass", ONE_ARG),
        ]),
        vec![java(
            "main",
            "()V",
            vec![
                0xb6, 0, 6, 0xb6, 0, 12, 0xb8, 0, 18, 0xb8, 0, 24, 0xb6, 0, 30, 0xb7, 0, 6,
                0xb6, 0, 36, 0xb1,
            ],
            &[(0, 3), (3, 4), (6, 5), (9, 6), (12, 7), (15, 8), (18, 9)],
        )],
    );
    Fixture {
        store,
        main,
        url,
        plain_sub,
        plain_loader,
        own_load,
    }
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

fn callee(class_name: &str, class_id: ClassId, method_name: &str) -> BacktraceFrame {
    callee_of(class_name, class_id, method_name, ONE_ARG)
}

fn callee_of(
    class_name: &str,
    class_id: ClassId,
    method_name: &str,
    descriptor: &str,
) -> BacktraceFrame {
    BacktraceFrame::Entry(StackTraceEntry {
        class_name: Arc::from(class_name),
        method_name: Arc::from(method_name),
        method_descriptor: Some(Arc::from(descriptor)),
        source_file: None,
        line_number: LINE_NUMBER_UNKNOWN,
        byte_code_index: 0,
        class_id: Some(class_id),
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

/// A one-argument call (virtual or `super`) gets both base frames, a
/// two-argument call only the `findClass` caller.
#[test]
fn a_served_load_class_gets_its_frames_above_find_class() {
    let fx = fixture();
    let mut resolve =
        |_: ClassId, name: &str| fx.store.iter().find(|c| &*c.name == name).map(|c| c.id);
    let find = callee("java/net/URLClassLoader", fx.url, "findClass");
    let both = Some(vec![
        row("java/lang/ClassLoader.loadClass", 490, 3),
        row("java/lang/ClassLoader.loadClass", 557, 9),
    ]);
    for bci in [0, 15] {
        let caller = at(fx.main, bci);
        assert!(may_need_served_load_class_middle_frames(&caller, &find));
        assert_eq!(
            rows(served_load_class_middle_frames(
                &fx.store,
                &caller,
                &find,
                &mut resolve
            )),
            both,
            "{bci}"
        );
    }
    assert_eq!(
        rows(served_load_class_middle_frames(
            &fx.store,
            &at(fx.main, 3),
            &find,
            &mut resolve
        )),
        Some(vec![row("java/lang/ClassLoader.loadClass", 557, 9)])
    );
}

/// Another call, another callee, or a `loadClass` override on the way
/// (from the call's class or from the `findClass` class): nothing.
#[test]
fn other_pairs_and_overrides_get_no_load_class_frames() {
    let fx = fixture();
    let mut resolve =
        |_: ClassId, name: &str| fx.store.iter().find(|c| &*c.name == name).map(|c| c.id);
    let find = callee("java/net/URLClassLoader", fx.url, "findClass");
    for bci in [6, 9, 12, 18] {
        assert!(
            served_load_class_middle_frames(&fx.store, &at(fx.main, bci), &find, &mut resolve)
                .is_none(),
            "{bci}"
        );
    }
    let overridden = callee("probe/OwnLoadLoader", fx.own_load, "findClass");
    assert!(served_load_class_middle_frames(
        &fx.store,
        &at(fx.main, 0),
        &overridden,
        &mut resolve
    )
    .is_none());
    let run = callee("probe/Task", fx.url, "run");
    let caller = at(fx.main, 0);
    assert!(!may_need_served_load_class_middle_frames(&caller, &run));
    assert!(served_load_class_middle_frames(&fx.store, &caller, &run, &mut resolve).is_none());
}

/// A served `Class.forName` whose `forName0` ran the loader's `findClass`:
/// the `forName` chain, then the two `loadClass` frames.
#[test]
fn a_served_for_name_gets_the_load_class_frames_above_its_chain() {
    let fx = fixture();
    let mut resolve =
        |_: ClassId, name: &str| fx.store.iter().find(|c| &*c.name == name).map(|c| c.id);
    let find = callee("java/net/URLClassLoader", fx.url, "findClass");
    assert_eq!(
        rows(callback_native_middle_frames(
            &fx.store,
            &at(fx.main, 9),
            &find,
            &mut resolve
        )),
        Some(vec![
            row("java/lang/Class.forName", 547, 4),
            row("java/lang/Class.forName0", LINE_NUMBER_NATIVE, -1),
            row("java/lang/ClassLoader.loadClass", 490, 3),
            row("java/lang/ClassLoader.loadClass", 557, 9),
        ])
    );
    let overridden = callee("probe/OwnLoadLoader", fx.own_load, "findClass");
    assert!(callback_native_middle_frames(
        &fx.store,
        &at(fx.main, 9),
        &overridden,
        &mut resolve
    )
    .is_none());
}

/// An override of `loadClass(String, boolean)` the served one-argument
/// `loadClass` dispatched to gets `ClassLoader.loadClass(String)` below it,
/// for a one-argument call (also under a served `forName`); a two-argument
/// call, or the base class's own method as the callee, gets nothing.
#[test]
fn a_resolve_override_gets_the_one_arg_frame_below_it() {
    let fx = fixture();
    let mut resolve =
        |_: ClassId, name: &str| fx.store.iter().find(|c| &*c.name == name).map(|c| c.id);
    let hop = callee_of("probe/OwnLoadLoader", fx.own_load, "loadClass", RESOLVE);
    let base = row("java/lang/ClassLoader.loadClass", 490, 3);
    assert!(may_need_loader_middle_frames(&at(fx.main, 0), &hop));
    assert_eq!(
        rows(served_load_class_middle_frames(
            &fx.store,
            &at(fx.main, 0),
            &hop,
            &mut resolve
        )),
        Some(vec![base.clone()])
    );
    let two_arg = at(fx.main, 3);
    assert!(served_load_class_middle_frames(&fx.store, &two_arg, &hop, &mut resolve).is_none());
    let class_loader = fx
        .store
        .iter()
        .find(|c| &*c.name == "java/lang/ClassLoader")
        .map_or(fx.url, |c| c.id);
    let own_base = callee_of("java/lang/ClassLoader", class_loader, "loadClass", RESOLVE);
    assert!(served_load_class_middle_frames(
        &fx.store,
        &at(fx.main, 0),
        &own_base,
        &mut resolve
    )
    .is_none());
    assert_eq!(
        rows(callback_native_middle_frames(
            &fx.store,
            &at(fx.main, 9),
            &hop,
            &mut resolve
        )),
        Some(vec![
            row("java/lang/Class.forName", 547, 4),
            row("java/lang/Class.forName0", LINE_NUMBER_NATIVE, -1),
            base,
        ])
    );
}

/// The miss hint of a subclass that inherits everything gets the bare
/// loader's frames; one with a `loadClass(String, boolean)` override hop
/// above the base gets none (its frame is already captured).
#[test]
fn a_url_loader_subclass_hint_gets_frames_only_without_an_override_hop() {
    let fx = fixture();
    assert_eq!(
        rows(served_loader_miss_frames(&fx.store, fx.plain_sub, CNFE)),
        Some(vec![
            row("java/lang/ClassLoader.loadClass", 490, 3),
            row("java/lang/ClassLoader.loadClass", 557, 9),
            row("java/net/URLClassLoader.findClass", 366, 15),
        ])
    );
    assert!(served_loader_miss_frames(&fx.store, fx.own_load, CNFE).is_none());
    // A user loader with no `findClass` of its own: the base one raises.
    assert_eq!(
        rows(served_loader_miss_frames(&fx.store, fx.plain_loader, CNFE)),
        Some(vec![
            row("java/lang/ClassLoader.loadClass", 490, 3),
            row("java/lang/ClassLoader.loadClass", 557, 9),
            row("java/lang/ClassLoader.findClass", 1220, 5),
        ])
    );
}

/// The innermost frame at a call of `findClass` (Java called the native
/// directly) is screened; one at a `loadClass` call is not.
#[test]
fn a_direct_find_class_call_is_screened() {
    let fx = fixture();
    assert!(innermost_calls_find_class(&fx.store, &at(fx.main, 18)));
    for bci in [0, 3, 9, -1] {
        assert!(
            !innermost_calls_find_class(&fx.store, &at(fx.main, bci)),
            "{bci}"
        );
    }
}

/// Round 15 wave 6 (lane trace6; TR5W5-4): a direct call of the
/// `findClass` native on a receiver whose selected `findClass` is
/// `URLClassLoader`'s own gets that frame at its plain raise; a receiver
/// whose `findClass` is another class's, another call, or another
/// throwable gets nothing. Failed before: no rule answered, the caller
/// stayed on top (`R15Trace5LoaderMiddle` `direct`, `--compatible`).
#[test]
fn a_direct_find_class_native_call_gets_its_url_loader_frame() {
    let fx = fixture();
    let direct = at(fx.main, 18);
    for loader in [fx.url, fx.plain_sub, fx.own_load] {
        assert_eq!(
            rows(
                url_find_class_native_frame(&fx.store, &direct, loader, CNFE).map(|f| vec![f])
            ),
            Some(vec![row("java/net/URLClassLoader.findClass", 366, 15)])
        );
    }
    // `ClassLoader.findClass` selected: not this rule's frame.
    assert!(url_find_class_native_frame(&fx.store, &direct, fx.plain_loader, CNFE).is_none());
    assert!(url_find_class_native_frame(
        &fx.store,
        &direct,
        fx.plain_sub,
        "java/lang/IllegalStateException"
    )
    .is_none());
    for bci in [0, 3, 15, -1] {
        assert!(
            url_find_class_native_frame(&fx.store, &at(fx.main, bci), fx.plain_sub, CNFE)
                .is_none(),
            "{bci}"
        );
    }
}

/// Interpreter round i3 wave 73, lane L2 (`CRATONVM_THROWABLE_LOADER_MISS_MEMO`):
/// a real-JDK delegation's pair -- a `loadClass(String, boolean)` call above
/// a `loadClass(String, boolean)` activation -- engages the lock-free
/// screen but is decided quiet; a one-argument call above an override hop,
/// a two-argument call above `findClass`, and a `Class.forName` call are
/// not; a caller whose class is not in the store is never decided.
#[test]
fn i73_l2_a_delegation_pair_is_decided_quiet() {
    let fx = fixture();
    let hop = callee_of("probe/OwnLoadLoader", fx.own_load, "loadClass", RESOLVE);
    let find = callee("java/net/URLClassLoader", fx.url, "findClass");
    let two_arg = at(fx.main, 3);
    assert!(
        may_need_loader_middle_frames(&two_arg, &hop),
        "the screen engages"
    );
    assert!(loader_middle_pair_decided_quiet(&fx.store, &two_arg, &hop));
    assert!(!loader_middle_pair_decided_quiet(
        &fx.store,
        &at(fx.main, 0),
        &hop
    ));
    assert!(!loader_middle_pair_decided_quiet(
        &fx.store, &two_arg, &find
    ));
    assert!(!loader_middle_pair_decided_quiet(
        &fx.store,
        &at(fx.main, 9),
        &find
    ));
    // `invokestatic helper()`: neither rule's call.
    assert!(loader_middle_pair_decided_quiet(
        &fx.store,
        &at(fx.main, 6),
        &find
    ));
    let unknown = at(ClassId::new(9999), 3);
    assert!(!loader_middle_pair_decided_quiet(&fx.store, &unknown, &hop));
    assert!(loader_middle_pairs_decided_quiet(
        &fx.store,
        &[two_arg.clone(), hop.clone()]
    ));
    assert!(!loader_middle_pairs_decided_quiet(
        &fx.store,
        &[at(fx.main, 0), hop]
    ));
    assert!(loader_middle_pairs_decided_quiet(&fx.store, &[]));
}

/// The property the capture memo relies on: a pair decided quiet gets no
/// frames from either rule, for every call site and callee of the fixture.
#[test]
fn i73_l2_decided_quiet_implies_both_rules_answer_nothing() {
    let fx = fixture();
    let mut resolve =
        |_: ClassId, name: &str| fx.store.iter().find(|c| &*c.name == name).map(|c| c.id);
    let class_loader = fx
        .store
        .iter()
        .find(|c| &*c.name == "java/lang/ClassLoader")
        .map_or(fx.url, |c| c.id);
    let callees = [
        callee("java/net/URLClassLoader", fx.url, "findClass"),
        callee("probe/OwnLoadLoader", fx.own_load, "findClass"),
        callee_of("probe/OwnLoadLoader", fx.own_load, "loadClass", RESOLVE),
        callee_of("java/lang/ClassLoader", class_loader, "loadClass", RESOLVE),
        callee_of("java/net/URLClassLoader", fx.url, "loadClass", ONE_ARG),
        callee("probe/Task", fx.url, "run"),
    ];
    let mut decided = 0;
    for bci in [0, 3, 6, 9, 12, 15, 18] {
        let caller = at(fx.main, bci);
        for callee in &callees {
            if !loader_middle_pair_decided_quiet(&fx.store, &caller, callee) {
                continue;
            }
            decided += 1;
            assert!(
                callback_native_middle_frames(&fx.store, &caller, callee, &mut resolve)
                    .is_none(),
                "callback rule at {bci} for {:?}",
                callee.to_entry().method_name
            );
            assert!(
                served_load_class_middle_frames(&fx.store, &caller, callee, &mut resolve)
                    .is_none(),
                "served rule at {bci} for {:?}",
                callee.to_entry().method_name
            );
        }
    }
    assert!(decided > 0, "the property was exercised");
}
