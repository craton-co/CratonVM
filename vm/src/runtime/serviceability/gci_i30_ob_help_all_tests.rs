use super::*;

fn command(name: &str, description: &str) -> DiagnosticCommand {
    DiagnosticCommand::new(
        name,
        description,
        CommandImpact::Low,
        CommandPermission::ReadOnly,
        vec![],
        Box::new(|_| CommandResult::ok(String::new(), 0)),
    )
}

#[test]
fn gci_i30ob_help_all_option_is_a_boolean() {
    let s = |v: &[&str]| v.iter().map(|a| a.to_string()).collect::<Vec<String>>();
    assert_eq!(jcmd_help_all_option(&s(&["-all"])), (true, vec![]));
    assert_eq!(jcmd_help_all_option(&s(&["-all=true"])), (true, vec![]));
    assert_eq!(
        jcmd_help_all_option(&s(&["-all=false", "VM.uptime"])),
        (false, s(&["VM.uptime"]))
    );
    assert_eq!(
        jcmd_help_all_option(&s(&["VM.uptime", "-all"])),
        (true, s(&["VM.uptime"]))
    );
    assert_eq!(jcmd_help_all_option(&s(&["all"])), (false, s(&["all"])));
}

/// HotSpot's `HelpDCmd` with `_all`: sorted names (and `help`), each
/// `name\n\tdescription\n\n`; internal attach operations (a space in the
/// name) are not commands. `-all` wins over a command name.
#[test]
fn gci_i30ob_help_all_lists_every_command_with_its_description() {
    let commands = vec![
        command("VM.uptime", "Print VM uptime."),
        command("GC.run", "Call java.lang.System.gc()."),
        command("attach load", "internal"),
    ];
    let out = jcmd_help(&commands, &["-all".to_string()]);
    assert_eq!(
        out,
        format!(
            "GC.run\n\tCall java.lang.System.gc().\n\n\
             VM.uptime\n\tPrint VM uptime.\n\n\
             help\n\t{DCMD_HELP_DESCRIPTION}\n\n"
        )
    );
    assert_eq!(jcmd_help(&commands, &["VM.uptime".to_string(), "-all".to_string()]), out);
    // Without the option, unchanged.
    assert!(jcmd_help(&commands, &[]).starts_with("The following commands are available:\n"));
    assert!(jcmd_help(&commands, &["-all=false".to_string()])
        .starts_with("The following commands are available:\n"));
    assert_eq!(
        jcmd_help(&commands, &["No.such".to_string()]),
        "Help unavailable : 'No.such' : No such command\n"
    );
}
