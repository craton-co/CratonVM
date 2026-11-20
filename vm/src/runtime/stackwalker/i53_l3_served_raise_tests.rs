use super::r13_trace3_standin_tests::{add_class, java, native, pool};
use super::*;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

const NPE: &str = "java/lang/NullPointerException";
const OBJECTS: &str = "java/util/Objects";
const REQUIRE_NON_NULL: (&str, &str, &str) = (
    OBJECTS,
    "requireNonNull",
    "(Ljava/lang/Object;)Ljava/lang/Object;",
);
const FIELD_DESCRIPTOR: &str = "(Ljava/lang/String;)Ljava/lang/reflect/Field;";

/// `Objects.requireNonNull` raising its NPE at pc 4 (line 220); a
/// `java/lang/Class` whose `getField` calls it at pc 1 (line 2065) and
/// raises `NoSuchFieldException` at pc 10, and whose `twice()V` calls it
/// twice; a `probe/Main.main()V` calling `getField` at bci 1 and `twice`
/// at bci 6.
fn fixture() -> (ClassStore, ClassId) {
    let mut store = ClassStore::new();
    let object = add_class(&mut store, "java/lang/Object", None, pool(&[]), Vec::new());
    add_class(
        &mut store,
        OBJECTS,
        Some(object),
        pool(&[(NPE, "<init>", "()V")]),
        vec![java(
            REQUIRE_NON_NULL.1,
            REQUIRE_NON_NULL.2,
            vec![0xbb, 0, 2, 0x59, 0xb7, 0, 6, 0xbf],
            &[(0, 220)],
        )],
    );
    add_class(
        &mut store,
        MEMBER_LOOKUP_CLASS,
        Some(object),
        pool(&[
            REQUIRE_NON_NULL,
            (NO_SUCH_FIELD_EXCEPTION, "<init>", "(Ljava/lang/String;)V"),
            ("java/lang/Object", "getClass", "()Ljava/lang/Class;"),
            (MEMBER_LOOKUP_CLASS, "check", "(Ljava/lang/Object;)V"),
        ]),
        vec![
            // `obj.getClass()` at pc 1 (line 40).
            java(
                "check",
                "(Ljava/lang/Object;)V",
                vec![0x2b, 0xb6, 0, 18, 0x57, 0xb1],
                &[(0, 40)],
            ),
            // `check(obj)` at pc 2 (line 30).
            java(
                "guarded",
                "(Ljava/lang/Object;)V",
                vec![0x2a, 0x2b, 0xb6, 0, 24, 0xb1],
                &[(0, 30)],
            ),
            native("isAssignableFrom", "(Ljava/lang/Class;)Z"),
            java(
                "getField",
                FIELD_DESCRIPTOR,
                vec![
                    0x2b, 0xb8, 0, 6, 0x57, 0xbb, 0, 8, 0x59, 0x01, 0xb7, 0, 12, 0xbf,
                ],
                &[(0, 2065)],
            ),
            java(
                "twice",
                "()V",
                vec![0x01, 0xb8, 0, 6, 0x57, 0x01, 0xb8, 0, 6, 0x57, 0xb1],
                &[(0, 9)],
            ),
        ],
    );
    let main = add_class(
        &mut store,
        "probe/Main",
        Some(object),
        pool(&[
            (MEMBER_LOOKUP_CLASS, "getField", FIELD_DESCRIPTOR),
            (MEMBER_LOOKUP_CLASS, "twice", "()V"),
            (MEMBER_LOOKUP_CLASS, "guarded", "(Ljava/lang/Object;)V"),
            (
                MEMBER_LOOKUP_CLASS,
                "isAssignableFrom",
                "(Ljava/lang/Class;)Z",
            ),
            ("p/Two", "twoRaises", "()V"),
        ]),
        vec![java(
            "main",
            "()V",
            vec![
                0x01, 0xb6, 0, 6, 0x57, 0x01, 0xb6, 0, 12, 0x01, 0xb6, 0, 18, 0x01, 0xb6, 0,
                24, 0xb8, 0, 30, 0xb1,
            ],
            &[(0, 3)],
        )],
    );
    (store, main)
}

fn expand(store: &ClassStore, main: ClassId, bci: i32, throwable: &str) -> Option<Vec<String>> {
    expand_with(store, main, bci, throwable, None, None)
}

fn expand_with(
    store: &ClassStore,
    main: ClassId,
    bci: i32,
    throwable: &str,
    npe_action: Option<&str>,
    message: Option<&str>,
) -> Option<Vec<String>> {
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
    let hints = ServedRaiseHints {
        npe_action,
        message,
        ..ServedRaiseHints::default()
    };
    served_raise_frames(store, &at, throwable, hints, &mut resolve).map(|frames| {
        frames
            .iter()
            .map(|f| {
                let e = f.to_entry();
                format!("{}:{}@{}", e.method_name, e.line_number, e.byte_code_index)
            })
            .collect()
    })
}

/// The served method at its one construction, or at its one call of a
/// method that constructs the throwable and that callee at its one
/// construction; any other shape answers nothing.
#[test]
fn a_marked_raise_lists_the_served_method_at_its_throw() {
    let (store, main) = fixture();
    // `getField(null)`: `requireNonNull` above `getField`'s call of it.
    assert_eq!(
        expand(&store, main, 1, NPE),
        Some(vec![
            "getField:2065@1".to_string(),
            "requireNonNull:220@4".to_string()
        ])
    );
    // Its own `throw new NoSuchFieldException`.
    assert_eq!(
        expand(&store, main, 1, NO_SUCH_FIELD_EXCEPTION),
        Some(vec!["getField:2065@10".to_string()])
    );
    // Two raising calls, a throwable nothing constructs, a resource
    // error, and a frame that does not stand at a call: nothing.
    assert_eq!(expand(&store, main, 6, NPE), None);
    assert_eq!(
        expand(&store, main, 1, "java/lang/IllegalStateException"),
        None
    );
    assert_eq!(
        expand(&store, main, 1, "java/lang/StackOverflowError"),
        None
    );
    assert_eq!(expand(&store, main, 0, NPE), None);
    // A JDK `native` method: its own `(Native Method)` frame.
    assert_eq!(
        expand(&store, main, 14, NPE),
        Some(vec!["isAssignableFrom:-2@-1".to_string()])
    );
    assert_eq!(expand(&store, main, 14, "java/lang/OutOfMemoryError"), None);
}

/// An implicit NPE's action finds the one invoke that renders it, one hop
/// down, and no construction.
#[test]
fn an_implicit_npe_lists_the_invoke_its_action_names() {
    let (store, main) = fixture();
    let message = "Cannot invoke \"Object.getClass()\" because \"obj\" is null";
    let action = ServedNativeRaise::npe_action_of(message);
    assert_eq!(
        action.as_deref(),
        Some("Cannot invoke \"Object.getClass()\"")
    );
    assert_eq!(ServedNativeRaise::npe_action_of("name is null"), None);
    assert_eq!(
        expand_with(&store, main, 10, NPE, action.as_deref(), Some(message)),
        Some(vec!["guarded:30@2".to_string(), "check:40@1".to_string()])
    );
    // `getField` reaches no such invoke: nothing, not its
    // `requireNonNull` construction.
    assert_eq!(
        expand_with(&store, main, 1, NPE, action.as_deref(), Some(message)),
        None
    );
    // Without the action the construction rule answers.
    assert_eq!(expand(&store, main, 10, NPE), None);
}

/// Of two constructions, the one whose string literal is the error's
/// message; none says it, or no message: nothing.
#[test]
fn a_message_picks_its_construction_among_two() {
    const UOE: &str = "java/lang/UnsupportedOperationException";
    let (mut store, main) = fixture();
    let entries = vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Utf8(UOE.into()),
        ConstantPoolEntry::ClassReference { name_index: 1 },
        ConstantPoolEntry::Utf8("<init>".into()),
        ConstantPoolEntry::Utf8("(Ljava/lang/String;)V".into()),
        ConstantPoolEntry::NameAndType {
            name_index: 3,
            descriptor_index: 4,
        },
        ConstantPoolEntry::MethodReference {
            class_index: 2,
            name_and_type_index: 5,
        },
        ConstantPoolEntry::Utf8("first".into()),
        ConstantPoolEntry::StringReference { string_index: 7 },
        ConstantPoolEntry::Utf8("second".into()),
        ConstantPoolEntry::StringReference { string_index: 9 },
    ];
    let object = store
        .iter()
        .find(|c| &*c.name == "java/lang/Object")
        .map(|c| c.id);
    // `throw new UOE("first")` (constructor call at pc 6), then
    // `throw new UOE("second")` (at pc 16).
    add_class(
        &mut store,
        "p/Two",
        object,
        ConstantPool::new(entries),
        vec![java(
            "twoRaises",
            "()V",
            vec![
                0xbb, 0, 2, 0x59, 0x12, 8, 0xb7, 0, 6, 0xbf, 0xbb, 0, 2, 0x59, 0x12, 10, 0xb7,
                0, 6, 0xbf,
            ],
            &[(0, 50), (10, 51)],
        )],
    );
    assert_eq!(
        expand_with(&store, main, 17, UOE, None, Some("second")),
        Some(vec!["twoRaises:51@16".to_string()])
    );
    assert_eq!(
        expand_with(&store, main, 17, UOE, None, Some("first")),
        Some(vec!["twoRaises:50@6".to_string()])
    );
    assert_eq!(
        expand_with(&store, main, 17, UOE, None, Some("third")),
        None
    );
    assert_eq!(expand_with(&store, main, 17, UOE, None, None), None);
}

/// The frames of a capture standing at `class.method()V` bci `bci`, as
/// [`expand_with`] lists them, for `hints`.
fn expand_at(
    store: &ClassStore,
    class: (ClassId, &str, &str),
    bci: i32,
    throwable: &str,
    hints: ServedRaiseHints<'_>,
) -> Option<Vec<String>> {
    let at = BacktraceFrame::Entry(StackTraceEntry {
        class_name: Arc::from(class.1),
        method_name: Arc::from(class.2),
        method_descriptor: Some(Arc::from("()V")),
        source_file: None,
        line_number: LINE_NUMBER_UNKNOWN,
        byte_code_index: bci,
        class_id: Some(class.0),
        method_index: None,
    });
    let mut resolve =
        |_: ClassId, name: &str| store.iter().find(|c| &*c.name == name).map(|c| c.id);
    served_raise_frames(store, &at, throwable, hints, &mut resolve).map(|frames| {
        frames
            .iter()
            .map(|f| {
                let e = f.to_entry();
                format!("{}:{}@{}", e.method_name, e.line_number, e.byte_code_index)
            })
            .collect()
    })
}

fn id_of(store: &ClassStore, name: &str) -> ClassId {
    store
        .iter()
        .find(|c| &*c.name == name)
        .map(|c| c.id)
        .expect(name)
}

/// Interpreter round i2 wave 54, lane L3: an interface call's served
/// method is selected from the receiver's class the mark kept; a receiver
/// that is not the owner's subtype, or an `invokestatic`, keeps the
/// static selection.
#[test]
fn the_receivers_class_selects_an_interface_calls_method() {
    use cratonvm_reader::class_access_flags::MethodAccessFlags;
    const M: &str = "(Ljava/lang/Object;)V";
    let (mut store, _) = fixture();
    let object = id_of(&store, "java/lang/Object");
    let abstract_m = ClassFileMethod {
        access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::ABSTRACT,
        name: Arc::from("m"),
        descriptor: Arc::from(M),
        attributes: Vec::new(),
    };
    let iface = add_class(
        &mut store,
        "p/Iface",
        Some(object),
        pool(&[]),
        vec![abstract_m],
    );
    // `Objects.requireNonNull(null)` at pc 1 (line 70). (A superclass link
    // stands for the interface edge: the subtype test walks both.)
    let implementor = add_class(
        &mut store,
        "p/Impl",
        Some(iface),
        pool(&[REQUIRE_NON_NULL]),
        vec![java("m", M, vec![0x01, 0xb8, 0, 6, 0x57, 0xb1], &[(0, 70)])],
    );
    // `iface.m(null)` (invokeinterface) at bci 2.
    let caller = add_class(
        &mut store,
        "p/Caller",
        Some(object),
        pool(&[("p/Iface", "m", M)]),
        vec![java(
            "call",
            "()V",
            vec![0x01, 0x01, 0xb9, 0, 6, 2, 0, 0xb1],
            &[(0, 5)],
        )],
    );
    let at = (caller, "p/Caller", "call");
    let with = |receiver: Option<ClassId>| ServedRaiseHints {
        receiver_class: receiver,
        ..ServedRaiseHints::default()
    };
    // Statically, the interface's abstract method: nothing.
    assert_eq!(expand_at(&store, at, 2, NPE, with(None)), None);
    assert_eq!(
        expand_at(&store, at, 2, NPE, with(Some(implementor))),
        Some(vec![
            "m:70@1".to_string(),
            "requireNonNull:220@4".to_string()
        ])
    );
    // A receiver of another class: the static selection again.
    let objects = id_of(&store, OBJECTS);
    assert_eq!(expand_at(&store, at, 2, NPE, with(Some(objects))), None);
    assert!(class_is_subtype_of(&store, implementor, iface));
    assert!(!class_is_subtype_of(&store, iface, implementor));
}

/// Interpreter round i2 wave 54, lane L3: a noted chain whose first hop is
/// the served method lists every hop at its site; a chain of another
/// method is ignored.
#[test]
fn a_noted_chain_lists_each_hop_at_its_site() {
    use cratonvm_native_api::{ServedRaiseChain, ServedRaiseHop, ServedRaiseHopSite};
    const GET: &str = "(Ljava/lang/Object;)Ljava/lang/Object;";
    let (mut store, _) = fixture();
    let object = id_of(&store, "java/lang/Object");
    // Two calls of `Acc.get`: at pc 2 (line 10) and pc 8 (line 11).
    add_class(
        &mut store,
        "p/Field",
        Some(object),
        pool(&[("p/Acc", "get", GET)]),
        vec![java(
            "get",
            GET,
            vec![0x01, 0x01, 0xb6, 0, 6, 0x57, 0x01, 0x01, 0xb6, 0, 6, 0xb0],
            &[(0, 10), (6, 11)],
        )],
    );
    // `Objects.requireNonNull(null)` at pc 1 (line 20).
    add_class(
        &mut store,
        "p/Acc",
        Some(object),
        pool(&[REQUIRE_NON_NULL]),
        vec![java("get", GET, vec![0x01, 0xb8, 0, 6, 0xb0], &[(0, 20)])],
    );
    // `field.get(null)` at bci 2.
    let user = add_class(
        &mut store,
        "p/User",
        Some(object),
        pool(&[("p/Field", "get", GET)]),
        vec![java(
            "use",
            "()V",
            vec![0x01, 0x01, 0xb6, 0, 6, 0xb1],
            &[(0, 3)],
        )],
    );
    let chain = |first: ServedRaiseHopSite, served: &'static str| ServedRaiseChain {
        throwable_class: NPE,
        hops: vec![
            ServedRaiseHop {
                class: served,
                name: "get",
                descriptor: GET,
                site: first,
            },
            ServedRaiseHop {
                class: "p/Acc",
                name: "get",
                descriptor: GET,
                site: ServedRaiseHopSite::LastCall {
                    name: "requireNonNull",
                    descriptor: None,
                },
            },
            ServedRaiseHop {
                class: OBJECTS,
                name: REQUIRE_NON_NULL.1,
                descriptor: REQUIRE_NON_NULL.2,
                site: ServedRaiseHopSite::Raise,
            },
        ],
    };
    let last = chain(
        ServedRaiseHopSite::LastCall {
            name: "get",
            descriptor: Some(GET),
        },
        "p/Field",
    );
    let first = chain(
        ServedRaiseHopSite::FirstCall {
            name: "get",
            descriptor: Some(GET),
        },
        "p/Field",
    );
    let stale = chain(
        ServedRaiseHopSite::LastCall {
            name: "get",
            descriptor: Some(GET),
        },
        "p/Other",
    );
    let with = |chain: &ServedRaiseChain| -> Option<Vec<String>> {
        let hints = ServedRaiseHints {
            chain: Some(chain),
            ..ServedRaiseHints::default()
        };
        expand_at(&store, (user, "p/User", "use"), 2, NPE, hints)
    };
    assert_eq!(
        with(&last),
        Some(vec![
            "get:11@8".to_string(),
            "get:20@1".to_string(),
            "requireNonNull:220@4".to_string()
        ])
    );
    assert_eq!(
        with(&first),
        Some(vec![
            "get:10@2".to_string(),
            "get:20@1".to_string(),
            "requireNonNull:220@4".to_string()
        ])
    );
    // Stale: the one-hop rule answers, and finds no construction.
    assert_eq!(with(&stale), None);
}

/// JIT round j27 wave 4, lane MHROWS2: an innermost hop that raises at its
/// last `checkcast` of a just-loaded local (`ValueConversions
/// .primitiveConversion`'s `(Number) x`) is listed at that cast, for a
/// `ClassCastException` only; anywhere but innermost it voids the chain.
#[test]
fn a_local_checkcast_hop_lists_the_failing_cast() {
    use cratonvm_native_api::{ServedRaiseChain, ServedRaiseHop, ServedRaiseHopSite};
    const CCE: &str = "java/lang/ClassCastException";
    const UNBOX: &str = "(Ljava/lang/Object;Z)I";
    const CONV: &str = "(Ljava/lang/Object;)Ljava/lang/Number;";
    let (mut store, _) = fixture();
    let object = id_of(&store, "java/lang/Object");
    // Class 2 = p/VC, Methodref 6 = p/VC.conv; class 8 = java/lang/Number.
    add_class(
        &mut store,
        "p/VC",
        Some(object),
        pool(&[("p/VC", "conv", CONV), ("java/lang/Number", "intValue", "()I")]),
        vec![
            // `conv(null)` at pc 1 (line 81).
            java("unbox", UNBOX, vec![0x01, 0xb8, 0, 6, 0x57, 0x03, 0xac], &[(0, 81)]),
            // `(Number) x` at pc 1 (line 239, never fails) and pc 5 (line 247).
            java(
                "conv",
                CONV,
                vec![0x2a, 0xc0, 0, 8, 0x2a, 0xc0, 0, 8, 0xb0],
                &[(0, 239), (4, 247)],
            ),
        ],
    );
    // `VC.unbox(null, false)` at bci 2.
    let user = add_class(
        &mut store,
        "p/Unboxer",
        Some(object),
        pool(&[("p/VC", "unbox", UNBOX)]),
        vec![java("use", "()V", vec![0x01, 0x03, 0xb8, 0, 6, 0x57, 0xb1], &[(0, 3)])],
    );
    let cast = ServedRaiseHopSite::LocalCheckcast {
        class_name: "java/lang/Number",
    };
    let chain = |unbox_site, conv_site| ServedRaiseChain {
        throwable_class: CCE,
        hops: vec![
            ServedRaiseHop {
                class: "p/VC",
                name: "unbox",
                descriptor: UNBOX,
                site: unbox_site,
            },
            ServedRaiseHop {
                class: "p/VC",
                name: "conv",
                descriptor: CONV,
                site: conv_site,
            },
        ],
    };
    let call_conv = ServedRaiseHopSite::LastCall {
        name: "conv",
        descriptor: Some(CONV),
    };
    let with = |chain: &ServedRaiseChain, throwable: &str| -> Option<Vec<String>> {
        let hints = ServedRaiseHints {
            chain: Some(chain),
            ..ServedRaiseHints::default()
        };
        expand_at(&store, (user, "p/Unboxer", "use"), 2, throwable, hints)
    };
    let good = chain(call_conv, cast);
    assert_eq!(
        with(&good, CCE),
        Some(vec!["unbox:81@1".to_string(), "conv:247@5".to_string()])
    );
    // Another throwable: no cast site, and nothing constructs an NPE.
    assert_eq!(with(&good, NPE), None);
    // Not innermost: the chain is void (the one-hop rule finds nothing).
    assert_eq!(with(&chain(cast, call_conv), CCE), None);
}
