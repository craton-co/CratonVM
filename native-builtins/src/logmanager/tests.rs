use super::*;
use crate::test_utils::mock_ctx;
use cratonvm_native_api::NativeMethodRegistry;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::Value;

static LOGF_SECOND_OLD: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static LOGF_SECOND_NEW: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static LOGF_THROWABLE_OLD: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
static LOGF_THROWABLE_NEW: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
static LOGF_SECOND_SEEN: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
static LOGF_TOSTRING_CALLS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

fn relocating_logf_to_string(
    ctx: &mut crate::test_utils::MockNativeContext,
    receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if method_name != "toString" {
        return None;
    }
    use std::sync::atomic::Ordering;
    let call = LOGF_TOSTRING_CALLS.fetch_add(1, Ordering::SeqCst);
    if call == 0 {
        ctx.remap_native_pin_addr_for_test(
            LOGF_SECOND_OLD.load(Ordering::SeqCst),
            LOGF_SECOND_NEW.load(Ordering::SeqCst),
        );
        ctx.remap_native_pin_addr_for_test(
            LOGF_THROWABLE_OLD.load(Ordering::SeqCst),
            LOGF_THROWABLE_NEW.load(Ordering::SeqCst),
        );
    } else if call == 1 {
        LOGF_SECOND_SEEN.store(receiver.as_ptr() as usize, Ordering::SeqCst);
    }
    let rendered = ctx.create_string(if call == 0 { "first" } else { "second" });
    Some(Ok(Some(Value::Object(Some(rendered)))))
}

// Tests that mutate the singleton + registry share process-wide
// state; guard them with a mutex so parallel threads don't race.
fn test_lock() -> &'static std::sync::Mutex<()> {
    static L: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    L.get_or_init(|| std::sync::Mutex::new(()))
}

#[test]
fn t19_h3_register_logmanager_natives_registers_all_expected_entries() {
    let mut r = NativeMethodRegistry::new();
    register_logmanager_natives(&mut r);

    assert!(r
        .find(
            CLS_JUL_LOG_MANAGER,
            "getLogManager",
            "()Ljava/util/logging/LogManager;"
        )
        .is_some());
    assert!(r
        .find(
            CLS_JUL_LOG_MANAGER,
            "getLogger",
            "(Ljava/lang/String;)Ljava/util/logging/Logger;"
        )
        .is_some());
    assert!(r
        .find(
            CLS_JUL_LOG_MANAGER,
            "addLogger",
            "(Ljava/util/logging/Logger;)Z"
        )
        .is_some());
    assert!(r
        .find(CLS_JUL_LOG_MANAGER, "readConfiguration", "()V")
        .is_some());
    assert!(r.find(CLS_JUL_LOG_MANAGER, "reset", "()V").is_some());
    assert!(r
        .find(
            CLS_JUL_LOG_MANAGER,
            "getLoggerNames",
            "()Ljava/util/Enumeration;"
        )
        .is_some());
    // JBoss subclass registered too.
    assert!(r
        .find(
            CLS_JBOSS_LOG_MANAGER,
            "getLogManager",
            "()Ljava/util/logging/LogManager;"
        )
        .is_some());
    assert!(r.find(CLS_JBOSS_LOG_MANAGER, "<init>", "()V").is_some());
    assert!(r
        .find(
            "org/jboss/logmanager/LogContext",
            "addCloseHandler",
            "(Ljava/lang/AutoCloseable;)V"
        )
        .is_some());
    assert!(r
        .find(
            "org/jboss/logmanager/LogContext",
            "getCloseHandlers",
            "()Ljava/util/Set;"
        )
        .is_some());
    // Enumeration wrapper.
    assert!(r
        .find(CLS_LOGGER_ENUMERATION, "hasMoreElements", "()Z")
        .is_some());
    assert!(r
        .find(
            CLS_LOGGER_ENUMERATION,
            "nextElement",
            "()Ljava/lang/Object;"
        )
        .is_some());
}

#[test]
fn do_logf_refreshes_later_params_after_earlier_to_string_moves_them() {
    use std::sync::atomic::Ordering;

    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    let mut ctx = mock_ctx();
    let first = ctx.fresh_object_ref();
    let second_old = ctx.fresh_object_ref();
    let second_new = ctx.fresh_object_ref();
    let throwable_old = ctx.fresh_object_ref();
    let throwable_new = ctx.fresh_object_ref();
    let params = ctx.new_ref_array(cratonvm_types::ClassId::new(0), 2);
    ctx.set_array_element(params, 0, Value::Object(Some(first)));
    ctx.set_array_element(params, 1, Value::Object(Some(second_old)));
    let format = ctx.create_string("%s %s");

    LOGF_SECOND_OLD.store(second_old.as_ptr() as usize, Ordering::SeqCst);
    LOGF_SECOND_NEW.store(second_new.as_ptr() as usize, Ordering::SeqCst);
    LOGF_THROWABLE_OLD.store(throwable_old.as_ptr() as usize, Ordering::SeqCst);
    LOGF_THROWABLE_NEW.store(throwable_new.as_ptr() as usize, Ordering::SeqCst);
    LOGF_SECOND_SEEN.store(0, Ordering::SeqCst);
    LOGF_TOSTRING_CALLS.store(0, Ordering::SeqCst);
    ctx.set_invoke_virtual_hook(relocating_logf_to_string);

    native_jboss_logging_logger_do_logf(
        &mut ctx,
        &[
            Value::Object(None),
            Value::Object(None),
            Value::Object(None),
            Value::Object(Some(format)),
            Value::Object(Some(params)),
            Value::Object(Some(throwable_old)),
        ],
    )
    .unwrap();

    assert_eq!(LOGF_TOSTRING_CALLS.load(Ordering::SeqCst), 2);
    assert_eq!(
        LOGF_SECOND_SEEN.load(Ordering::SeqCst),
        second_new.as_ptr() as usize,
        "the second parameter must be re-read from its remapped native pin"
    );
    assert_eq!(ctx.native_pin_count_for_test(), 0);
}

// gc-common w12-b: per-thread state for the relocating JUL-config hook, so
// the test shares nothing with the rest of the suite beyond `test_lock`.
thread_local! {
    static W12B_JUL_SIZE_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static W12B_JUL_MOVED: std::cell::Cell<(usize, usize)> =
        const { std::cell::Cell::new((0, 0)) };
    static W12B_JUL_SET_FORMATTER: std::cell::RefCell<Vec<usize>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Inside the SECOND handler's `addHandler` (its second `size` call), move
/// the FIRST handler: every native pin on its address now names a fresh
/// object, as a moving collection would leave it. Records every
/// `setFormatter` receiver.
fn w12b_relocating_jul_config_hook(
    ctx: &mut crate::test_utils::MockNativeContext,
    receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    match method_name {
        "size" => {
            let call = W12B_JUL_SIZE_CALLS.with(|c| {
                let n = c.get();
                c.set(n + 1);
                n
            });
            if call == 1 {
                let vm = ctx.vm_identity();
                let first = logger_handlers(vm)
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get("")
                    .and_then(|list| list.first().map(|&(_, handler)| handler))
                    .expect("the first handler is registered on the root logger")
                    as usize;
                let moved = ctx.fresh_object_ref().as_ptr() as usize;
                ctx.remap_native_pin_addr_for_test(first, moved);
                W12B_JUL_MOVED.with(|c| c.set((first, moved)));
            }
            None
        }
        "setFormatter" => {
            W12B_JUL_SET_FORMATTER.with(|v| v.borrow_mut().push(receiver.as_ptr() as usize));
            Some(Ok(None))
        }
        _ => None,
    }
}

#[test]
fn w12b_jul_config_reads_created_handlers_through_their_roots() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(0xB12_0001);
    W12B_JUL_SIZE_CALLS.with(|c| c.set(0));
    W12B_JUL_MOVED.with(|c| c.set((0, 0)));
    W12B_JUL_SET_FORMATTER.with(|v| v.borrow_mut().clear());
    ctx.set_invoke_virtual_hook(w12b_relocating_jul_config_hook);

    let entries = vec![
        ("handlers".to_string(), "w12b.One, w12b.Two".to_string()),
        ("w12b.One.formatter".to_string(), "w12b.Fmt".to_string()),
    ];
    apply_jul_config_entries(&mut ctx, &entries).unwrap();

    let (first_old, first_new) = W12B_JUL_MOVED.with(std::cell::Cell::get);
    assert_ne!(first_old, 0, "the hook must have moved the first handler");
    let second = logger_handlers(0xB12_0001)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get("")
        .and_then(|list| list.get(1).map(|&(_, handler)| handler))
        .expect("the second handler is registered on the root logger")
        as usize;
    let receivers = W12B_JUL_SET_FORMATTER.with(|v| v.borrow().clone());
    assert_eq!(
        receivers,
        vec![first_new, second],
        "the configured formatter must reach the MOVED first handler, and the \
         default one the second handler"
    );
    assert_eq!(ctx.native_pin_count_for_test(), 0, "the scope must close");
}

#[test]
fn jul_explicit_handler_and_log_record_message_bridge() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    let logger = allocate_logger(&mut ctx, "org.example.capture").unwrap();
    let handler =
        try_alloc_concurrent_synthetic(&mut ctx, "java/util/logging/Handler", 0).unwrap();
    native_jul_logger_add_handler(
        &mut ctx,
        &[Value::Object(Some(logger)), Value::Object(Some(handler))],
    )
    .unwrap();
    assert_eq!(
        logger_handlers(TEST_VM)
            .lock()
            .unwrap()
            .get("org.example.capture")
            .map(Vec::len),
        Some(1)
    );

    let record =
        try_alloc_concurrent_synthetic(&mut ctx, "java/util/logging/LogRecord", 5).unwrap();
    ctx.set_field(record, 1, Value::Long(42));
    log_record_messages(TEST_VM)
        .lock()
        .unwrap()
        .insert(42, "captured banner".to_string());
    let message = native_jul_log_record_get_message(&mut ctx, &[Value::Object(Some(record))])
        .unwrap()
        .unwrap();
    let Value::Object(Some(message)) = message else {
        panic!("expected LogRecord message");
    };
    assert_eq!(ctx.read_string(message).as_deref(), Some("captured banner"));

    native_jul_logger_remove_handler(
        &mut ctx,
        &[Value::Object(Some(logger)), Value::Object(Some(handler))],
    )
    .unwrap();
    assert!(logger_handlers(TEST_VM)
        .lock()
        .unwrap()
        .get("org.example.capture")
        .is_some_and(Vec::is_empty));
}

#[test]
fn t19_h3_get_log_manager_returns_singleton_identity() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    let a = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
        Value::Object(Some(o)) => o,
        other => panic!("expected manager ObjectRef, got {:?}", other),
    };
    let b = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
        Value::Object(Some(o)) => o,
        other => panic!("expected manager ObjectRef, got {:?}", other),
    };
    assert_eq!(a, b, "getLogManager() must return the same singleton");
    // j27 wave 4 (lane TRACEROWS, FS3-4): exactly the real JDK width -- no
    // VM-owned slot pads it (the two that did were written, never read).
    assert_eq!(ctx.object_num_fields(a), LM_REAL_FIELDS);
    assert!(matches!(ctx.get_field(a, LM_FIELD_ROOT_LOGGER), Value::Object(None)));
}

#[test]
fn t19_h3_jboss_get_log_manager_returns_same_singleton() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    let jul = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
        Value::Object(Some(o)) => o,
        other => panic!("expected manager ObjectRef, got {:?}", other),
    };
    let jboss = match native_get_jboss_log_manager(&mut ctx, &[])
        .unwrap()
        .unwrap()
    {
        Value::Object(Some(o)) => o,
        other => panic!("expected manager ObjectRef, got {:?}", other),
    };
    assert_eq!(
        jul, jboss,
        "JUL and JBoss getLogManager() must share singleton"
    );
}

#[test]
fn jboss_log_context_singleton_has_tree_lock_for_real_bytecode() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    let log_context = ensure_jboss_log_context(&mut ctx).unwrap();
    assert!(
        matches!(ctx.get_field(log_context, 0), Value::Object(Some(_))),
        "real LogContext.addCloseHandler synchronizes on treeLock"
    );
}

#[test]
fn t19_h3_get_logger_is_idempotent_by_name() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    let mgr = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
        Value::Object(Some(o)) => o,
        _ => panic!(),
    };
    let name = ctx.create_string("com.example.App");
    let a = match native_get_logger(
        &mut ctx,
        &[Value::Object(Some(mgr)), Value::Object(Some(name))],
    )
    .unwrap()
    .unwrap()
    {
        Value::Object(Some(o)) => o,
        other => panic!("expected logger, got {:?}", other),
    };
    let b = match native_get_logger(
        &mut ctx,
        &[Value::Object(Some(mgr)), Value::Object(Some(name))],
    )
    .unwrap()
    .unwrap()
    {
        Value::Object(Some(o)) => o,
        _ => panic!(),
    };
    assert_eq!(a, b, "getLogger(same name) must return the same Logger");
    // Name field round-trips.
    let stored_name = match ctx.get_field(a, LOGGER_FIELD_NAME) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    assert_eq!(stored_name, "com.example.App");    }

#[test]
fn t19_h3_static_jul_get_logger_defaults_to_jul_logger() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    let name = ctx.create_string("com.example.App");
    let logger = match native_jul_static_get_logger(&mut ctx, &[Value::Object(Some(name))])
        .unwrap()
        .unwrap()
    {
        Value::Object(Some(o)) => o,
        other => panic!("expected logger, got {:?}", other),
    };
    let cid = ctx.class_id_of_object(logger);
    let class_name = ctx.class_name_of_id(cid).unwrap_or_default();
    assert_eq!(class_name, CLS_JUL_LOGGER);
}

/// gci i31/hp (`gci-i30gv-compatible-jul-console-sink-is-not-hotspots-
/// console-handler-20261015`): `Logger.getLogger` initializes the
/// LogManager first, as HotSpot's `demandLogger` does -- the primordial
/// configuration has run (the root's `ConsoleHandler` step) and the root
/// and `global` are registered -- so a handler-less program's records
/// reach the root's real `ConsoleHandler`, not the stdout console sink.
#[test]
fn gci_i31hp_static_get_logger_initializes_the_log_manager_first() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    assert!(jul_console_sink_applies(&ctx), "no LogManager yet: the old sink");
    let name = ctx.create_string("gci.hp.p");
    assert!(native_jul_static_get_logger(&mut ctx, &[Value::Object(Some(name))])
        .unwrap()
        .is_some());
    assert!(primordial_done(TEST_VM));
    {
        let reg = logger_registry(TEST_VM)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut names: Vec<&str> = reg.keys().map(|k| k.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["", "gci.hp.p", "global"]);
    }
    // Initialized: a record no handler took is dropped, as on HotSpot.
    assert!(!jul_console_sink_applies(&ctx));
    // The JBoss manager keeps its own door (no JDK primordial read).
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    ctx.set_system_property(
        "java.util.logging.manager",
        "org.jboss.logmanager.LogManager",
    );
    let name = ctx.create_string("gci.hp.q");
    let _ = native_jul_static_get_logger(&mut ctx, &[Value::Object(Some(name))]).unwrap();
    // gci i40/jb: no stdout sink under the JBoss manager either (default
    // `CRATONVM_JBOSS_NO_CONSOLE_SINK`); it still runs no primordial read.
    assert!(!jul_console_sink_applies(&ctx), "JBoss: no stdout sink (i40/jb)");
    assert!(!primordial_done(TEST_VM));
    reset_state_for_tests();
}

/// gci i38/jb (`gci-i32jh-compatible-jboss-and-juli-console-sinks-are-not-
/// hotspots-20261015`): under Tomcat's JULI LogManager no record reaches
/// the stdout console sink (HotSpot's JULI has no stdout path), even
/// before the JDK's primordial configuration ran; the JBoss arm and the
/// plain pre-initialization arm are unchanged. The test environment does
/// not set `CRATONVM_JULI_NO_CONSOLE_SINK`, so this is the default.
#[test]
fn gci_i38jb_juli_log_manager_has_no_console_sink() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    assert!(jul_console_sink_applies(&ctx), "plain, uninitialized: unchanged");
    ctx.set_system_property(
        "java.util.logging.manager",
        "org.apache.juli.ClassLoaderLogManager",
    );
    assert!(!primordial_done(TEST_VM));
    assert!(!jul_console_sink_applies(&ctx), "JULI: no stdout sink");
    ctx.set_system_property(
        "java.util.logging.manager",
        "org.jboss.logmanager.LogManager",
    );
    // gci i40/jb: the census decided (Batch 9b B-VERDICT (i)): off too.
    assert!(!jul_console_sink_applies(&ctx), "JBoss: no stdout sink (i40/jb)");
    reset_state_for_tests();
}

/// gci i38/jb (AGENTS.md: state the kind at the site): the JBoss console
/// stand-ins are registered under their own `Bridge` scope, so a caller's
/// ambient category cannot leak into them, and the scope is restored.
/// Every one of them is a retired shadow, so the stated `Bridge` is
/// re-tagged `SyntheticStub` (dropped under `--real-jdk`).
#[test]
fn gci_i38jb_jboss_console_stand_ins_state_their_kind() {
    use cratonvm_native_api::NativeKind;
    let mut r = NativeMethodRegistry::new();
    register_logmanager_natives(&mut r);
    // gci i40/jb: the four `org/jboss/logmanager/Logger` rows only; the
    // twelve backend `doLog` / `doLogf` rows are retired by default
    // (`gci_i40jb_backend_do_log_stand_ins_are_retired`).
    let triples: Vec<(&str, &str, &str)> = vec![
        ("org/jboss/logmanager/Logger", "logRaw", "(Lorg/jboss/logmanager/ExtLogRecord;)V"),
        ("org/jboss/logmanager/Logger", "logRaw", "(Ljava/util/logging/LogRecord;)V"),
        (
            "org/jboss/logmanager/Logger",
            "log",
            "(Ljava/util/logging/Level;Ljava/util/function/Supplier;)V",
        ),
        ("org/jboss/logmanager/Logger", "log", "(Ljava/util/logging/LogRecord;)V"),
    ];
    assert_eq!(triples.len(), 4);
    for (c, m, d) in triples {
        assert_eq!(
            r.kind_of(c, m, d),
            Some(NativeKind::SyntheticStub),
            "{c}.{m}{d}: a retired shadow registered under the stated Bridge scope"
        );
    }
}

/// gci i40/jb (`gci-i32jh-compatible-jboss-and-juli-console-sinks-are-not-
/// hotspots-20261015`, Batch 9b B-VERDICT (i)): with the default
/// `CRATONVM_JBOSS_NO_CONSOLE_SINK` (the test environment does not set
/// it) the six jboss-logging backends' `doLog` / `doLogf` stand-ins are
/// not registered, so the library's bytecode is the only body; the
/// `=0` arm's registrar still states `Bridge` and lands every row as a
/// retired-shadow `SyntheticStub` (the twelve the registry census lost).
#[test]
fn gci_i40jb_backend_do_log_stand_ins_are_retired() {
    use cratonvm_native_api::NativeKind;
    assert!(jboss_console_sink_off(), "default ON in the test environment");
    let do_log = "(Lorg/jboss/logging/Logger$Level;Ljava/lang/String;Ljava/lang/Object;[Ljava/lang/Object;Ljava/lang/Throwable;)V";
    let do_logf = "(Lorg/jboss/logging/Logger$Level;Ljava/lang/String;Ljava/lang/String;[Ljava/lang/Object;Ljava/lang/Throwable;)V";
    let classes = [
        "org/jboss/logging/JBossLogManagerLogger",
        "org/jboss/logging/JDKLogger",
        "org/jboss/logging/Slf4jLogger",
        "org/jboss/logging/Slf4jLocationAwareLogger",
        "org/jboss/logging/Log4j2Logger",
        "org/jboss/logging/Log4jLogger",
    ];
    let mut r = NativeMethodRegistry::new();
    register_logmanager_natives(&mut r);
    for cls in classes {
        for (m, d) in [("doLog", do_log), ("doLogf", do_logf)] {
            assert!(r.find(cls, m, d).is_none(), "{cls}.{m}{d}: retired by default");
        }
    }
    let mut k = NativeMethodRegistry::new();
            register_jboss_backend_do_log_stand_ins(&mut k);
    let mut n = 0;
    for cls in classes {
        for (m, d) in [("doLog", do_log), ("doLogf", do_logf)] {
            assert_eq!(k.kind_of(cls, m, d), Some(NativeKind::SyntheticStub), "{cls}.{m}");
            n += 1;
        }
    }
    assert_eq!(n, 12);
}

/// gci i40/jb: under jboss-logmanager's LogManager the JUL console-sink
/// predicate answers "no sink" (default), as under JULI; the plain
/// uninitialized arm is unchanged.
#[test]
fn gci_i40jb_jboss_log_manager_has_no_console_sink() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    assert!(jul_console_sink_applies(&ctx), "plain, uninitialized: unchanged");
    ctx.set_system_property(
        "java.util.logging.manager",
        "org.jboss.logmanager.LogManager",
    );
    assert!(!jul_console_sink_applies(&ctx), "JBoss: no stdout sink");
    ctx.set_system_property(
        "java.util.logging.manager",
        "org.apache.juli.ClassLoaderLogManager",
    );
    assert!(!jul_console_sink_applies(&ctx), "JULI: no stdout sink (i38/jb)");
    reset_state_for_tests();
}

/// gci i41/rv (`gci-i41rv-waves-i39-i40-review-residuals-FIXED-20261016` item 1):
/// the retired console-sink switches cannot re-enable either sink; the
/// primordial latch beside them remains independent.
#[test]
fn gci_i41rv_console_sink_switches_are_latched_per_vm() {
    const JULI: &str = concat!("CRATON", "VM_JULI_NO_CONSOLE_SINK");
    const JBOSS: &str = concat!("CRATON", "VM_JBOSS_NO_CONSOLE_SINK");
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let killed = cratonvm_types::flags::with_thread_overrides(
        &[(JULI, Some("0")), (JBOSS, Some("0"))],
        || console_sink_switches(TEST_VM),
    );
    assert_eq!(
        killed,
        ConsoleSinkSwitches {
            juli_off: true,
            jboss_off: true
        }
    );
    // Outside the overrides the latch still answers what it read.
    assert_eq!(console_sink_switches(TEST_VM), killed, "latched per VM");
    let mut ctx = mock_ctx();
    ctx.set_system_property(
        "java.util.logging.manager",
        "org.jboss.logmanager.LogManager",
    );
    assert!(!jul_console_sink_applies(&ctx), "JBoss keeps its retired sink policy");
    ctx.set_system_property(
        "java.util.logging.manager",
        "org.apache.juli.ClassLoaderLogManager",
    );
    assert!(!jul_console_sink_applies(&ctx), "JULI keeps its retired sink policy");
    assert!(!primordial_done(TEST_VM), "the primordial latch is its own field");
    reset_state_for_tests();
    assert_eq!(
        console_sink_switches(TEST_VM),
        ConsoleSinkSwitches {
            juli_off: true,
            jboss_off: true
        },
        "a dropped row reads the default again"
    );
    reset_state_for_tests();
}

/// The native override no longer selects by class name; JUL remains registered.
#[test]
fn gci_i41rv_vm_exec_drops_the_dead_jboss_do_log_disjunct() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../vm/src/vm/vm_exec/invoke_on_class_shared_inner.rs"
    );
    let src = std::fs::read_to_string(path).expect("VM dispatch source is readable");
    let dead = concat!("&& matches!(method_name, \"doLog\" ", "| \"doLogf\"))");
    assert!(!src.contains(dead), "the dead doLog/doLogf disjunct is back");
    assert!(src.contains("let check_override = method.is_abstract();"));
    let mut registry = NativeMethodRegistry::new();
    register_logmanager_natives(&mut registry);
    assert!(registry
        .find(CLS_JUL_LOGGER, "info", "(Ljava/lang/String;)V")
        .is_some());
}

/// gci i41/rv: one rule for the two manager names, shared by the
/// `*_requested` doors and the console-sink predicate's single read.
#[test]
fn gci_i41rv_manager_name_rules() {
    for jboss in ["org.jboss.logmanager.LogManager", "org/jboss/logmanager/LogManager"] {
        assert!(is_jboss_manager_name(Some(jboss)) && !is_tomcat_manager_name(Some(jboss)));
    }
    for juli in [
        "org.apache.juli.ClassLoaderLogManager",
        "org/apache/juli/ClassLoaderLogManager",
    ] {
        assert!(is_tomcat_manager_name(Some(juli)) && !is_jboss_manager_name(Some(juli)));
    }
    for other in [None, Some(""), Some("java.util.logging.LogManager")] {
        assert!(!is_jboss_manager_name(other) && !is_tomcat_manager_name(other));
    }
}

/// gci i40/jb: the two `org/jboss/logmanager/Logger` stand-ins that stay
/// registered print no sink line by default: `logRaw` / `log(LogRecord)`
/// return after an undelivered publish, and `log(Level, Supplier)` is
/// the JUL overload's gate-publish-drop body. Needles split with
/// `concat!` so this test's own text never matches them.
#[test]
fn gci_i40jb_jboss_logger_stand_ins_drop_what_no_handler_took() {
    let src = concat!(include_str!("../logmanager/mod.rs"), "\n", include_str!("../logmanager/per_vm_table.rs"), "\n", include_str!("../logmanager/native_jboss_logger_log_level.rs"), "\n", include_str!("../logmanager/register_jboss_backend_do_log_stand.rs")).replace("\r\n", "\n");
    for kept in [
        // gci i41/rv: both read the per-VM latch (`console_sink_switches`).
        concat!(
            "    if console_sink_switches(ctx.vm_identity()).jboss_off \u{7b}\n",
            "        return Ok(None);\n    \u{7d}\n",
            "    // Logger name: prefer record.loggerName"
        ),
        concat!(
            "    if console_sink_switches(ctx.vm_identity()).jboss_off \u{7b}\n",
            "        return native_jul_logger_log_supplier(ctx, args);\n    \u{7d}\n"
        ),
        concat!(
            "    if !jboss_console_sink_off() \u{7b}\n",
            "        register_jboss_backend_do_log_stand_ins(registry);\n    \u{7d}\n"
        ),
    ] {
        assert!(src.contains(kept), "{kept}");
    }
}

/// gci i32/jh (`i31-hp-report.md` review item 5): every JUL publish door
/// lets the `LogRecord` constructor's / filter's / handler's failure
/// propagate (HotSpot's `log(LogRecord)` has no catch), and none prints
/// the console line for a failed publish. Needles split with `concat!`
/// so this test's own text never matches them.
#[test]
fn gci_i32jh_jul_publish_failures_propagate() {
    let src = concat!(include_str!("../logmanager/mod.rs"), "\n", include_str!("../logmanager/per_vm_table.rs"), "\n", include_str!("../logmanager/native_jboss_logger_log_level.rs"), "\n", include_str!("../logmanager/register_jboss_backend_do_log_stand.rs")).replace("\r\n", "\n");
    for gone in [
        // `jul_log_parameterized` swallowed the failure into the sink.
        concat!("None, None, params, thrown)\n", "                // A handler that threw"),
        // `jul_trace_marker` dropped it.
        concat!("let _ =\n            publish_to_jul_", "handlers_full(ctx, this, level"),
        // `native_jul_logger_logp` printed the sink line for an `Err`.
        concat!("let disposed = result.as_ref().", "is_ok_and(|o| o.disposed());"),
    ] {
        assert!(!src.contains(gone), "{gone}");
    }
    for kept in [
        concat!("let disposed = !result.as_ref().", "is_ok_and(|o| !o.disposed());"),
        concat!("    ctx.unpin_native_roots(base_pin);\n", "    published.map(|()| None)\n"),
    ] {
        assert!(src.contains(kept), "{kept}");
    }
}

#[test]
fn t19_h3_static_jul_get_logger_honors_jboss_logmanager_property() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    ctx.set_system_property(
        "java.util.logging.manager",
        "org.jboss.logmanager.LogManager",
    );
    let name = ctx.create_string("com.example.App");
    let logger = match native_jul_static_get_logger(&mut ctx, &[Value::Object(Some(name))])
        .unwrap()
        .unwrap()
    {
        Value::Object(Some(o)) => o,
        other => panic!("expected logger, got {:?}", other),
    };
    let cid = ctx.class_id_of_object(logger);
    let class_name = ctx.class_name_of_id(cid).unwrap_or_default();
    assert_eq!(class_name, "org/jboss/logmanager/Logger");
}

#[test]
fn t19_h3_jboss_logger_get_handlers_returns_empty_array() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    let logger = get_or_create_jboss_logger(&mut ctx, "com.example.App").unwrap();
    let handlers =
        match native_jboss_logger_get_handlers(&mut ctx, &[Value::Object(Some(logger))])
            .unwrap()
            .unwrap()
        {
            Value::Object(Some(arr)) => arr,
            other => panic!("expected handler array, got {:?}", other),
        };
    assert_eq!(ctx.array_length(handlers), 0);
}

/// The JDK's own `$java.home/conf/logging.properties` is the JDK
/// `LogManager`'s implicit fallback and must not be taken for the JBoss
/// subclass, whose `readConfiguration()` override resolves a
/// `ConfiguratorFactory` through `ServiceLoader` instead and never reads
/// that file.
///
/// Importing it put `.level=INFO` on the ROOT, and since every logger
/// inherits the nearest ancestor's explicit level, that one entry became a
/// process-wide INFO floor — the thing that silenced
/// `AbstractQuarkusExtensionTest.traceCategories(...)`. Measured on
/// Temurin 25 + jboss-logmanager 3.2.2: `getEffectiveLevel()` on an
/// unconfigured logger is `Integer.MIN_VALUE`, not 800.
///
/// Both arms read the SAME file, so the difference this asserts is the
/// manager property and nothing else.
#[test]
fn the_jdk_conf_logging_properties_is_read_for_jul_and_skipped_for_jboss() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    let dir =
        std::env::temp_dir().join(format!("cratonvm-logmanager-conf-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("conf")).unwrap();
    std::fs::write(dir.join("conf/logging.properties"), b".level= INFO\n").unwrap();
    let java_home = dir.to_string_lossy().replace('\\', "/");

    // Arm 1: plain JUL — the fallback applies, so the root gets INFO and
    // every descendant inherits it.
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    ctx.set_system_property("java.home", &java_home);
    read_configuration_no_arg_impl(&mut ctx, &[]).unwrap();
    assert_eq!(
        jul_ancestor_explicit_level(ctx.vm_identity(), "any.logger"),
        Some(800),
        "the JDK fallback must still apply under the plain JUL manager"
    );

    // Arm 2: same file, same call, JBoss manager — nothing is imported.
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    ctx.set_system_property("java.home", &java_home);
    ctx.set_system_property(
        "java.util.logging.manager",
        "org.jboss.logmanager.LogManager",
    );
    read_configuration_no_arg_impl(&mut ctx, &[]).unwrap();
    assert_eq!(
        jul_ancestor_explicit_level(ctx.vm_identity(), "any.logger"),
        None,
        "the JBoss LogManager never reads $java.home/conf/logging.properties"
    );

    // An EXPLICITLY named config file is a deliberate act and is still
    // honoured under either manager — only the implicit fallback is gone.
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    ctx.set_system_property(
        "java.util.logging.manager",
        "org.jboss.logmanager.LogManager",
    );
    ctx.set_system_property(
        "java.util.logging.config.file",
        &format!("{java_home}/conf/logging.properties"),
    );
    read_configuration_no_arg_impl(&mut ctx, &[]).unwrap();
    assert_eq!(
        jul_ancestor_explicit_level(ctx.vm_identity(), "any.logger"),
        Some(800),
        "an explicitly designated config file is still read"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// An unconfigured JBoss logger stops at INFO, exactly like an
/// unconfigured JUL one.
///
/// **This test previously asserted the opposite**, on a measurement taken
/// with `quarkus-bootstrap-runner` on the classpath — where
/// `InitialConfigurator.getInitialLevel("")` hands the ROOT `Level.ALL`
/// and every logger inherits it. Re-run against jboss-logmanager 3.2.2
/// with NO `LogContextInitializer` provider on the classpath, the same
/// probe reads `root.effective=800`, `freshControl.effective=800`,
/// `freshControl.isLoggableTrace=false`. One application's configuration
/// had been written down as the library's default, in a constant, a test
/// name and a doc.
///
/// The control that separates them is DELETING THE PROVIDER from the
/// classpath, not reading harder: both arms are "jboss-logmanager under
/// its own LogManager", and only the SPI differs.
/// `the_initializers_root_level_is_inherited_by_every_logger` is the other
/// half — the same assertion with a provider installed.
#[test]
fn an_unconfigured_jboss_logger_stops_at_info_exactly_like_a_jul_one() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    let jboss = get_or_create_jboss_logger(&mut ctx, "probe.fresh").unwrap();
    let trace = make_level(&mut ctx, "TRACE", 400);
    let info = make_level(&mut ctx, "INFO", 800);
    assert_eq!(
        native_jboss_logger_is_loggable(
            &mut ctx,
            &[Value::Object(Some(jboss)), Value::Object(Some(trace))]
        )
        .unwrap(),
        Some(Value::Int(0)),
        "with no LogContextInitializer provider, TRACE is below the INFO seed"
    );
    assert_eq!(
        native_jboss_logger_is_loggable(
            &mut ctx,
            &[Value::Object(Some(jboss)), Value::Object(Some(info))]
        )
        .unwrap(),
        Some(Value::Int(1))
    );
    assert_eq!(
        native_jboss_logger_get_effective_level(&mut ctx, &[Value::Object(Some(jboss))])
            .unwrap(),
        Some(Value::Int(800)),
        "LoggerNode.<init> seeds effectiveLevel with Logger.INFO_INT"
    );
    // The JUL face, same level object, the SAME answer. Built through the
    // real allocator, not `make_logger`: `allocate_logger` initialises the
    // level slot to a null REFERENCE, where a bare `alloc_object` leaves
    // it `Int(0)` and the threshold walk reads that as "level 0", i.e.
    // everything loggable — the mock would agree for the wrong reason.
    let jul = get_or_create_logger(&mut ctx, "probe.fresh.jul").unwrap();
    assert_eq!(
        native_jul_logger_is_loggable(
            &mut ctx,
            &[Value::Object(Some(jul)), Value::Object(Some(trace))]
        )
        .unwrap(),
        Some(Value::Int(0)),
        "an unconfigured JUL logger must NOT be loggable at TRACE either"
    );
}

/// `isLoggable(Level.OFF)` is false however low the thresholds are.
///
/// `LoggerNode.isLoggableLevel` opens with `level != OFF_INT`, and no
/// comparison against a threshold can express that: `Level.OFF.intValue()`
/// is `Integer.MAX_VALUE`, at or above every threshold there is. A
/// threshold-only implementation answers `true` — the exact opposite —
/// and does so most confidently in the case where a caller has just
/// switched a category OFF.
#[test]
fn is_loggable_off_is_false_even_when_everything_else_is_enabled() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    let logger = get_or_create_jboss_logger(&mut ctx, "probe.off").unwrap();
    let all = make_level(&mut ctx, "ALL", i32::MIN);
    let off = make_level(&mut ctx, "OFF", i32::MAX);
    // Open the logger as wide as it goes first, so the OFF answer cannot
    // come from a threshold that happened to be high.
    native_jboss_logger_set_level(
        &mut ctx,
        &[Value::Object(Some(logger)), Value::Object(Some(all))],
    )
    .unwrap();
    assert_eq!(
        native_jboss_logger_is_loggable(
            &mut ctx,
            &[Value::Object(Some(logger)), Value::Object(Some(all))]
        )
        .unwrap(),
        Some(Value::Int(1)),
        "control: with the level at ALL, ALL is loggable"
    );
    assert_eq!(
        native_jboss_logger_is_loggable(
            &mut ctx,
            &[Value::Object(Some(logger)), Value::Object(Some(off))]
        )
        .unwrap(),
        Some(Value::Int(0)),
        "OFF is refused by name, not by threshold"
    );
}

/// The initializer's ROOT level is what every other logger inherits.
///
/// This is the mechanism behind the `Integer.MIN_VALUE` a Quarkus process
/// reports for every logger: `InitialConfigurator.getInitialLevel("")`
/// answers `Level.ALL` for the ROOT and null for everything else, and the
/// nearest-ancestor walk carries it down. Asserted here through the table
/// the SPI writes rather than through a mock ServiceLoader, so it holds
/// whatever route a provider is discovered by.
#[test]
fn the_initializers_root_level_is_inherited_by_every_logger() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    let deep = get_or_create_jboss_logger(&mut ctx, "a.b.c").unwrap();
    let trace = make_level(&mut ctx, "TRACE", 400);
    assert_eq!(
        native_jboss_logger_is_loggable(
            &mut ctx,
            &[Value::Object(Some(deep)), Value::Object(Some(trace))]
        )
        .unwrap(),
        Some(Value::Int(0)),
        "control: before the root is configured, TRACE is below INFO"
    );

    // What `apply_log_context_initializer` does for `getInitialLevel("")`.
    set_name_keyed_level(
        logger_explicit_levels(ctx.vm_identity()),
        "",
        Some(i32::MIN),
    );
    assert_eq!(
        native_jboss_logger_is_loggable(
            &mut ctx,
            &[Value::Object(Some(deep)), Value::Object(Some(trace))]
        )
        .unwrap(),
        Some(Value::Int(1)),
        "a level on the ROOT reaches a.b.c — this is the whole mechanism"
    );
    assert_eq!(
        native_jboss_logger_get_effective_level(&mut ctx, &[Value::Object(Some(deep))])
            .unwrap(),
        Some(Value::Int(i32::MIN))
    );
}

/// `getMinimumLevel` is a SECOND threshold, not a synonym for the level.
///
/// `LoggerNode.isLoggableLevel` is
/// `level != OFF && level >= effectiveMinLevel && level >= effectiveLevel`,
/// so a provider that raises the minimum silences a category that
/// `setLevel` alone would have enabled. Collapsing the two tables into one
/// would pass every other test in this file and lose exactly this.
#[test]
fn the_minimum_level_floor_outranks_an_explicit_set_level() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    let logger = get_or_create_jboss_logger(&mut ctx, "floor.probe").unwrap();
    let trace = make_level(&mut ctx, "TRACE", 400);
    let all = make_level(&mut ctx, "ALL", i32::MIN);

    native_jboss_logger_set_level(
        &mut ctx,
        &[Value::Object(Some(logger)), Value::Object(Some(all))],
    )
    .unwrap();
    assert_eq!(
        native_jboss_logger_is_loggable(
            &mut ctx,
            &[Value::Object(Some(logger)), Value::Object(Some(trace))]
        )
        .unwrap(),
        Some(Value::Int(1)),
        "control: setLevel(ALL) alone enables TRACE"
    );

    // What `apply_log_context_initializer` does for `getMinimumLevel`.
    set_name_keyed_level(logger_minimum_levels(ctx.vm_identity()), "floor", Some(800));
    assert_eq!(
        native_jboss_logger_is_loggable(
            &mut ctx,
            &[Value::Object(Some(logger)), Value::Object(Some(trace))]
        )
        .unwrap(),
        Some(Value::Int(0)),
        "an ancestor's minimum level vetoes the logger's own setLevel(ALL)"
    );
    // ...and the effective level still reports what setLevel wrote: the
    // floor gates delivery, it does not rewrite the level.
    assert_eq!(
        native_jboss_logger_get_effective_level(&mut ctx, &[Value::Object(Some(logger))])
            .unwrap(),
        Some(Value::Int(i32::MIN))
    );
}

/// The SPI must be asked ONCE, and its absence cached.
///
/// Real `LogContext.discoverDefaultInitializer0` resolves into a static.
/// Without a cached NEGATIVE, every logger creation in a process with no
/// provider — which is most of them — pays a full `ServiceLoader` scan of
/// the classpath. The mock cannot load `org.jboss.logmanager`, so this
/// asserts the shape that matters: the answer is remembered, and asking
/// again does not re-resolve.
#[test]
fn the_log_context_initializer_lookup_caches_its_negative() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    assert!(resolve_jboss_log_context_initializer(&mut ctx).is_none());
    assert_eq!(
        *jboss_initializer_cell(ctx.vm_identity())
            .lock()
            .unwrap_or_else(|e| e.into_inner()),
        Some(0),
        "the negative must be cached, not re-resolved per logger"
    );
    assert!(resolve_jboss_log_context_initializer(&mut ctx).is_none());
}

/// `setLevel` must be OBSERVABLE — through `getLevel`, through
/// `getEffectiveLevel`, and through `isLoggable`.
///
/// All three used to be constants on this class (`null`, `800`, `true`),
/// which is what made `AbstractQuarkusExtensionTest.overrideLoggerLevel`
/// (stash `getLevel()`, `setLevel(TRACE)`, restore) a no-op whose restore
/// also could not be checked.
#[test]
fn a_jboss_logger_level_round_trips_and_gates_is_loggable() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    let logger = get_or_create_jboss_logger(&mut ctx, "com.example.App").unwrap();
    let warning = make_level(&mut ctx, "WARNING", 900);
    let info = make_level(&mut ctx, "INFO", 800);
    let severe = make_level(&mut ctx, "SEVERE", 1000);

    assert_eq!(
        native_jboss_logger_get_level(&mut ctx, &[Value::Object(Some(logger))]).unwrap(),
        Some(Value::Object(None)),
        "a logger nobody configured has no level of its own"
    );
    native_jboss_logger_set_level(
        &mut ctx,
        &[Value::Object(Some(logger)), Value::Object(Some(warning))],
    )
    .unwrap();
    assert_eq!(
        native_jboss_logger_get_level(&mut ctx, &[Value::Object(Some(logger))]).unwrap(),
        Some(Value::Object(Some(warning))),
        "getLevel must hand back the very Level setLevel was given"
    );
    assert_eq!(
        native_jboss_logger_get_effective_level(&mut ctx, &[Value::Object(Some(logger))])
            .unwrap(),
        Some(Value::Int(900))
    );
    assert_eq!(
        native_jboss_logger_is_loggable(
            &mut ctx,
            &[Value::Object(Some(logger)), Value::Object(Some(info))]
        )
        .unwrap(),
        Some(Value::Int(0)),
        "INFO is below the configured WARNING threshold"
    );
    assert_eq!(
        native_jboss_logger_is_loggable(
            &mut ctx,
            &[Value::Object(Some(logger)), Value::Object(Some(severe))]
        )
        .unwrap(),
        Some(Value::Int(1))
    );

    // The restore half. `setLevel(null)` puts the logger back to
    // "inherits", which is what `afterAll` does with the stashed value.
    native_jboss_logger_set_level(
        &mut ctx,
        &[Value::Object(Some(logger)), Value::Object(None)],
    )
    .unwrap();
    assert_eq!(
        native_jboss_logger_get_level(&mut ctx, &[Value::Object(Some(logger))]).unwrap(),
        Some(Value::Object(None))
    );
    assert_eq!(
        native_jboss_logger_is_loggable(
            &mut ctx,
            &[Value::Object(Some(logger)), Value::Object(Some(info))]
        )
        .unwrap(),
        Some(Value::Int(1)),
        "with the override removed the logger is unconfigured again"
    );
}

/// A level set on an ancestor governs a descendant that has none of its
/// own — `probe.tree` at WARNING makes `probe.tree.kid` refuse INFO.
/// Measured: `tree.kid.effective=900`, `tree.kid.isLoggableInfo=false`.
#[test]
fn a_jboss_logger_inherits_its_nearest_ancestors_level() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    let parent = get_or_create_jboss_logger(&mut ctx, "probe.tree").unwrap();
    let kid = get_or_create_jboss_logger(&mut ctx, "probe.tree.kid").unwrap();
    let warning = make_level(&mut ctx, "WARNING", 900);
    let info = make_level(&mut ctx, "INFO", 800);
    let severe = make_level(&mut ctx, "SEVERE", 1000);
    native_jboss_logger_set_level(
        &mut ctx,
        &[Value::Object(Some(parent)), Value::Object(Some(warning))],
    )
    .unwrap();

    assert_eq!(
        native_jboss_logger_get_level(&mut ctx, &[Value::Object(Some(kid))]).unwrap(),
        Some(Value::Object(None)),
        "the child has no level OF ITS OWN"
    );
    assert_eq!(
        native_jboss_logger_get_effective_level(&mut ctx, &[Value::Object(Some(kid))]).unwrap(),
        Some(Value::Int(900)),
        "but it inherits the ancestor's"
    );
    assert_eq!(
        native_jboss_logger_is_loggable(
            &mut ctx,
            &[Value::Object(Some(kid)), Value::Object(Some(info))]
        )
        .unwrap(),
        Some(Value::Int(0))
    );
    assert_eq!(
        native_jboss_logger_is_loggable(
            &mut ctx,
            &[Value::Object(Some(kid)), Value::Object(Some(severe))]
        )
        .unwrap(),
        Some(Value::Int(1))
    );
}

/// `getParent` answered a constant null, so every logger looked like a
/// root. It is the immediate DOTTED predecessor — jboss-logmanager builds
/// the whole `LoggerNode` path, so `probe.fresh`'s parent is `probe` even
/// though nobody ever asked for `probe` (measured:
/// `level.freshControl.parentName=[probe]`) — and null only for the root,
/// which is what terminates a caller's walk.
#[test]
fn a_jboss_loggers_parent_is_its_immediate_dotted_predecessor() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    let kid = get_or_create_jboss_logger(&mut ctx, "probe.fresh").unwrap();
    let parent =
        match native_jboss_logger_get_parent(&mut ctx, &[Value::Object(Some(kid))]).unwrap() {
            Some(Value::Object(Some(p))) => p,
            other => panic!("expected a parent logger, got {other:?}"),
        };
    assert_eq!(read_jul_logger_name(&ctx, parent), "probe");
    assert_eq!(
        ctx.class_name_arc_of_id(ctx.class_id_of_object(parent))
            .as_deref(),
        Some("org/jboss/logmanager/Logger"),
        "the declared return type is org.jboss.logmanager.Logger — handing \
         back a JUL mirror is the ClassCastException this doc family began with"
    );

    let grandparent =
        match native_jboss_logger_get_parent(&mut ctx, &[Value::Object(Some(parent))]).unwrap()
        {
            Some(Value::Object(Some(p))) => p,
            other => panic!("expected the root logger, got {other:?}"),
        };
    assert_eq!(read_jul_logger_name(&ctx, grandparent), "");
    assert_eq!(
        native_jboss_logger_get_parent(&mut ctx, &[Value::Object(Some(grandparent))]).unwrap(),
        Some(Value::Object(None)),
        "the root has no parent — this is what makes a caller's walk finite"
    );
}

/// `getLoggerNames` read only the JUL registry, so under the JBoss manager
/// — where every factory mints into the OTHER registry — it enumerated the
/// loggers nobody had created and omitted every one that existed.
#[test]
fn get_logger_names_lists_jboss_shaped_loggers_too() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    get_or_create_jboss_logger(&mut ctx, "probe.a").unwrap();
    get_or_create_logger(&mut ctx, "plain.jul").unwrap();
    // A name in BOTH registries must appear once, not twice.
    get_or_create_jboss_logger(&mut ctx, "in.both").unwrap();
    get_or_create_logger(&mut ctx, "in.both").unwrap();

    let enumeration = match native_get_logger_names(&mut ctx, &[]).unwrap().unwrap() {
        Value::Object(Some(e)) => e,
        other => panic!("expected an Enumeration, got {other:?}"),
    };
    let arr = match ctx.get_field(enumeration, 0) {
        Value::Object(Some(a)) => a,
        other => panic!("expected a backing array, got {other:?}"),
    };
    let mut names: Vec<String> = Vec::new();
    for i in 0..ctx.array_length(arr) {
        if let Value::Object(Some(s)) = ctx.get_array_element(arr, i) {
            names.push(ctx.read_string(s).unwrap_or_default());
        }
    }
    assert!(
        names.iter().any(|n| n == "probe.a"),
        "a JBoss-shaped logger must be enumerated; got {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "plain.jul"),
        "and the JUL-shaped ones must not have been dropped; got {names:?}"
    );
    assert_eq!(
        names.iter().filter(|n| *n == "in.both").count(),
        1,
        "a name held by both registries is ONE logger name; got {names:?}"
    );
}

#[test]
fn t19_h3_add_logger_returns_true_then_false_on_duplicate() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    let mgr = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
        Value::Object(Some(o)) => o,
        _ => panic!(),
    };
    // Build a Logger manually.
    let logger =
        try_alloc_concurrent_synthetic(&mut ctx, CLS_JUL_LOGGER, LOGGER_NUM_FIELDS).unwrap();
    let name_obj = ctx.create_string("dup.logger");
    ctx.set_field(logger, LOGGER_FIELD_NAME, Value::Object(Some(name_obj)));

    let first = native_add_logger(
        &mut ctx,
        &[Value::Object(Some(mgr)), Value::Object(Some(logger))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(first, Value::Int(1));
    let second = native_add_logger(
        &mut ctx,
        &[Value::Object(Some(mgr)), Value::Object(Some(logger))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        second,
        Value::Int(0),
        "duplicate addLogger must return false"
    );
}

#[test]
fn tomcat0807_juli_add_logger_indexes_real_jdk_logger_name_field() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    let mgr = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
        Value::Object(Some(o)) => o,
        _ => panic!(),
    };

    // Tomcat JULI's ClassLoaderLogManager.addLogger receives real-JDK
    // Logger instances. On that layout slot 0 is `config`; the logger name
    // lives in the `name` field. A later LogManager.getLogger(name) must
    // find this same logger instead of creating a separate entry.
    let logger_cid = ctx.ensure_class_initialized(CLS_JUL_LOGGER).unwrap();
    let logger = ctx.alloc_object(logger_cid, LOGGER_NUM_FIELDS);
    let config_cid = ctx
        .ensure_class_initialized("java/util/logging/Logger$ConfigurationData")
        .unwrap();
    let config = ctx.alloc_object(config_cid, 1);
    let logger_name = "org.apache.catalina.core.AsyncContextImpl";
    let name_obj = ctx.create_string(logger_name);
    ctx.set_field(logger, LOGGER_FIELD_NAME, Value::Object(Some(config)));
    ctx.set_field_by_name(logger, "name", Value::Object(Some(name_obj)));

    let added = native_add_logger(
        &mut ctx,
        &[Value::Object(Some(mgr)), Value::Object(Some(logger))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(added, Value::Int(1));

    let lookup_name = ctx.create_string(logger_name);
    let looked_up = match native_get_logger(
        &mut ctx,
        &[Value::Object(Some(mgr)), Value::Object(Some(lookup_name))],
    )
    .unwrap()
    .unwrap()
    {
        Value::Object(Some(o)) => o,
        other => panic!("expected logger, got {:?}", other),
    };
    assert_eq!(
        looked_up, logger,
        "LogManager.getLogger(name) must see the real-JDK logger registered by addLogger"
    );
}

#[test]
fn t19_h3_add_logger_rejects_path_traversal_name() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    let mgr = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
        Value::Object(Some(o)) => o,
        _ => panic!(),
    };
    for bad in &[
        "../../../etc/passwd",
        "..\\windows\\system32",
        "C:\\evil",
        "foo\tbar",
        "foo\u{0000}bar",
    ] {
        let logger =
            try_alloc_concurrent_synthetic(&mut ctx, CLS_JUL_LOGGER, LOGGER_NUM_FIELDS)
                .unwrap();
        let name_obj = ctx.create_string(bad);
        ctx.set_field(logger, LOGGER_FIELD_NAME, Value::Object(Some(name_obj)));
        let r = native_add_logger(
            &mut ctx,
            &[Value::Object(Some(mgr)), Value::Object(Some(logger))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            r,
            Value::Int(0),
            "addLogger must reject suspicious name: {bad}"
        );
    }
}

/// `addLogger(null)` rejects by THROWING, not by answering `false`.
///
/// MEASURED on Temurin 25.0.4+7-LTS — `LogManager.getLogManager()
/// .addLogger(null)` throws `NullPointerException: Cannot invoke
/// "java.util.logging.Logger.getName()" because "logger" is null`, because
/// `LogManager.addLogger` opens with an unguarded `logger.getName()`. The
/// same run answers `false` for `addLogger(alreadyRegistered)`, which is
/// the SEPARATE rule `t19_h3_add_logger_returns_true_then_false_on_duplicate`
/// covers.
///
/// This row used to assert `Int(0)` — the old deliberate swallow, whose
/// own comment at `native_add_logger` conceded "the contract says NPE" and
/// kept `false` to keep bootstraps alive. That swallow was removed once it
/// was measured (it kept alive nothing a real JDK would have run), and this
/// row was not updated with it, so it went on pinning the behaviour the fix
/// deleted. `log_manager_add_logger_null_throws_instead_of_answering_false`
/// is the test that landed WITH the fix; the two agree now, and this one
/// stays because T19.H3 is a tracked family.
#[test]
fn t19_h3_add_logger_rejects_null_logger_argument() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    let mgr = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
        Value::Object(Some(o)) => o,
        _ => panic!(),
    };
    assert_npe(
        native_add_logger(&mut ctx, &[Value::Object(Some(mgr)), Value::Object(None)]),
        JUL_NPE_NULL_LOGGER,
        "LogManager.addLogger(null)",
    );
}

#[test]
fn t19_h3_read_configuration_is_graceful_no_op() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    assert!(native_read_configuration_no_arg(&mut ctx, &[])
        .unwrap()
        .is_none());
    assert!(
        native_read_configuration_with_stream(&mut ctx, &[Value::Object(None)])
            .unwrap()
            .is_none()
    );
}

/// `reset()` keeps every logger and resets their CONFIGURATION.
///
/// This test used to assert the opposite — that `reset()` empties the
/// registry — and passed, because the native did exactly that. HotSpot
/// 25 disagrees (see `native_reset`'s doc for the measurement): the name
/// list is byte-identical across `reset()`, and
/// `Logger.getLogger(name)` still answers the SAME object.
#[test]
fn t19_h3_reset_keeps_loggers_and_resets_their_configuration() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    let mgr = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
        Value::Object(Some(o)) => o,
        _ => panic!(),
    };
    // Register two loggers.
    for name in &["a.b.c", "d.e.f"] {
        let name_obj = ctx.create_string(name);
        let _ = native_get_logger(
            &mut ctx,
            &[Value::Object(Some(mgr)), Value::Object(Some(name_obj))],
        )
        .unwrap();
    }
    // JUL does NOT materialise the intermediate namespace nodes:
    // `LogManager.addLogger` links a new Logger to the nearest ancestor
    // that ALREADY has a Logger object, falling back to the root, and
    // re-parents existing descendants when an intermediate appears
    // later. Registering "a.b.c" and "d.e.f" therefore creates exactly
    // three entries -- the two requested loggers plus the shared root
    // -- and `getLogger("a.b.c").getParent()` is the ROOT, which is
    // what HotSpot answers. (This used to demand-create "a", "a.b",
    // "d" and "d.e" as well, so the registry held 7 entries and
    // `getParent()` returned a logger HotSpot never creates.)
    {
        let reg = logger_registry(TEST_VM)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut names: Vec<&str> = reg.keys().map(|k| k.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["", "a.b.c", "d.e.f", "global"]);
    }
    // Configure two of them, so "reset the configuration" has something
    // to reset — and configure the ROOT too, so "the root keeps a level"
    // is distinguishable from "the root was left alone".
    //
    // A marker object rather than a real `Level`: this crate's mock
    // context does not implement `static_field_index_by_name`, so
    // `resolve_standard_level` answers `None` for every name here. A
    // `Level.FINEST` assertion would be measuring the mock. Any object is
    // enough for the property under test, which is that the slot is
    // OVERWRITTEN.
    let marker = ctx.create_string("configured-level-marker");
    let a_before = get_or_create_logger(&mut ctx, "a.b.c").unwrap();
    ctx.set_field(a_before, LOGGER_FIELD_LEVEL, Value::Object(Some(marker)));
    let root_before = get_or_create_logger(&mut ctx, "").unwrap();
    ctx.set_field(root_before, LOGGER_FIELD_LEVEL, Value::Object(Some(marker)));

    native_reset(&mut ctx, &[Value::Object(Some(mgr))]).unwrap();

    {
        let reg = logger_registry(TEST_VM)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut names: Vec<&str> = reg.keys().map(|k| k.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            ["", "a.b.c", "d.e.f", "global"],
            "reset() must NOT drop loggers — HotSpot's name list is unchanged"
        );
    }
    // Identity survives: the application's own reference is still the
    // registry's. This is the half that made the old behaviour a silent
    // config split rather than just a short `getLoggerNames()`.
    let a_after = get_or_create_logger(&mut ctx, "a.b.c").unwrap();
    assert_eq!(
        a_before, a_after,
        "getLogger(name) must answer the same object across reset()"
    );
    // A named logger's level goes back to null (inherit).
    assert_eq!(
        ctx.get_field(a_after, LOGGER_FIELD_LEVEL),
        Value::Object(None),
        "reset() sets a named logger's level to null"
    );
    // The root's is re-derived from `Level.INFO` rather than left as the
    // caller set it. Both halves are asserted: it is no longer the marker
    // (so reset() did touch the root), and it is exactly what
    // `resolve_standard_level("INFO")` yields on this context — which is
    // the same expression `native_reset` uses, so the row stays true on a
    // real VM where that resolves to the actual `Level.INFO` singleton.
    let root_after = get_or_create_logger(&mut ctx, "").unwrap();
    let root_level = ctx.get_field(root_after, LOGGER_FIELD_LEVEL);
    assert_ne!(
        root_level,
        Value::Object(Some(marker)),
        "reset() must re-derive the root logger's level, not leave it configured"
    );
    assert_eq!(
        root_level,
        Value::Object(resolve_standard_level(&mut ctx, "INFO")),
        "reset() sets the root logger's level from Level.INFO"
    );
    // Singleton identity preserved.
    let again = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
        Value::Object(Some(o)) => o,
        _ => panic!(),
    };
    assert_eq!(mgr, again, "singleton must survive reset()");    }

#[test]
fn t19_h3_get_logger_names_returns_snapshot_enumeration() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    let mgr = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
        Value::Object(Some(o)) => o,
        _ => panic!(),
    };
    for name in &["foo", "bar", "baz"] {
        let n = ctx.create_string(name);
        let _ = native_get_logger(
            &mut ctx,
            &[Value::Object(Some(mgr)), Value::Object(Some(n))],
        )
        .unwrap();
    }
    let enumeration = match native_get_logger_names(&mut ctx, &[Value::Object(Some(mgr))])
        .unwrap()
        .unwrap()
    {
        Value::Object(Some(o)) => o,
        _ => panic!(),
    };
    let mut observed = Vec::new();
    loop {
        let has = native_enumeration_has_more(&mut ctx, &[Value::Object(Some(enumeration))])
            .unwrap()
            .unwrap();
        if matches!(has, Value::Int(0)) {
            break;
        }
        let elem = native_enumeration_next(&mut ctx, &[Value::Object(Some(enumeration))])
            .unwrap()
            .unwrap();
        match elem {
            Value::Object(Some(s)) => {
                observed.push(ctx.read_string(s).unwrap_or_default());
            }
            _ => break,
        }
    }
    observed.sort();
    // The root logger "" is always present: `get_or_create_logger` links
    // every logger to a real parent chain terminating at it (5bfc73479),
    // and real `LogManager.getLoggerNames()` likewise always enumerates
    // the root. These three names have no dots, so "" is the only
    // ancestor added.
    //
    // `global` is present because `getLogManager()` registers it, which is
    // what the JDK does — `LogManager.ensureLogManagerInitialized` adds
    // both the root and `Logger.global`. Measured on HotSpot 25: a fresh
    // manager enumerates `[, global]`, and this set enumerates
    // `[, bar, baz, foo, global]`.
    let mut expected = vec![
        String::new(),
        "bar".to_string(),
        "baz".to_string(),
        "foo".to_string(),
        "global".to_string(),
    ];
    expected.sort();
    assert_eq!(observed, expected);
}

#[test]
fn t19_h3_is_valid_logger_name_accepts_and_rejects() {
    assert!(is_valid_logger_name(""));
    assert!(is_valid_logger_name("com.example.App"));
    assert!(is_valid_logger_name("my-app.subsystem_1"));
    assert!(!is_valid_logger_name("../etc"));
    assert!(!is_valid_logger_name("foo\\bar"));
    assert!(!is_valid_logger_name("C:drive"));
    assert!(!is_valid_logger_name("has\rcr"));
    assert!(!is_valid_logger_name(&"a".repeat(513)));
}

#[test]
fn t19_h3_get_logger_with_bad_name_returns_anonymous_not_cached() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    let mgr = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
        Value::Object(Some(o)) => o,
        _ => panic!(),
    };
    let bad = ctx.create_string("../bad");
    let l1 = match native_get_logger(
        &mut ctx,
        &[Value::Object(Some(mgr)), Value::Object(Some(bad))],
    )
    .unwrap()
    .unwrap()
    {
        Value::Object(Some(o)) => o,
        _ => panic!(),
    };
    let l2 = match native_get_logger(
        &mut ctx,
        &[Value::Object(Some(mgr)), Value::Object(Some(bad))],
    )
    .unwrap()
    .unwrap()
    {
        Value::Object(Some(o)) => o,
        _ => panic!(),
    };
    assert_ne!(
        l1, l2,
        "bad names must not be cached (each call allocates a throw-away Logger)"
    );
    // Registry untouched — it still holds exactly what `getLogManager()`
    // put there (the root and `global`, as the JDK does), and the rejected
    // name was not added under any spelling.
    //
    // Asserting the CONTENTS rather than `is_empty()` is what keeps this
    // test meaning what it says now that `getLogManager()` registers
    // `global`: an emptiness check would have had to be RELAXED to keep
    // passing, while this one gets stricter.
    {
        let reg = logger_registry(TEST_VM)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut names: Vec<&str> = reg.keys().map(|k| k.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["", "global"]);
    }
}

#[test]
fn t19_h3_get_property_returns_null_for_any_key() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    let key = ctx.create_string("foo");
    let v = native_get_property(&mut ctx, &[Value::Object(None), Value::Object(Some(key))])
        .unwrap()
        .unwrap();
    assert!(matches!(v, Value::Object(None)));
}

// -- Block 2B: -Djava.util.logging.manager honoured by getLogManager --

#[test]
fn block_2b_property_unset_returns_default_class_singleton() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    // Property absent — expected to return the default JDK class.
    let obj = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
        Value::Object(Some(o)) => o,
        other => panic!("expected manager ObjectRef, got {:?}", other),
    };
    let cid = ctx.class_id_of_object(obj);
    let name = ctx.class_name_of_id(cid).unwrap_or_default();
    assert_eq!(
        name, CLS_JUL_LOG_MANAGER,
        "no property set => default LogManager class"
    );
}

#[test]
fn block_2b_property_set_to_subclass_returns_named_class() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    ctx.set_system_property("java.util.logging.manager", "LmSubclass$MyLm");
    let obj = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
        Value::Object(Some(o)) => o,
        other => panic!("expected manager ObjectRef, got {:?}", other),
    };
    let cid = ctx.class_id_of_object(obj);
    let name = ctx.class_name_of_id(cid).unwrap_or_default();
    assert_eq!(
        name, "LmSubclass$MyLm",
        "property set => getLogManager returns instance of named class"
    );
}

#[test]
fn block_2b_property_built_in_alias_falls_through_to_singleton() {
    // The JDK class name short-circuits through the pre-allocated default
    // singleton path. This pins the contract that
    // `try_allocate_property_log_manager` returns `None` for the JDK alias
    // so the existing synthetic field layout is preserved.
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    ctx.set_system_property("java.util.logging.manager", "java.util.logging.LogManager");
    let obj = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
        Value::Object(Some(o)) => o,
        _ => panic!(),
    };
    let cid = ctx.class_id_of_object(obj);
    let name = ctx.class_name_of_id(cid).unwrap_or_default();
    assert_eq!(name, CLS_JUL_LOG_MANAGER);
    // Singleton identity preserved across calls.
    let obj2 = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
        Value::Object(Some(o)) => o,
        _ => panic!(),
    };
    assert_eq!(obj, obj2);    }

#[test]
fn block_2b_property_jboss_alias_returns_jboss_class_singleton() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    ctx.set_system_property(
        "java.util.logging.manager",
        "org.jboss.logmanager.LogManager",
    );
    let obj = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
        Value::Object(Some(o)) => o,
        other => panic!("expected manager ObjectRef, got {:?}", other),
    };
    let cid = ctx.class_id_of_object(obj);
    let name = ctx.class_name_of_id(cid).unwrap_or_default();
    assert_eq!(
        name, CLS_JBOSS_LOG_MANAGER,
        "WildFly's logging extension requires the active manager class to be JBoss LogManager"
    );

    let jboss = match native_get_jboss_log_manager(&mut ctx, &[])
        .unwrap()
        .unwrap()
    {
        Value::Object(Some(o)) => o,
        other => panic!("expected JBoss manager ObjectRef, got {:?}", other),
    };
    assert_eq!(obj, jboss, "JUL and JBoss entry points share singleton");
}

#[test]
fn block_2b_property_empty_or_whitespace_returns_default() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    ctx.set_system_property("java.util.logging.manager", "   ");
    let obj = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
        Value::Object(Some(o)) => o,
        _ => panic!(),
    };
    let cid = ctx.class_id_of_object(obj);
    let name = ctx.class_name_of_id(cid).unwrap_or_default();
    assert_eq!(name, CLS_JUL_LOG_MANAGER);
}

#[test]
fn block_2b_property_path_traversal_class_name_rejected() {
    // Hostile property values must not coax the unified loader
    // into probing arbitrary disk paths via the class-name string.
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    ctx.set_system_property("java.util.logging.manager", "../../../etc/passwd");
    let obj = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
        Value::Object(Some(o)) => o,
        _ => panic!(),
    };
    let cid = ctx.class_id_of_object(obj);
    let name = ctx.class_name_of_id(cid).unwrap_or_default();
    // Falls through to the default LogManager class on rejection.
    assert_eq!(name, CLS_JUL_LOG_MANAGER);
}

// -- B4: GC root scan + post-move remap for the cached ObjectRefs --

/// Snapshot the raw addresses currently held in this module's
/// side-tables (singleton, both logger registries, the JBoss
/// `LogContext` singleton, and every attachment receiver/key/value).
fn cached_addrs_snapshot() -> Vec<u64> {
    let mut v = Vec::new();
    if let Some(a) = *singleton_cell(TEST_VM)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
    {
        v.push(a);
    }
    if let Some(a) = *jboss_log_context_singleton(TEST_VM)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
    {
        v.push(a);
    }
    v.extend(
        logger_registry(TEST_VM)
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .copied(),
    );
    v.extend(
        jboss_logger_registry(TEST_VM)
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .copied(),
    );
    for (&(this, key), &value) in attachments(TEST_VM)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
    {
        v.extend_from_slice(&[this, key, value]);
    }
    v
}

#[test]
fn b4_gc_scan_reports_every_cached_object_as_root() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();

    // Populate every side-table: singleton, a JUL logger, a JBoss
    // logger, the JBoss LogContext singleton, and one attachment.
    let mgr = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
        Value::Object(Some(o)) => o,
        _ => panic!(),
    };
    let jul_name = ctx.create_string("scan.jul.logger");
    let _ = native_get_logger(
        &mut ctx,
        &[Value::Object(Some(mgr)), Value::Object(Some(jul_name))],
    )
    .unwrap();
    let jb_name = ctx.create_string("scan.jboss.logger");
    let _ = native_jboss_log_context_get_logger(
        &mut ctx,
        &[Value::Object(None), Value::Object(Some(jb_name))],
    )
    .unwrap();
    let _ = ensure_jboss_log_context(&mut ctx);
    let recv = try_alloc_concurrent_synthetic(
        &mut ctx,
        "org/jboss/logmanager/Logger",
        LOGGER_NUM_FIELDS,
    )
    .unwrap();
    let key = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Object", 1).unwrap();
    let val = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Object", 1).unwrap();
    let _ = native_jboss_logger_attach(
        &mut ctx,
        &[
            Value::Object(Some(recv)),
            Value::Object(Some(key)),
            Value::Object(Some(val)),
        ],
    )
    .unwrap();

    let expected = cached_addrs_snapshot();
    assert!(!expected.is_empty(), "side-tables must be populated");

    let mut roots = Vec::new();
    gc_scan_logmanager_roots(TEST_VM, &mut roots);
    let root_addrs: std::collections::HashSet<u64> =
        roots.iter().map(|o| o.as_ptr() as u64).collect();
    for a in expected {
        assert!(
            root_addrs.contains(&a),
            "cached object {a:#x} must be reported as a GC root"
        );
    }
}

#[test]
fn b4_gc_update_repoints_every_cached_address() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();

    let mgr = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
        Value::Object(Some(o)) => o,
        _ => panic!(),
    };
    let jul_name = ctx.create_string("remap.jul.logger");
    let _ = native_get_logger(
        &mut ctx,
        &[Value::Object(Some(mgr)), Value::Object(Some(jul_name))],
    )
    .unwrap();
    let jb_name = ctx.create_string("remap.jboss.logger");
    let _ = native_jboss_log_context_get_logger(
        &mut ctx,
        &[Value::Object(None), Value::Object(Some(jb_name))],
    )
    .unwrap();
    let _ = ensure_jboss_log_context(&mut ctx);
    let recv = try_alloc_concurrent_synthetic(
        &mut ctx,
        "org/jboss/logmanager/Logger",
        LOGGER_NUM_FIELDS,
    )
    .unwrap();
    let key = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Object", 1).unwrap();
    let val = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Object", 1).unwrap();
    let _ = native_jboss_logger_attach(
        &mut ctx,
        &[
            Value::Object(Some(recv)),
            Value::Object(Some(key)),
            Value::Object(Some(val)),
        ],
    )
    .unwrap();

    // Simulate a moving GC: every cached old address maps to a fresh,
    // non-overlapping synthetic "relocated" address.
    let old_addrs = cached_addrs_snapshot();
    assert!(!old_addrs.is_empty());
    let mut pointer_map: cratonvm_types::PointerMap = cratonvm_types::PointerMap::default();
    // Use a high base so the synthetic targets never collide with a
    // real old address (which would make the assertion ambiguous).
    let base: usize = 0x1_0000_0000_0000;
    for (i, &a) in old_addrs.iter().enumerate() {
        pointer_map.insert(a as usize, base + (i + 1) * 0x1000);
    }

    gc_update_logmanager_refs(TEST_VM, &pointer_map);

    // Every stored address must now be the relocated target — no old
    // address may survive (that would be the use-after-free B4 flags).
    let new_addrs = cached_addrs_snapshot();
    assert_eq!(
        new_addrs.len(),
        old_addrs.len(),
        "remap must preserve table cardinality (attachment key rebuild intact)"
    );
    for a in &new_addrs {
        assert!(
            (*a as usize) >= base,
            "address {a:#x} was not remapped to its relocated slot"
        );
        assert!(
            pointer_map.values().any(|&v| v as u64 == *a),
            "address {a:#x} is not one of the synthetic relocated targets"
        );
    }
}

#[test]
fn b4_gc_update_is_noop_for_empty_pointer_map() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    let mgr = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
        Value::Object(Some(o)) => o,
        _ => panic!(),
    };
    let n = ctx.create_string("noop.logger");
    let _ = native_get_logger(
        &mut ctx,
        &[Value::Object(Some(mgr)), Value::Object(Some(n))],
    )
    .unwrap();
    let before = cached_addrs_snapshot();
    gc_update_logmanager_refs(TEST_VM, &cratonvm_types::PointerMap::default());
    let after = cached_addrs_snapshot();
    assert_eq!(
        before, after,
        "empty pointer map must not mutate any address"
    );
}

#[test]
fn b4_gc_update_leaves_unmoved_addresses_untouched() {
    // Objects the young collector did not relocate are absent from the
    // pointer map; their cached address must be preserved verbatim.
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    let mgr = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
        Value::Object(Some(o)) => o,
        _ => panic!(),
    };
    let n = ctx.create_string("stable.logger");
    let _ = native_get_logger(
        &mut ctx,
        &[Value::Object(Some(mgr)), Value::Object(Some(n))],
    )
    .unwrap();
    let before = cached_addrs_snapshot();
    // A pointer map that mentions only some unrelated address.
    let mut pm: cratonvm_types::PointerMap = cratonvm_types::PointerMap::default();
    pm.insert(0xdead_beef, 0xfeed_face);
    gc_update_logmanager_refs(TEST_VM, &pm);
    let after = cached_addrs_snapshot();
    assert_eq!(
        before, after,
        "addresses absent from the pointer map must be left unchanged"
    );
}

// ======================================================================
// The JUL null axis
//
// Every expectation below is a transcription of a HotSpot 25.0.3+9-LTS
// run, not a reading of the JDK source. The table is in
// docs/feature-designs/concurrent-gc-maturation.md
//
// The tests come in PAIRS on purpose. A test file that only asserts the
// throws would be passed by a blanket "JUL rejects null" rule — which is
// the rule HANDOFF-20260814 §5 records as having broken working paths in
// this exact family. The `..._is_legal_and_must_not_throw` half is what
// fails if someone generalises.
// ======================================================================

fn runtime_error(failed: MethodCallFailed) -> RuntimeError {
    match failed {
        MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(e)) => e,
        other => panic!("expected a RuntimeError, got {other:?}"),
    }
}

/// Assert an NPE carrying HotSpot's message VERBATIM. The message is the
/// point: every row on this axis already threw the right CLASS somewhere,
/// and `Level.parse` threw the right class with a message
/// (`"Name cannot be null"`) that no HotSpot build produces.
fn assert_npe(result: MethodCallResult, expected: &str, what: &str) {
    let err = result
        .err()
        .unwrap_or_else(|| panic!("{what}: expected a throw, got a return"));
    match runtime_error(err) {
        RuntimeError::NullPointerException { message } => assert_eq!(
            message.as_deref(),
            Some(expected),
            "{what}: NPE message must be HotSpot's text verbatim"
        ),
        other => panic!("{what}: expected NullPointerException, got {other:?}"),
    }
}

/// The six messages, transcribed. If HotSpot's text ever changes these
/// are the cells to re-measure; nothing else in this module hard-codes it.
#[test]
fn the_jul_null_messages_are_the_measured_hotspot_text() {
    assert_eq!(
        JUL_NPE_NULL_LEVEL,
        "Cannot invoke \"java.util.logging.Level.intValue()\" because \"level\" is null"
    );
    assert_eq!(
        JUL_NPE_NULL_SUPPLIER,
        "Cannot invoke \"java.util.function.Supplier.get()\" because \"msgSupplier\" is null"
    );
    assert_eq!(
        JUL_NPE_NULL_KEY,
        "Cannot invoke \"Object.hashCode()\" because \"key\" is null"
    );
    assert_eq!(
        JUL_NPE_NULL_RECORD,
        "Cannot invoke \"java.util.logging.LogRecord.getLevel()\" because \"record\" is null"
    );
    assert_eq!(
        JUL_NPE_NULL_LOGGER,
        "Cannot invoke \"java.util.logging.Logger.getName()\" because \"logger\" is null"
    );
    assert_eq!(
        JUL_NPE_NULL_LEVEL_NAME,
        "Cannot invoke \"String.length()\" because \"name\" is null"
    );
}

/// `Level.parse(null)` threw the right class with an invented message.
#[test]
fn level_parse_null_npe_carries_hotspots_message_not_an_invented_one() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    assert_npe(
        native_level_parse(&mut ctx, &[Value::Object(None)]),
        JUL_NPE_NULL_LEVEL_NAME,
        "Level.parse(null)",
    );
}

/// `Level.findLevel` is `parse`'s non-throwing sibling and MUST keep
/// answering `null` for the same input — the message change above must not
/// leak through it. `LogManager.getLevelProperty` depends on this.
#[test]
fn level_find_level_null_is_still_null_and_must_not_throw() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    assert!(
        matches!(
            native_level_find_level(&mut ctx, &[Value::Object(None)]),
            Ok(Some(Value::Object(None)))
        ),
        "findLevel must report an unresolvable name as null, never throw"
    );
}

/// A null name is not the empty name: this used to hand back the ROOT
/// logger, so `getLogger(null)` returned a live object where HotSpot NPEs.
#[test]
fn log_manager_get_logger_null_key_throws() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    let mgr = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
        Value::Object(Some(o)) => o,
        _ => panic!("no LogManager"),
    };
    assert_npe(
        native_get_logger(&mut ctx, &[Value::Object(Some(mgr)), Value::Object(None)]),
        JUL_NPE_NULL_KEY,
        "LogManager.getLogger(null)",
    );
}

/// The static factory takes the name in slot 0, not slot 1. Getting that
/// wrong would make the check fire on the RECEIVER of a method that has
/// none.
#[test]
fn logger_static_get_logger_null_name_throws() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    assert_npe(
        native_jul_static_get_logger(&mut ctx, &[Value::Object(None)]),
        JUL_NPE_NULL_KEY,
        "Logger.getLogger(null)",
    );
}

/// The other half of the same signature. MEASURED: `Logger.getLogger
/// ("more.a", null)` returns a Logger — the BUNDLE may be null even
/// though the NAME may not.
#[test]
fn logger_static_get_logger_null_bundle_is_legal_and_must_not_throw() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    let name = ctx.create_string("axis.named");
    let got = native_jul_static_get_logger_with_bundle(
        &mut ctx,
        &[Value::Object(Some(name)), Value::Object(None)],
    );
    assert!(
        matches!(got, Ok(Some(Value::Object(Some(_))))),
        "a null resourceBundleName is legal and must still yield a Logger"
    );
}

#[test]
fn log_manager_get_property_null_key_throws_but_a_missing_key_is_null() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    let mgr = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
        Value::Object(Some(o)) => o,
        _ => panic!("no LogManager"),
    };
    assert_npe(
        native_get_property(&mut ctx, &[Value::Object(Some(mgr)), Value::Object(None)]),
        JUL_NPE_NULL_KEY,
        "LogManager.getProperty(null)",
    );
    // The ABSENT key is a different rule and stays null on both VMs.
    let missing = ctx.create_string("no.such.key.at.all");
    assert!(
        matches!(
            native_get_property(
                &mut ctx,
                &[Value::Object(Some(mgr)), Value::Object(Some(missing))]
            ),
            Ok(Some(Value::Object(None)))
        ),
        "an absent key must still answer null, not throw"
    );
}

/// This one reverses a DELIBERATE swallow whose comment said the contract
/// was NPE but that returning `false` kept bootstraps alive. It kept
/// nothing alive that HotSpot would have run.
#[test]
fn log_manager_add_logger_null_throws_instead_of_answering_false() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    let mgr = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
        Value::Object(Some(o)) => o,
        _ => panic!("no LogManager"),
    };
    assert_npe(
        native_add_logger(&mut ctx, &[Value::Object(Some(mgr)), Value::Object(None)]),
        JUL_NPE_NULL_LOGGER,
        "LogManager.addLogger(null)",
    );
}

/// `isLoggable` is the level gate the supplier overloads share, so this
/// single check is also what orders `log(null, supplier)` correctly: the
/// level is reported and the supplier is never called.
#[test]
fn logger_is_loggable_null_level_throws() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    let logger = make_logger(&mut ctx, "axis.recv");
    assert_npe(
        native_jul_logger_is_loggable(
            &mut ctx,
            &[Value::Object(Some(logger)), Value::Object(None)],
        ),
        JUL_NPE_NULL_LEVEL,
        "Logger.isLoggable(null)",
    );
}

#[test]
fn logger_log_record_null_throws() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    let logger = make_logger(&mut ctx, "axis.recv");
    assert_npe(
        native_jul_logger_log_record(
            &mut ctx,
            &[Value::Object(Some(logger)), Value::Object(None)],
        ),
        JUL_NPE_NULL_RECORD,
        "Logger.log((LogRecord) null)",
    );
}

/// Every `log`/`logp` overload that does NOT route through the shared gate
/// restates the level check, so every one of them is asserted here. A
/// missing restatement is invisible otherwise: the overload simply logs at
/// the INFO default and returns.
#[test]
fn every_log_overload_refuses_a_null_level() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    let logger = make_logger(&mut ctx, "axis.recv");
    let msg = ctx.create_string("m");
    let recv = Value::Object(Some(logger));
    let text = Value::Object(Some(msg));
    let null = Value::Object(None);

    assert_npe(
        native_jul_logger_log_level_msg(&mut ctx, &[recv.clone(), null.clone(), text.clone()]),
        JUL_NPE_NULL_LEVEL,
        "log(null, String)",
    );
    assert_npe(
        native_jul_logger_log_param(
            &mut ctx,
            &[recv.clone(), null.clone(), text.clone(), null.clone()],
        ),
        JUL_NPE_NULL_LEVEL,
        "log(null, String, Object)",
    );
    assert_npe(
        native_jul_logger_log_params(
            &mut ctx,
            &[recv.clone(), null.clone(), text.clone(), null.clone()],
        ),
        JUL_NPE_NULL_LEVEL,
        "log(null, String, Object[])",
    );
    assert_npe(
        native_jul_logger_logp(
            &mut ctx,
            &[
                recv.clone(),
                null.clone(),
                null.clone(),
                null.clone(),
                text.clone(),
            ],
        ),
        JUL_NPE_NULL_LEVEL,
        "logp(null, null, null, String)",
    );
    assert_npe(
        native_jul_logger_log_throwable(
            &mut ctx,
            &[recv.clone(), null.clone(), text.clone(), null.clone()],
        ),
        JUL_NPE_NULL_LEVEL,
        "log(null, String, Throwable)",
    );
    assert_npe(
        native_jul_logger_log_supplier(&mut ctx, &[recv, null.clone(), null]),
        JUL_NPE_NULL_LEVEL,
        "log(null, Supplier)",
    );
}

/// The half a blanket rule would break. `logp`'s SOURCE CLASS and SOURCE
/// METHOD are nulls in the same call whose level null is fatal — measured,
/// `logp(SEVERE, null, null, "msg")` returns on HotSpot. If someone
/// "tidies" the level check into a loop over the reference arguments, this
/// is the test that fails.
#[test]
fn logp_null_source_class_and_method_are_legal_and_must_not_throw() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    let logger = make_logger(&mut ctx, "axis.recv");
    let level = make_level(&mut ctx, "SEVERE", 1000);
    let msg = ctx.create_string("m");
    let got = native_jul_logger_logp(
        &mut ctx,
        &[
            Value::Object(Some(logger)),
            Value::Object(Some(level)),
            Value::Object(None),
            Value::Object(None),
            Value::Object(Some(msg)),
        ],
    );
    assert!(
        got.is_ok(),
        "a null source class/method pair is legal on HotSpot and must not throw: {got:?}"
    );
}

/// The other legal nulls in the same family, asserted together so a
/// blanket rule cannot pass this module. MEASURED on HotSpot: all return.
#[test]
fn the_legal_nulls_in_the_log_family_must_not_throw() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    reset_state_for_tests();
    let mut ctx = mock_ctx();
    let logger = make_logger(&mut ctx, "axis.recv");
    let level = make_level(&mut ctx, "SEVERE", 1000);
    let recv = Value::Object(Some(logger));
    let lvl = Value::Object(Some(level));
    let null = Value::Object(None);

    // log(SEVERE, (String) null)
    let got =
        native_jul_logger_log_level_msg(&mut ctx, &[recv.clone(), lvl.clone(), null.clone()]);
    assert!(got.is_ok(), "log(level, null message) is legal: {got:?}");

    // log(SEVERE, "m", (Object[]) null)
    let msg = ctx.create_string("m");
    let got = native_jul_logger_log_params(
        &mut ctx,
        &[
            recv.clone(),
            lvl.clone(),
            Value::Object(Some(msg)),
            null.clone(),
        ],
    );
    assert!(got.is_ok(), "a null parameter array is legal: {got:?}");

    // log(SEVERE, (String) null, (Throwable) null) — TWO nulls in the same
    // two slots whose `(Level, Throwable, Supplier)` reading is fatal.
    let got = native_jul_logger_log_throwable(&mut ctx, &[recv, lvl, null.clone(), null]);
    assert!(
        got.is_ok(),
        "log(level, null message, null throwable) is legal and must not be \
         mistaken for a null Supplier: {got:?}"
    );
}

/// `jul_arg_is_null` is the whole discriminator; a primitive in the slot
/// must not read as "null" (that would make a `(Z)V`-shaped sibling throw).
#[test]
fn jul_arg_is_null_distinguishes_absent_null_and_primitive() {
    assert!(jul_arg_is_null(&[], 0), "an absent slot reads as null");
    assert!(
        jul_arg_is_null(&[Value::Object(None)], 0),
        "an explicit null reference reads as null"
    );
    assert!(
        jul_arg_is_null(&[Value::Int(0)], 0),
        "a primitive is not a non-null REFERENCE"
    );
}

/// Build a `Logger`-shaped receiver. A `String` standing in for the
/// receiver would be read at `LOGGER_FIELD_NAME` by half these natives,
/// which is a field index a `String` does not have — the same raw-slot
/// aliasing hazard `jul_logger_handlers_table`'s doc comment describes,
/// reproduced in a test instead of in production.
fn make_logger(ctx: &mut crate::test_utils::MockNativeContext, name: &str) -> ObjectRef {
    let cid = ctx.ensure_class_initialized(CLS_JUL_LOGGER).unwrap();
    let obj = ctx.alloc_object(cid, LOGGER_NUM_FIELDS);
    let n = ctx.create_string(name);
    ctx.set_field(obj, LOGGER_FIELD_NAME, Value::Object(Some(n)));
    ctx.set_field_by_name(obj, "name", Value::Object(Some(n)));
    obj
}

/// Build a `Level`-shaped object the threshold walk can read.
///
/// The `set_declared_fields` call is load-bearing and was MISSING. The
/// mock resolves a field name through a per-class table plus whatever a
/// test declared, and it has no entry for `java/util/logging/Level` — so
/// the two `set_field_by_name` calls below silently wrote nowhere and the
/// matching read answered `Value::Int(0)`, which
/// `jul_requested_level_value`/`record_jul_logger_level` accept as a
/// perfectly good level value of ZERO. Every level built here was
/// therefore level 0 whatever the caller asked for; it went unnoticed
/// because no test using this helper had ever asserted on the VALUE, only
/// on "did not throw". Declaring the two fields makes the by-name path
/// real, and slots 0/1 keep the synthetic-shape fallback agreeing.
fn make_level(
    ctx: &mut crate::test_utils::MockNativeContext,
    name: &str,
    value: i32,
) -> ObjectRef {
    let cid = ctx.ensure_class_initialized(CLS_JUL_LEVEL).unwrap();
    ctx.set_declared_fields(
        cid,
        vec![
            cratonvm_native_api::FieldMetadata {
                name: "name".to_string(),
                descriptor: "Ljava/lang/String;".to_string(),
                access_flags: 0,
                slot_index: 0,
                declaring_class_id: cid,
                is_static: false,
            },
            cratonvm_native_api::FieldMetadata {
                name: "value".to_string(),
                descriptor: "I".to_string(),
                access_flags: 0,
                slot_index: 1,
                declaring_class_id: cid,
                is_static: false,
            },
        ],
    );
    let obj = ctx.alloc_object(cid, 2);
    let n = ctx.create_string(name);
    ctx.set_field(obj, 0, Value::Object(Some(n)));
    ctx.set_field(obj, 1, Value::Int(value));
    ctx.set_field_by_name(obj, "name", Value::Object(Some(n)));
    ctx.set_field_by_name(obj, "value", Value::Int(value));
    obj
}
