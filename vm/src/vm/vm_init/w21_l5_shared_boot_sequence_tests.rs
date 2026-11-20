fn repo_file(rel: &str) -> String {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the vm crate sits in the workspace");
    std::fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

fn in_order(src: &str, needles: &[&str], what: &str) {
    let mut at = 0usize;
    for n in needles {
        let found = src[at..]
            .find(n)
            .unwrap_or_else(|| panic!("{what}: `{n}` missing or out of order"));
        at += found + n.len();
    }
}

#[test]
fn the_launcher_runs_the_shared_sequence() {
    let main = repo_file("vm-cli/src/main_parts/arch_label.rs");
    in_order(
        &main,
        &[
            "vm.boot_init_phase1()",
            "vm.boot_module_system(phase1)",
            "vm.shared.set_init_level(4)",
            "vm.boot_publish_jdk_booted()",
        ],
        "vm-cli run()",
    );
    assert!(
        !main.contains("jboss_jdkspecific::build_boot_layer"),
        "the launcher builds the layer only through Vm::boot_module_system"
    );
}

#[test]
fn the_embedded_boot_runs_the_same_sequence_behind_its_switch() {
    let lib = repo_file("libcratonvm/src/lib.rs");
    let start = lib
        .find("fn bootstrap(vm: &mut Vm)")
        .expect("libcratonvm bootstrap");
    let body = &lib[start..];
    in_order(
        body,
        &[
            "embed_launcher_boot_enabled()",
            "vm.boot_init_phase1()",
            "vm.boot_module_system(phase1)",
            "vm.shared.set_init_level(4)",
            "vm.boot_publish_jdk_booted()",
            "finish_oome_setup(vm)",
        ],
        "libcratonvm bootstrap",
    );
}
