use super::*;

/// A live state whose Java-side answers are scripted.
struct JavaSideState {
    jfr: bool,
}

impl VmDiagnosticState for JavaSideState {
    fn thread_snapshots(&self) -> Vec<ThreadSnapshot> {
        Vec::new()
    }
    fn heap_summary(&self) -> HeapSummary {
        HeapSummary {
            young_gen_used: 0,
            young_gen_capacity: 0,
            old_gen_used: 0,
            old_gen_capacity: 0,
            metaspace_used: 0,
            metaspace_capacity: 0,
            total_used: 0,
            total_capacity: 0,
        }
    }
    fn class_histogram(&self) -> Vec<ClassHistogramEntry> {
        Vec::new()
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
    fn jfr_help(&self, class: &str) -> Result<String, String> {
        if self.jfr {
            Ok(format!("Syntax : from {class}\n\nOptions:\n"))
        } else {
            Err("no Java thread".to_string())
        }
    }
    fn finalizer_info(&self) -> Result<String, String> {
        Ok(finalizer_info_text(&["p.A".to_string()]))
    }
    fn attach_load_enabled(&self) -> bool {
        true
    }
}

fn live(state: &Arc<dyn VmDiagnosticState>) -> AttachListener {
    let mut listener = AttachListener::new("");
    JcmdProcessor::register_live_commands(&mut listener, Arc::downgrade(state));
    listener
}

fn names(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// HotSpot 25's `FinalizerInfoDCmd` text: the empty answer, the header
/// and `%10d  %s` rows, the most numerous class first.
#[test]
fn gci_i33jf_finalizer_info_text_is_hotspots() {
    assert_eq!(
        finalizer_info_text(&[]),
        "No instances waiting for finalization found\n"
    );
    assert_eq!(
        finalizer_info_text(&names(&["p.A", "p.Bee", "p.Bee"])),
        "Unreachable instances waiting for finalization\n\
         #instances  class name\n\
         -----------------------\n\
         \x20        2  p.Bee\n\
         \x20        1  p.A\n"
    );
}

/// Equal counts keep `FinalizerHistogram`'s `HashMap` order: by bucket,
/// a new key at the head of its bin, and the resize that
/// `computeIfAbsent` makes before it looks once `size > threshold`.
#[test]
fn gci_i33jf_finalizer_histogram_ties_follow_hashmaps_order() {
    assert_eq!(java_string_hash("a"), 97);
    assert_eq!(java_string_hash("java.lang.Object"), 1063877011);
    fn order(v: &[&str]) -> Vec<String> {
        finalizer_histogram(&names(v)).into_iter().map(|(n, _)| n).collect()
    }
    // Bucket order, whatever the walk order.
    assert_eq!(order(&["b", "a"]), names(&["a", "b"]));
    // "a" (97) and "q" (113) share bin 1 of 16: the later one heads it.
    assert_eq!(order(&["a", "q"]), names(&["q", "a"]));
    // The 14th call finds 13 keys (> 12) and doubles the table first:
    // "q" lands in bin 17 of 32, after "a".."m".
    let mut walk: Vec<&str> = vec![
        "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m",
    ];
    walk.push("q");
    let mut expected = walk.clone();
    expected.sort_unstable();
    assert_eq!(order(&walk), names(&expected));
    // Counts first (a stable sort, descending).
    assert_eq!(order(&["a", "q", "a"]), names(&["a", "q"]));
}

/// The live table registers HotSpot 25's `GC.finalizer_info` and
/// `GC.run_finalization` (parserless `DCmd`s, HotSpot's descriptions and
/// impacts); a state that cannot run them answers a failure, not a text.
#[test]
fn gci_i33jf_live_set_has_hotspots_finalization_commands() {
    let state: Arc<dyn VmDiagnosticState> = Arc::new(JavaSideState { jfr: false });
    let listener = live(&state);
    let commands = listener.commands.read();
    assert_eq!(
        jcmd_help(&commands, &names(&["GC.finalizer_info"])),
        "GC.finalizer_info\nProvide information about Java finalization queue.\n\n\
         Impact: Medium\n\nSyntax: GC.finalizer_info\n"
    );
    assert_eq!(
        jcmd_help(&commands, &names(&["GC.run_finalization"])),
        "GC.run_finalization\nCall java.lang.System.runFinalization().\n\n\
         Impact: Medium: Depends on Java content.\n\nSyntax: GC.run_finalization\n"
    );
    let run = |name: &str| {
        commands
            .iter()
            .find(|c| c.name == name)
            .expect("registered")
            .execute(&[])
    };
    let info = run("GC.finalizer_info");
    assert!(info.success);
    assert!(info.output.ends_with("\n         1  p.A\n"), "{}", info.output);
    // The default trait body: an honest refusal.
    assert!(!run("GC.run_finalization").success);
    let listed = dcmd_mbean_names(&commands);
    assert!(listed.contains(&"GC.finalizer_info".to_string()));
    assert!(listed.contains(&"GC.run_finalization".to_string()));
}

/// `help JFR.<x>` is HotSpot's header followed by the JDK class's own
/// `getHelp()` text when the state can run it, and the generic page
/// otherwise; `JFR.dump` / `JFR.stop` ask their own classes.
#[test]
fn gci_i33jf_jfr_help_is_the_jdks_text() {
    let state: Arc<dyn VmDiagnosticState> = Arc::new(JavaSideState { jfr: true });
    let listener = live(&state);
    let commands = listener.commands.read();
    assert_eq!(
        jcmd_help(&commands, &names(&["JFR.start"])),
        "JFR.start\nStarts a new JFR recording\n\n\
         Impact: Medium: Depending on the settings for a recording, the impact can range \
         from low to high.\n\n\
         Syntax : from jdk/jfr/internal/dcmd/DCmdStart\n\nOptions:\n"
    );
    assert!(jcmd_help(&commands, &names(&["JFR.dump"]))
        .ends_with("\nImpact: Low\n\nSyntax : from jdk/jfr/internal/dcmd/DCmdDump\n\nOptions:\n"));
    assert!(jcmd_help(&commands, &names(&["JFR.stop"]))
        .ends_with("\nImpact: Low\n\nSyntax : from jdk/jfr/internal/dcmd/DCmdStop\n\nOptions:\n"));
    // A parser command keeps its own page.
    assert!(jcmd_help(&commands, &names(&["VM.uptime"])).contains("\nSyntax : VM.uptime [options]\n"));
    drop(commands);
    drop(listener);

    let state: Arc<dyn VmDiagnosticState> = Arc::new(JavaSideState { jfr: false });
    let listener = live(&state);
    let page = jcmd_help(&listener.commands.read(), &names(&["JFR.start"]));
    assert!(page.contains("\n\nSyntax : JFR.start [options]\n"), "{page}");
}

#[test]
fn hotspot_25_all_55_jcmd_commands_and_mbean_parity() {
    let state: Arc<dyn VmDiagnosticState> = Arc::new(JavaSideState { jfr: true });
    let listener = live(&state);
    let commands = listener.commands.read();
    let visible_55: [&str; 55] = [
        "AOT.end_recording",
        "Compiler.CodeHeap_Analytics",
        "Compiler.codecache",
        "Compiler.codelist",
        "Compiler.directives_add",
        "Compiler.directives_clear",
        "Compiler.directives_print",
        "Compiler.directives_remove",
        "Compiler.memory",
        "Compiler.queue",
        "GC.class_histogram",
        "GC.finalizer_info",
        "GC.heap_dump",
        "GC.heap_info",
        "GC.run",
        "GC.run_finalization",
        "JFR.check",
        "JFR.configure",
        "JFR.dump",
        "JFR.start",
        "JFR.stop",
        "JFR.view",
        "JVMTI.agent_load",
        "JVMTI.data_dump",
        "ManagementAgent.start",
        "ManagementAgent.start_local",
        "ManagementAgent.status",
        "ManagementAgent.stop",
        "System.dump_map",
        "System.map",
        "Thread.dump_to_file",
        "Thread.print",
        "Thread.vthread_pollers",
        "Thread.vthread_scheduler",
        "VM.cds",
        "VM.class_hierarchy",
        "VM.classes",
        "VM.classloader_stats",
        "VM.classloaders",
        "VM.command_line",
        "VM.dynlibs",
        "VM.events",
        "VM.flags",
        "VM.info",
        "VM.log",
        "VM.metaspace",
        "VM.native_memory",
        "VM.set_flag",
        "VM.stringtable",
        "VM.symboltable",
        "VM.system_properties",
        "VM.systemdictionary",
        "VM.uptime",
        "VM.version",
        "help",
    ];
    assert_eq!(visible_55.len(), 55);
    assert_eq!(
        commands.len(),
        58,
        "54 jcmd commands + SIGQUIT_HEAP_COMMAND + SIGQUIT_CLASS_HISTOGRAM_COMMAND (zt t27/lg1) + SIGQUIT_THREADS_COMMAND (zt t28/mf) + ATTACH_LOAD_COMMAND = 58 registered commands"
    );

    let help_list = jcmd_help(&commands, &[]);
    for &cmd in &visible_55 {
        assert!(
            help_list.contains(&format!("\n{cmd}\n")),
            "jcmd help missing visible command {cmd}"
        );
    }

    let mbean_names = dcmd_mbean_names(&commands);
    assert_eq!(
        mbean_names.len(),
        50,
        "HotSpot 25 DiagnosticCommand MBean exports 50 commands: {mbean_names:?}"
    );
    assert!(
        mbean_names.iter().any(|n| n == "JVMTI.agent_load"),
        "JVMTI.agent_load is full_export in HotSpot 25"
    );
    for attach_only in [
        "GC.heap_dump",
        "ManagementAgent.start",
        "ManagementAgent.start_local",
        "ManagementAgent.status",
        "ManagementAgent.stop",
    ] {
        assert!(
            !mbean_names.iter().any(|n| n == attach_only),
            "{attach_only} is attach-only (export: false) in HotSpot 25"
        );
    }

    // Verify JFR.configure static C++ help page
    let jfr_conf_help = jcmd_help(&commands, &names(&["JFR.configure"]));
    assert!(
        jfr_conf_help.starts_with("JFR.configure\nConfigure JFR\n\nImpact: Low\n\nOptions:\n"),
        "JFR.configure help mismatch: {jfr_conf_help}"
    );
    assert!(
        jfr_conf_help.contains(&format!(
            "  {:<20}(Optional) Path to the location",
            "dumppath"
        )),
        "JFR.configure help missing dumppath: {jfr_conf_help}"
    );

    // Verify JFR.check and JFR.view use DCmdCheck / DCmdView help pages
    assert!(jcmd_help(&commands, &names(&["JFR.check"]))
        .ends_with("\nImpact: Low\n\nSyntax : from jdk/jfr/internal/dcmd/DCmdCheck\n\nOptions:\n"));
    assert!(jcmd_help(&commands, &names(&["JFR.view"]))
        .ends_with("\nImpact: Medium\n\nSyntax : from jdk/jfr/internal/dcmd/DCmdView\n\nOptions:\n"));

    // Verify MBean argument info for JFR.* commands matches HotSpot 25
    let jfr_start_info = dcmd_mbean_info(&commands, "JFR.start").expect("JFR.start");
    assert_eq!(jfr_start_info.arguments.len(), 11);
    let settings_arg = jfr_start_info
        .arguments
        .iter()
        .find(|a| a.name == "settings")
        .expect("settings");
    assert_eq!(settings_arg.arg_type, "STRING SET");
    assert!(settings_arg.option && settings_arg.multiple && settings_arg.position == -1);

    let jfr_view_info = dcmd_mbean_info(&commands, "JFR.view").expect("JFR.view");
    assert_eq!(jfr_view_info.arguments.len(), 7);
    let view_arg = jfr_view_info
        .arguments
        .iter()
        .find(|a| a.name == "view")
        .expect("view");
    assert!(view_arg.mandatory && !view_arg.option && view_arg.position == -1);

    let jfr_conf_info = dcmd_mbean_info(&commands, "JFR.configure").expect("JFR.configure");
    assert_eq!(jfr_conf_info.arguments.len(), 10);
    drop(commands);

    // Verify VM.info and JFR.check command outputs
    let vm_info_out = dcmd_mbean_execute(&listener.commands, "VM.info").expect("VM.info");
    for section in [
        "# JRE version:",
        "---------------  S U M M A R Y ------------",
        "---------------  T H R E A D  ---------------",
        "---------------  P R O C E S S  ---------------",
        "---------------  S Y S T E M  ---------------",
        "GC Precious Log:",
        "Heap:",
        "Metaspace:",
        " CPUs: ",
    ] {
        assert!(vm_info_out.contains(section), "VM.info missing {section}");
    }
    let jfr_check_out = dcmd_mbean_execute(&listener.commands, "JFR.check").expect("JFR.check");
    assert_eq!(
        jfr_check_out,
        format!(
            "No available recordings.\n\nUse jcmd {} JFR.start to start a recording.\n",
            std::process::id()
        )
    );
}
