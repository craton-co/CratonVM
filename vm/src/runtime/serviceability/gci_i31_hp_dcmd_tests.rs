use super::*;

fn command(name: &str, impact: CommandImpact) -> DiagnosticCommand {
    DiagnosticCommand::new(
        name,
        "d",
        impact,
        CommandPermission::ReadOnly,
        vec![],
        Box::new(|_| CommandResult::ok(String::new(), 0)),
    )
}

/// HotSpot 25's `impact()` texts: the long ones by name, the bare level
/// otherwise; both `jcmd help <command>` and the MBean read them.
#[test]
fn gci_i31hp_impact_texts_are_hotspots() {
    let gc_run = command("GC.run", CommandImpact::Medium);
    assert_eq!(
        dcmd_impact_text(&gc_run),
        "Medium: Depends on Java heap size and content."
    );
    assert_eq!(
        dcmd_impact_text(&command("Thread.print", CommandImpact::Medium)),
        "Medium: Depends on the number of threads."
    );
    assert_eq!(dcmd_impact_text(&command("VM.uptime", CommandImpact::Low)), "Low");
    assert_eq!(dcmd_impact_text(&command("GC.heap_info", CommandImpact::Medium)), "Medium");
    let commands = vec![gc_run];
    assert!(jcmd_help(&commands, &["GC.run".to_string()])
        .contains("\nImpact: Medium: Depends on Java heap size and content.\n"));
    let info = dcmd_mbean_info(&commands, "GC.run").expect("listed");
    assert_eq!(info.impact, "Medium: Depends on Java heap size and content.");
}

/// The live registrar carries HotSpot 25's descriptions (read from the
/// 25.0.3 `jvm.dll`), not this VM's old paraphrases. Needles are split
/// with `concat!` so this test's own text never matches them.
#[test]
fn gci_i31hp_the_registrars_use_hotspots_descriptions() {
    let src = include_str!("../serviceability.rs").replace("\r\n", "\n");
    for old in [
        concat!("\"Trigger garbage ", "collection\""),
        concat!("\"Print heap summary ", "information\""),
        concat!("\"Print class ", "histogram\""),
        concat!("\"Print VM flag ", "settings\""),
        concat!("\"Print VM ", "uptime\""),
        concat!("\"Print command line ", "arguments\""),
        concat!("\"Start a flight ", "recording\""),
    ] {
        assert!(!src.contains(old), "{old}");
    }
    for new in [
        concat!("\"GC.run\",\n            \"Call java.lang.System.", "gc().\""),
        concat!("\"GC.heap_info\",\n            \"Provide generic Java heap ", "information.\""),
        concat!("\"GC.class_histogram\",\n            \"Provide statistics about the Java heap ", "usage.\""),
        concat!("\"VM.flags\",\n            \"Print VM flag options and their current ", "values.\""),
        concat!("\"VM.uptime\",\n            \"Print VM ", "uptime.\""),
        concat!("\"JFR.stop\",\n            \"Stops a JFR ", "recording\""),
    ] {
        assert!(src.contains(new), "{new}");
    }
}
