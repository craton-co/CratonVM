/// gcg g3/o (`gcg-g1o-embedded-vms-never-preallocate-the-oome`): both
/// creation paths share `bootstrap`, and `bootstrap` ends with the
/// launcher's `ensure_singleton_oom`, after the level reaches 4.
#[test]
fn gcg_g3o_bootstrap_preallocates_the_oome_after_level_four() {
    let src = include_str!("lib.rs");
    let body = src
        .split("fn bootstrap(vm: &mut Vm)")
        .nth(1)
        // `\u{7d}`: a closing brace, so brace-counting scanners see this
        // line balanced.
        .and_then(|rest| rest.split("\n\u{7d}\n").next())
        .expect("bootstrap body");
    let level4 = body.find("set_init_level(4)").expect("level 4");
    let setup = body.find("finish_oome_setup(vm)").expect("the OOME setup");
    assert!(
        level4 < setup,
        "the OOME is preallocated once the VM is booted"
    );
    let setup_body = src
        .split("fn finish_oome_setup(vm: &mut Vm)")
        .nth(1)
        .and_then(|rest| rest.split("\n\u{7d}\n").next())
        .expect("finish_oome_setup body");
    assert!(
        setup_body.contains("ensure_singleton_oom(&vm.shared, &mut vm.main_thread)"),
        "finish_oome_setup makes the launcher's call"
    );
    assert_eq!(
        src.matches("\n        if let Err(err) = bootstrap(&mut vm) {\n")
            .count(),
        2,
        "JNI_CreateJavaVM and cratonvm_create both boot through bootstrap"
    );
}
