use super::*;
use crate::classloading::{ClassLoaderId, ClassState};
use cratonvm_reader::attribute::{Attribute, CodeAttribute, LazyAttribute, LineNumberEntry};
use cratonvm_reader::class_access_flags::{ClassAccessFlags, MethodAccessFlags};
use cratonvm_reader::class_file_version::ClassFileVersion;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

const INTERRUPTED: &str = "java/lang/InterruptedException";
const MONITOR: &str = "java/lang/IllegalMonitorStateException";

/// Methodref `k` of `refs` lands at constant-pool index `6k + 6`.
pub(super) fn pool(refs: &[(&str, &str, &str)]) -> ConstantPool {
    let mut entries = vec![ConstantPoolEntry::Tombstone];
    for (k, (owner, name, descriptor)) in refs.iter().enumerate() {
        let base = (1 + 6 * k) as u16;
        entries.push(ConstantPoolEntry::Utf8((*owner).into()));
        entries.push(ConstantPoolEntry::ClassReference { name_index: base });
        entries.push(ConstantPoolEntry::Utf8((*name).into()));
        entries.push(ConstantPoolEntry::Utf8((*descriptor).into()));
        entries.push(ConstantPoolEntry::NameAndType {
            name_index: base + 2,
            descriptor_index: base + 3,
        });
        entries.push(ConstantPoolEntry::MethodReference {
            class_index: base + 1,
            name_and_type_index: base + 4,
        });
    }
    ConstantPool::new(entries)
}

pub(super) fn java(
    name: &str,
    descriptor: &str,
    code: Vec<u8>,
    lines: &[(u16, u16)],
) -> ClassFileMethod {
    let code = CodeAttribute {
        max_stack: 4,
        max_locals: 4,
        code: cratonvm_reader::ByteView::from_vec(code),
        exception_table: Vec::new(),
        attributes: vec![Attribute::LineNumberTable(
            lines
                .iter()
                .map(|&(start_pc, line_number)| LineNumberEntry {
                    start_pc,
                    line_number,
                })
                .collect(),
        )],
    };
    ClassFileMethod {
        access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
        name: Arc::from(name),
        descriptor: Arc::from(descriptor),
        attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(code)))],
    }
}

pub(super) fn native(name: &str, descriptor: &str) -> ClassFileMethod {
    ClassFileMethod {
        access_flags: MethodAccessFlags::PRIVATE | MethodAccessFlags::NATIVE,
        name: Arc::from(name),
        descriptor: Arc::from(descriptor),
        attributes: Vec::new(),
    }
}

pub(super) fn add_class(
    store: &mut ClassStore,
    name: &str,
    superclass: Option<ClassId>,
    constant_pool: ConstantPool,
    methods: Vec<ClassFileMethod>,
) -> ClassId {
    let id = store.next_id();
    let simple = name.rsplit('/').next().unwrap_or(name);
    let class = Class {
        id,
        loader_id: ClassLoaderId::Bootstrap,
        name: Arc::from(name),
        source_file: Some(format!("{simple}.java")),
        version: ClassFileVersion::JAVA_8,
        state: ClassState::Loaded,
        initializing_thread: None,
        constant_pool,
        access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        superclass,
        interfaces: Vec::new(),
        fields: Vec::new(),
        methods,
        first_field_index: 0,
        num_total_fields: 0,
        bootstrap_methods: Vec::new(),
        signature: None,
        annotations: Vec::new(),
        nest_host: None,
        nest_members: Vec::new(),
        record_components: Vec::new(),
        permitted_subclasses: Vec::new(),
        inner_classes: Vec::new(),
        enclosing_method: None,
        hidden: false,
        module_name: None,
        origin: cratonvm_classloading::ClassOrigin::default(),
        has_finalizer: false,
        code_source: None,
        array_info: None,
        init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
        record_object_methods: std::sync::atomic::AtomicU8::new(0),
    };
    store.add(class)    }

struct Fixture {
    store: ClassStore,
    main: ClassId,
    thread: ClassId,
}

/// `Object` / `Thread` as JDK 25 compiles them (lines as in its sources),
/// and a `probe/Main` whose `main()V` calls, at bci 1 / 5 / 9 / 13:
/// `Thread.sleep(long)`, `Worker.sleep(long)` (inherited),
/// `HiddenWorker.sleep(long)` (its own static hides `Thread`'s) and
/// `this.wait()`.
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
            // wait() { wait(0L); }
            java(
                "wait",
                "()V",
                vec![0x2a, 0x09, 0xb6, 0, 12, 0xb1],
                &[(0, 351)],
            ),
            // wait(long): the virtual-thread wait0 at pc 2, the platform one at pc 7.
            java(
                "wait",
                "(J)V",
                vec![0x2a, 0x1f, 0xb7, 0, 6, 0x2a, 0x1f, 0xb7, 0, 6, 0xb1],
                &[(0, 383), (5, 389)],
            ),
            native("wait0", "(J)V"),
        ],
    );
    let thread = add_class(
        &mut store,
        "java/lang/Thread",
        Some(object),
        pool(&[
            ("java/lang/Thread", "sleepNanos", "(J)V"),
            ("java/lang/Thread", "sleepNanos0", "(J)V"),
            ("java/lang/VirtualThread", "sleepNanos", "(J)V"),
        ]),
        vec![
            java(
                "sleep",
                "(J)V",
                vec![0x1e, 0xb8, 0, 6, 0xb1],
                &[(0, 535), (1, 540)],
            ),
            // The virtual-thread call (pc 1) is not a census method; the
            // `sleepNanos0` call is at pc 5.
            java(
                "sleepNanos",
                "(J)V",
                vec![0x1e, 0xb8, 0, 18, 0x1e, 0xb8, 0, 12, 0xb1],
                &[(0, 507), (4, 509)],
            ),
            native("sleepNanos0", "(J)V"),
        ],
    );
    add_class(
        &mut store,
        "probe/Worker",
        Some(thread),
        pool(&[]),
        Vec::new(),
    );
    add_class(
        &mut store,
        "probe/HiddenWorker",
        Some(thread),
        pool(&[]),
        vec![java("sleep", "(J)V", vec![0xb1], &[(0, 1)])],
    );
    let main = add_class(
        &mut store,
        "probe/Main",
        Some(object),
        pool(&[
            ("java/lang/Thread", "sleep", "(J)V"),
            ("probe/Worker", "sleep", "(J)V"),
            ("probe/Main", "wait", "()V"),
            ("probe/HiddenWorker", "sleep", "(J)V"),
        ]),
        vec![java(
            "main",
            "()V",
            vec![
                0x09, 0xb8, 0, 6, 0x09, 0xb8, 0, 12, 0x09, 0xb8, 0, 24, 0x2a, 0xb6, 0, 18, 0xb1,
            ],
            &[(0, 3), (4, 4), (8, 5), (12, 6)],
        )],
    );
    Fixture {
        store,
        main,
        thread,
    }
}

fn at(class_id: ClassId, name: &str, descriptor: &str, bci: i32) -> BacktraceFrame {
    BacktraceFrame::Entry(StackTraceEntry {
        class_name: Arc::from("unused"),
        method_name: Arc::from(name),
        method_descriptor: Some(Arc::from(descriptor)),
        source_file: None,
        line_number: LINE_NUMBER_UNKNOWN,
        byte_code_index: bci,
        class_id: Some(class_id),
        method_index: None,
    })
}

/// Interpreter round i2 wave 61, lane L3: the keyed families answer for a
/// METHOD a door selected, with no call site naming it: a census method
/// lists its chain down to the native leaf (the platform branch, or the
/// virtual-thread one), a native its own frame, a method that raises no
/// such throwable nothing; and a method that is not `@CallerSensitive`
/// has no caller-sensitive adapter.
#[test]
fn keyed_families_answer_for_a_method() {
    let fx = fixture();
    let store = &fx.store;
    let object = store
        .iter()
        .find(|c| &*c.name == "java/lang/Object")
        .map(|c| c.id)
        .expect("Object");
    let mut resolve =
        |_: ClassId, name: &str| store.iter().find(|c| &*c.name == name).map(|c| c.id);
    let rows = |frames: Option<Vec<BacktraceFrame>>| {
        frames.map(|frames| {
            frames
                .iter()
                .map(|f| {
                    let e = f.to_entry();
                    (format!("{}.{}", e.class_name, e.method_name), e.line_number)
                })
                .collect::<Vec<_>>()
        })
    };
    let hints = ServedRaiseHints::default();
    // `Object.wait(long)` (slot 1): `wait` at its platform `wait0` call,
    // then `wait0 (Native Method)`.
    assert_eq!(
        rows(keyed_served_raise_frames(
            store,
            (object, 1),
            MONITOR,
            false,
            hints,
            &mut resolve
        )),
        Some(vec![
            ("java/lang/Object.wait".to_string(), 389),
            ("java/lang/Object.wait0".to_string(), LINE_NUMBER_NATIVE),
        ])
    );
    // A virtual thread leaves from the first `wait0` call.
    assert_eq!(
        rows(keyed_served_raise_frames(
            store,
            (object, 1),
            MONITOR,
            true,
            hints,
            &mut resolve
        )),
        Some(vec![
            ("java/lang/Object.wait".to_string(), 383),
            ("java/lang/Object.wait0".to_string(), LINE_NUMBER_NATIVE),
        ])
    );
    // `wait0` itself (slot 2): its own native frame.
    assert_eq!(
        rows(keyed_served_raise_frames(
            store,
            (object, 2),
            MONITOR,
            false,
            hints,
            &mut resolve
        )),
        Some(vec![(
            "java/lang/Object.wait0".to_string(),
            LINE_NUMBER_NATIVE
        )])
    );
    // `Thread.sleep(long)` raises no `IllegalMonitorStateException`.
    assert_eq!(
        rows(keyed_served_raise_frames(
            store,
            (fx.thread, 0),
            MONITOR,
            false,
            hints,
            &mut resolve
        )),
        None
    );
    // A resource error is never a served raise.
    assert_eq!(
        rows(keyed_served_raise_frames(
            store,
            (object, 1),
            "java/lang/StackOverflowError",
            false,
            hints,
            &mut resolve
        )),
        None
    );
    assert_eq!(
        caller_sensitive_adapter_descriptor(store, fx.thread, "sleep", "(J)V"),
        None
    );
}

/// Interpreter round i1 wave 48, lane L1: an interrupted virtual-thread
/// sleep in the shape of HotSpot's default virtual threads lists
/// `VirtualThread.sleepNanos` at its first (before the sleep) or second
/// (after the park) `new InterruptedException()`, `Thread.sleepNanos` at
/// its call of it, and `Thread.sleep`; a `Thread.sleepNanos` frame that
/// ran (at its `sleepNanos0` call) is moved to that call.
#[test]
fn virtual_sleep_lists_the_continuation_throw() {
    let mut fx = fixture();
    let rows = |fx: &Fixture, innermost: &BacktraceFrame, during: bool| {
        let store = &fx.store;
        let mut resolve =
            |_: ClassId, name: &str| store.iter().find(|c| &*c.name == name).map(|c| c.id);
        virtual_sleep_continuation_frames(store, innermost, during, &mut resolve).map(
            |(replace, extra)| {
                let row = |f: &BacktraceFrame| {
                    let e = f.to_entry();
                    (
                        format!("{}.{}", e.class_name, e.method_name),
                        e.line_number,
                        e.byte_code_index,
                    )
                };
                (
                    replace.as_ref().map(row),
                    extra.iter().map(row).collect::<Vec<_>>(),
                )                },
        )
    };
    let caller = at(fx.main, "main", "()V", 1);
    // No `VirtualThread` loaded: nothing to read.
    assert_eq!(rows(&fx, &caller, false), None);
    let object = fx
        .store
        .iter()
        .find(|c| &*c.name == "java/lang/Object")
        .map(|c| c.id);
    add_class(
        &mut fx.store,
        "java/lang/VirtualThread",
        object,
        pool(&[(INTERRUPTED, "<init>", "()V")]),
        vec![java(
            "sleepNanos",
            "(J)V",
            vec![0x00, 0xb7, 0, 6, 0x00, 0xb7, 0, 6, 0xb1],
            &[(0, 971), (4, 982)],
        )],
    );
    let thread = |line: i32, bci: i32| ("java/lang/Thread.sleepNanos".to_string(), line, bci);
    let vt =
        |line: i32, bci: i32| ("java/lang/VirtualThread.sleepNanos".to_string(), line, bci);
    let sleep = ("java/lang/Thread.sleep".to_string(), 540, 1);
    assert_eq!(
        rows(&fx, &caller, false),
        Some((None, vec![sleep.clone(), thread(507, 1), vt(971, 1)]))
    );
    assert_eq!(
        rows(&fx, &caller, true),
        Some((None, vec![sleep, thread(507, 1), vt(982, 5)]))
    );
    // `Thread.sleepNanos` ran, standing at its `sleepNanos0` call (pc 5).
    let ran = at(fx.thread, "sleepNanos", "(J)V", 5);
    assert_eq!(
        rows(&fx, &ran, true),
        Some((Some(thread(507, 1)), vec![vt(982, 5)]))
    );
    // Any other call is not a sleep.
    assert_eq!(rows(&fx, &at(fx.main, "main", "()V", 13), true), None);
}

/// `(class.method, line, bci)` of each rebuilt frame, outermost first.
fn expand(
    fx: &Fixture,
    innermost: &BacktraceFrame,
    throwable: &str,
) -> Option<Vec<(String, i32, i32)>> {
    let store = &fx.store;
    let mut resolve =
        |_: ClassId, name: &str| store.iter().find(|c| &*c.name == name).map(|c| c.id);
    native_standin_frames(store, innermost, throwable, &mut resolve).map(|frames| {
        frames
            .iter()
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

/// `--compatible`: the registered `Thread.sleep` ran; all three JDK frames
/// come back, each Java level at its call of the next, the leaf native.
#[test]
fn a_registered_sleep_gets_hotspots_three_jdk_frames() {
    let fx = fixture();
    let got = expand(&fx, &at(fx.main, "main", "()V", 1), INTERRUPTED);
    assert_eq!(
        got,
        Some(vec![
            row("java/lang/Thread.sleep", 540, 1),
            row("java/lang/Thread.sleepNanos", 509, 5),
            row("java/lang/Thread.sleepNanos0", LINE_NUMBER_NATIVE, -1),
        ])
    );
}

/// `--real-jdk`: `sleep` and `sleepNanos` ran as bytecode; only the native
/// leaf is missing.
#[test]
fn real_sleep_bytecode_gets_only_the_native_leaf() {
    let fx = fixture();
    let got = expand(&fx, &at(fx.thread, "sleepNanos", "(J)V", 5), INTERRUPTED);
    assert_eq!(
        got,
        Some(vec![row(
            "java/lang/Thread.sleepNanos0",
            LINE_NUMBER_NATIVE,
            -1
        )])
    );
}

/// The leaf's own frame names its holder's source file, as HotSpot's does.
#[test]
fn the_native_leaf_names_the_source_file() {
    let fx = fixture();
    let frames = native_standin_frames(
        &fx.store,
        &at(fx.thread, "sleepNanos", "(J)V", 5),
        INTERRUPTED,
        &mut |_, name| fx.store.iter().find(|c| &*c.name == name).map(|c| c.id),
    )
    .expect("a leaf");
    assert_eq!(
        frames[0].to_entry().source_file.as_deref(),
        Some("Thread.java")
    );
}

/// Only the throwables raised INSIDE the leaf: an `IllegalArgumentException`
/// keeps the caller on top when `sleep`'s bytecode constructs none (round
/// 14 wave 2: it gets `sleep`'s own frame when it does, see
/// `an_argument_check_gets_the_one_frame_that_throws`).
#[test]
fn other_throwables_keep_the_caller_on_top() {
    let fx = fixture();
    let site = at(fx.main, "main", "()V", 1);
    assert_eq!(
        expand(&fx, &site, "java/lang/IllegalArgumentException"),
        None
    );
    assert_eq!(
        expand(&fx, &site, MONITOR),
        None,
        "IMSE is a wait-family throwable"
    );
}

/// A call qualified by a subclass resolves up to `Thread`, unless the
/// subclass hides `sleep` with its own static.
#[test]
fn a_subclass_qualified_call_resolves_unless_hidden() {
    let fx = fixture();
    let inherited = expand(&fx, &at(fx.main, "main", "()V", 5), INTERRUPTED);
    assert_eq!(inherited.map(|v| v.len()), Some(3));
    assert_eq!(
        expand(&fx, &at(fx.main, "main", "()V", 9), INTERRUPTED),
        None
    );
}

/// `wait()` -> `wait(long)` at its LAST `wait0` call (the platform-thread
/// branch) -> `wait0`, for both wait-family throwables.
#[test]
fn wait_rebuilds_the_platform_thread_chain() {
    let fx = fixture();
    for throwable in [INTERRUPTED, MONITOR] {
        let got = expand(&fx, &at(fx.main, "main", "()V", 13), throwable);
        assert_eq!(
            got,
            Some(vec![
                row("java/lang/Object.wait", 351, 2),
                row("java/lang/Object.wait", 389, 7),
                row("java/lang/Object.wait0", LINE_NUMBER_NATIVE, -1),
            ]),
            "{throwable}"
        );
    }
}

/// A frame not standing at an invoke, or an unknown bci, gets nothing.
#[test]
fn a_frame_not_at_an_invoke_gets_nothing() {
    let fx = fixture();
    assert_eq!(
        expand(&fx, &at(fx.main, "main", "()V", 0), INTERRUPTED),
        None
    );
    assert_eq!(
        expand(&fx, &at(fx.main, "main", "()V", -1), INTERRUPTED),
        None
    );
    assert!(!is_standin_throwable("java/lang/RuntimeException"));
    assert!(is_standin_throwable(INTERRUPTED));
}

/// Interpreter round i1 wave 46, lane L3: a reflective call's failed
/// argument check lists the accessor at the constructor call of its
/// arm's `new IllegalArgumentException` (the constructor accessor has
/// three: count, type, null to a primitive); a throwable of another class,
/// a check the accessor has no frame for, or a class that is not loaded
/// gets none.
#[test]
fn a_reflective_check_stands_at_its_arms_constructor_call() {
    use cratonvm_native_api::{ReflectiveCallKind, ReflectiveCheck};
    const IAE: &str = "java/lang/IllegalArgumentException";
    // Three times `new #2; dup; aconst_null; invokespecial #6; athrow`.
    let mut code: Vec<u8> = Vec::new();
    for _ in 0..3 {
        code.extend([0xbb, 0, 2, 0x59, 0x01, 0xb7, 0, 6, 0xbf]);
    }
    let mut store = ClassStore::new();
    let id = add_class(
        &mut store,
        "jdk/internal/reflect/DirectConstructorHandleAccessor",
        None,
        pool(&[(IAE, "<init>", "(Ljava/lang/Throwable;)V")]),
        vec![java(
            "newInstance",
            "([Ljava/lang/Object;)Ljava/lang/Object;",
            code,
            &[(0, 59), (9, 65), (18, 70)],
        )],
    );
    let class = store.get(id).unwrap();
    let method = &class.methods[0];
    assert_eq!(
        throwable_init_sites(&class.constant_pool, &method.code().unwrap().code, IAE),
        Some(vec![5, 14, 23])
    );
    let outer = standin_entry(class, id, method, 0, Some(0));
    let entries = vec![outer.clone(), outer.clone(), outer];
    let top = |throwable: &str, check: ReflectiveCheck| {
        reflective_check_entries(
            &store,
            ReflectiveCallKind::ConstructorNewInstance,
            &entries,
            throwable,
            check,
        )
        .map(|listed| {
            let top = listed.last().unwrap();
            (listed.len(), top.line_number, top.byte_code_index)
        })
    };
    assert_eq!(top(IAE, ReflectiveCheck::ArgumentCount), Some((3, 59, 5)));
    assert_eq!(top(IAE, ReflectiveCheck::ArgumentType), Some((3, 65, 14)));
    assert_eq!(
        top(IAE, ReflectiveCheck::NullToPrimitive('I')),
        Some((3, 70, 23))
    );
    // Not the check's throwable; no receiver on a constructor; the cause's
    // `ValueConversions` is not loaded here.
    assert_eq!(
        top(
            "java/lang/IllegalStateException",
            ReflectiveCheck::ArgumentCount
        ),
        None
    );
    assert_eq!(top(IAE, ReflectiveCheck::Receiver), None);
    assert_eq!(
        top(
            "java/lang/NullPointerException",
            ReflectiveCheck::NullToPrimitive('I')
        ),
        None
    );
    // Wave 47: a cast check's `IllegalArgumentException` is the type arm's;
    // its cause's `Class.cast` is not loaded here.
    let cast =
        ReflectiveCheck::ArgumentCast(cratonvm_native_api::ReflectiveArgumentCast::Reference);
    assert_eq!(top(IAE, cast), Some((3, 65, 14)));
    assert_eq!(top("java/lang/ClassCastException", cast), None);
}

/// Interpreter round i1 wave 47, lane L3: the `ClassCastException` cause
/// of a constructor's argument check lists, above the call's frames,
/// `Class.cast` for a reference parameter, and `unbox<Wrapper>` over
/// `primitiveConversion` at its `(Number) x` or over its `Class.cast`
/// for a primitive one (HotSpot 25.0.3's lines).
#[test]
fn a_constructor_argument_cast_cause_lists_where_hotspot_casts() {
    use cratonvm_native_api::ReflectiveArgumentCast as Cast;
    const CCE: &str = "java/lang/ClassCastException";
    let mut store = ClassStore::new();
    add_class(
        &mut store,
        "java/lang/Class",
        None,
        pool(&[(CCE, "<init>", "(Ljava/lang/String;)V")]),
        // new #2; dup; aconst_null; invokespecial #6; athrow
        vec![java(
            "cast",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            vec![0xbb, 0, 2, 0x59, 0x01, 0xb7, 0, 6, 0xbf],
            &[(0, 3492)],
        )],
    );
    add_class(
        &mut store,
        VALUE_CONVERSIONS_CLASS,
        None,
        pool(&[
            (
                VALUE_CONVERSIONS_CLASS,
                "primitiveConversion",
                "(Lsun/invoke/util/Wrapper;Ljava/lang/Object;Z)Ljava/lang/Number;",
            ),
            ("java/lang/Number", "intValue", "()I"),
            (
                "java/lang/Class",
                "cast",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
            ),
        ]),
        vec![
            // aload_0; invokestatic #6; invokevirtual #12; ireturn
            java(
                "unboxInteger",
                "(Ljava/lang/Object;Z)I",
                vec![0x2a, 0xb8, 0, 6, 0xb6, 0, 12, 0xac],
                &[(0, 81)],
            ),
            // JDK 25's shape: the `instanceof Number` arm's cast (line
            // 239, never fails), the final `else`'s (247), then the
            // cast of `Class.cast`'s result (252):
            // aload_0; checkcast #8; astore_1; aload_0; checkcast #8;
            // astore_1; aload_0; invokevirtual #18; checkcast #8; areturn
            java(
                "primitiveConversion",
                "(Lsun/invoke/util/Wrapper;Ljava/lang/Object;Z)Ljava/lang/Number;",
                vec![
                    0x2a, 0xc0, 0, 8, 0x4c, 0x2a, 0xc0, 0, 8, 0x4c, 0x2a, 0xb6, 0, 18, 0xc0, 0,
                    8, 0xb0,
                ],
                &[(0, 239), (5, 247), (10, 252)],
            ),
        ],
    );
    let lines = |cast: Cast| {
        argument_cast_frames(&store, cast).map(|frames| {
            frames
                .iter()
                .map(|e| format!("{}:{}", e.method_name, e.line_number))
                .collect::<Vec<_>>()
        })
    };
    assert_eq!(lines(Cast::Reference), Some(vec!["cast:3492".to_string()]));
    assert_eq!(
        lines(Cast::NotANumber('I')),
        Some(vec![
            "unboxInteger:81".to_string(),
            "primitiveConversion:247".to_string()
        ])
    );
    assert_eq!(
        lines(Cast::Unconvertible('I')),
        Some(vec![
            "unboxInteger:81".to_string(),
            "primitiveConversion:252".to_string(),
            "cast:3492".to_string(),
        ])
    );
    // No `unboxLong` in the fixture.
    assert_eq!(lines(Cast::Unconvertible('J')), None);
}

/// Wave 46, lane L3: nested `new`s of one class pair with their own
/// constructor calls (the inner call completes first).
#[test]
fn nested_throwable_sites_pair_with_their_own_constructor_calls() {
    const IAE: &str = "java/lang/IllegalArgumentException";
    let cp = pool(&[(IAE, "<init>", "(Ljava/lang/Throwable;)V")]);
    // new #2; dup; new #2; dup; aconst_null; invokespecial #6;
    // invokespecial #6; athrow
    let code = [
        0xbb, 0, 2, 0x59, 0xbb, 0, 2, 0x59, 0x01, 0xb7, 0, 6, 0xb7, 0, 6, 0xbf,
    ];
    assert_eq!(throwable_init_sites(&cp, &code, IAE), Some(vec![12, 9]));
    assert_eq!(
        throwable_init_sites(&cp, &code, "java/lang/Other"),
        Some(vec![])
    );
}

/// Round 14 wave 2 (lane trace): a `Thread` whose `sleep(long)` is `sleep`
/// (bytecode with `IllegalArgumentException` sites at the pcs given, line
/// 537 from pc 0) or native (`None`), and a `probe/Main.main()V` calling it
/// at bci 1.
fn arg_check_fixture(iae_sites: Option<&[usize]>) -> Fixture {
    let mut store = ClassStore::new();
    let object = add_class(&mut store, "java/lang/Object", None, pool(&[]), Vec::new());
    let sleep = match iae_sites {
        None => {
            let mut m = native("sleep", "(J)V");
            m.access_flags |= MethodAccessFlags::STATIC;
            m
        }
        Some(sites) => {
            // `new #2; dup; invokespecial #6; athrow` per site, padded
            // with `nop` up to each site's `invokespecial` pc.
            let mut code = Vec::new();
            for &site in sites {
                while code.len() + 4 < site {
                    code.push(0x00);
                }
                code.extend_from_slice(&[0xbb, 0, 2, 0x59, 0xb7, 0, 6, 0xbf]);
            }
            code.push(0xb1);
            java("sleep", "(J)V", code, &[(0, 537)])
        }
    };
    let thread = add_class(
        &mut store,
        "java/lang/Thread",
        Some(object),
        pool(&[(
            STANDIN_ARG_CHECK_THROWABLE,
            "<init>",
            "(Ljava/lang/String;)V",
        )]),
        vec![sleep],
    );
    let main = add_class(
        &mut store,
        "probe/Main",
        Some(object),
        pool(&[("java/lang/Thread", "sleep", "(J)V")]),
        vec![java("main", "()V", vec![0x09, 0xb8, 0, 6, 0xb1], &[(0, 3)])],
    );
    Fixture {
        store,
        main,
        thread,
    }
}

/// HotSpot's `Thread.sleep(-1)`: ONE frame, `sleep` at its
/// `IllegalArgumentException` construction; no leaf frames.
#[test]
fn an_argument_check_gets_the_one_frame_that_throws() {
    let fx = arg_check_fixture(Some(&[4]));
    let site = at(fx.main, "main", "()V", 1);
    assert_eq!(
        expand(&fx, &site, STANDIN_ARG_CHECK_THROWABLE),
        Some(vec![row("java/lang/Thread.sleep", 537, 4)])
    );
    // The leaf census is untouched by it.
    assert_eq!(expand(&fx, &site, INTERRUPTED), None);
    assert!(is_standin_throwable(STANDIN_ARG_CHECK_THROWABLE));
    let _ = fx.thread;
}

/// Two construction sites are ambiguous (no frame); a `native` `sleep`
/// (JDK 17) is the frame itself, at `(Native Method)`.
#[test]
fn an_ambiguous_or_native_argument_check() {
    let fx = arg_check_fixture(Some(&[4, 12]));
    assert_eq!(
        expand(
            &fx,
            &at(fx.main, "main", "()V", 1),
            STANDIN_ARG_CHECK_THROWABLE
        ),
        None
    );
    let fx = arg_check_fixture(None);
    assert_eq!(
        expand(
            &fx,
            &at(fx.main, "main", "()V", 1),
            STANDIN_ARG_CHECK_THROWABLE
        ),
        Some(vec![row("java/lang/Thread.sleep", LINE_NUMBER_NATIVE, -1)])
    );
}
