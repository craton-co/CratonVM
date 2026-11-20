use super::*;

fn command(name: &str, description: &str, impact: CommandImpact) -> DiagnosticCommand {
    DiagnosticCommand::new(
        name,
        description,
        impact,
        CommandPermission::ReadOnly,
        hotspot_dcmd_arguments(name),
        Box::new(|_| CommandResult::ok(String::new(), 0)),
    )
}

fn help(commands: &[DiagnosticCommand], name: &str) -> String {
    jcmd_help(commands, &[name.to_string()])
}

/// Options only: `Syntax : <name> [options]`, the `Options:` block, no
/// `Arguments:` block and no `Permission:` line (JDK 25 has none).
#[test]
fn gci_i32jh_options_only_page_is_hotspots() {
    let commands = vec![command(
        "Thread.print",
        "Print all threads with stacktraces.",
        CommandImpact::Medium,
    )];
    assert_eq!(
        help(&commands, "Thread.print"),
        "Thread.print\nPrint all threads with stacktraces.\n\n\
         Impact: Medium: Depends on the number of threads.\n\n\
         Syntax : Thread.print [options]\n\n\
         Options: (options must be specified using the <key> or <key>=<value> syntax)\n\
         \t-l : [optional] print java.util.concurrent locks (BOOLEAN, false)\n\
         \t-e : [optional] print extended thread information (BOOLEAN, false)\n"
    );
}

/// A mandatory argument and options: the argument in the syntax line, a
/// mandatory row with an empty `[optional]` (two spaces), HotSpot's type
/// names and defaults.
#[test]
fn gci_i32jh_arguments_and_options_page_is_hotspots() {
    let commands = vec![command(
        "GC.heap_dump",
        "Generate a HPROF format dump of the Java heap.",
        CommandImpact::High,
    )];
    let out = help(&commands, "GC.heap_dump");
    assert!(out.starts_with(
        "GC.heap_dump\nGenerate a HPROF format dump of the Java heap.\n\nImpact: High: "
    ));
    assert!(out.contains(
        "\n\nSyntax : GC.heap_dump [options] <filename>\n\nArguments:\n\
         \tfilename :  Name of the dump file (FILE, no default value)\n\n\
         Options: (options must be specified using the <key> or <key>=<value> syntax)\n\
         \t-all : [optional] Dump all objects, including unreachable objects (BOOLEAN, false)\n\
         \t-gz : [optional] If specified, the heap dump is written in gzipped format"
    ));
    assert!(out.contains("9 the strongest compression. (INT, 1)\n\t-overwrite : [optional] "));
    assert!(out.ends_with("but might use fewer. (INT, 1)\n"));
    assert!(!out.contains("Permission"));
}

/// Arguments only (no `[options]`, no `Options:` block); an optional
/// argument is `[<name>]`.
#[test]
fn gci_i32jh_arguments_only_page_is_hotspots() {
    let commands = vec![command(
        "JVMTI.agent_load",
        "Load JVMTI native agent.",
        CommandImpact::Low,
    )];
    assert_eq!(
        help(&commands, "JVMTI.agent_load"),
        "JVMTI.agent_load\nLoad JVMTI native agent.\n\nImpact: Low\n\n\
         Syntax : JVMTI.agent_load  <library path> [<agent option>]\n\n\
         Arguments:\n\
         \tlibrary path :  Absolute path of the JVMTI agent to load. (STRING, no default value)\n\
         \tagent option : [optional] Option string to pass the agent. (STRING, no default value)\n"
    );
}

/// A parserless HotSpot `DCmd` prints `Syntax: <name>`; `help help` is
/// `HelpDCmd`'s own page.
#[test]
fn gci_i32jh_parserless_and_help_pages_are_hotspots() {
    let commands = vec![command(
        "GC.run",
        "Call java.lang.System.gc().",
        CommandImpact::Medium,
    )];
    assert_eq!(
        help(&commands, "GC.run"),
        "GC.run\nCall java.lang.System.gc().\n\n\
         Impact: Medium: Depends on Java heap size and content.\n\nSyntax: GC.run\n"
    );
    assert_eq!(
        help(&commands, "help"),
        format!(
            "help\n{DCMD_HELP_DESCRIPTION}\n\nImpact: Low\n\n\
             Syntax : help [options] [<command name>]\n\n\
             Arguments:\n\
             \tcommand name : [optional] The name of the command for which we want help \
             (STRING, no default value)\n\n\
             Options: (options must be specified using the <key> or <key>=<value> syntax)\n\
             \t-all : [optional] Show help for all commands (BOOLEAN, false)\n"
        )
    );
}

/// The tables: HotSpot's parserless commands declare nothing; the MBean
/// hands HotSpot's raw type names (JDK 25's `sanitiseType` maps `FILE`).
#[test]
fn gci_i32jh_argument_tables_feed_the_mbean() {
    for parserless in [
        "GC.run",
        "GC.heap_info",
        "VM.version",
        "VM.system_properties",
        "VM.command_line",
    ] {
        assert!(hotspot_dcmd_arguments(parserless).is_empty(), "{parserless}");
    }
    let commands = vec![command(
        "Thread.dump_to_file",
        "Dump threads, with stack traces, to a file in plain text or JSON format.",
        CommandImpact::Medium,
    )];
    let info = dcmd_mbean_info(&commands, "Thread.dump_to_file").expect("listed");
    let shape: Vec<(String, String, bool, i32, Option<String>)> = info
        .arguments
        .iter()
        .map(|a| {
            (
                a.name.clone(),
                a.arg_type.clone(),
                a.option,
                a.position,
                a.default_value.clone(),
            )
        })
        .collect();
    assert_eq!(
        shape,
        vec![
            ("-overwrite".to_string(), "BOOLEAN".to_string(), true, -1, Some("false".to_string())),
            ("-format".to_string(), "STRING".to_string(), true, -1, Some("plain".to_string())),
            ("filepath".to_string(), "FILE".to_string(), false, 0, None),
        ]
    );
    assert!(help(&commands, "Thread.dump_to_file")
        .contains("\nSyntax : Thread.dump_to_file [options] <filepath>\n"));
}

/// gci i35/jh (vi32-jh H1 / H2, MEASURED on i34b): HotSpot 25 does not
/// export `GC.heap_dump` to the DiagnosticCommand MBean -- not listed, no
/// info row, no `help` page, not executable there -- while `jcmd` (the
/// attach source) still has all of it.
#[test]
fn gci_i35jh_heap_dump_is_not_exported_to_the_mbean() {
    let table = parking_lot::RwLock::new(vec![
        command(
            "GC.heap_dump",
            "Generate a HPROF format dump of the Java heap.",
            CommandImpact::High,
        ),
        command("GC.run", "Call java.lang.System.gc().", CommandImpact::Medium),
    ]);
    assert_eq!(dcmd_mbean_names(&table.read()), vec!["GC.run", "help"]);
    assert!(dcmd_mbean_info(&table.read(), "GC.heap_dump").is_none());
    assert!(dcmd_mbean_info(&table.read(), "GC.run").is_some());
    assert_eq!(
        dcmd_mbean_execute(&table, "help GC.heap_dump"),
        Ok("Help unavailable : 'GC.heap_dump' : No such command\n".to_string())
    );
    assert_eq!(
        dcmd_mbean_execute(&table, "GC.heap_dump /tmp/x.hprof"),
        Err("Unknown diagnostic command".to_string())
    );
    let listing = dcmd_mbean_execute(&table, "help").expect("help");
    assert!(!listing.contains("GC.heap_dump"), "{listing}");
    let all = dcmd_mbean_execute(&table, "help -all").expect("help -all");
    assert!(!all.contains("GC.heap_dump") && all.contains("GC.run\n\t"), "{all}");
    // The attach source (`jcmd`) is unchanged.
    assert!(help(&table.read(), "GC.heap_dump").starts_with("GC.heap_dump\nGenerate a HPROF"));
    assert!(jcmd_help(&table.read(), &[]).contains("\nGC.heap_dump\n"));
    assert!(dcmd_visible("GC.run", true) && !dcmd_visible("attach load", false));
}
