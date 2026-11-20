use super::*;


/// Source pin: the take runs in the `initPhase2` slot
/// (`boot_module_system`), through the accessor `Module.ensureNativeAccess`
/// calls.
#[test]
fn zt14x11_the_take_runs_in_the_init_phase2_slot() {
    let src = concat!(include_str!("../vm_init/mod.rs"), "\n", include_str!("../vm_init/missing_native_entry.rs"), "\n", include_str!("../vm_init/real_jdk_to_array_typed.rs"), "\n", include_str!("../vm_init/zgc_relocation_permitted.rs"), "\n", include_str!("../vm_init/wait_site.rs"), "\n", include_str!("../vm_init/class_store_cut_with.rs")).replace("\r\n", "\n");
    let start = src
        .find("    pub fn boot_module_system(")
        .expect("boot_module_system");
    let end = start + src[start..].find("\n    }\n").expect("its end");
    assert!(src[start..end].contains("self.boot_take_illegal_native_access();"));
    // Split so this test's own text never matches first (it sits above
    // the function in the file).
    let take = src
        .find(concat!(
            "    pub(in crate::vm::vm_init) fn boot_take_",
            "illegal_native_access(&mut self)"
        ))
        .expect("the take");
    let body = &src[take..take + src[take..].find("\n    }\n").expect("its end")];
    assert!(body.contains("\"illegalNativeAccess\""));
    assert!(body.contains("()Ljdk/internal/module/ModuleBootstrap$IllegalNativeAccess;"));
}
