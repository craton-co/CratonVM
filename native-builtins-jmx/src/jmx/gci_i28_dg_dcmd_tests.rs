use super::*;
use cratonvm_native_api::NativeHeapAccess;

const TRIPLES: [(&str, &str); 4] = [
    ("getDiagnosticCommands", "()[Ljava/lang/String;"),
    (
        "getDiagnosticCommandInfo",
        "([Ljava/lang/String;)[Lcom/sun/management/internal/DiagnosticCommandInfo;",
    ),
    (
        "executeDiagnosticCommand",
        "(Ljava/lang/String;)Ljava/lang/String;",
    ),
    ("setNotificationEnabled", "(Z)V"),
];

#[test]
fn gci_i28dg_dcmd_natives_are_stated_bridges() {
    for registrar in [
        register_diagnostic_command_impl as fn(&mut NativeMethodRegistry),
        register_vm_management_impl,
    ] {
        let mut r = NativeMethodRegistry::new();
        registrar(&mut r);
        for (name, desc) in TRIPLES {
            assert_eq!(
                r.kind_of(DCMD_IMPL, name, desc),
                Some(cratonvm_native_api::NativeKind::Bridge),
                "{name}{desc}"
            );
        }
    }
}


#[test]
fn gci_i28dg_without_a_command_table_the_natives_refuse() {
    let mut r = NativeMethodRegistry::new();
    register_diagnostic_command_impl(&mut r);
    let mut ctx = cratonvm_native_builtins::test_utils::mock_ctx();
    let exec = r
        .find(DCMD_IMPL, TRIPLES[2].0, TRIPLES[2].1)
        .expect("executeDiagnosticCommand");
    let line = ctx.create_string_uninterned("GC.run");
    assert!(exec(&mut ctx, &[Value::Object(None), Value::Object(Some(line))]).is_err());
    assert!(exec(&mut ctx, &[Value::Object(None), Value::Object(None)]).is_err());
    let info = r
        .find(DCMD_IMPL, TRIPLES[1].0, TRIPLES[1].1)
        .expect("getDiagnosticCommandInfo");
    assert!(info(&mut ctx, &[Value::Object(None), Value::Object(None)]).is_err());
    let notify = r
        .find(DCMD_IMPL, TRIPLES[3].0, TRIPLES[3].1)
        .expect("setNotificationEnabled");
    assert!(matches!(
        notify(&mut ctx, &[Value::Object(None), Value::Int(1)]),
        Ok(None)
    ));
}
