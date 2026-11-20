use super::*;

/// zt t9/pl: lvmid 0 and this process's id are this VM, with or without
/// a receiver before them; another id, or no `int` at all, is not.
#[test]
fn zt9pl_attach0_targets_this_vm_only_for_zero_or_own_pid() {
    let receiver = Value::Object(None);
    assert!(perf_attach_targets_this_vm(&[Value::Int(0)]));
    assert!(perf_attach_targets_this_vm(&[receiver.clone(), Value::Int(0)]));
    let own = i32::try_from(std::process::id()).unwrap();
    assert!(perf_attach_targets_this_vm(&[receiver.clone(), Value::Int(own)]));
    let other = if own == i32::MAX { 1 } else { own + 1 };
    assert!(!perf_attach_targets_this_vm(&[receiver.clone(), Value::Int(other)]));
    assert!(!perf_attach_targets_this_vm(&[receiver]));
    assert!(!perf_attach_targets_this_vm(&[]));
}

/// zt t10/pl2: the lvmid `attach0` hands to the foreign door is the
/// last `int` argument, with or without the receiver before it.
#[test]
fn zt10pl2_attach0_reads_the_lvmid_as_the_last_int() {
    let receiver = Value::Object(None);
    assert_eq!(perf_attach_lvmid(&[receiver.clone(), Value::Int(4242)]), Some(4242));
    assert_eq!(perf_attach_lvmid(&[Value::Int(1), Value::Int(2)]), Some(2));
    assert_eq!(perf_attach_lvmid(&[receiver]), None);
    assert_eq!(perf_attach_lvmid(&[]), None);
}

/// zt t10/pl2: `Perf.detach` is the native that unmaps a foreign view
/// (it was a no-op), and `attach0` sends ids other than this VM's to the
/// foreign door. Text pins: the unit-test context maps no perf data.
#[test]
fn zt10pl2_detach_unmaps_and_attach0_has_a_foreign_door() {
    let lib = include_str!("../lib_parts/url_path_or_file_field.rs");
    let at = lib.find("\"detach\",").expect("Perf.detach");
    let before = &lib[at.saturating_sub(80)..at];
    assert!(before.contains("\"jdk/internal/perf/Perf\","), "{before}");
    let end = at + lib[at..].find(");").expect("the registration's end");
    assert!(lib[at..end].contains("lang_system::native_perf_detach"));
    let src = concat!(include_str!("../lang_system/mod.rs"), "\n", include_str!("../lang_system/shutdown_state.rs"), "\n", include_str!("../lang_system/runtime_version_publish.rs"), "\n", include_str!("../lang_system/note_supertype_initiation.rs"));
    let start = src.find("pub fn native_perf_attach0(").expect("attach0");
    let body = &src[start..start + src[start..].find("\n}\n").expect("its end")];
    assert!(body.contains("ctx.attach_foreign_perf_data(lvmid)"));
    assert!(body.contains("ctx.detach_perf_data(address)"));
}
