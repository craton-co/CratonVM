#[test]
fn saved_props_are_published_after_vm_is_initialised() {
    let src = include_str!("lib_parts/essential_dur_parse.rs").replace("\r\n", "\n");
    let start = src
        .find("pub fn compat_fast_boot_init_phase1(")
        .expect("compat_fast_boot_init_phase1");
    let body = &src[start..start + src[start..].find("\n}\n").expect("end of fn")];
    let streams = body
        .find("lang_system::native_system_init_phase1(")
        .expect("the streams body runs");
    let init = body
        .find(concat!("ensure_class_initialized(\"jdk/internal/misc/", "VM\")"))
        .expect("VM is initialised before the publish");
    let publish = body
        .find(concat!("publish_vm_saved_props", "(ctx)"))
        .expect("savedProps is published");
    assert!(streams < publish, "published after the streams body, as initPhase1 orders it");
    assert!(init < publish, "VM is initialised before its static is written");
}
