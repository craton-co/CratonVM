use super::*;

// ── SignalHandler basics ────────────────────────────────────────────

#[test]
fn signal_handler_new_is_empty() {
    let sh = SignalHandler::new();
    assert!(sh.registered_signals.is_empty());
}

#[test]
fn register_signal_default() {
    let mut sh = SignalHandler::new();
    sh.register_signal(SIGINT, SignalAction::Default);
    assert!(sh.registered_signals.contains_key(&SIGINT));
}

#[test]
fn register_signal_ignore() {
    let mut sh = SignalHandler::new();
    sh.register_signal(SIGTERM, SignalAction::Ignore);
    assert!(matches!(
        sh.registered_signals.get(&SIGTERM),
        Some(SignalAction::Ignore)
    ));
}

#[test]
fn register_signal_handle() {
    fn my_handler(_sig: i32) {}
    let mut sh = SignalHandler::new();
    sh.register_signal(SIGUSR1, SignalAction::Handle(my_handler));
    assert!(matches!(
        sh.registered_signals.get(&SIGUSR1),
        Some(SignalAction::Handle(_))
    ));
}

#[test]
fn register_signal_replaces_existing() {
    let mut sh = SignalHandler::new();
    sh.register_signal(SIGINT, SignalAction::Default);
    sh.register_signal(SIGINT, SignalAction::Ignore);
    assert!(matches!(
        sh.registered_signals.get(&SIGINT),
        Some(SignalAction::Ignore)
    ));
}

#[test]
fn register_multiple_signals() {
    let mut sh = SignalHandler::new();
    sh.register_signal(SIGINT, SignalAction::Default);
    sh.register_signal(SIGTERM, SignalAction::Ignore);
    sh.register_signal(SIGQUIT, SignalAction::Default);
    assert_eq!(sh.registered_signals.len(), 3);
}

#[test]
fn signal_handler_default_trait() {
    let sh = SignalHandler::default();
    assert!(sh.registered_signals.is_empty());
}

#[test]
fn signal_action_debug_format() {
    let d = format!("{:?}", SignalAction::Default);
    assert_eq!(d, "Default");
    let i = format!("{:?}", SignalAction::Ignore);
    assert_eq!(i, "Ignore");
    fn noop(_: i32) {}
    let h = format!("{:?}", SignalAction::Handle(noop));
    assert_eq!(h, "Handle(<fn>)");
}

// ── ThreadDumper ────────────────────────────────────────────────────

#[test]
fn dump_empty_threads() {
    let dump = ThreadDumper::dump_all_threads(&[]);
    assert!(dump.is_empty());
}

#[test]
fn dump_single_thread_no_frames() {
    let t = ThreadInfo {
        id: 1,
        name: "main".into(),
        state: "RUNNABLE".into(),
        stack_frames: vec![],
    };
    let dump = ThreadDumper::dump_all_threads(&[t]);
    assert!(dump.contains("\"main\" #1"));
    assert!(dump.contains("[RUNNABLE]"));
}

#[test]
fn dump_thread_with_frames() {
    let t = ThreadInfo {
        id: 7,
        name: "worker-1".into(),
        state: "WAITING".into(),
        stack_frames: vec![
            StackFrame {
                class_name: "com.example.App".into(),
                method_name: "doWork".into(),
                file_name: Some("App.java".into()),
                line_number: 42,
            },
            StackFrame {
                class_name: "com.example.App".into(),
                method_name: "run".into(),
                file_name: Some("App.java".into()),
                line_number: 10,
            },
        ],
    };
    let dump = ThreadDumper::dump_all_threads(&[t]);
    assert!(dump.contains("at com.example.App.doWork(App.java:42)"));
    assert!(dump.contains("at com.example.App.run(App.java:10)"));
}

#[test]
fn dump_thread_frame_unknown_source() {
    let t = ThreadInfo {
        id: 2,
        name: "t".into(),
        state: "BLOCKED".into(),
        stack_frames: vec![StackFrame {
            class_name: "X".into(),
            method_name: "m".into(),
            file_name: None,
            line_number: -1,
        }],
    };
    let dump = ThreadDumper::dump_all_threads(&[t]);
    assert!(dump.contains("(Unknown Source)"));
}

#[test]
fn dump_thread_frame_file_no_line() {
    let t = ThreadInfo {
        id: 3,
        name: "t".into(),
        state: "RUNNABLE".into(),
        stack_frames: vec![StackFrame {
            class_name: "A".into(),
            method_name: "b".into(),
            file_name: Some("A.java".into()),
            line_number: 0,
        }],
    };
    let dump = ThreadDumper::dump_all_threads(&[t]);
    assert!(dump.contains("(A.java)"));
    assert!(!dump.contains("(A.java:0)"));
}

#[test]
fn dump_multiple_threads() {
    let threads = vec![
        ThreadInfo {
            id: 1,
            name: "main".into(),
            state: "RUNNABLE".into(),
            stack_frames: vec![],
        },
        ThreadInfo {
            id: 2,
            name: "gc".into(),
            state: "WAITING".into(),
            stack_frames: vec![],
        },
    ];
    let dump = ThreadDumper::dump_all_threads(&threads);
    assert!(dump.contains("\"main\""));
    assert!(dump.contains("\"gc\""));
}

// ── NpeMessageGenerator — bytecode analysis ─────────────────────────

#[test]
fn npe_getfield() {
    // GETFIELD with cp index 0x0005
    let bytecode = vec![GETFIELD, 0x00, 0x05];
    let msg = NpeMessageGenerator::generate_message(&bytecode, 0, 0).unwrap();
    assert!(msg.contains("Cannot read field"));
    assert!(msg.contains("is null"));
}

#[test]
fn npe_putfield() {
    let bytecode = vec![PUTFIELD, 0x00, 0x0A];
    let msg = NpeMessageGenerator::generate_message(&bytecode, 0, 0).unwrap();
    assert!(msg.contains("Cannot assign field"));
}

#[test]
fn npe_invokevirtual() {
    let bytecode = vec![INVOKEVIRTUAL, 0x00, 0x03];
    let msg = NpeMessageGenerator::generate_message(&bytecode, 0, 0).unwrap();
    assert!(msg.contains("Cannot invoke"));
    assert!(msg.contains("is null"));
}

#[test]
fn npe_invokeinterface() {
    let bytecode = vec![INVOKEINTERFACE, 0x00, 0x07, 0x02, 0x00];
    let msg = NpeMessageGenerator::generate_message(&bytecode, 0, 0).unwrap();
    assert!(msg.contains("Cannot invoke"));
}

#[test]
fn npe_arraylength() {
    let bytecode = vec![ARRAYLENGTH];
    let msg = NpeMessageGenerator::generate_message(&bytecode, 0, 0).unwrap();
    assert_eq!(
        msg,
        "Cannot read the array length because \"<local>\" is null"
    );
}

#[test]
fn npe_aaload() {
    let bytecode = vec![AALOAD];
    let msg = NpeMessageGenerator::generate_message(&bytecode, 0, 0).unwrap();
    assert!(msg.contains("Cannot load from array"));
}

#[test]
fn npe_iaload() {
    let bytecode = vec![IALOAD];
    let msg = NpeMessageGenerator::generate_message(&bytecode, 0, 0).unwrap();
    assert!(msg.contains("Cannot load from array"));
}

#[test]
fn npe_aastore() {
    let bytecode = vec![AASTORE];
    let msg = NpeMessageGenerator::generate_message(&bytecode, 0, 0).unwrap();
    assert!(msg.contains("Cannot store to array"));
}

#[test]
fn npe_iastore() {
    let bytecode = vec![IASTORE];
    let msg = NpeMessageGenerator::generate_message(&bytecode, 0, 0).unwrap();
    assert!(msg.contains("Cannot store to array"));
}

#[test]
fn npe_athrow() {
    let bytecode = vec![ATHROW];
    let msg = NpeMessageGenerator::generate_message(&bytecode, 0, 0).unwrap();
    assert!(msg.contains("Cannot throw exception"));
}

#[test]
fn npe_monitorenter() {
    let bytecode = vec![MONITORENTER];
    let msg = NpeMessageGenerator::generate_message(&bytecode, 0, 0).unwrap();
    assert!(msg.contains("Cannot enter synchronized block"));
}

#[test]
fn npe_unknown_opcode_returns_none() {
    let bytecode = vec![0x00]; // nop
    assert!(NpeMessageGenerator::generate_message(&bytecode, 0, 0).is_none());
}

#[test]
fn npe_bci_out_of_bounds() {
    let bytecode = vec![GETFIELD, 0x00, 0x01];
    assert!(NpeMessageGenerator::generate_message(&bytecode, 10, 0).is_none());
}

#[test]
fn npe_empty_bytecode() {
    assert!(NpeMessageGenerator::generate_message(&[], 0, 0).is_none());
}

#[test]
fn npe_getfield_truncated() {
    // GETFIELD but only one operand byte — should return None (can't read u16).
    let bytecode = vec![GETFIELD, 0x01];
    assert!(NpeMessageGenerator::generate_message(&bytecode, 0, 0).is_none());
}

#[test]
fn npe_analyze_returns_entity_type_field() {
    let bytecode = vec![GETFIELD, 0x00, 0x01];
    let r = NpeMessageGenerator::analyze(&bytecode, 0).unwrap();
    assert_eq!(r.entity_type, NpeEntityType::Field);
    assert!(r.entity_name.is_some());
}

#[test]
fn npe_analyze_returns_entity_type_method() {
    let bytecode = vec![INVOKEVIRTUAL, 0x00, 0x02];
    let r = NpeMessageGenerator::analyze(&bytecode, 0).unwrap();
    assert_eq!(r.entity_type, NpeEntityType::Method);
    assert!(r.class_name.is_some());
}

#[test]
fn npe_analyze_array_length_entity() {
    let bytecode = vec![ARRAYLENGTH];
    let r = NpeMessageGenerator::analyze(&bytecode, 0).unwrap();
    assert_eq!(r.entity_type, NpeEntityType::ArrayLength);
}

#[test]
fn npe_analyze_array_store_entity() {
    let bytecode = vec![AASTORE];
    let r = NpeMessageGenerator::analyze(&bytecode, 0).unwrap();
    assert_eq!(r.entity_type, NpeEntityType::ArrayStore);
}

#[test]
fn npe_analyze_monitor_entity() {
    let bytecode = vec![MONITORENTER];
    let r = NpeMessageGenerator::analyze(&bytecode, 0).unwrap();
    assert_eq!(r.entity_type, NpeEntityType::Monitor);
}

#[test]
fn npe_analyze_throw_entity() {
    let bytecode = vec![ATHROW];
    let r = NpeMessageGenerator::analyze(&bytecode, 0).unwrap();
    assert_eq!(r.entity_type, NpeEntityType::Throw);
}

// ── Convenience message builders ────────────────────────────────────

#[test]
fn field_npe_message_get() {
    let msg = NpeMessageGenerator::field_npe_message("value", "this.obj", false);
    assert_eq!(
        msg,
        "Cannot read field \"value\" because \"this.obj\" is null"
    );
}

#[test]
fn field_npe_message_put() {
    let msg = NpeMessageGenerator::field_npe_message("x", "ref", true);
    assert_eq!(msg, "Cannot assign field \"x\" because \"ref\" is null");
}

#[test]
fn invoke_npe_message_format() {
    let msg = NpeMessageGenerator::invoke_npe_message("java.util.List", "size", "list");
    assert_eq!(
        msg,
        "Cannot invoke \"java.util.List.size()\" because \"list\" is null"
    );
}

#[test]
fn array_length_npe_msg() {
    let msg = NpeMessageGenerator::array_length_npe_message("arr");
    assert_eq!(msg, "Cannot read the array length because \"arr\" is null");
}

#[test]
fn array_load_npe_msg() {
    let msg = NpeMessageGenerator::array_load_npe_message("data");
    assert_eq!(msg, "Cannot load from array because \"data\" is null");
}

#[test]
fn array_store_npe_msg() {
    let msg = NpeMessageGenerator::array_store_npe_message("buf");
    assert_eq!(msg, "Cannot store to array because \"buf\" is null");
}

#[test]
fn throw_npe_msg() {
    let msg = NpeMessageGenerator::throw_npe_message("ex");
    assert_eq!(msg, "Cannot throw exception because \"ex\" is null");
}

#[test]
fn monitor_npe_msg() {
    let msg = NpeMessageGenerator::monitor_npe_message("lock");
    assert_eq!(
        msg,
        "Cannot enter synchronized block because \"lock\" is null"
    );
}

// ── Signal constants ────────────────────────────────────────────────

#[test]
fn signal_constants_values() {
    assert_eq!(SIGINT, 2);
    assert_eq!(SIGTERM, 15);
    assert_eq!(SIGQUIT, 3);
    assert_eq!(SIGHUP, 1);
    assert_eq!(SIGUSR1, 10);
    assert_eq!(SIGUSR2, 12);
}

// ── NpeEntityType ───────────────────────────────────────────────────

#[test]
fn npe_entity_type_debug() {
    assert_eq!(format!("{:?}", NpeEntityType::Field), "Field");
    assert_eq!(format!("{:?}", NpeEntityType::Unresolved), "Unresolved");
}

// ── Bytecode at non-zero BCI ────────────────────────────────────────

#[test]
fn npe_getfield_at_nonzero_bci() {
    // Some prefix bytes, then GETFIELD at bci=3
    let bytecode = vec![0x00, 0x01, 0x02, GETFIELD, 0x00, 0x0A];
    let msg = NpeMessageGenerator::generate_message(&bytecode, 3, 0).unwrap();
    assert!(msg.contains("Cannot read field"));
    assert!(msg.contains("field_10"));
}

#[test]
fn npe_additional_array_load_opcodes() {
    for &op in &[FALOAD, DALOAD, LALOAD, BALOAD, CALOAD, SALOAD] {
        let bytecode = vec![op];
        let r = NpeMessageGenerator::analyze(&bytecode, 0).unwrap();
        assert_eq!(r.entity_type, NpeEntityType::ArrayAccess);
    }
}

#[test]
fn npe_additional_array_store_opcodes() {
    for &op in &[FASTORE, DASTORE, LASTORE, BASTORE, CASTORE, SASTORE] {
        let bytecode = vec![op];
        let r = NpeMessageGenerator::analyze(&bytecode, 0).unwrap();
        assert_eq!(r.entity_type, NpeEntityType::ArrayStore);
    }
}
