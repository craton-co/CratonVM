use super::*;

fn link(name: &str, o: bool) -> ChainLink<'_> {
    ChainLink {
        name,
        declares_concrete_override: o,
    }
}

#[test]
fn gci_i11jd_filter_value_rule() {
    assert_eq!(parse_filter(None), None);
    for off in ["", "0", "false", "OFF", "no", "  "] {
        assert_eq!(parse_filter(Some(off)), None, "{off:?}");
    }
    for on in ["1", "true", "on", "YES"] {
        assert_eq!(parse_filter(Some(on)).as_deref(), Some(DEFAULT_PREFIX), "{on:?}");
    }
    assert_eq!(parse_filter(Some("*")).as_deref(), Some(""));
    assert_eq!(parse_filter(Some("all")).as_deref(), Some(""));
    assert_eq!(parse_filter(Some(" com/acme/ ")).as_deref(), Some("com/acme/"));
}

/// The S2 shape: the base `LocationHandler` body entered with a
/// `ModuleSourcePathLocationHandler` receiver, which overrides it.
#[test]
fn gci_i11jd_base_body_with_an_overriding_receiver_is_reported() {
    let base = "com/sun/tools/javac/file/Locations$LocationHandler";
    let chain = [
        link("com/sun/tools/javac/file/Locations$ModuleSourcePathLocationHandler", true),
        link("com/sun/tools/javac/file/Locations$BasicLocationHandler", false),
        link(base, true),
        link("java/lang/Object", false),
    ];
    assert_eq!(
        overrider_below(&chain, base),
        Some("com/sun/tools/javac/file/Locations$ModuleSourcePathLocationHandler")
    );
}

#[test]
fn gci_i11jd_correct_dispatch_is_silent() {
    let base = "com/sun/tools/javac/file/Locations$LocationHandler";
    // The receiver's own body (declaring == receiver class).
    let own = [link("A", true), link(base, true)];
    assert_eq!(overrider_below(&own, "A"), None);
    // An inherited body nobody below overrides (SimpleLocationHandler).
    let inherited = [
        link("Simple", false),
        link("Basic", false),
        link(base, true),
        link("java/lang/Object", false),
    ];
    assert_eq!(overrider_below(&inherited, base), None);
    // Declaring class not on the chain (a static method's first argument,
    // an interface default method).
    assert_eq!(overrider_below(&inherited, "some/Iface"), None);
    // Empty chain.
    assert_eq!(overrider_below(&[], base), None);
}

/// Source pin: every frame-push funnel that holds a `SharedVm` asks the
/// witness after its push.
#[test]
fn gci_i11jd_witness_is_called_at_the_push_funnels() {
    let call = "super::gci_i11jd_witness::note_entry(shared, thread);";
    assert_eq!(include_str!("../invoke_fast.rs").matches(call).count(), 5);
    assert_eq!(include_str!("../jvmti_events.rs").matches(call).count(), 2);
    assert_eq!(include_str!("../invoke.rs").matches(call).count(), 1);
    let own = "\n    gci_i11jd_witness::note_entry(shared, thread);\n";
    assert_eq!(concat!(include_str!("../../interpreter/mod.rs"), "\n", include_str!("../../interpreter/cached_invoke_target.rs"), "\n", include_str!("../../interpreter/execute.rs"), "\n", include_str!("../../interpreter/resume_continuation.rs"), "\n", include_str!("../../interpreter/execute_frame_from_index_loop.rs"), "\n", include_str!("../../interpreter/resolve_array_class_loader_aware.rs"), "\n", include_str!("../../interpreter/park_for_debugger_under.rs"), "\n", include_str!("../../interpreter/double_arith_apply.rs")).matches(own).count(), 2);
}

/// The filter is per-VM state: latched into `SharedVm` at construction,
/// read through `shared` at the gate, and this module declares no static.
#[test]
fn gci_i11jd_filter_is_per_vm_state() {
    let own = include_str!("../gci_i11jd_witness.rs").replace("\r\n", "\n");
    let kw = ["sta", "tic "].concat();
    assert!(
        !own.lines().any(|l| l.trim_start().starts_with(&kw)),
        "no process static in the witness module"
    );
    assert!(own.contains("if let Some(prefix) = shared.dbg_base_body_entry_filter.as_deref() {"));
    let init = concat!(include_str!("../../../vm/vm_init/mod.rs"), "\n", include_str!("../../../vm/vm_init/missing_native_entry.rs"), "\n", include_str!("../../../vm/vm_init/real_jdk_to_array_typed.rs"), "\n", include_str!("../../../vm/vm_init/zgc_relocation_permitted.rs"), "\n", include_str!("../../../vm/vm_init/wait_site.rs"), "\n", include_str!("../../../vm/vm_init/class_store_cut_with.rs")).replace("\r\n", "\n");
    assert!(init.contains("pub dbg_base_body_entry_filter: Option<Box<str>>,"));
    assert!(init.contains(
        "dbg_base_body_entry_filter:\n                crate::runtime::interpreter::gci_i11jd_witness::filter_from_env(),"
    ));
}

/// An override ABOVE the entered body is not one below it.
#[test]
fn gci_i11jd_override_above_the_body_does_not_count() {
    let chain = [link("C", false), link("B", true), link("A", true)];
    assert_eq!(overrider_below(&chain, "B"), None);
    assert_eq!(overrider_below(&chain, "A"), Some("B"));
}
