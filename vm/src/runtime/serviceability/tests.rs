// Windows exposes one attach endpoint per PID. Tests that start listeners
// cannot use Unix's independent socket paths, so serialize their lifetimes.
#[cfg(windows)]
static WINDOWS_ENDPOINT_TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

#[cfg(windows)]
#[test]
fn windows_attach_endpoint_has_one_vm_owner_and_releases_it() {
    #[cfg(windows)]
    let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
    use super::windows_attach::WindowsAttachTarget;
    let commands = std::sync::Arc::new(parking_lot::RwLock::new(Vec::new()));
    let owner = WindowsAttachTarget::start(commands.clone()).unwrap();
    assert!(WindowsAttachTarget::start(commands.clone()).is_err());
    drop(owner);
    let successor = WindowsAttachTarget::start(commands).unwrap();
    drop(successor);
}
#[test]
fn attach_request_versions_and_bounds() {
    use super::read_attach_request;
    use std::io::Cursor;
    let expected = vec!["1", "load", "a", "b", "c"];
    assert_eq!(
        read_attach_request(&mut Cursor::new(b"1\0load\0a\0b\0c\0")).unwrap(),
        expected
    );
    assert_eq!(
        read_attach_request(&mut Cursor::new(b"2\x0011\0load\0a\0b\0c\0")).unwrap(),
        vec!["2", "load", "a", "b", "c"]
    );
    for bytes in [
        b"2\x00262145\0".as_slice(),
        b"2\x0011\0load\0a\0",
        b"1\0load\0a\0",
    ] {
        assert!(read_attach_request(&mut Cursor::new(bytes)).is_err());
    }
}
use super::*;

// --- AttachListener tests ---

#[test]
fn test_attach_listener_new() {
    let listener = AttachListener::new("/tmp/test_attach");
    assert_eq!(listener.socket_path, "/tmp/test_attach");
    assert!(!listener.is_listening);
    assert!(listener.commands.read().is_empty());
}

#[test]
fn test_attach_listener_start_stop() {
    #[cfg(windows)]
    let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
    // obsaudit D15: a unique path — start_listening() now binds a real
    // socket, so a path shared with another test risks a parallel-test
    // bind race (see the comment on TEST_JCMD_SOCKET_COUNTER).
    let mut listener = AttachListener::new("/tmp/cratonvm_test_start_stop");
    assert!(!listener.is_listening);
    listener.start_listening();
    assert!(listener.is_listening);
    listener.stop_listening();
    assert!(!listener.is_listening);
}

#[test]
fn test_attach_listener_register_command() {
    let mut listener = AttachListener::new("/tmp/test");
    listener.register_command(DiagnosticCommand::new(
        "Test.cmd",
        "A test command",
        CommandImpact::Low,
        CommandPermission::ReadOnly,
        vec![],
        Box::new(|_| CommandResult::ok("ok".to_string(), 0)),
    ));
    assert_eq!(listener.commands.read().len(), 1);
    assert_eq!(listener.commands.read()[0].name, "Test.cmd");
}

#[test]
fn test_attach_listener_dispatch_command() {
    // obsaudit D15: renamed from test_attach_listener_find_command —
    // find_command was removed (it could not return a reference into
    // the now-lock-guarded `commands` without a dangling-guard
    // lifetime issue), replaced by dispatch_command, which looks up
    // and executes under a single lock acquisition.
    let mut listener = AttachListener::new("/tmp/test");
    listener.register_command(DiagnosticCommand::new(
        "Test.cmd",
        "desc",
        CommandImpact::Low,
        CommandPermission::ReadOnly,
        vec![],
        Box::new(|_| CommandResult::ok("ok".to_string(), 0)),
    ));
    assert!(listener.dispatch_command("Test.cmd", &[]).is_some());
    assert!(listener.dispatch_command("Nonexistent", &[]).is_none());
}

#[test]
fn test_attach_listener_list_commands() {
    let mut listener = AttachListener::new("/tmp/test");
    listener.register_command(DiagnosticCommand::new(
        "A.cmd",
        "d",
        CommandImpact::Low,
        CommandPermission::ReadOnly,
        vec![],
        Box::new(|_| CommandResult::ok("ok".to_string(), 0)),
    ));
    listener.register_command(DiagnosticCommand::new(
        "B.cmd",
        "d",
        CommandImpact::Medium,
        CommandPermission::ManagementAction,
        vec![],
        Box::new(|_| CommandResult::ok("ok".to_string(), 0)),
    ));
    let names = listener.list_commands();
    assert_eq!(names, vec!["A.cmd".to_string(), "B.cmd".to_string()]);
}

// --- DiagnosticCommand tests ---

#[test]
fn test_diagnostic_command_execute() {
    let cmd = DiagnosticCommand::new(
        "Test.echo",
        "Echo args",
        CommandImpact::Low,
        CommandPermission::ReadOnly,
        vec![],
        Box::new(|args| CommandResult::ok(args.join(", "), 0)),
    );
    let result = cmd.execute(&["hello".to_string(), "world".to_string()]);
    assert!(result.success);
    assert_eq!(result.output, "hello, world");
}

#[test]
fn test_command_result_ok() {
    let r = CommandResult::ok("done".to_string(), 42);
    assert!(r.success);
    assert_eq!(r.output, "done");
    assert!(r.error.is_none());
    assert_eq!(r.execution_time_ms, 42);
}

#[test]
fn test_command_result_err() {
    let r = CommandResult::err("fail".to_string(), 5);
    assert!(!r.success);
    assert!(r.output.is_empty());
    assert_eq!(r.error.as_deref(), Some("fail"));
}

#[test]
fn test_command_impact_display() {
    assert_eq!(format!("{}", CommandImpact::Low), "Low");
    assert_eq!(format!("{}", CommandImpact::Medium), "Medium");
    assert_eq!(format!("{}", CommandImpact::High), "High");
}

#[test]
fn test_command_with_arguments() {
    let cmd = DiagnosticCommand::new(
        "Test.args",
        "Test with args",
        CommandImpact::Low,
        CommandPermission::ReadOnly,
        vec![
            CommandArgument {
                name: "path".to_string(),
                description: "File path".to_string(),
                arg_type: ArgType::FilePath,
                required: true,
                default_value: None,
            },
            CommandArgument {
                name: "verbose".to_string(),
                description: "Verbose output".to_string(),
                arg_type: ArgType::Bool,
                required: false,
                default_value: Some("false".to_string()),
            },
        ],
        Box::new(|_| CommandResult::ok("ok".to_string(), 0)),
    );
    assert_eq!(cmd.arguments.len(), 2);
    assert_eq!(cmd.arguments[0].arg_type, ArgType::FilePath);
    assert!(cmd.arguments[0].required);
    assert!(!cmd.arguments[1].required);
    assert_eq!(cmd.arguments[1].default_value.as_deref(), Some("false"));
}

// --- JcmdProcessor tests ---

#[test]
fn test_jcmd_new_has_all_commands() {
    #[cfg(windows)]
    let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
    let jcmd = JcmdProcessor::new();
    let names = jcmd.attach_listener.list_commands();
    assert_eq!(names.len(), 16);
    assert!(names.contains(&"Thread.print".to_string()));
    assert!(names.contains(&"GC.heap_dump".to_string()));
    assert!(names.contains(&"GC.run".to_string()));
    assert!(names.contains(&"GC.heap_info".to_string()));
    assert!(names.contains(&"GC.class_histogram".to_string()));
    assert!(names.contains(&"VM.version".to_string()));
    assert!(names.contains(&"VM.flags".to_string()));
    assert!(names.contains(&"VM.system_properties".to_string()));
    assert!(names.contains(&"VM.uptime".to_string()));
    assert!(names.contains(&"VM.info".to_string()));
    assert!(names.contains(&"VM.command_line".to_string()));
    assert!(names.contains(&"Thread.dump_to_file".to_string()));
    assert!(names.contains(&"Compiler.queue".to_string()));
    assert!(names.contains(&"JFR.start".to_string()));
    assert!(names.contains(&"JFR.stop".to_string()));
    assert!(names.contains(&"JFR.dump".to_string()));
}

/// Wave 138 L1b: every command of the VM-less set that used to invent
/// its answer refuses, with no output (page
/// `i137-L1b-jcmd-vm-flags-shape-and-the-fake-standard-set`).
fn assert_vm_less_refusal(jcmd: &JcmdProcessor, command: &str, forbidden: &[&str]) {
    let result = jcmd.process_command(command);
    assert!(!result.success, "`{command}` must refuse, not invent: {:?}", result.output);
    assert!(result.output.is_empty(), "`{command}`: {:?}", result.output);
    let error = result.error.as_deref().unwrap_or_default();
    assert!(error.contains("not implemented"), "`{command}`: {error}");
    for f in forbidden {
        assert!(!error.contains(f), "`{command}` must not echo `{f}`: {error}");
    }
}

#[test]
fn test_jcmd_thread_print() {
    #[cfg(windows)]
    let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
    let jcmd = JcmdProcessor::new();
    assert_vm_less_refusal(&jcmd, "Thread.print", &["com.example.Main"]);
}

#[test]
fn test_jcmd_gc_heap_dump() {
    #[cfg(windows)]
    let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
    let jcmd = JcmdProcessor::new();
    assert_vm_less_refusal(&jcmd, "GC.heap_dump /tmp/dump.hprof", &["Heap dump written"]);
}

#[test]
fn test_jcmd_gc_run() {
    #[cfg(windows)]
    let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
    let jcmd = JcmdProcessor::new();
    assert_vm_less_refusal(&jcmd, "GC.run", &["GC triggered"]);
}

#[test]
fn test_jcmd_gc_heap_info() {
    #[cfg(windows)]
    let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
    let jcmd = JcmdProcessor::new();
    assert_vm_less_refusal(&jcmd, "GC.heap_info", &["Young Generation"]);
}

#[test]
fn test_jcmd_gc_class_histogram() {
    #[cfg(windows)]
    let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
    let jcmd = JcmdProcessor::new();
    assert_vm_less_refusal(&jcmd, "GC.class_histogram", &["#instances"]);
}

#[test]
fn test_jcmd_vm_version() {
    #[cfg(windows)]
    let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
    let jcmd = JcmdProcessor::new();
    assert_vm_less_refusal(&jcmd, "VM.version", &["CratonVM 1.0.0"]);
}

#[test]
fn test_jcmd_vm_flags() {
    #[cfg(windows)]
    let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
    let jcmd = JcmdProcessor::new();
    let result = jcmd.process_command("VM.flags");
    assert!(result.success);
    assert!(result.output.contains("-XX:+UseG1GC"));
    // zt t24/x20: HotSpot's one-line, blank-separated shape (dev
    // cabdb9f4ae; `hotspot_set_flags_line`): the 15 flags are words of
    // one line, each followed by a blank.
    assert_eq!(result.output.lines().count(), 1, "{}", result.output);
    assert!(result.output.split_whitespace().count() >= 15);
    assert!(result.output.ends_with(' '));
    assert_eq!(hotspot_set_flags_line::<&str>(&[]), "");
    assert_eq!(
        hotspot_set_flags_line(&["-XX:+UseZGC", "-XX:MaxHeapSize=1"]),
        "-XX:+UseZGC -XX:MaxHeapSize=1 "
    );
}

#[test]
fn test_jcmd_vm_system_properties() {
    #[cfg(windows)]
    let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
    let jcmd = JcmdProcessor::new();
    let result = jcmd.process_command("VM.system_properties");
    // Wave 137 L1b: refuses instead of inventing a property list.
    assert!(!result.success);
    assert!(result.output.is_empty());
    assert!(result.error.as_deref().is_some_and(|e| e.contains("not implemented")));
}

#[test]
fn test_jcmd_vm_uptime() {
    #[cfg(windows)]
    let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
    let jcmd = JcmdProcessor::new();
    assert_vm_less_refusal(&jcmd, "VM.uptime", &["3600.000"]);
}

#[test]
fn test_jcmd_vm_info() {
    #[cfg(windows)]
    let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
    let jcmd = JcmdProcessor::new();
    assert_vm_less_refusal(&jcmd, "VM.info", &["Threads: 12 live", "CPUs: 4"]);
}

#[test]
fn test_jcmd_vm_command_line() {
    #[cfg(windows)]
    let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
    let jcmd = JcmdProcessor::new();
    assert_vm_less_refusal(&jcmd, "VM.command_line", &["-Xmx256m"]);
}

#[test]
fn test_jcmd_thread_dump_to_file() {
    #[cfg(windows)]
    let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
    let jcmd = JcmdProcessor::new();
    let result = jcmd.process_command("Thread.dump_to_file /tmp/threads.txt");
    // gci i12/td: the argument-less set has no VM to run the JDK's
    // dumper on; it used to answer `Thread dump written to <path>` and
    // write nothing.
    assert!(!result.success, "a dump nothing wrote must not report success");
    assert!(!result.output.contains("Thread dump written"));
}

/// A state that serves `Thread.dump_to_file` (gci i12/td), recording what
/// it was asked.
struct DumpingVmState(parking_lot::Mutex<Vec<(String, bool, bool)>>);
impl VmDiagnosticState for DumpingVmState {
    fn thread_snapshots(&self) -> Vec<ThreadSnapshot> {
        Vec::new()
    }
    fn heap_summary(&self) -> HeapSummary {
        MockVmState.heap_summary()
    }
    fn class_histogram(&self) -> Vec<ClassHistogramEntry> {
        Vec::new()
    }
    fn trigger_gc(&self) -> bool {
        false
    }
    fn uptime_secs(&self) -> f64 {
        0.0
    }
    fn command_line(&self) -> String {
        String::new()
    }
    fn system_properties(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    fn vm_flags(&self) -> Vec<String> {
        Vec::new()
    }
    fn thread_dump_to_file(&self, path: &str, overwrite: bool, json: bool) -> Result<String, String> {
        self.0.lock().push((path.to_string(), overwrite, json));
        Ok(format!("Created {path}\n"))
    }
}

/// gci i12/td: a `jcmd` line's arguments split as HotSpot 25's
/// `DCmdArgIter` splits them -- quotes keep a space inside a key or a
/// value, and an unclosed quote is HotSpot's format error.
#[test]
fn gci_i12td_dcmd_arguments_honour_hotspots_quotes() {
    let ok = |line: &str| dcmd_arguments(line).expect(line);
    let v = |parts: &[&str]| parts.iter().map(|p| p.to_string()).collect::<Vec<String>>();
    assert_eq!(ok(""), Vec::<String>::new());
    assert_eq!(ok("  -overwrite   /tmp/t.txt "), v(&["-overwrite", "/tmp/t.txt"]));
    assert_eq!(ok("\"C:/a b/t.txt\""), v(&["C:/a b/t.txt"]));
    assert_eq!(ok("-format=json 'C:/a b/t.json'"), v(&["-format=json", "C:/a b/t.json"]));
    assert_eq!(ok("filename=\"/x y/r.jfr\" name=r1"), v(&["filename=/x y/r.jfr", "name=r1"]));
    assert_eq!(ok("a=b=c"), v(&["a=b=c"]));
    assert_eq!(ok("\"a \\\" b\""), v(&["a \\\" b"]));
    assert_eq!(dcmd_arguments("\"/tmp/open"), Err(DCMD_FORMAT_ERROR.to_string()));
    assert_eq!(dcmd_arguments("filename='/tmp/open"), Err(DCMD_FORMAT_ERROR.to_string()));
    // Through the attach operation: a quoted path with a space reaches
    // the command whole; an unclosed quote is refused with status 1.
    #[cfg(windows)]
    let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
    let dumping = Arc::new(DumpingVmState(parking_lot::Mutex::new(Vec::new())));
    let state: Arc<dyn VmDiagnosticState> = dumping.clone();
    let processor = JcmdProcessor::new_with_vm_state(state);
    let op = |line: &str| {
        let fields: Vec<String> =
            ["1", "jcmd", line, "", ""].iter().map(|s| s.to_string()).collect();
        execute_attach_operation(&fields, &processor.attach_listener.commands)
    };
    assert_eq!(op("Thread.dump_to_file \"/tmp/a b/t.txt\"").0, 0);
    assert_eq!(dumping.0.lock().as_slice(), &[("/tmp/a b/t.txt".to_string(), false, false)]);
    assert_eq!(op("Thread.dump_to_file \"/tmp/a b"), (1, DCMD_FORMAT_ERROR.to_string()));
    // An unknown command stays unknown, whatever its arguments.
    assert_eq!(op("No.such \"x").1, JCMD_UNKNOWN_COMMAND);
}

/// gci i12/td: `VM.version` answers HotSpot's `VersionDCmd` shape from the
/// VM's own properties, not a constant.
#[test]
fn gci_i12td_vm_version_is_hotspots_shape_from_the_vms_properties() {
    let props = |version: &str| {
        vec![
            ("java.vm.name".to_string(), "CratonVM".to_string()),
            ("java.vm.version".to_string(), "0.9.0".to_string()),
            ("java.version".to_string(), version.to_string()),
        ]
    };
    assert_eq!(vm_version_dcmd_lines(&props("25.0.3")), "CratonVM version 0.9.0\nJDK 25.0.3\n");
    assert_eq!(vm_version_dcmd_lines(&props("25")), "CratonVM version 0.9.0\nJDK 25.0.0\n");
    assert_eq!(vm_version_dcmd_lines(&props("25.0.3.1")), "CratonVM version 0.9.0\nJDK 25.0.3.1\n");
    assert_eq!(vm_version_dcmd_lines(&props("25-ea")), "CratonVM version 0.9.0\nJDK 25.0.0\n");
    #[cfg(windows)]
    let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
    // The processor's commands hold only a `Weak` (`new_with_vm_state`):
    // keep the state alive, or the command answers "VM is shutting down"
    // with an empty output.
    let plain: Arc<dyn VmDiagnosticState> = Arc::new(MockVmState);
    let processor = JcmdProcessor::new_with_vm_state(Arc::clone(&plain));
    let out = processor
        .attach_listener
        .dispatch_command("VM.version", &[])
        .expect("registered")
        .output;
    assert!(!out.contains("1.0.0 (JDK 25 compatible)"), "{out}");
    assert!(out.ends_with("JDK 25.0.0\n"), "{out}");
}

/// gci i12/td: `Thread.dump_to_file`'s arguments read as HotSpot 25's
/// `ThreadDumpToFileDCmd` declares them.
#[test]
fn gci_i12td_thread_dump_to_file_reads_hotspots_arguments() {
    let parse = |args: &[&str]| {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        thread_dump_to_file_request(&args)
    };
    let req = |path: &str, overwrite: bool, json: bool| ThreadDumpToFileRequest {
        path: path.to_string(),
        overwrite,
        json,
    };
    assert_eq!(parse(&["/tmp/t.txt"]), Ok(req("/tmp/t.txt", false, false)));
    assert_eq!(parse(&["-overwrite", "/tmp/t.txt"]), Ok(req("/tmp/t.txt", true, false)));
    assert_eq!(parse(&["-overwrite=false", "/tmp/t.txt"]), Ok(req("/tmp/t.txt", false, false)));
    assert_eq!(parse(&["-format=json", "/tmp/t.json"]), Ok(req("/tmp/t.json", false, true)));
    assert_eq!(parse(&["-format=plain", "/tmp/t.txt"]), Ok(req("/tmp/t.txt", false, false)));
    assert_eq!(
        parse(&["-overwrite", "-format=json", "/tmp/t.json"]),
        Ok(req("/tmp/t.json", true, true))
    );
    assert_eq!(
        parse(&[]),
        Err("java.lang.IllegalArgumentException: The argument 'filepath' is mandatory.\n".to_string())
    );
    assert_eq!(
        parse(&["-overwrite"]),
        Err("java.lang.IllegalArgumentException: The argument 'filepath' is mandatory.\n".to_string())
    );
    assert!(parse(&["-nosuch", "/tmp/t.txt"]).unwrap_err().contains("Unknown argument '-nosuch'"));
    assert!(parse(&["/tmp/a", "/tmp/b"]).unwrap_err().contains("Unknown argument '/tmp/b'"));
    assert!(parse(&["-overwrite=maybe", "/tmp/t.txt"]).unwrap_err().contains("Boolean parsing error"));
}

/// gci i12/td: the live table serves `Thread.dump_to_file` through the
/// state (it was absent: `Unknown diagnostic command`), answers the
/// dumper's body with status 0, and a missing path with HotSpot's
/// non-zero `IllegalArgumentException` text.
#[test]
fn gci_i12td_thread_dump_to_file_is_a_live_command() {
    #[cfg(windows)]
    let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
    let dumping = Arc::new(DumpingVmState(parking_lot::Mutex::new(Vec::new())));
    let state: Arc<dyn VmDiagnosticState> = dumping.clone();
    let processor = JcmdProcessor::new_with_vm_state(state);
    let fields: Vec<String> = ["1", "jcmd", "Thread.dump_to_file -format=json /tmp/t.json", "", ""]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let (code, body) = execute_attach_operation(&fields, &processor.attach_listener.commands);
    assert_eq!(code, 0, "{body}");
    assert_eq!(body, "Created /tmp/t.json\n");
    assert_eq!(dumping.0.lock().as_slice(), &[("/tmp/t.json".to_string(), false, true)]);
    let fields: Vec<String> = ["1", "jcmd", "Thread.dump_to_file", "", ""]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let (code, body) = execute_attach_operation(&fields, &processor.attach_listener.commands);
    assert_ne!(code, 0);
    assert_eq!(body, "java.lang.IllegalArgumentException: The argument 'filepath' is mandatory.\n");
    assert_eq!(dumping.0.lock().len(), 1, "a refused request reached the dumper");
    // A state that cannot run the dumper refuses rather than claim a file.
    drop(processor);
    // Kept alive: the commands hold a `Weak`, and a dropped state would
    // refuse for the wrong reason ("VM is shutting down").
    let plain: Arc<dyn VmDiagnosticState> = Arc::new(MockVmState);
    let processor = JcmdProcessor::new_with_vm_state(Arc::clone(&plain));
    let result = processor
        .attach_listener
        .dispatch_command("Thread.dump_to_file", &["/tmp/t.txt".to_string()])
        .expect("registered");
    assert!(!result.success);
}

/// gci i12/td (page `gci-i11vt-...-review-findings` item 9's remainder):
/// a carrier with a mounted virtual thread prints HotSpot's `Carrying
/// virtual thread #<id>` in place of its `java.lang.Thread.State:` line;
/// every other thread keeps its state line.
#[test]
fn gci_i12td_a_carrier_prints_carrying_virtual_thread_for_its_state() {
    let carrier = i81_snapshot(
        "ForkJoinPool-1-worker-1",
        ThreadState::Waiting,
        Some(DumpWaitKind::Parking),
        true,
        5,
        ThreadDumpFacts {
            java_tid: Some(22),
            carrying_virtual_thread: Some(21),
            ..Default::default()
        },
    );
    let plain = i81_snapshot(
        "main",
        ThreadState::Runnable,
        None,
        false,
        5,
        ThreadDumpFacts { java_tid: Some(1), ..Default::default() },
    );
    let dump = JstackProcessor::generate_thread_dump(&[carrier, plain]);
    let carrier_block: Vec<&str> = dump
        .lines()
        .skip_while(|l| !l.starts_with("\"ForkJoinPool-1-worker-1\""))
        .take(2)
        .collect();
    assert_eq!(carrier_block[1], "   Carrying virtual thread #21", "{dump}");
    assert_eq!(dump.matches("java.lang.Thread.State:").count(), 1, "{dump}");
    assert!(dump.contains("   java.lang.Thread.State: RUNNABLE\n"), "{dump}");
    // The live state answers it from the mounted virtual thread's
    // `carrierThread` (`vm_init.rs` `thread_snapshots`).
    let src = concat!(include_str!("../../vm/vm_init/mod.rs"), "\n", include_str!("../../vm/vm_init/missing_native_entry.rs"), "\n", include_str!("../../vm/vm_init/real_jdk_to_array_typed.rs"), "\n", include_str!("../../vm/vm_init/zgc_relocation_permitted.rs"), "\n", include_str!("../../vm/vm_init/wait_site.rs"), "\n", include_str!("../../vm/vm_init/class_store_cut_with.rs")).replace("\r\n", "\n");
    assert!(src.contains("carrying_virtual_thread: entry.java_thread_obj.and_then("));
    assert!(src.contains("resolve_field_index_in_hierarchy(class_id, \"carrierThread\""));
}

/// Observability audit (2026-07-26): `Compiler.queue` must NOT invent a
/// compilation queue. It previously returned a hard-coded listing naming
/// `com.example.Main.hotLoop()V` — fiction an operator would have read as
/// ground truth. Until the JIT queue is exposed to serviceability, the
/// command must fail honestly.
#[test]
fn obsaudit_jcmd_compiler_queue_does_not_fabricate() {
    #[cfg(windows)]
    let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
    let jcmd = JcmdProcessor::new();
    let result = jcmd.process_command("Compiler.queue");
    assert!(
        !result.success,
        "Compiler.queue must not report success while unimplemented"
    );
    assert!(
        !result.output.contains("com.example.Main"),
        "fabricated compile-queue entries must never reappear"
    );
    assert!(
        !result.output.contains("C1 compile queue"),
        "fabricated compile-queue entries must never reappear"
    );
    assert!(result
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("not implemented"));
}

/// Observability audit (2026-07-26): the JFR jcmd verbs must not claim
/// success while never touching the FlightRecorder. A false "recording
/// started" costs the operator the incident window they were capturing.
#[test]
fn obsaudit_jcmd_jfr_verbs_do_not_claim_false_success() {
    #[cfg(windows)]
    let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
    let jcmd = JcmdProcessor::new();
    for (cmd, forbidden) in [
        ("JFR.start myrecording", "Flight recording started"),
        ("JFR.stop myrecording", "Flight recording stopped"),
        ("JFR.dump /tmp/rec.jfr", "Flight recording dumped"),
    ] {
        let result = jcmd.process_command(cmd);
        assert!(
            !result.success,
            "`{cmd}` must not report success: nothing wires it to the FlightRecorder"
        );
        assert!(
            !result.output.contains(forbidden),
            "`{cmd}` must not emit the old fake-success string `{forbidden}`"
        );
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("not implemented"),
            "`{cmd}` must say plainly that it is unimplemented"
        );
    }
}

/// Pins the attach surface's real socket behaviour: `start_listening`
/// binds, `stop_listening` unlinks. If someone changes either half they
/// must also revisit the LIVENESS block at the top of this module and the
/// fabricated-data audit notes it points at.
///
/// Platform split, deliberate and asserted rather than skipped: the
/// implementation is `#[cfg(unix)]` (see `AttachListener::start_listening`
/// — HotSpot itself splits per OS, Unix domain socket on Linux/macOS
/// versus a named pipe on Windows, and the Windows side is unimplemented
/// here). This test used to assert the Unix shape unconditionally, so on
/// a Windows checkout it demanded a `/tmp/...` socket file that the
/// `#[cfg(not(unix))]` `start_listening` — which only flips
/// `is_listening` — never creates, and failed 100% of the time. It now
/// pins the Unix contract in full and the non-Unix contract exactly as
/// documented, so the gap stays visible instead of being papered over by
/// a skip.
#[test]
fn obsaudit_attach_listener_creates_a_real_socket() {
    #[cfg(windows)]
    let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
    // obsaudit D15 (2026-07-26), FIXED: renamed from
    // obsaudit_attach_listener_creates_no_socket, which pinned the
    // opposite (no-socket) behaviour this fix replaced. start_listening
    // now binds a real Unix domain socket at `socket_path` — see the
    // struct doc comment for the wire protocol, verified empirically
    // against a real OpenJDK 21 jcmd/jstack/jmap.
    #[cfg(unix)]
    {
        let path = "/tmp/cratonvm-obsaudit-attach-socket-pin-test";
        let mut l = AttachListener::new(path);
        l.start_listening();
        assert!(l.is_listening);
        assert!(
            std::path::Path::new(path).exists(),
            "AttachListener must create a real socket file at socket_path"
        );
        use std::os::unix::fs::FileTypeExt;
        let meta = std::fs::symlink_metadata(path).unwrap();
        assert!(
            meta.file_type().is_socket(),
            "the file at socket_path must actually be a Unix domain socket, \
             not e.g. a stray regular file"
        );
        l.stop_listening();
        assert!(
            !std::path::Path::new(path).exists(),
            "stop_listening must remove the socket file"
        );
    }
    #[cfg(not(unix))]
    {
        // Windows publishes an in-process queue for the client's named
        // pipe. There is no Unix socket file at socket_path.
        let path = std::env::temp_dir().join("cratonvm-obsaudit-attach-socket-pin-test");
        let _ = std::fs::remove_file(&path);
        let mut l = AttachListener::new(&path.to_string_lossy());
        l.start_listening();
        assert!(l.is_listening);
        assert!(
            !path.exists(),
            "the Windows transport must not create a Unix socket file"
        );
        l.stop_listening();
        assert!(!l.is_listening);
        assert!(!path.exists());
    }
}

#[test]
fn test_jcmd_unknown_command() {
    #[cfg(windows)]
    let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
    let jcmd = JcmdProcessor::new();
    let result = jcmd.process_command("Nonexistent.command");
    assert!(!result.success);
    assert!(result.error.as_ref().unwrap().contains("Unknown command"));
}

#[test]
fn test_jcmd_help() {
    #[cfg(windows)]
    let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
    let jcmd = JcmdProcessor::new();
    let result = jcmd.process_command("help");
    assert!(result.success);
    assert!(result.output.contains("Available commands"));
    assert!(result.output.contains("Thread.print"));
    assert!(result.output.contains("VM.version"));
}

#[test]
fn test_jcmd_help_method() {
    #[cfg(windows)]
    let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
    let jcmd = JcmdProcessor::new();
    let help = jcmd.help();
    assert!(help.contains("Available commands"));
    // Should list all 16 commands
    for name in jcmd.attach_listener.list_commands() {
        assert!(help.contains(&name));
    }
}

// --- jstack tests ---

#[test]
fn test_thread_state_display() {
    assert_eq!(format!("{}", ThreadState::New), "NEW");
    assert_eq!(format!("{}", ThreadState::Runnable), "RUNNABLE");
    assert_eq!(format!("{}", ThreadState::Blocked), "BLOCKED");
    assert_eq!(format!("{}", ThreadState::Waiting), "WAITING");
    assert_eq!(format!("{}", ThreadState::TimedWaiting), "TIMED_WAITING");
    assert_eq!(format!("{}", ThreadState::Terminated), "TERMINATED");
}

/// Interpreter round i3 wave 79, lane L6 (page
/// `i63-L1-thread-dump-has-no-wait-kind-suffix`): a known wait kind
/// prints HotSpot 25's suffix and header word; no kind prints the bare
/// state and the old header, as before.
#[test]
fn thread_dump_prints_the_wait_kind_suffix_and_header_word() {
    let snap =
        |name: &str, state: ThreadState, wait_kind: Option<DumpWaitKind>| ThreadSnapshot {
            id: 7,
            name: name.to_string(),
            daemon: false,
            priority: 5,
            state,
            stack_frames: Vec::new(),
            lock_info: None,
            blocked_by: None,
            waiting_on: None,
            wait_kind,
            dump: Default::default(),
        };
    let rows = [
        (
            snap(
                "sleeper",
                ThreadState::TimedWaiting,
                Some(DumpWaitKind::Sleeping),
            ),
            "waiting on condition",
            "java.lang.Thread.State: TIMED_WAITING (sleeping)\n",
        ),
        (
            snap("parker", ThreadState::Waiting, Some(DumpWaitKind::Parking)),
            "waiting on condition",
            "java.lang.Thread.State: WAITING (parking)\n",
        ),
        (
            snap(
                "timed-parker",
                ThreadState::TimedWaiting,
                Some(DumpWaitKind::Parking),
            ),
            "waiting on condition",
            "java.lang.Thread.State: TIMED_WAITING (parking)\n",
        ),
        (
            snap(
                "waiter",
                ThreadState::Waiting,
                Some(DumpWaitKind::ObjectWait),
            ),
            "in Object.wait()",
            "java.lang.Thread.State: WAITING (on object monitor)\n",
        ),
        (
            snap(
                "blocked",
                ThreadState::Blocked,
                Some(DumpWaitKind::MonitorEnter),
            ),
            "waiting for monitor entry",
            "java.lang.Thread.State: BLOCKED (on object monitor)\n",
        ),
        (
            snap("unknown", ThreadState::Waiting, None),
            "waiting on condition",
            "java.lang.Thread.State: WAITING\n",
        ),
        (
            snap("runner", ThreadState::Runnable, Some(DumpWaitKind::Parking)),
            "runnable",
            "java.lang.Thread.State: RUNNABLE\n",
        ),
    ];
    for (thread, header, state_line) in rows {
        let name = thread.name.clone();
        let dump = JstackProcessor::generate_thread_dump(&[thread]);
        let header_line = dump
            .lines()
            .find(|l| l.starts_with('"'))
            .expect("a header line");
        assert!(header_line.contains(header), "{name}: {header_line}");
        assert!(dump.contains(state_line), "{name}: {dump}");
    }
}

#[test]
fn test_jstack_thread_dump_format() {
    let threads = vec![ThreadSnapshot {
        id: 1,
        name: "main".to_string(),
        daemon: false,
        priority: 5,
        state: ThreadState::Runnable,
        stack_frames: vec![FrameInfo {
            class_name: "com.example.Main".to_string(),
            method_name: "run".to_string(),
            file_name: Some("Main.java".to_string()),
            line_number: 42,
            native_method: false,
        }],
        lock_info: None,
        blocked_by: None,
        waiting_on: None,
        wait_kind: None,
        dump: Default::default(),
    }];

    let dump = JstackProcessor::generate_thread_dump(&threads);
    assert!(dump.contains("\"main\" #1 [0] prio=5 os_prio=0 tid=0x"));
    assert!(dump.contains("java.lang.Thread.State: RUNNABLE"));
    assert!(dump.contains("at com.example.Main.run(Main.java:42)"));
}

#[test]
fn test_jstack_daemon_thread() {
    let threads = vec![ThreadSnapshot {
        id: 5,
        name: "GC-Worker".to_string(),
        daemon: true,
        priority: 8,
        state: ThreadState::Waiting,
        stack_frames: vec![],
        lock_info: None,
        blocked_by: None,
        waiting_on: None,
        wait_kind: None,
        dump: Default::default(),
    }];

    let dump = JstackProcessor::generate_thread_dump(&threads);
    assert!(dump.contains("\"GC-Worker\" #5 [0] daemon prio=8"));
    assert!(dump.contains("java.lang.Thread.State: WAITING"));
}

#[test]
fn test_jstack_native_method() {
    let threads = vec![ThreadSnapshot {
        id: 3,
        name: "native-thread".to_string(),
        daemon: false,
        priority: 5,
        state: ThreadState::Runnable,
        stack_frames: vec![FrameInfo {
            class_name: "sun.misc.Unsafe".to_string(),
            method_name: "park".to_string(),
            file_name: None,
            line_number: -1,
            native_method: true,
        }],
        lock_info: None,
        blocked_by: None,
        waiting_on: None,
        wait_kind: None,
        dump: Default::default(),
    }];

    let dump = JstackProcessor::generate_thread_dump(&threads);
    assert!(dump.contains("at sun.misc.Unsafe.park(Native Method)"));
}

#[test]
fn test_jstack_lock_info() {
    let threads = vec![ThreadSnapshot {
        id: 1,
        name: "locker".to_string(),
        daemon: false,
        priority: 5,
        state: ThreadState::Runnable,
        stack_frames: vec![],
        lock_info: Some(LockInfo {
            class_name: "java.util.HashMap".to_string(),
            identity_hash: 0xDEAD_BEEF,
        }),
        blocked_by: None,
        waiting_on: None,
        wait_kind: None,
        dump: Default::default(),
    }];

    let dump = JstackProcessor::generate_thread_dump(&threads);
    assert!(dump.contains("locked <0x00000000deadbeef> (a java.util.HashMap)"));
}

#[test]
fn test_jstack_waiting_on() {
    let threads = vec![ThreadSnapshot {
        id: 2,
        name: "waiter".to_string(),
        daemon: false,
        priority: 5,
        state: ThreadState::Waiting,
        stack_frames: vec![],
        lock_info: None,
        blocked_by: None,
        waiting_on: Some("<0x000000c0> (a java.lang.Object)".to_string()),
        wait_kind: None,
        dump: Default::default(),
    }];

    let dump = JstackProcessor::generate_thread_dump(&threads);
    assert!(dump.contains("waiting on <0x000000c0> (a java.lang.Object)"));
}

#[test]
fn test_jstack_deadlock_detection_no_deadlock() {
    let threads = vec![
        ThreadSnapshot {
            id: 1,
            name: "t1".to_string(),
            daemon: false,
            priority: 5,
            state: ThreadState::Runnable,
            stack_frames: vec![],
            lock_info: None,
            blocked_by: None,
            waiting_on: None,
            wait_kind: None,
            dump: Default::default(),
        },
        ThreadSnapshot {
            id: 2,
            name: "t2".to_string(),
            daemon: false,
            priority: 5,
            state: ThreadState::Runnable,
            stack_frames: vec![],
            lock_info: None,
            blocked_by: None,
            waiting_on: None,
            wait_kind: None,
            dump: Default::default(),
        },
    ];
    assert!(JstackProcessor::generate_deadlock_report(&threads).is_none());
}

#[test]
fn test_jstack_deadlock_detection_cycle() {
    let threads = vec![
        ThreadSnapshot {
            id: 1,
            name: "thread-A".to_string(),
            daemon: false,
            priority: 5,
            state: ThreadState::Blocked,
            stack_frames: vec![],
            lock_info: None,
            blocked_by: Some(2),
            waiting_on: None,
            wait_kind: None,
            dump: Default::default(),
        },
        ThreadSnapshot {
            id: 2,
            name: "thread-B".to_string(),
            daemon: false,
            priority: 5,
            state: ThreadState::Blocked,
            stack_frames: vec![],
            lock_info: None,
            blocked_by: Some(1),
            waiting_on: None,
            wait_kind: None,
            dump: Default::default(),
        },
    ];
    let report = JstackProcessor::generate_deadlock_report(&threads);
    assert!(report.is_some());
    let report = report.unwrap();
    assert!(report.contains("deadlock"));
    assert!(report.contains("thread-A"));
    assert!(report.contains("thread-B"));
}

#[test]
fn test_jstack_empty_threads() {
    let dump = JstackProcessor::generate_thread_dump(&[]);
    assert!(dump.contains("Full thread dump CratonVM"));
}

#[test]
fn test_jstack_multiple_frames() {
    let threads = vec![ThreadSnapshot {
        id: 1,
        name: "deep-stack".to_string(),
        daemon: false,
        priority: 5,
        state: ThreadState::Runnable,
        stack_frames: vec![
            FrameInfo {
                class_name: "com.a.A".to_string(),
                method_name: "foo".to_string(),
                file_name: Some("A.java".to_string()),
                line_number: 10,
                native_method: false,
            },
            FrameInfo {
                class_name: "com.b.B".to_string(),
                method_name: "bar".to_string(),
                file_name: Some("B.java".to_string()),
                line_number: 20,
                native_method: false,
            },
        ],
        lock_info: None,
        blocked_by: None,
        waiting_on: None,
        wait_kind: None,
        dump: Default::default(),
    }];

    let dump = JstackProcessor::generate_thread_dump(&threads);
    assert!(dump.contains("at com.a.A.foo(A.java:10)"));
    assert!(dump.contains("at com.b.B.bar(B.java:20)"));
}

// --- Interpreter round i3 wave 81, lane L6: HotSpot 25.0.4's dump shape ---
//
// The expected texts are the HotSpot 25.0.4 `SIGQUIT` dump of
// `M81Sleeper` measured for the page `i59-L1-sigquit-terminates-the-vm`
// (its "Fixed (wave 81)" section quotes them).

fn i81_snapshot(
    name: &str,
    state: ThreadState,
    wait_kind: Option<DumpWaitKind>,
    daemon: bool,
    priority: i32,
    dump: ThreadDumpFacts,
) -> ThreadSnapshot {
    ThreadSnapshot {
        id: 99,
        name: name.to_string(),
        daemon,
        priority,
        state,
        stack_frames: Vec::new(),
        lock_info: None,
        blocked_by: None,
        waiting_on: None,
        wait_kind,
        dump,
    }
}

fn i81_frame(class: &str, method: &str, file: Option<&str>, line: i32) -> FrameInfo {
    FrameInfo {
        class_name: class.to_string(),
        method_name: method.to_string(),
        file_name: file.map(str::to_string),
        line_number: line,
        native_method: line == -2,
    }
}

#[test]
fn i81_thread_line_is_hotspots() {
    let main = i81_snapshot(
        "main",
        ThreadState::TimedWaiting,
        Some(DumpWaitKind::Sleeping),
        false,
        5,
        ThreadDumpFacts {
            java_tid: Some(3),
            os_tid: Some(1_332_861),
            cpu_nanos: Some(22_080_000),
            elapsed_nanos: Some(3_020_000_000),
            thread_addr: 0x0000_787a_4c02_a620,
            stack_addr: 0x0000_787a_539f_e000,
            ..Default::default()
        },
    );
    assert_eq!(
        JstackProcessor::thread_dump_line(&main),
        "\"main\" #3 [1332861] prio=5 os_prio=0 cpu=22.08ms elapsed=3.02s \
         tid=0x0000787a4c02a620 nid=1332861 waiting on condition  [0x0000787a539fe000]"
    );
    let finalizer = i81_snapshot(
        "Finalizer",
        ThreadState::Waiting,
        Some(DumpWaitKind::ObjectWait),
        true,
        8,
        ThreadDumpFacts {
            java_tid: Some(15),
            os_tid: Some(1_332_873),
            cpu_nanos: Some(140_000),
            elapsed_nanos: Some(3_010_000_000),
            thread_addr: 0x0000_787a_4c0f_2cb0,
            stack_addr: 0x0000_787a_287f_e000,
            ..Default::default()
        },
    );
    assert_eq!(
        JstackProcessor::thread_dump_line(&finalizer),
        "\"Finalizer\" #15 [1332873] daemon prio=8 os_prio=0 cpu=0.14ms elapsed=3.01s \
         tid=0x0000787a4c0f2cb0 nid=1332873 in Object.wait()  [0x0000787a287fe000]"
    );
    // RUNNABLE in a wait inside the VM: HotSpot's Reference Handler.
    let handler = i81_snapshot(
        "Reference Handler",
        ThreadState::Runnable,
        None,
        true,
        10,
        ThreadDumpFacts {
            java_tid: Some(14),
            os_tid: Some(1_332_872),
            cpu_nanos: Some(120_000),
            elapsed_nanos: Some(3_010_000_000),
            thread_addr: 0x0000_787a_4c0f_1ac0,
            stack_addr: 0x0000_787a_28bf_4000,
            vm_wait: true,
            ..Default::default()
        },
    );
    assert_eq!(
        JstackProcessor::thread_dump_line(&handler),
        "\"Reference Handler\" #14 [1332872] daemon prio=10 os_prio=0 cpu=0.12ms elapsed=3.01s \
         tid=0x0000787a4c0f1ac0 nid=1332872 waiting on condition  [0x0000787a28bf4000]"
    );
    // Unknown CPU and elapsed time: the fields are left out, the rest stays.
    let bare = i81_snapshot(
        "t",
        ThreadState::Runnable,
        None,
        false,
        5,
        ThreadDumpFacts::default(),
    );
    assert_eq!(
        JstackProcessor::thread_dump_line(&bare),
        "\"t\" #99 [0] prio=5 os_prio=0 tid=0x0000000000000000 nid=0 runnable  [0x0000000000000000]"
    );
}

#[test]
fn i81_frame_line_has_the_module_and_hotspots_locations() {
    let native = i81_frame("java.lang.Thread", "sleepNanos0", Some("Thread.java"), -2);
    assert_eq!(
        JstackProcessor::frame_line(&native, Some("java.base@25.0.4")),
        "\tat java.lang.Thread.sleepNanos0(java.base@25.0.4/Native Method)"
    );
    let jdk = i81_frame("java.lang.Thread", "sleep", Some("Thread.java"), 540);
    assert_eq!(
        JstackProcessor::frame_line(&jdk, Some("java.base@25.0.4")),
        "\tat java.lang.Thread.sleep(java.base@25.0.4/Thread.java:540)"
    );
    let app = i81_frame("M81Sleeper", "main", Some("M81Sleeper.java"), 7);
    assert_eq!(
        JstackProcessor::frame_line(&app, None),
        "\tat M81Sleeper.main(M81Sleeper.java:7)"
    );
    let proxy = i81_frame("M81Sleeper$$Lambda/0x0000000039040210", "run", None, -1);
    assert_eq!(
        JstackProcessor::frame_line(&proxy, None),
        "\tat M81Sleeper$$Lambda/0x0000000039040210.run(Unknown Source)"
    );
    // A line the VM does not know prints the file alone, never `:-1`.
    let unknown = i81_frame("java.lang.Object", "wait", Some("Object.java"), -1);
    assert_eq!(
        JstackProcessor::frame_line(&unknown, Some("java.base")),
        "\tat java.lang.Object.wait(java.base/Object.java)"
    );
}

#[test]
fn i81_smr_info_puts_four_addresses_on_a_line() {
    let elements = [
        0x0000_787a_4c02_a620,
        0x0000_787a_4c0f_1ac0,
        0x0000_787a_4c0f_2cb0,
        0x0000_787a_4c0f_4700,
        0x0000_787a_4c0f_6020,
        0x0000_787a_4c0f_78a0,
        0x0000_787a_4c0f_96a0,
        0x0000_787a_4c0f_b090,
        0x0000_787a_4c10_7430,
        0x0000_787a_4c10_a200,
        0x0000_787a_4c11_1d60,
    ];
    assert_eq!(
        JstackProcessor::smr_info(0x0000_787a_4c11_2f60, &elements),
        "Threads class SMR info:\n\
         _java_thread_list=0x0000787a4c112f60, length=11, elements={\n\
         0x0000787a4c02a620, 0x0000787a4c0f1ac0, 0x0000787a4c0f2cb0, 0x0000787a4c0f4700,\n\
         0x0000787a4c0f6020, 0x0000787a4c0f78a0, 0x0000787a4c0f96a0, 0x0000787a4c0fb090,\n\
         0x0000787a4c107430, 0x0000787a4c10a200, 0x0000787a4c111d60\n\
         }\n"
    );
    assert_eq!(
        JstackProcessor::smr_info(0x10, &[]),
        "Threads class SMR info:\n_java_thread_list=0x0000000000000010, length=0, elements={\n}\n"
    );
}

#[test]
fn i81_lock_lines_follow_print_lock_info_on() {
    let lock = DumpMonitor {
        address: 0x0000_0006_29c1_6e28,
        class_name: "java.lang.Object".to_string(),
    };
    // `M81Sleeper`'s waiter: `wait0` on top, the monitor its lambda locked.
    let lines = dump_lock_lines(
        Some(("java.lang.Object", "wait0")),
        ThreadState::Waiting,
        Some(&lock),
        None,
        None,
        &[(3, lock.clone())],
    );
    assert_eq!(
        lines,
        vec![
            (
                0,
                "waiting on <0x0000000629c16e28> (a java.lang.Object)".to_string()
            ),
            (
                3,
                "locked <0x0000000629c16e28> (a java.lang.Object)".to_string()
            ),
        ]
    );
    // Notified and re-entering: BLOCKED with `wait0` on top.
    let lines = dump_lock_lines(
        Some(("java.lang.Object", "wait0")),
        ThreadState::Blocked,
        Some(&lock),
        None,
        Some(&lock),
        &[(3, lock.clone())],
    );
    assert_eq!(
        lines[0].1,
        "waiting to re-lock in wait() <0x0000000629c16e28> (a java.lang.Object)"
    );
    assert_eq!(lines.len(), 2);
    // Contending in a `synchronized` block of frame 0, holding another
    // monitor one frame down.
    let held = DumpMonitor {
        address: 0x10,
        class_name: "java.lang.String".to_string(),
    };
    let lines = dump_lock_lines(
        Some(("p.C", "enter")),
        ThreadState::Blocked,
        None,
        None,
        Some(&lock),
        &[(1, held.clone())],
    );
    assert_eq!(
        lines,
        vec![
            (
                0,
                "waiting to lock <0x0000000629c16e28> (a java.lang.Object)".to_string()
            ),
            (
                1,
                "locked <0x0000000000000010> (a java.lang.String)".to_string()
            ),
        ]
    );
    // `LockSupport.park(blocker)`: HotSpot's `"\t- %s <"` with
    // `"parking to wait for "` -- two spaces.
    let blocker = DumpMonitor {
        address: 0x20,
        class_name: "java.util.concurrent.CountDownLatch$Sync".to_string(),
    };
    let lines = dump_lock_lines(
        Some(("jdk.internal.misc.Unsafe", "park")),
        ThreadState::Waiting,
        None,
        Some(&blocker),
        None,
        &[],
    );
    assert_eq!(
        lines,
        vec![(
            0,
            "parking to wait for  <0x0000000000000020> (a java.util.concurrent.CountDownLatch$Sync)"
                .to_string()
        )]
    );
    // No frame: no line at all.
    assert!(
        dump_lock_lines(None, ThreadState::Blocked, None, None, Some(&lock), &[]).is_empty()
    );
}

#[test]
fn i81_dump_layout_is_hotspots() {
    let mut waiter = i81_snapshot(
        "waiter",
        ThreadState::Waiting,
        Some(DumpWaitKind::ObjectWait),
        true,
        5,
        ThreadDumpFacts {
            java_tid: Some(25),
            os_tid: Some(1_332_881),
            cpu_nanos: Some(100_000),
            elapsed_nanos: Some(2_990_000_000),
            thread_addr: 0x0000_787a_4c11_1d60,
            stack_addr: 0x0000_7879_e7ff_e000,
            vm_wait: false,
            frame_modules: vec![
                Some("java.base@25.0.4".to_string()),
                Some("java.base@25.0.4".to_string()),
                Some("java.base@25.0.4".to_string()),
                None,
                None,
                Some("java.base@25.0.4".to_string()),
                Some("java.base@25.0.4".to_string()),
            ],
            lock_lines: vec![
                (
                    0,
                    "waiting on <0x0000000629c16e28> (a java.lang.Object)".to_string(),
                ),
                (
                    3,
                    "locked <0x0000000629c16e28> (a java.lang.Object)".to_string(),
                ),
            ],
            no_java_thread: false,
            carrying_virtual_thread: None,
            owned_synchronizers: None,
        },
    );
    waiter.stack_frames = vec![
        i81_frame("java.lang.Object", "wait0", Some("Object.java"), -2),
        i81_frame("java.lang.Object", "wait", Some("Object.java"), 389),
        i81_frame("java.lang.Object", "wait", Some("Object.java"), 351),
        i81_frame("M81Sleeper", "lambda$main$0", Some("M81Sleeper.java"), 4),
        i81_frame("M81Sleeper$$Lambda/0x0000000039040210", "run", None, -1),
        i81_frame("java.lang.Thread", "runWith", Some("Thread.java"), 1487),
        i81_frame("java.lang.Thread", "run", Some("Thread.java"), 1474),
    ];
    let header = ThreadDumpHeader {
        timestamp: "2026-10-09 14:48:14".to_string(),
        vm_name: "OpenJDK 64-Bit Server VM".to_string(),
        vm_version: "25.0.4+7-LTS".to_string(),
        vm_info: "mixed mode, sharing".to_string(),
        thread_list_addr: 0x0000_787a_4c11_2f60,
        jni_global_refs: 6,
        jni_weak_refs: 0,
    };
    let dump = JstackProcessor::generate_thread_dump_with(&header, &[waiter]);
    assert_eq!(
        dump,
        "2026-10-09 14:48:14\n\
         Full thread dump OpenJDK 64-Bit Server VM (25.0.4+7-LTS mixed mode, sharing):\n\
         \n\
         Threads class SMR info:\n\
         _java_thread_list=0x0000787a4c112f60, length=1, elements={\n\
         0x0000787a4c111d60\n\
         }\n\
         \n\
         \"waiter\" #25 [1332881] daemon prio=5 os_prio=0 cpu=0.10ms elapsed=2.99s \
         tid=0x0000787a4c111d60 nid=1332881 in Object.wait()  [0x00007879e7ffe000]\n   \
         java.lang.Thread.State: WAITING (on object monitor)\n\
         \tat java.lang.Object.wait0(java.base@25.0.4/Native Method)\n\
         \t- waiting on <0x0000000629c16e28> (a java.lang.Object)\n\
         \tat java.lang.Object.wait(java.base@25.0.4/Object.java:389)\n\
         \tat java.lang.Object.wait(java.base@25.0.4/Object.java:351)\n\
         \tat M81Sleeper.lambda$main$0(M81Sleeper.java:4)\n\
         \t- locked <0x0000000629c16e28> (a java.lang.Object)\n\
         \tat M81Sleeper$$Lambda/0x0000000039040210.run(Unknown Source)\n\
         \tat java.lang.Thread.runWith(java.base@25.0.4/Thread.java:1487)\n\
         \tat java.lang.Thread.run(java.base@25.0.4/Thread.java:1474)\n\
         \n\
         JNI global refs: 6, weak refs: 0\n\
         \n"
    );
}

/// A VM thread with no `java.lang.Thread` goes after the Java threads in
/// HotSpot's non-Java form, outside the SMR list.
#[test]
fn i81_vm_own_threads_print_as_non_java_threads() {
    let java = i81_snapshot(
        "main",
        ThreadState::Runnable,
        None,
        false,
        5,
        ThreadDumpFacts {
            java_tid: Some(1),
            thread_addr: 0x1000,
            ..Default::default()
        },
    );
    let service = i81_snapshot(
        "Craton Gen Concurrent Mark",
        ThreadState::Runnable,
        None,
        true,
        5,
        ThreadDumpFacts {
            os_tid: Some(42),
            cpu_nanos: Some(70_000),
            elapsed_nanos: Some(3_020_000_000),
            thread_addr: 0x2000,
            vm_wait: true,
            no_java_thread: true,
            ..Default::default()
        },
    );
    assert_eq!(
        JstackProcessor::non_java_thread_line(&service),
        "\"Craton Gen Concurrent Mark\" os_prio=0 cpu=0.07ms elapsed=3.02s \
         tid=0x0000000000002000 nid=42 waiting on condition  "
    );
    let header = ThreadDumpHeader::from_properties(&[]);
    let dump = JstackProcessor::generate_thread_dump_with(&header, &[service, java]);
    assert!(
        dump.contains("length=1, elements={\n0x0000000000001000\n}\n"),
        "{dump}"
    );
    let main_at = dump.find("\"main\" #1").expect("main");
    let service_at = dump
        .find("\"Craton Gen Concurrent Mark\" os_prio=0")
        .expect("service");
    assert!(main_at < service_at, "{dump}");
    assert!(
        dump.ends_with("waiting on condition  \n\nJNI global refs: 0, weak refs: 0\n\n"),
        "{dump}"
    );
}

#[test]
fn i81_header_names_the_vm_by_its_properties() {
    let props = vec![
        ("java.vm.name".to_string(), "cratonvm".to_string()),
        ("java.vm.version".to_string(), "25.0.1+8".to_string()),
        (
            "java.vm.info".to_string(),
            "mixed mode, jdk-only".to_string(),
        ),
    ];
    let header = ThreadDumpHeader::from_properties(&props);
    let dump = JstackProcessor::generate_thread_dump_with(&header, &[]);
    let mut lines = dump.lines();
    let stamp = lines.next().expect("a timestamp line");
    assert_eq!(stamp.len(), "yyyy-MM-dd HH:mm:ss".len(), "{stamp}");
    assert_eq!(
        lines.next(),
        Some("Full thread dump cratonvm (25.0.1+8 mixed mode, jdk-only):")
    );
    assert_eq!(lines.next(), Some(""));
    assert_eq!(lines.next(), Some("Threads class SMR info:"));
    assert!(
        dump.ends_with("}\n\nJNI global refs: 0, weak refs: 0\n\n"),
        "{dump}"
    );
}

#[test]
fn i81_external_class_name_keeps_a_hidden_class_suffix() {
    assert_eq!(external_class_name("java/lang/Object"), "java.lang.Object");
    assert_eq!(
        external_class_name("p/M81Sleeper$$Lambda/0x0000000039040210"),
        "p.M81Sleeper$$Lambda/0x0000000039040210"
    );
    assert_eq!(external_class_name("a/0xfeed/B"), "a.0xfeed.B");
    assert_eq!(external_class_name("Plain"), "Plain");
}

#[test]
fn i81_utc_time_string_is_civil() {
    assert_eq!(dump_utc_time_string(0), "1970-01-01 00:00:00");
    assert_eq!(dump_utc_time_string(951_868_800), "2000-03-01 00:00:00");
    assert_eq!(dump_utc_time_string(1_791_557_291), "2026-10-09 14:48:11");
    assert_eq!(dump_utc_time_string(4_107_542_399), "2100-02-28 23:59:59");
}

/// gci i14/hd: the heap-info lines print one per line, each ended as
/// `print_cr` ends it; the trait's default states nothing (a mock keeps
/// the summary text).
#[test]
fn gci_i14hd_heap_info_text_ends_every_line() {
    assert_eq!(
        heap_info_text(&["Heap".to_string(), " Metaspace       used 1K".to_string()]),
        "Heap\n Metaspace       used 1K\n"
    );
    assert_eq!(heap_info_text(&[]), "");
    assert!(VmDiagnosticState::heap_info_lines(&MockVmState).is_none());
}

/// gci i47w/oa47: `jcmd GC.heap_info` is HotSpot 25's `print_heap_on`
/// alone (vi14-hd H, Temurin 25.0.3): the `Universe::print_on` lines lose
/// their `Heap` header, their Metaspace lines and one column of indent.
#[test]
fn gci_i47w_oa47_heap_info_dcmd_is_print_heap_on_alone() {
    let g1 = [
        "Heap".to_string(),
        " garbage-first heap   total reserved 131072K, committed 131072K, used 4754K [0x00000000f8000000, 0x0000000100000000)".to_string(),
        "  region size 1024K, 4 young (4096K), 0 survivors (0K)".to_string(),
        " Metaspace       used 8546K, committed 8704K, reserved 1114112K".to_string(),
        "  class space    used 677K, committed 768K, reserved 1048576K".to_string(),
    ];
    assert_eq!(
        heap_info_dcmd_text(&g1),
        "garbage-first heap   total reserved 131072K, committed 131072K, used 4754K [0x00000000f8000000, 0x0000000100000000)\n region size 1024K, 4 young (4096K), 0 survivors (0K)\n"
    );
    let serial = [
        "Heap".to_string(),
        " DefNew     total 39296K, used 4193K [0x1, 0x2, 0x3)".to_string(),
        "  eden space 34944K,  12% used [0x1, 0x2, 0x3)".to_string(),
        " Tenured    total 87424K, used 0K [0x1, 0x2, 0x3)".to_string(),
        "   the space 87424K,   0% used [0x1, 0x2, 0x3)".to_string(),
        " Metaspace       used 1K, committed 1K, reserved 1K".to_string(),
    ];
    assert_eq!(
        heap_info_dcmd_text(&serial),
        "DefNew     total 39296K, used 4193K [0x1, 0x2, 0x3)\n eden space 34944K,  12% used [0x1, 0x2, 0x3)\nTenured    total 87424K, used 0K [0x1, 0x2, 0x3)\n  the space 87424K,   0% used [0x1, 0x2, 0x3)\n"
    );
    // The SIGQUIT section keeps the full `Universe::print_on` text.
    assert!(heap_info_text(&g1).starts_with("Heap\n garbage-first heap"));
}

#[test]
fn i81_sigquit_heap_section_is_kib() {
    let summary = HeapSummary {
        young_gen_used: 2048,
        young_gen_capacity: 4096,
        old_gen_used: 1024,
        old_gen_capacity: 8192,
        metaspace_used: 3072,
        metaspace_capacity: 3072,
        total_used: 3072,
        total_capacity: 12288,
    };
    let heap = JstackProcessor::sigquit_heap_section(&summary);
    assert!(
        heap.starts_with("Heap\n java heap   total 12K, used 3K\n"),
        "{heap}"
    );
    assert!(
        heap.ends_with(" Metaspace       used 3K, committed 3K\n\n"),
        "{heap}"
    );
}

// --- jmap tests ---

#[test]
fn test_jmap_class_histogram() {
    let entries = vec![
        ClassHistogramEntry {
            class_name: "[B".to_string(),
            instance_count: 1000,
            total_bytes: 50000,
        },
        ClassHistogramEntry {
            class_name: "java.lang.String".to_string(),
            instance_count: 500,
            total_bytes: 20000,
        },
    ];
    let output = JmapProcessor::generate_class_histogram(&entries);
    assert!(output.contains("#instances"));
    assert!(output.contains("[B"));
    assert!(output.contains("java.lang.String"));
    assert!(output.contains("Total "));
    assert!(output.contains("1500")); // total instances
    assert!(output.contains("70000")); // total bytes
}

#[test]
fn test_jmap_class_histogram_empty() {
    let output = JmapProcessor::generate_class_histogram(&[]);
    assert!(output.contains("#instances"));
    assert!(output.contains("Total "));
}

/// gci i3/hd: HotSpot 25's `KlassInfoHisto` text byte for byte (header,
/// 55-dash rule, `%4d: %13d  %13u  name (module)` rows, `Total %13d
/// %13u`, no closing rule).
#[test]
fn gci_i3hd_class_histogram_text_is_hotspots() {
    let entries = vec![
        ClassHistogramEntry {
            class_name: "[B (java.base@25.0.3)".to_string(),
            instance_count: 1234,
            total_bytes: 567890,
        },
        ClassHistogramEntry {
            class_name: "App$Node".to_string(),
            instance_count: 7,
            total_bytes: 168,
        },
    ];
    let output = JmapProcessor::generate_class_histogram(&entries);
    // Spelled with explicit space counts: a literal with a long space run trips
    // `types/tests/string_literal_line_breaks.rs`.
    let sp = |n: usize| " ".repeat(n);
    let expected = [
        format!(" num{}#instances{}#bytes  class name (module)\n", sp(5), sp(9)),
        "-------------------------------------------------------\n".to_string(),
        format!("   1:{}1234{}567890  [B (java.base@25.0.3)\n", sp(10), sp(9)),
        format!("   2:{}7{}168  App$Node\n", sp(13), sp(12)),
        format!("Total{}1241{}568058\n", sp(10), sp(9)),
    ]
    .concat();
    assert_eq!(output, expected);
    assert_eq!(output.lines().nth(1).map(str::len), Some(55));
}

/// gci i3/hd: `GC.heap_dump [options] <filename>`.
#[test]
fn gci_i3hd_heap_dump_args_follow_hotspots_grammar() {
    let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(heap_dump_args(&v(&[])), ("heap.hprof".to_string(), false));
    assert_eq!(heap_dump_args(&v(&["/tmp/a"])), ("/tmp/a".to_string(), false));
    assert_eq!(
        heap_dump_args(&v(&["-overwrite", "/tmp/a"])),
        ("/tmp/a".to_string(), true)
    );
    assert_eq!(
        heap_dump_args(&v(&["-all", "-gz=1", "-parallel=2", "/tmp/a", "/tmp/b"])),
        ("/tmp/a".to_string(), false)
    );
    assert_eq!(
        heap_dump_args(&v(&["-overwrite=false", "/tmp/a"])),
        ("/tmp/a".to_string(), false)
    );
    assert!(heap_dump_args(&v(&["-overwrite=true", "x"])).1);
}

/// gci i3/hd: the live `GC.heap_dump` refuses an existing file as
/// HotSpot does (`Unable to create <file>: File exists`) and goes on to the
/// dump with `-overwrite`.
#[test]
fn gci_i3hd_live_heap_dump_refuses_an_existing_file() {
    #[cfg(windows)]
    let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
    let tmp = std::env::temp_dir().join("cratonvm_gci_i3hd_exists.hprof");
    std::fs::write(&tmp, b"x").unwrap();
    let path = tmp.to_str().unwrap().to_string();
    // The processor holds the state weakly: keep it alive.
    let state: Arc<dyn VmDiagnosticState> = Arc::new(MockVmState);
    let processor = JcmdProcessor::new_with_vm_state(Arc::clone(&state));
    let refused = processor
        .attach_listener
        .dispatch_command("GC.heap_dump", &[path.clone()])
        .expect("registered");
    assert_eq!(refused.output, format!("Unable to create {path}: File exists"));
    assert_eq!(std::fs::read(&tmp).unwrap(), b"x", "the file is untouched");
    let tried = processor
        .attach_listener
        .dispatch_command("GC.heap_dump", &["-overwrite".to_string(), path.clone()])
        .expect("registered");
    assert!(
        tried.error.as_deref().is_some_and(|e| e.contains("heap dump not supported")),
        "{tried:?}"
    );
    drop(state);
    let _ = std::fs::remove_file(&tmp);
}

/// gci i3/hd: `jcmd GC.heap_dump`'s two HotSpot lines.
#[test]
fn gci_i3hd_heap_dump_lines_are_hotspots() {
    let text =
        hotspot_heap_dump_lines("/tmp/x.hprof", 12345, std::time::Duration::from_millis(1500));
    assert_eq!(
        text,
        "Dumping heap to /tmp/x.hprof ...\nHeap dump file created [12345 bytes in 1.500 secs]"
    );
}

/// gci i4/hd: HotSpot's `-all` dcmd option and the two `jmap` attach
/// operations' live/all argument.
#[test]
fn gci_i4hd_all_option_and_jmap_live_args_follow_hotspot() {
    let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert!(!dcmd_all_objects(&v(&[])));
    assert!(dcmd_all_objects(&v(&["-all"])));
    assert!(dcmd_all_objects(&v(&["-all=true", "/tmp/a"])));
    assert!(!dcmd_all_objects(&v(&["-all=false"])));
    assert!(!dcmd_all_objects(&v(&["-overwrite", "/tmp/a"])));
    // jmap -histo (the client sends -all), -histo:live (-live), an old
    // client's empty argument (live, as attachListener.cpp reads it).
    assert_eq!(attach_heap_walk_args("inspectheap", "-all", ""), v(&["-all"]));
    assert!(attach_heap_walk_args("inspectheap", "-live", "").is_empty());
    assert!(attach_heap_walk_args("inspectheap", "", "").is_empty());
    // jmap -dump:file=X (-all), -dump:live,file=X (-live), empty (all).
    assert_eq!(attach_heap_walk_args("dumpheap", "/tmp/x", "-all"), v(&["-all", "/tmp/x"]));
    assert_eq!(attach_heap_walk_args("dumpheap", "/tmp/x", "-live"), v(&["/tmp/x"]));
    assert_eq!(attach_heap_walk_args("dumpheap", "/tmp/x", ""), v(&["-all", "/tmp/x"]));
    assert_eq!(heap_dump_args(&attach_heap_walk_args("dumpheap", "/tmp/x", "-all")).0, "/tmp/x");
}

/// gci i5/hd (review item 10): `-gz=<level>` as HotSpot's `HeapDumpDCmd`
/// reads it -- absent, 1-9, out of range (printed, no dump), not an
/// integer (the parse exception); the last one wins.
#[test]
fn gci_i5hd_gz_level_follows_hotspot() {
    let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(heap_dump_gz_level(&v(&["/tmp/a"])), Ok(None));
    assert_eq!(heap_dump_gz_level(&v(&["-gz=1", "/tmp/a"])), Ok(Some(1)));
    assert_eq!(heap_dump_gz_level(&v(&["-gz=9", "-overwrite", "/tmp/a"])), Ok(Some(9)));
    assert_eq!(heap_dump_gz_level(&v(&["-gz=3", "-gz=5", "x"])), Ok(Some(5)));
    assert_eq!(
        heap_dump_gz_level(&v(&["-gz=0", "x"])),
        Err(GzLevelError::OutOfRange("Compression level out of range (1-9): 0".to_string()))
    );
    assert_eq!(
        heap_dump_gz_level(&v(&["-gz=10", "x"])),
        Err(GzLevelError::OutOfRange("Compression level out of range (1-9): 10".to_string()))
    );
    assert_eq!(
        heap_dump_gz_level(&v(&["-gz=-1", "x"])),
        Err(GzLevelError::OutOfRange("Compression level out of range (1-9): -1".to_string()))
    );
    assert!(matches!(
        heap_dump_gz_level(&v(&["-gz=fast", "x"])),
        Err(GzLevelError::Unparsable(t)) if t.ends_with("Could not parse: fast.")
    ));
    // The path is still the first non-option token.
    assert_eq!(heap_dump_args(&v(&["-gz=1", "/tmp/a"])).0, "/tmp/a");
}

/// gci i5/hd: an out-of-range `-gz` answers HotSpot's line and dumps
/// nothing (the file is not even created).
#[test]
fn gci_i5hd_out_of_range_gz_dumps_nothing() {
    #[cfg(windows)]
    let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
    let tmp = std::env::temp_dir().join("cratonvm_gci_i5hd_gz_range.hprof");
    let _ = std::fs::remove_file(&tmp);
    let path = tmp.to_str().unwrap().to_string();
    let state: Arc<dyn VmDiagnosticState> = Arc::new(MockVmState);
    let processor = JcmdProcessor::new_with_vm_state(Arc::clone(&state));
    let r = processor
        .attach_listener
        .dispatch_command("GC.heap_dump", &["-gz=12".to_string(), path.clone()])
        .expect("registered");
    assert_eq!(r.output, "Compression level out of range (1-9): 12");
    assert!(!tmp.exists());
    drop(state);
}

/// A state recording the `live` argument of the attach heap walks.
struct LiveRecordingState(parking_lot::Mutex<Vec<(&'static str, bool)>>);
impl VmDiagnosticState for LiveRecordingState {
    fn thread_snapshots(&self) -> Vec<ThreadSnapshot> {
        Vec::new()
    }
    fn heap_summary(&self) -> HeapSummary {
        MockVmState.heap_summary()
    }
    fn class_histogram(&self) -> Vec<ClassHistogramEntry> {
        MockVmState.class_histogram()
    }
    fn class_histogram_live(&self, live: bool) -> Vec<ClassHistogramEntry> {
        self.0.lock().push(("histo", live));
        self.class_histogram()
    }
    fn heap_dump_live(&self, path: &str, live: bool) -> Result<u64, String> {
        self.0.lock().push(("dump", live));
        self.heap_dump(path)
    }
    fn trigger_gc(&self) -> bool {
        true
    }
    fn uptime_secs(&self) -> f64 {
        1.0
    }
    fn command_line(&self) -> String {
        String::new()
    }
    fn system_properties(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    fn vm_flags(&self) -> Vec<String> {
        Vec::new()
    }
}

/// gci i4/hd (`gci-i3hd-jcmd-heap-walks-count-dead-objects-FIXED-20261013`): the
/// live commands ask for live objects unless `-all`.
#[test]
fn gci_i4hd_live_heap_walk_commands_pass_live_unless_all() {
    #[cfg(windows)]
    let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
    let rec = Arc::new(LiveRecordingState(parking_lot::Mutex::new(Vec::new())));
    let state: Arc<dyn VmDiagnosticState> = rec.clone();
    let processor = JcmdProcessor::new_with_vm_state(Arc::clone(&state));
    let run = |name: &str, args: &[&str]| {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        processor
            .attach_listener
            .dispatch_command(name, &args)
            .expect("registered")
    };
    let _ = run("GC.class_histogram", &[]);
    let _ = run("GC.class_histogram", &["-all"]);
    let missing = std::env::temp_dir().join("cratonvm_gci_i4hd_live_absent.hprof");
    let _ = std::fs::remove_file(&missing);
    let missing = missing.to_str().unwrap().to_string();
    let _ = run("GC.heap_dump", &[missing.as_str()]);
    let _ = run("GC.heap_dump", &["-all", missing.as_str()]);
    assert_eq!(
        *rec.0.lock(),
        vec![("histo", true), ("histo", false), ("dump", true), ("dump", false)]
    );
    drop(state);
}

#[test]
fn test_jmap_heap_summary() {
    let summary = HeapSummary {
        young_gen_used: 25 * 1024 * 1024,
        young_gen_capacity: 64 * 1024 * 1024,
        old_gen_used: 100 * 1024 * 1024,
        old_gen_capacity: 256 * 1024 * 1024,
        metaspace_used: 30 * 1024 * 1024,
        metaspace_capacity: 64 * 1024 * 1024,
        total_used: 155 * 1024 * 1024,
        total_capacity: 384 * 1024 * 1024,
    };
    let output = JmapProcessor::generate_heap_summary(&summary);
    assert!(output.contains("Young Generation"));
    assert!(output.contains("Old Generation"));
    assert!(output.contains("Metaspace"));
    assert!(output.contains("Total"));
    assert!(output.contains("MB"));
}

#[test]
fn test_jmap_finalizer_info() {
    let output = JmapProcessor::generate_finalizer_info();
    assert!(output.contains("Finalizer Information"));
    assert!(output.contains("Pending finalizers: 0"));
    assert!(output.contains("Finalizer thread: active"));
}

// --- HPROF tests ---

#[test]
fn test_hprof_magic() {
    assert_eq!(HprofWriter::HPROF_MAGIC, "JAVA PROFILE 1.0.2");
}

#[test]
fn test_hprof_record_type_constants() {
    assert_eq!(HprofWriter::HPROF_UTF8, 0x01);
    assert_eq!(HprofWriter::HPROF_LOAD_CLASS, 0x02);
    assert_eq!(HprofWriter::HPROF_FRAME, 0x04);
    assert_eq!(HprofWriter::HPROF_TRACE, 0x05);
    assert_eq!(HprofWriter::HPROF_HEAP_DUMP, 0x0C);
    assert_eq!(HprofWriter::HPROF_HEAP_DUMP_SEGMENT, 0x1C);
    assert_eq!(HprofWriter::HPROF_HEAP_DUMP_END, 0x2C);
}

#[test]
fn test_hprof_write_header() {
    let header = HprofWriter::write_header();
    // Check magic
    let magic_end = HprofWriter::HPROF_MAGIC.len();
    assert_eq!(&header[..magic_end], HprofWriter::HPROF_MAGIC.as_bytes());
    // Null terminator
    assert_eq!(header[magic_end], 0);
    // Identifier size = 8
    let id_size = u32::from_be_bytes([
        header[magic_end + 1],
        header[magic_end + 2],
        header[magic_end + 3],
        header[magic_end + 4],
    ]);
    assert_eq!(id_size, 8);
    // Total size check
    assert_eq!(header.len(), HprofWriter::header_size());
}

#[test]
fn test_hprof_header_size() {
    let expected = HprofWriter::HPROF_MAGIC.len() + 1 + 4 + 4 + 4;
    assert_eq!(HprofWriter::header_size(), expected);
}

#[test]
fn test_hprof_write_string_record() {
    let record = HprofWriter::write_string_record(42, "hello");
    assert_eq!(record[0], HprofWriter::HPROF_UTF8);
    // Timestamp = 0
    assert_eq!(&record[1..5], &[0, 0, 0, 0]);
    // Body length = 8 (id) + 5 (string) = 13
    let body_len = u32::from_be_bytes([record[5], record[6], record[7], record[8]]);
    assert_eq!(body_len, 13);
    // ID = 42
    let id = u64::from_be_bytes([
        record[9], record[10], record[11], record[12], record[13], record[14], record[15],
        record[16],
    ]);
    assert_eq!(id, 42);
    // String data
    assert_eq!(&record[17..], b"hello");
}

#[test]
fn test_hprof_write_string_record_empty() {
    let record = HprofWriter::write_string_record(1, "");
    let body_len = u32::from_be_bytes([record[5], record[6], record[7], record[8]]);
    assert_eq!(body_len, 8); // just the id, no string bytes
}

// --- Utility tests ---

#[test]
fn test_format_bytes() {
    assert_eq!(format_bytes(500), "500 B");
    assert_eq!(format_bytes(2048), "2.0 KB");
    assert_eq!(format_bytes(1048576), "1.0 MB");
    assert_eq!(format_bytes(1073741824), "1.0 GB");
}

#[test]
fn test_percent() {
    assert!((percent(50, 100) - 50.0).abs() < 0.001);
    assert!((percent(0, 100) - 0.0).abs() < 0.001);
    assert!((percent(100, 0) - 0.0).abs() < 0.001); // div by zero guard
}

#[test]
fn test_sample_thread_snapshots() {
    let threads = sample_thread_snapshots();
    assert_eq!(threads.len(), 2);
    assert_eq!(threads[0].name, "main");
    assert_eq!(threads[1].name, "GC-Thread");
}

#[test]
fn test_sample_class_histogram() {
    let entries = sample_class_histogram();
    assert_eq!(entries.len(), 10);
    assert_eq!(entries[0].class_name, "[B");
}

#[test]
fn test_jcmd_default_trait() {
    let jcmd = JcmdProcessor::default();
    assert_eq!(jcmd.attach_listener.commands.read().len(), 16);
}

#[test]
fn test_hprof_load_class_record() {
    let data = HprofWriter::write_load_class(1, 100, 0, 200);
    assert_eq!(data[0], HprofWriter::HPROF_LOAD_CLASS);
    assert!(!data.is_empty());
}

#[test]
fn test_hprof_heap_dump_end() {
    let data = HprofWriter::write_heap_dump_end();
    assert_eq!(data[0], HprofWriter::HPROF_HEAP_DUMP_END);
    // Body length should be 0
    let body_len = u32::from_be_bytes([data[5], data[6], data[7], data[8]]);
    assert_eq!(body_len, 0);
}

#[test]
fn test_hprof_stack_trace_record() {
    let frames = vec![1u64, 2, 3];
    let data = HprofWriter::write_stack_trace(1, 1, &frames);
    assert_eq!(data[0], HprofWriter::HPROF_TRACE);
}

#[test]
fn test_hprof_stack_frame_record() {
    let data = HprofWriter::write_stack_frame(1, 10, 20, 30, 1, 42);
    assert_eq!(data[0], HprofWriter::HPROF_FRAME);
}

// Test VmDiagnosticState with a mock implementation
struct MockVmState;
impl VmDiagnosticState for MockVmState {
    fn thread_snapshots(&self) -> Vec<ThreadSnapshot> {
        vec![ThreadSnapshot {
            id: 1,
            name: "main".to_string(),
            daemon: false,
            priority: 5,
            state: ThreadState::Runnable,
            stack_frames: vec![],
            lock_info: None,
            blocked_by: None,
            waiting_on: None,
            wait_kind: None,
            dump: Default::default(),
        }]
    }
    fn heap_summary(&self) -> HeapSummary {
        HeapSummary {
            young_gen_used: 10 * 1024 * 1024,
            young_gen_capacity: 64 * 1024 * 1024,
            old_gen_used: 50 * 1024 * 1024,
            old_gen_capacity: 256 * 1024 * 1024,
            metaspace_used: 20 * 1024 * 1024,
            metaspace_capacity: 64 * 1024 * 1024,
            total_used: 80 * 1024 * 1024,
            total_capacity: 384 * 1024 * 1024,
        }
    }
    fn class_histogram(&self) -> Vec<ClassHistogramEntry> {
        vec![ClassHistogramEntry {
            class_name: "java.lang.String".to_string(),
            instance_count: 100,
            total_bytes: 4000,
        }]
    }
    fn trigger_gc(&self) -> bool {
        true
    }
    fn uptime_secs(&self) -> f64 {
        42.5
    }
    fn command_line(&self) -> String {
        "java -jar test.jar".to_string()
    }
    fn system_properties(&self) -> Vec<(String, String)> {
        vec![("java.version".to_string(), "25".to_string())]
    }
    fn vm_flags(&self) -> Vec<String> {
        vec!["-Xmx256m".to_string()]
    }
}

/// A state that serves the attach `load` operation (interpreter round i2
/// wave 58, lane L1), recording what it was asked.
struct LoadingVmState(parking_lot::Mutex<Vec<(String, bool, String)>>);
impl VmDiagnosticState for LoadingVmState {
    fn thread_snapshots(&self) -> Vec<ThreadSnapshot> {
        Vec::new()
    }
    fn heap_summary(&self) -> HeapSummary {
        MockVmState.heap_summary()
    }
    fn class_histogram(&self) -> Vec<ClassHistogramEntry> {
        Vec::new()
    }
    fn trigger_gc(&self) -> bool {
        false
    }
    fn uptime_secs(&self) -> f64 {
        0.0
    }
    fn command_line(&self) -> String {
        String::new()
    }
    fn system_properties(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    fn vm_flags(&self) -> Vec<String> {
        Vec::new()
    }
    fn attach_load_enabled(&self) -> bool {
        true
    }
    fn attach_load(&self, lib: &str, absolute: bool, options: &str) -> String {
        self.0
            .lock()
            .push((lib.to_string(), absolute, options.to_string()));
        "return code: 0".to_string()
    }
    fn attach_properties_enabled(&self) -> bool {
        true
    }
    fn attach_properties(&self, agent: bool) -> Result<String, String> {
        Ok(format!("k=agent {agent}\n"))
    }
}

/// A [`LoadingVmState`] run with `-XX:-EnableDynamicAgentLoading`
/// (interpreter round i3 wave 81, lane L1).
struct NoDynamicAgentsVmState(LoadingVmState);
impl VmDiagnosticState for NoDynamicAgentsVmState {
    fn thread_snapshots(&self) -> Vec<ThreadSnapshot> {
        Vec::new()
    }
    fn heap_summary(&self) -> HeapSummary {
        MockVmState.heap_summary()
    }
    fn class_histogram(&self) -> Vec<ClassHistogramEntry> {
        Vec::new()
    }
    fn trigger_gc(&self) -> bool {
        false
    }
    fn uptime_secs(&self) -> f64 {
        0.0
    }
    fn command_line(&self) -> String {
        String::new()
    }
    fn system_properties(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    fn vm_flags(&self) -> Vec<String> {
        Vec::new()
    }
    fn attach_load_enabled(&self) -> bool {
        true
    }
    fn attach_load(&self, lib: &str, absolute: bool, options: &str) -> String {
        self.0.attach_load(lib, absolute, options)
    }
    fn dynamic_agent_loading_disabled(&self) -> bool {
        true
    }
}

/// Interpreter round i3 wave 81, lane L1: under
/// `-XX:-EnableDynamicAgentLoading` the `load` operation answers a
/// non-zero status with HotSpot's text and loads nothing (MEASURED,
/// HotSpot 25.0.4: the client throws `AgentLoadException("Failed to load
/// agent library: Dynamic agent loading is not enabled. Use
/// -XX:+EnableDynamicAgentLoading to launch target VM.")`), while `jcmd
/// JVMTI.agent_load` still loads (HotSpot gates the operation, not the
/// diagnostic command).
#[test]
fn the_attach_load_operation_is_refused_without_dynamic_agent_loading() {
    #[cfg(windows)]
    let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
    let state = Arc::new(NoDynamicAgentsVmState(LoadingVmState(
        parking_lot::Mutex::new(Vec::new()),
    )));
    let shared: Arc<dyn VmDiagnosticState> = state.clone();
    let processor = JcmdProcessor::new_with_vm_state(shared);
    let fields: Vec<String> = ["1", "load", "instrument", "false", "/x/a.jar"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let (code, body) = execute_attach_operation(&fields, &processor.attach_listener.commands);
    assert_ne!(code, 0);
    assert_eq!(
        body,
        "Dynamic agent loading is not enabled. Use -XX:+EnableDynamicAgentLoading to launch target VM."
    );
    assert!(state.0 .0.lock().is_empty(), "the refused load ran");
    let jcmd = processor
        .attach_listener
        .dispatch_command("JVMTI.agent_load", &["/x/a.jar".to_string()])
        .expect("registered");
    assert_eq!(jcmd.output, "return code: 0\n");
    assert_eq!(state.0 .0.lock().len(), 1);
}

/// Interpreter round i2 wave 59, lane L1: a lazy listener binds nothing
/// until the trigger file `.attach_pid<pid>` appears (HotSpot's
/// `is_init_trigger`), then binds and serves; one stopped before any
/// trigger retires its polling thread.
#[cfg(unix)]
#[test]
fn a_lazy_attach_listener_binds_on_the_trigger_file() {
    use std::path::Path;
    let pid = std::process::id();
    let path = format!("/tmp/cratonvm_attach_lazy_test_{pid}");
    let trigger = format!("/tmp/.attach_pid{pid}");
    let _ = std::fs::remove_file(&trigger);

    let mut idle = AttachListener::new(&format!("{path}_idle"));
    idle.start_listening_lazily();
    assert!(idle.is_listening);
    idle.stop_listening();
    assert!(!Path::new(&format!("{path}_idle")).exists());

    let mut listener = AttachListener::new(&path);
    listener.start_listening_lazily();
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(!Path::new(&path).exists(), "bound before any trigger");
    std::fs::write(&trigger, b"").expect("trigger file");
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    while !Path::new(&path).exists() && Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let _ = std::fs::remove_file(&trigger);
    assert!(
        Path::new(&path).exists(),
        "the trigger did not bind the socket"
    );
    listener.stop_listening();
    assert!(!Path::new(&path).exists());
}

/// Interpreter round i2 wave 59, lane L1: `JVMTI.agent_load` is a live
/// command beside the `load` operation, reads its arguments as HotSpot's
/// `DCmdParser` does, and the `properties` / `agentProperties`
/// operations are live commands only for a state that serves them.
#[test]
fn jvmti_agent_load_and_the_property_operations_are_served_when_enabled() {
    #[cfg(windows)]
    let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
    let plain: Arc<dyn VmDiagnosticState> = Arc::new(MockVmState);
    let processor = JcmdProcessor::new_with_vm_state(Arc::clone(&plain));
    let names = processor.attach_listener.list_commands();
    for absent in [
        ATTACH_PROPERTIES_COMMAND,
        ATTACH_AGENT_PROPERTIES_COMMAND,
        "JVMTI.agent_load",
    ] {
        assert!(!names.contains(&absent.to_string()), "{absent}");
    }
    drop(processor);
    let loading = Arc::new(LoadingVmState(parking_lot::Mutex::new(Vec::new())));
    let state: Arc<dyn VmDiagnosticState> = loading.clone();
    let processor = JcmdProcessor::new_with_vm_state(state);
    let run = |name: &str, args: &[&str]| {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        processor
            .attach_listener
            .dispatch_command(name, &args)
            .expect("registered")
    };
    assert_eq!(
        run("JVMTI.agent_load", &["/x/a.jar"]).output,
        "return code: 0\n"
    );
    run("JVMTI.agent_load", &["/x/a.jar", "a=b"]);
    run("JVMTI.agent_load", &["/x/libagent.so", "opt"]);
    run("JVMTI.agent_load", &["/x/libagent.so"]);
    let missing = run("JVMTI.agent_load", &[]);
    assert!(!missing.success);
    assert_eq!(
        missing.error.as_deref(),
        Some("java.lang.IllegalArgumentException: The argument 'library path' is mandatory.\n")
    );
    assert_eq!(
        loading.0.lock().as_slice(),
        &[
            ("instrument".to_string(), false, "/x/a.jar".to_string()),
            ("instrument".to_string(), false, "/x/a.jar=a".to_string()),
            ("/x/libagent.so".to_string(), true, "opt".to_string()),
            ("/x/libagent.so".to_string(), true, String::new()),
        ]
    );
    assert_eq!(
        run(ATTACH_PROPERTIES_COMMAND, &[]).output,
        "k=agent false\n"
    );
    assert_eq!(
        run(ATTACH_AGENT_PROPERTIES_COMMAND, &[]).output,
        "k=agent true\n"
    );
    assert_eq!(run("VM.system_properties", &[]).output, "k=agent false\n");

    // `jcmd <pid> help`, in HotSpot's shape, without the internal
    // operation commands.
    let list = jcmd_help(&processor.attach_listener.commands.read(), &[]);
    assert!(
        list.starts_with("The following commands are available:\n"),
        "{list}"
    );
    assert!(
        list.contains("\nJVMTI.agent_load\n") && list.contains("\nhelp\n"),
        "{list}"
    );
    assert!(!list.contains("attach "), "{list}");
    assert!(list.ends_with(
        "\n\nFor more information about a specific command use 'help <command>'.\n"
    ));
    let names: Vec<&str> = list.lines().skip(1).take_while(|l| !l.is_empty()).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted);
    let unknown = jcmd_help(
        &processor.attach_listener.commands.read(),
        &["No.such".to_string()],
    );
    assert_eq!(unknown, "Help unavailable : 'No.such' : No such command\n");
    let one = jcmd_help(
        &processor.attach_listener.commands.read(),
        &["VM.uptime".to_string()],
    );
    assert!(one.starts_with("VM.uptime\n"), "{one}");
}

/// Interpreter round i2 wave 58, lane L1: the attach `load` operation is a
/// live command only for a state that serves it, and answers HotSpot's
/// body for completion status 0 with the operation's three fields.
#[test]
fn the_attach_load_command_is_registered_only_when_served() {
    #[cfg(windows)]
    let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
    let plain: Arc<dyn VmDiagnosticState> = Arc::new(MockVmState);
    let processor = JcmdProcessor::new_with_vm_state(Arc::clone(&plain));
    assert!(!processor
        .attach_listener
        .list_commands()
        .contains(&ATTACH_LOAD_COMMAND.to_string()));
    drop(processor);
    let loading = Arc::new(LoadingVmState(parking_lot::Mutex::new(Vec::new())));
    let state: Arc<dyn VmDiagnosticState> = loading.clone();
    let processor = JcmdProcessor::new_with_vm_state(state);
    let args = [
        "/x/libagent.so".to_string(),
        "true".to_string(),
        "a=b c".to_string(),
    ];
    let result = processor
        .attach_listener
        .dispatch_command(ATTACH_LOAD_COMMAND, &args)
        .expect("registered");
    assert!(result.success);
    assert_eq!(result.output, "return code: 0\n");
    assert_eq!(
        loading.0.lock().as_slice(),
        &[("/x/libagent.so".to_string(), true, "a=b c".to_string())]
    );
}

/// A `JcmdProcessor` must NOT keep the VM alive.
///
/// Its commands used to capture `Arc<dyn VmDiagnosticState>` clones, and
/// the processor lives in `SharedVm::debug.jcmd_processor` -- so the VM
/// held nine strong references to itself and could never be dropped. Pin
/// the weak-handle contract at the level it broke.
#[test]
fn jcmd_processor_does_not_keep_vm_state_alive() {
    #[cfg(windows)]
    let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
    let state: Arc<dyn VmDiagnosticState> = Arc::new(MockVmState);
    let weak = Arc::downgrade(&state);
    let processor = JcmdProcessor::new_with_vm_state(Arc::clone(&state));
    assert_eq!(
        Arc::strong_count(&state),
        1,
        "constructing the processor must not retain a strong handle"
    );
    drop(state);
    assert!(
        weak.upgrade().is_none(),
        "the processor kept the VM state alive"
    );
    // And it degrades honestly rather than reporting stale data.
    let result = processor.process_command("GC.run");
    assert!(!result.success);
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|e| e.contains("shutting down")),
        "expected a shutting-down error, got {:?}",
        result.error
    );
}

/// Interpreter round i3 wave 80, lane L6: the latch word's pending bit is
/// set and taken alone; the Windows installed bit survives a take.
#[cfg(any(unix, windows))]
#[test]
fn i80_signal_latch_takes_only_the_pending_bit() {
    let latch = std::sync::atomic::AtomicU8::new(0);
    assert!(!take_signal_pending(&latch));
    set_signal_pending(&latch);
    set_signal_pending(&latch);
    assert!(
        take_signal_pending(&latch),
        "two signals before a take are one dump"
    );
    assert!(!take_signal_pending(&latch));
    latch.store(4 | SIGNAL_PENDING, std::sync::atomic::Ordering::Relaxed);
    assert!(take_signal_pending(&latch));
    assert_eq!(
        latch.load(std::sync::atomic::Ordering::Relaxed),
        4,
        "other bits stay"
    );
}

/// Interpreter round i3 wave 80, lane L6: the `-XX:+DisableAttachMechanism`
/// dispatcher binds nothing, serves the live `Thread.print`, keeps no
/// strong handle on the VM, and its thread is reaped promptly on drop.
#[cfg(any(unix, windows))]
#[test]
fn i80_signal_dispatcher_binds_nothing_and_is_reaped() {
    let state: Arc<dyn VmDiagnosticState> = Arc::new(MockVmState);
    let processor = JcmdProcessor::new_signal_dispatcher(Arc::clone(&state));
    assert_eq!(Arc::strong_count(&state), 1, "the dispatcher holds a Weak");
    assert!(processor.attach_listener.socket_path.is_empty());
    assert!(
        !processor.attach_listener.is_listening,
        "no attach endpoint"
    );
    assert!(
        processor.attach_listener.accept_thread.is_some(),
        "a dispatcher thread"
    );
    let dump = processor.process_command("Thread.print");
    assert!(dump.success, "{:?}", dump.error);
    let started = std::time::Instant::now();
    drop(processor);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "the dispatcher must be unparked and joined, not waited out"
    );
}

#[test]
fn test_jcmd_with_live_vm_state() {
    #[cfg(windows)]
    let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
    // The processor holds only a `Weak` (see `new_with_vm_state`), so the
    // test must keep its own strong handle alive across the assertions --
    // exactly as the real caller does, where the `SharedVm` being reported
    // on is owned by the live `Vm`. Move the only `Arc` in and every
    // command below correctly answers "VM is shutting down".
    let state = Arc::new(MockVmState);
    let processor = JcmdProcessor::new_with_vm_state(state.clone());

    let result = processor.process_command("Thread.print");
    assert!(result.success);
    assert!(result.output.contains("main"));

    let result = processor.process_command("GC.run");
    assert!(result.success);
    // gci i2/oo: a completed `GC.run` prints nothing, as HotSpot's.
    assert_eq!(result.output, "");

    let result = processor.process_command("VM.uptime");
    assert!(result.success);
    assert!(result.output.contains("42.5"));
}

// -----------------------------------------------------------------------
// HPROF Heap Dump tests (Session 42)
// -----------------------------------------------------------------------

#[test]
fn test_hprof_basic_type_sizes() {
    assert_eq!(HprofBasicType::Boolean.size(), 1);
    assert_eq!(HprofBasicType::Byte.size(), 1);
    assert_eq!(HprofBasicType::Char.size(), 2);
    assert_eq!(HprofBasicType::Short.size(), 2);
    assert_eq!(HprofBasicType::Int.size(), 4);
    assert_eq!(HprofBasicType::Float.size(), 4);
    assert_eq!(HprofBasicType::Long.size(), 8);
    assert_eq!(HprofBasicType::Double.size(), 8);
    assert_eq!(HprofBasicType::Object.size(), 8);
}

#[test]
fn test_hprof_basic_type_from_descriptor() {
    assert_eq!(HprofBasicType::from_descriptor("I"), HprofBasicType::Int);
    assert_eq!(HprofBasicType::from_descriptor("J"), HprofBasicType::Long);
    assert_eq!(HprofBasicType::from_descriptor("F"), HprofBasicType::Float);
    assert_eq!(HprofBasicType::from_descriptor("D"), HprofBasicType::Double);
    assert_eq!(
        HprofBasicType::from_descriptor("Z"),
        HprofBasicType::Boolean
    );
    assert_eq!(HprofBasicType::from_descriptor("B"), HprofBasicType::Byte);
    assert_eq!(HprofBasicType::from_descriptor("C"), HprofBasicType::Char);
    assert_eq!(HprofBasicType::from_descriptor("S"), HprofBasicType::Short);
    assert_eq!(
        HprofBasicType::from_descriptor("Ljava/lang/String;"),
        HprofBasicType::Object
    );
    assert_eq!(
        HprofBasicType::from_descriptor("[I"),
        HprofBasicType::Object
    );
}

#[test]
fn test_hprof_header_has_correct_magic() {
    let header = HprofWriter::write_header();
    let magic_len = HprofWriter::HPROF_MAGIC.len();
    assert_eq!(&header[..magic_len], HprofWriter::HPROF_MAGIC.as_bytes());
    assert_eq!(header[magic_len], 0); // null terminator
                                      // Identifier size = 8
    let id_size = u32::from_be_bytes([
        header[magic_len + 1],
        header[magic_len + 2],
        header[magic_len + 3],
        header[magic_len + 4],
    ]);
    assert_eq!(id_size, 8);
}

#[test]
fn test_hprof_gc_root_thread_obj() {
    let mut buf = Vec::new();
    HprofWriter::write_gc_root_thread_obj(&mut buf, 0xDEAD, 1, 1);
    assert_eq!(buf[0], HprofWriter::GC_ROOT_THREAD_OBJ);
    // thread_obj_id at bytes 1..9
    let obj_id = u64::from_be_bytes(buf[1..9].try_into().unwrap());
    assert_eq!(obj_id, 0xDEAD);
    // thread_serial at bytes 9..13
    let tserial = u32::from_be_bytes(buf[9..13].try_into().unwrap());
    assert_eq!(tserial, 1);
}

#[test]
fn test_hprof_gc_root_jni_global() {
    let mut buf = Vec::new();
    HprofWriter::write_gc_root_jni_global(&mut buf, 0xCAFE, 0xBEEF);
    assert_eq!(buf[0], HprofWriter::GC_ROOT_JNI_GLOBAL);
    let obj_id = u64::from_be_bytes(buf[1..9].try_into().unwrap());
    assert_eq!(obj_id, 0xCAFE);
    let ref_id = u64::from_be_bytes(buf[9..17].try_into().unwrap());
    assert_eq!(ref_id, 0xBEEF);
}

#[test]
fn test_hprof_gc_class_dump_minimal() {
    let ci = HprofClassInfo {
        class_id: 1,
        name: "TestClass".to_string(),
        super_class_id: 0,
        instance_fields: vec![],
        static_fields: vec![],
        source_file: None,
        instance_size: 32,
    };
    let mut string_ids = std::collections::HashMap::new();
    string_ids.insert("TestClass".to_string(), 1u64);

    let mut buf = Vec::new();
    HprofWriter::write_gc_class_dump(&mut buf, &ci, &string_ids);

    assert_eq!(buf[0], HprofWriter::GC_CLASS_DUMP);
    // class_obj_id at bytes 1..9
    let class_obj = u64::from_be_bytes(buf[1..9].try_into().unwrap());
    assert_eq!(class_obj, 0x1000_0000_0000_0001);
    // instance_size at bytes 73..77 (1 + 8 + 4 + 8 + 8 + 8 + 8 + 8 + 8 = 61 offset, then 4 bytes)
    let inst_size = u32::from_be_bytes(buf[61..65].try_into().unwrap());
    assert_eq!(inst_size, 32);
    // constant pool count = 0
    let cp_count = u16::from_be_bytes(buf[65..67].try_into().unwrap());
    assert_eq!(cp_count, 0);
    // static field count = 0
    let sf_count = u16::from_be_bytes(buf[67..69].try_into().unwrap());
    assert_eq!(sf_count, 0);
    // instance field count = 0
    let if_count = u16::from_be_bytes(buf[69..71].try_into().unwrap());
    assert_eq!(if_count, 0);
}

#[test]
fn test_hprof_gc_class_dump_with_fields() {
    let ci = HprofClassInfo {
        class_id: 2,
        name: "Point".to_string(),
        super_class_id: 1,
        instance_fields: vec![
            ("x".to_string(), "I".to_string()),
            ("y".to_string(), "I".to_string()),
        ],
        static_fields: vec![("ORIGIN".to_string(), "LPoint;".to_string())],
        source_file: Some("Point.java".to_string()),
        instance_size: 64,
    };
    let mut string_ids = std::collections::HashMap::new();
    string_ids.insert("Point".to_string(), 1u64);
    string_ids.insert("x".to_string(), 2u64);
    string_ids.insert("y".to_string(), 3u64);
    string_ids.insert("ORIGIN".to_string(), 4u64);

    let mut buf = Vec::new();
    HprofWriter::write_gc_class_dump(&mut buf, &ci, &string_ids);

    assert_eq!(buf[0], HprofWriter::GC_CLASS_DUMP);
    // Should have super_class_id encoded
    let super_obj = u64::from_be_bytes(buf[13..21].try_into().unwrap());
    assert_eq!(super_obj, 0x1000_0000_0000_0001); // super class id = 1

    // static field count = 1 (at offset 67)
    let sf_count = u16::from_be_bytes(buf[67..69].try_into().unwrap());
    assert_eq!(sf_count, 1);
}

#[test]
fn test_hprof_gc_prim_array_dump() {
    use cratonvm_gc::heap::{ArrayElementType, ObjectHeader, ObjectKind, HEADER_SIZE};
    // Simulate a small int[3] array in memory
    let array_length: u32 = 3;
    let elem_size = 4usize; // int = 4 bytes
    let data_size = array_length as usize * elem_size;
    let total_size = HEADER_SIZE + ((data_size + 7) & !7);
    let mut mem = vec![0u8; total_size];

    // Write header
    let header = ObjectHeader::new(
        cratonvm_types::ClassId::new(99),
        ObjectKind::Array,
        ArrayElementType::Int,
        array_length,
        array_length,
    );
    unsafe {
        std::ptr::write(mem.as_mut_ptr() as *mut ObjectHeader, header);
    }

    // Write array elements: [10, 20, 30] as native-endian i32
    for (i, val) in [10i32, 20, 30].iter().enumerate() {
        let offset = HEADER_SIZE + i * elem_size;
        unsafe {
            std::ptr::write_unaligned(mem.as_mut_ptr().add(offset) as *mut i32, *val);
        }
    }

    let obj = HprofObjectInfo {
        object_id: mem.as_ptr() as u64,
        class_id: 99,
        is_array: true,
        element_type: ArrayElementType::Int as u8,
        array_length: 3,
        total_size,
        data_ptr: mem.as_ptr(),
    };

    let mut buf = Vec::new();
    HprofWriter::write_gc_prim_array_dump(&mut buf, &obj);

    assert_eq!(buf[0], HprofWriter::GC_PRIM_ARRAY_DUMP);
    // array length at bytes 13..17
    let len = u32::from_be_bytes(buf[13..17].try_into().unwrap());
    assert_eq!(len, 3);
    // element type at byte 17
    assert_eq!(buf[17], HprofBasicType::Int as u8);
    // First element (big-endian int32) at bytes 18..22
    let val0 = i32::from_be_bytes(buf[18..22].try_into().unwrap());
    assert_eq!(val0, 10);
    let val1 = i32::from_be_bytes(buf[22..26].try_into().unwrap());
    assert_eq!(val1, 20);
    let val2 = i32::from_be_bytes(buf[26..30].try_into().unwrap());
    assert_eq!(val2, 30);
}

#[test]
fn test_hprof_gc_obj_array_dump() {
    use cratonvm_gc::heap::{
        ArrayElementType, ObjectHeader, ObjectKind, HEADER_SIZE, REF_ELEMENT_SIZE,
    };
    // Simulate an Object[2] array
    let array_length: u32 = 2;
    let data_size = array_length as usize * ref_element_size();
    let total_size = HEADER_SIZE + ((data_size + 7) & !7);
    let mut mem = vec![0u8; total_size];

    let header = ObjectHeader::new(
        cratonvm_types::ClassId::new(50),
        ObjectKind::Array,
        ArrayElementType::Reference,
        array_length,
        array_length,
    );
    unsafe {
        std::ptr::write(mem.as_mut_ptr() as *mut ObjectHeader, header);
    }

    // Write element references: [0xCAFE, 0xBEEF]
    for (i, val) in [0xCAFEu64, 0xBEEF].iter().enumerate() {
        let offset = HEADER_SIZE + i * ref_element_size();
        unsafe {
            std::ptr::write_unaligned(mem.as_mut_ptr().add(offset) as *mut u64, *val);
        }
    }

    let obj = HprofObjectInfo {
        object_id: mem.as_ptr() as u64,
        class_id: 50,
        is_array: true,
        element_type: ArrayElementType::Reference as u8,
        array_length: 2,
        total_size,
        data_ptr: mem.as_ptr(),
    };

    let mut buf = Vec::new();
    HprofWriter::write_gc_obj_array_dump(&mut buf, &obj);

    assert_eq!(buf[0], HprofWriter::GC_OBJ_ARRAY_DUMP);
    let len = u32::from_be_bytes(buf[13..17].try_into().unwrap());
    assert_eq!(len, 2);
    // First element ref at bytes 25..33
    let ref0 = u64::from_be_bytes(buf[25..33].try_into().unwrap());
    assert_eq!(ref0, 0xCAFE);
    let ref1 = u64::from_be_bytes(buf[33..41].try_into().unwrap());
    assert_eq!(ref1, 0xBEEF);
}

#[test]
fn test_hprof_full_dump_empty_heap() {
    let classes: Vec<HprofClassInfo> = vec![];
    let objects: Vec<HprofObjectInfo> = vec![];
    let threads = vec![ThreadSnapshot {
        id: 1,
        name: "main".to_string(),
        daemon: false,
        priority: 5,
        state: ThreadState::Runnable,
        stack_frames: vec![],
        lock_info: None,
        blocked_by: None,
        waiting_on: None,
        wait_kind: None,
        dump: Default::default(),
    }];

    let dump = HprofWriter::write_full_heap_dump(&classes, &objects, &threads);

    // Verify header magic
    let magic_len = HprofWriter::HPROF_MAGIC.len();
    assert_eq!(&dump[..magic_len], HprofWriter::HPROF_MAGIC.as_bytes());

    // Should contain HEAP_DUMP_END marker somewhere
    assert!(dump
        .windows(1)
        .any(|w| w[0] == HprofWriter::HPROF_HEAP_DUMP_END));

    // Should be at least header + stack trace + segment + end
    assert!(dump.len() > HprofWriter::header_size() + 20);
}

#[test]
fn test_hprof_full_dump_with_class_and_objects() {
    use cratonvm_gc::heap::{
        ArrayElementType, ObjectHeader, ObjectKind, HEADER_SIZE, SLOT_SIZE,
    };

    let classes = vec![HprofClassInfo {
        class_id: 1,
        name: "TestObj".to_string(),
        super_class_id: 0,
        instance_fields: vec![("value".to_string(), "I".to_string())],
        static_fields: vec![],
        source_file: Some("TestObj.java".to_string()),
        instance_size: (HEADER_SIZE + SLOT_SIZE) as u32,
    }];

    // Create a fake heap object
    let total_size = HEADER_SIZE + SLOT_SIZE;
    let mut mem = vec![0u8; total_size];
    let header = ObjectHeader::new(
        cratonvm_types::ClassId::new(1),
        ObjectKind::Object,
        ArrayElementType::Boolean,
        0,
        1,
    );
    unsafe {
        std::ptr::write(mem.as_mut_ptr() as *mut ObjectHeader, header);
        // Write an int value (42) at the field slot
        std::ptr::write_unaligned(mem.as_mut_ptr().add(HEADER_SIZE) as *mut i32, 42);
    }

    let objects = vec![HprofObjectInfo {
        object_id: mem.as_ptr() as u64,
        class_id: 1,
        is_array: false,
        element_type: 0,
        array_length: 0,
        total_size,
        data_ptr: mem.as_ptr(),
    }];

    let threads = vec![ThreadSnapshot {
        id: 1,
        name: "main".to_string(),
        daemon: false,
        priority: 5,
        state: ThreadState::Runnable,
        stack_frames: vec![],
        lock_info: None,
        blocked_by: None,
        waiting_on: None,
        wait_kind: None,
        dump: Default::default(),
    }];

    let dump = HprofWriter::write_full_heap_dump(&classes, &objects, &threads);

    // Verify magic
    let magic_len = HprofWriter::HPROF_MAGIC.len();
    assert_eq!(&dump[..magic_len], HprofWriter::HPROF_MAGIC.as_bytes());

    // Should have UTF-8 records (for "TestObj", "value", etc.)
    assert!(dump.contains(&HprofWriter::HPROF_UTF8));

    // Should have LOAD_CLASS record
    assert!(dump.contains(&HprofWriter::HPROF_LOAD_CLASS));

    // Should have a HEAP_DUMP_SEGMENT
    assert!(dump.contains(&HprofWriter::HPROF_HEAP_DUMP_SEGMENT));

    // Should have HEAP_DUMP_END
    assert!(dump.contains(&HprofWriter::HPROF_HEAP_DUMP_END));

    // Size should be reasonable (not trivially small)
    assert!(dump.len() > 200);
}

#[test]
fn test_hprof_full_dump_with_threads_and_frames() {
    let threads = vec![
        ThreadSnapshot {
            id: 1,
            name: "main".to_string(),
            daemon: false,
            priority: 5,
            state: ThreadState::Runnable,
            stack_frames: vec![
                FrameInfo {
                    class_name: "com.example.Main".to_string(),
                    method_name: "run".to_string(),
                    file_name: Some("Main.java".to_string()),
                    line_number: 42,
                    native_method: false,
                },
                FrameInfo {
                    class_name: "com.example.Main".to_string(),
                    method_name: "main".to_string(),
                    file_name: Some("Main.java".to_string()),
                    line_number: 10,
                    native_method: false,
                },
            ],
            lock_info: None,
            blocked_by: None,
            waiting_on: None,
            wait_kind: None,
            dump: Default::default(),
        },
        ThreadSnapshot {
            id: 2,
            name: "worker-1".to_string(),
            daemon: true,
            priority: 5,
            state: ThreadState::Waiting,
            stack_frames: vec![],
            lock_info: None,
            blocked_by: None,
            waiting_on: None,
            wait_kind: None,
            dump: Default::default(),
        },
    ];

    let dump = HprofWriter::write_full_heap_dump(&[], &[], &threads);

    // Should have STACK_FRAME records (for main's 2 frames)
    assert!(dump.contains(&HprofWriter::HPROF_FRAME));

    // Should have STACK_TRACE records
    assert!(dump.contains(&HprofWriter::HPROF_TRACE));

    // Should have UTF-8 records for thread/frame names
    assert!(dump.contains(&HprofWriter::HPROF_UTF8));
}

#[test]
fn test_hprof_full_dump_class_hierarchy() {
    use cratonvm_gc::heap::{
        ArrayElementType, ObjectHeader, ObjectKind, HEADER_SIZE, SLOT_SIZE,
    };

    let classes = vec![
        HprofClassInfo {
            class_id: 1,
            name: "Base".to_string(),
            super_class_id: 0,
            instance_fields: vec![("id".to_string(), "I".to_string())],
            static_fields: vec![],
            source_file: None,
            instance_size: (HEADER_SIZE + SLOT_SIZE) as u32,
        },
        HprofClassInfo {
            class_id: 2,
            name: "Derived".to_string(),
            super_class_id: 1,
            instance_fields: vec![("name".to_string(), "Ljava/lang/String;".to_string())],
            static_fields: vec![],
            source_file: None,
            instance_size: (HEADER_SIZE + 2 * SLOT_SIZE) as u32,
        },
    ];

    // Create a Derived object with 2 fields (inherited id + own name)
    let total_size = HEADER_SIZE + 2 * SLOT_SIZE;
    let mut mem = vec![0u8; total_size];
    let header = ObjectHeader::new(
        cratonvm_types::ClassId::new(2),
        ObjectKind::Object,
        ArrayElementType::Boolean,
        0,
        2,
    );
    unsafe {
        std::ptr::write(mem.as_mut_ptr() as *mut ObjectHeader, header);
    }

    let objects = vec![HprofObjectInfo {
        object_id: mem.as_ptr() as u64,
        class_id: 2,
        is_array: false,
        element_type: 0,
        array_length: 0,
        total_size,
        data_ptr: mem.as_ptr(),
    }];

    let dump = HprofWriter::write_full_heap_dump(&classes, &objects, &[]);

    // Should have two LOAD_CLASS records
    let _load_class_count = dump
        .iter()
        .enumerate()
        .filter(|(i, &b)| {
            b == HprofWriter::HPROF_LOAD_CLASS
                && *i > 0
                && dump
                    .get(i.wrapping_sub(4)..=i.wrapping_sub(1))
                    .map(|s| s == &[0, 0, 0, 0]) // timestamp = 0
                    .unwrap_or(false)
        })
        .count();
    // At minimum we should have LOAD_CLASS tags in the output
    assert!(dump.contains(&HprofWriter::HPROF_LOAD_CLASS));

    // Two GC_CLASS_DUMP sub-records should be present in segment body
    // CLASS_DUMP tag is 0x20
    let class_dump_count = dump
        .iter()
        .filter(|&&b| b == HprofWriter::GC_CLASS_DUMP)
        .count();
    // Should be >= 2 (Base + Derived)
    assert!(
        class_dump_count >= 2,
        "Expected at least 2 class dumps, got {}",
        class_dump_count
    );
}

#[test]
fn test_hprof_segment_wrapping() {
    let seg = HprofWriter::wrap_segment(&[1, 2, 3, 4, 5]);
    assert_eq!(seg[0], HprofWriter::HPROF_HEAP_DUMP_SEGMENT);
    // timestamp at 1..5
    assert_eq!(&seg[1..5], &[0, 0, 0, 0]);
    // body length at 5..9
    let body_len = u32::from_be_bytes(seg[5..9].try_into().unwrap());
    assert_eq!(body_len, 5);
    // body content
    assert_eq!(&seg[9..14], &[1, 2, 3, 4, 5]);
}

#[test]
fn test_hprof_write_to_file() {
    // Test that write_full_heap_dump produces valid data that can be written
    let dump = HprofWriter::write_full_heap_dump(&[], &[], &[]);
    // Even with no data, should produce a valid HPROF file structure
    assert!(dump.len() >= HprofWriter::header_size());
    // First bytes are the magic
    assert!(dump.starts_with(HprofWriter::HPROF_MAGIC.as_bytes()));

    // Write to temp file and verify size
    let tmp_path = std::env::temp_dir().join("test_heap_dump.hprof");
    std::fs::write(&tmp_path, &dump).unwrap();
    let written = std::fs::read(&tmp_path).unwrap();
    assert_eq!(written.len(), dump.len());
    assert_eq!(written, dump);
    std::fs::remove_file(&tmp_path).ok();
}

#[test]
fn test_hprof_instance_dump_reads_field_values() {
    use cratonvm_gc::heap::{
        ArrayElementType, ObjectHeader, ObjectKind, HEADER_SIZE, SLOT_SIZE,
    };

    let classes_map: std::collections::HashMap<u32, HprofClassInfo> = [(
        1u32,
        HprofClassInfo {
            class_id: 1,
            name: "IntHolder".to_string(),
            super_class_id: 0,
            instance_fields: vec![("val".to_string(), "I".to_string())],
            static_fields: vec![],
            source_file: None,
            instance_size: (HEADER_SIZE + SLOT_SIZE) as u32,
        },
    )]
    .into_iter()
    .collect();

    let total_size = HEADER_SIZE + SLOT_SIZE;
    let mut mem = vec![0u8; total_size];
    let header = ObjectHeader::new(
        cratonvm_types::ClassId::new(1),
        ObjectKind::Object,
        ArrayElementType::Boolean,
        0,
        1,
    );
    unsafe {
        std::ptr::write(mem.as_mut_ptr() as *mut ObjectHeader, header);
        // Write field value: 0x12345678
        std::ptr::write_unaligned(mem.as_mut_ptr().add(HEADER_SIZE) as *mut i32, 0x12345678);
    }

    let ci = classes_map.get(&1).unwrap();
    let obj = HprofObjectInfo {
        object_id: 0xABCD,
        class_id: 1,
        is_array: false,
        element_type: 0,
        array_length: 0,
        total_size,
        data_ptr: mem.as_ptr(),
    };

    let mut buf = Vec::new();
    HprofWriter::write_gc_instance_dump(&mut buf, &obj, ci, &classes_map);

    assert_eq!(buf[0], HprofWriter::GC_INSTANCE_DUMP);
    // object_id at 1..9
    let oid = u64::from_be_bytes(buf[1..9].try_into().unwrap());
    assert_eq!(oid, 0xABCD);
    // data_size at 21..25 (after tag + obj_id(8) + stack_serial(4) + class_obj(8))
    let data_size = u32::from_be_bytes(buf[21..25].try_into().unwrap());
    assert_eq!(data_size, 4); // one int field = 4 bytes
                              // Field value at 25..29 (big-endian)
    let fval = i32::from_be_bytes(buf[25..29].try_into().unwrap());
    assert_eq!(fval, 0x12345678);
}

#[test]
fn test_hprof_instance_dump_reads_compact_ref_field_value() {
    use cratonvm_gc::heap::{ArrayElementType, ObjectHeader, ObjectKind};
    use cratonvm_gc::register_class_layout;
    use cratonvm_types::{CompactLayout, FieldStorageKind, GC_FLAG_COMPACT};
    use std::sync::Arc;

    const CLASS_ID: u32 = 61_001;

    register_class_layout(
        cratonvm_types::FIRST_LAYOUT_DOMAIN,
        CLASS_ID,
        Arc::new(CompactLayout {
            field_disps: vec![8],
            is_ref: vec![true],
            field_kinds: vec![FieldStorageKind::Reference],
            ref_disps: vec![8],
            total_size: 16,
        }),
    );

    let classes_map: std::collections::HashMap<u32, HprofClassInfo> = [(
        CLASS_ID,
        HprofClassInfo {
            class_id: CLASS_ID,
            name: "RefHolder".to_string(),
            super_class_id: 0,
            instance_fields: vec![("ref".to_string(), "Ljava/lang/Object;".to_string())],
            static_fields: vec![],
            source_file: None,
            instance_size: cratonvm_types::MIN_OBJECT_SIZE as u32,
        },
    )]
    .into_iter()
    .collect();

    // A compact instance: the 8-byte header word, then its one field.
    let total_size = cratonvm_types::MIN_OBJECT_SIZE;
    let mut mem = vec![0u8; total_size];
    let expected_ref = 0x1_2345_6789_abcd_u64;
    let mut header = ObjectHeader::new(
        cratonvm_types::ClassId::new(CLASS_ID),
        ObjectKind::Object,
        ArrayElementType::Boolean,
        0,
        1,
    );
    header.set_compact_shape(1, 8);
    unsafe {
        std::ptr::write(mem.as_mut_ptr() as *mut ObjectHeader, header);
        std::ptr::write_unaligned(
            mem.as_mut_ptr().add(cratonvm_types::COMPACT_HEADER_SIZE) as *mut u64,
            expected_ref,
        );
    }

    let ci = classes_map.get(&CLASS_ID).unwrap();
    let obj = HprofObjectInfo {
        object_id: 0xABCD,
        class_id: CLASS_ID,
        is_array: false,
        element_type: 0,
        array_length: 0,
        total_size,
        data_ptr: mem.as_ptr(),
    };

    let mut buf = Vec::new();
    HprofWriter::write_gc_instance_dump(&mut buf, &obj, ci, &classes_map);

    let fval = u64::from_be_bytes(buf[25..33].try_into().unwrap());
    assert_eq!(fval, expected_ref);
}

// --- HSDB protocol tests ---

struct DummyVmState;
impl VmDiagnosticState for DummyVmState {
    fn thread_snapshots(&self) -> Vec<ThreadSnapshot> {
        vec![ThreadSnapshot {
            id: 42,
            name: "main".to_string(),
            daemon: false,
            priority: 5,
            state: ThreadState::Runnable,
            stack_frames: vec![],
            lock_info: None,
            blocked_by: None,
            waiting_on: None,
            wait_kind: None,
            dump: Default::default(),
        }]
    }
    fn heap_summary(&self) -> HeapSummary {
        HeapSummary {
            young_gen_used: 100,
            young_gen_capacity: 200,
            old_gen_used: 300,
            old_gen_capacity: 400,
            metaspace_used: 500,
            metaspace_capacity: 600,
            total_used: 1024,
            total_capacity: 2048,
        }
    }
    fn class_histogram(&self) -> Vec<ClassHistogramEntry> {
        Vec::new()
    }
    fn trigger_gc(&self) -> bool {
        false
    }
    fn uptime_secs(&self) -> f64 {
        1.5
    }
    fn command_line(&self) -> String {
        "java -jar test.jar".to_string()
    }
    fn system_properties(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    fn vm_flags(&self) -> Vec<String> {
        Vec::new()
    }
}

#[test]
fn hsdb_magic_value() {
    assert_eq!(HSDB_MAGIC, 0x4853_4442);
    assert_eq!(&HSDB_MAGIC.to_be_bytes(), b"HSDB");
}

#[test]
fn hsdb_command_roundtrip() {
    assert_eq!(HsdbCommand::from_u8(0x01), Some(HsdbCommand::Version));
    assert_eq!(HsdbCommand::from_u8(0x02), Some(HsdbCommand::ProcessInfo));
    assert_eq!(HsdbCommand::from_u8(0x03), Some(HsdbCommand::HeapSummary));
    assert_eq!(HsdbCommand::from_u8(0x04), Some(HsdbCommand::ThreadList));
    assert_eq!(HsdbCommand::from_u8(0xFF), None);
}

#[test]
fn hsdb_decode_request_header_rejects_short() {
    assert!(hsdb_decode_request_header(&[0x01, 0x00]).is_err());
}

#[test]
fn hsdb_decode_request_header_parses_len() {
    let bytes = [0x01u8, 0x00, 0x00, 0x01, 0x00];
    let (cmd, len) = hsdb_decode_request_header(&bytes).unwrap();
    assert_eq!(cmd, HsdbCommand::Version);
    assert_eq!(len, 256);
}

#[test]
fn hsdb_decode_request_header_rejects_bad_cmd() {
    assert!(hsdb_decode_request_header(&[0xFE, 0, 0, 0, 0]).is_err());
}

#[test]
fn hsdb_encode_response_prefix() {
    let resp = hsdb_encode_response(HsdbStatus::Ok, b"hi");
    assert_eq!(resp[0], 0); // OK status
    assert_eq!(u32::from_be_bytes([resp[1], resp[2], resp[3], resp[4]]), 2);
    assert_eq!(&resp[5..], b"hi");
}

#[test]
fn hsdb_version_response_carries_version_string() {
    let (status, payload) = hsdb_handle_request(HsdbCommand::Version, &[], None);
    assert_eq!(status, HsdbStatus::Ok);
    // First 2 bytes = string length, then UTF-8
    assert!(payload.len() >= 2);
    let len = u16::from_be_bytes([payload[0], payload[1]]) as usize;
    assert_eq!(len, HSDB_VERSION_STRING.len());
    let s = std::str::from_utf8(&payload[2..2 + len]).unwrap();
    assert_eq!(s, HSDB_VERSION_STRING);
}

#[test]
fn hsdb_process_info_includes_pid_and_cmdline() {
    let state = DummyVmState;
    let (status, payload) = hsdb_handle_request(HsdbCommand::ProcessInfo, &[], Some(&state));
    assert_eq!(status, HsdbStatus::Ok);
    // pid u64 + len u16 + cmdline bytes
    assert!(payload.len() >= 10);
    let pid = u64::from_be_bytes(payload[0..8].try_into().unwrap());
    assert_eq!(pid, std::process::id() as u64);
    let len = u16::from_be_bytes([payload[8], payload[9]]) as usize;
    let cmdline = std::str::from_utf8(&payload[10..10 + len]).unwrap();
    assert_eq!(cmdline, "java -jar test.jar");
}

#[test]
fn hsdb_heap_summary_returns_eight_u64() {
    let state = DummyVmState;
    let (status, payload) = hsdb_handle_request(HsdbCommand::HeapSummary, &[], Some(&state));
    assert_eq!(status, HsdbStatus::Ok);
    assert_eq!(payload.len(), 64);
    let young_used = u64::from_be_bytes(payload[0..8].try_into().unwrap());
    let young_cap = u64::from_be_bytes(payload[8..16].try_into().unwrap());
    let total_used = u64::from_be_bytes(payload[48..56].try_into().unwrap());
    let total_cap = u64::from_be_bytes(payload[56..64].try_into().unwrap());
    assert_eq!(young_used, 100);
    assert_eq!(young_cap, 200);
    assert_eq!(total_used, 1024);
    assert_eq!(total_cap, 2048);
}

#[test]
fn hsdb_heap_summary_without_state_returns_error() {
    let (status, _) = hsdb_handle_request(HsdbCommand::HeapSummary, &[], None);
    assert_eq!(status, HsdbStatus::Error);
}

#[test]
fn hsdb_thread_list_contains_threads() {
    let state = DummyVmState;
    let (status, payload) = hsdb_handle_request(HsdbCommand::ThreadList, &[], Some(&state));
    assert_eq!(status, HsdbStatus::Ok);
    assert!(payload.len() >= 4);
    let count = u32::from_be_bytes(payload[0..4].try_into().unwrap());
    assert_eq!(count, 1);
    let tid = u64::from_be_bytes(payload[4..12].try_into().unwrap());
    assert_eq!(tid, 42);
    let name_len = u16::from_be_bytes([payload[12], payload[13]]) as usize;
    let name = std::str::from_utf8(&payload[14..14 + name_len]).unwrap();
    assert_eq!(name, "main");
    let state_byte = payload[14 + name_len];
    assert_eq!(state_byte, 1); // Runnable
}

#[test]
fn hsdb_thread_list_without_state_errors() {
    let (status, _) = hsdb_handle_request(HsdbCommand::ThreadList, &[], None);
    assert_eq!(status, HsdbStatus::Error);
}

/// Byte-for-byte recorded exchange: client handshake + VERSION request,
/// server handshake + OK response. Verifies wire compatibility without
/// needing a live client library.
#[test]
fn hsdb_recorded_exchange_matches() {
    // Server-side handshake echo: magic bytes
    let magic_bytes = HSDB_MAGIC.to_be_bytes();
    assert_eq!(&magic_bytes, b"HSDB");

    // Client sends VERSION request (no payload)
    let client_request = [0x01u8, 0x00, 0x00, 0x00, 0x00];
    let (cmd, len) = hsdb_decode_request_header(&client_request).unwrap();
    assert_eq!(cmd, HsdbCommand::Version);
    assert_eq!(len, 0);

    // Server processes and replies
    let (status, payload) = hsdb_handle_request(cmd, &[], None);
    let wire = hsdb_encode_response(status, &payload);

    // Expected wire bytes: [0][0][0][0][19][0][17]"CratonVM HSDB v1.0"
    // status(1) + payload_len(4) + str_len(2) + 17 ascii bytes = 24 bytes
    assert_eq!(wire.len(), 1 + 4 + 2 + HSDB_VERSION_STRING.len());
    assert_eq!(wire[0], 0); // OK
    let payload_len = u32::from_be_bytes([wire[1], wire[2], wire[3], wire[4]]);
    assert_eq!(payload_len as usize, 2 + HSDB_VERSION_STRING.len());
    let str_len = u16::from_be_bytes([wire[5], wire[6]]) as usize;
    assert_eq!(str_len, HSDB_VERSION_STRING.len());
    assert_eq!(&wire[7..], HSDB_VERSION_STRING.as_bytes());
}

#[test]
fn hsdb_listener_starts_and_responds() {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let state: Arc<dyn VmDiagnosticState> = Arc::new(DummyVmState);
    let listener = hsdb_start_listener(0, state).expect("bind");
    let addr = listener.addr;

    // Connect and do handshake + VERSION round-trip
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    stream.write_all(&HSDB_MAGIC.to_be_bytes()).unwrap();
    let mut echo = [0u8; 4];
    stream.read_exact(&mut echo).unwrap();
    assert_eq!(u32::from_be_bytes(echo), HSDB_MAGIC);

    // VERSION request
    stream.write_all(&[0x01, 0, 0, 0, 0]).unwrap();
    let mut hdr = [0u8; 5];
    stream.read_exact(&mut hdr).unwrap();
    assert_eq!(hdr[0], 0); // OK
    let plen = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
    let mut payload = vec![0u8; plen];
    stream.read_exact(&mut payload).unwrap();
    let str_len = u16::from_be_bytes([payload[0], payload[1]]) as usize;
    let s = std::str::from_utf8(&payload[2..2 + str_len]).unwrap();
    assert_eq!(s, HSDB_VERSION_STRING);

    drop(stream);
    listener.stop();
}
