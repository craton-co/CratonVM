use super::*;

fn table() -> parking_lot::RwLock<Vec<DiagnosticCommand>> {
    let echo = DiagnosticCommand::new(
        "Echo.args",
        "Echo the arguments",
        CommandImpact::Medium,
        CommandPermission::ManagementAction,
        vec![
            CommandArgument {
                name: "-loud".to_string(),
                description: "Shout".to_string(),
                arg_type: ArgType::Bool,
                required: false,
                default_value: Some("false".to_string()),
            },
            CommandArgument {
                name: "text".to_string(),
                description: "What to echo".to_string(),
                arg_type: ArgType::String,
                required: true,
                default_value: None,
            },
        ],
        Box::new(|args| CommandResult::ok(args.join("|"), 0)),
    );
    let refuse = DiagnosticCommand::new(
        "Refuse.it",
        "Always refuses",
        CommandImpact::Low,
        CommandPermission::ReadOnly,
        vec![],
        Box::new(|_| {
            CommandResult::err(
                "java.lang.IllegalArgumentException: Nope\n".to_string(),
                0,
            )
        }),
    );
    let internal = DiagnosticCommand::new(
        "attach internal",
        "",
        CommandImpact::Low,
        CommandPermission::ReadOnly,
        vec![],
        Box::new(|_| CommandResult::ok(String::new(), 0)),
    );
    parking_lot::RwLock::new(vec![echo, refuse, internal])
}

#[test]
fn gci_i28dg_mbean_lists_jcmd_names_and_help() {
    let t = table();
    assert_eq!(
        dcmd_mbean_names(&t.read()),
        vec!["Echo.args", "Refuse.it", "help"]
    );
}

#[test]
fn gci_i28dg_mbean_info_is_jmms_shape() {
    let t = table();
    let echo = dcmd_mbean_info(&t.read(), "Echo.args").expect("listed");
    assert_eq!(echo.impact, "Medium");
    assert_eq!(
        echo.permission_class.as_deref(),
        Some("java.lang.management.ManagementPermission")
    );
    assert_eq!(echo.permission_name.as_deref(), Some("control"));
    assert!(echo.enabled);
    assert_eq!(echo.arguments.len(), 2);
    assert!(echo.arguments[0].option && echo.arguments[0].position == -1);
    assert_eq!(echo.arguments[0].arg_type, "BOOLEAN");
    assert!(!echo.arguments[1].option && echo.arguments[1].position == 0);
    assert!(echo.arguments[1].mandatory);
    let refuse = dcmd_mbean_info(&t.read(), "Refuse.it").expect("listed");
    assert_eq!(refuse.permission_name.as_deref(), Some("monitor"));
    let help = dcmd_mbean_info(&t.read(), "help").expect("help");
    assert!(help.permission_class.is_none());
    assert_eq!(help.arguments.len(), 2);
    assert!(dcmd_mbean_info(&t.read(), "attach internal").is_none());
    assert!(dcmd_mbean_info(&t.read(), "No.such").is_none());
}

#[test]
fn gci_i28dg_mbean_execute_is_jmms() {
    let t = table();
    assert_eq!(
        dcmd_mbean_execute(&t, "Echo.args -loud \"a b\""),
        Ok("-loud|a b".to_string())
    );
    assert_eq!(
        dcmd_mbean_execute(&t, "No.such x"),
        Err("Unknown diagnostic command".to_string())
    );
    assert_eq!(
        dcmd_mbean_execute(&t, "attach internal"),
        Err("Unknown diagnostic command".to_string())
    );
    assert_eq!(dcmd_mbean_execute(&t, "Refuse.it"), Err("Nope".to_string()));
    assert_eq!(dcmd_mbean_execute(&t, "   "), Ok(String::new()));
    assert_eq!(
        dcmd_mbean_execute(&t, "Echo.args \"open"),
        Err("Format error in diagnostic command arguments".to_string())
    );
    let help = dcmd_mbean_execute(&t, "help").expect("help");
    assert!(help.starts_with("The following commands are available:\n"));
}
