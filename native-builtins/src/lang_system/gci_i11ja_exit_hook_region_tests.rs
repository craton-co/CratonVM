use super::invoke_pre_exit_hook_from;
use crate::test_utils::MockNativeContext;


/// zt t12/tg5 (`zgc-zt11tg4-*` item 4, `CRATONVM_ZGC_TICK_EXIT_NATIVE_WAITS`):
/// the exit natives' helper asks its context to stop the ZGC director
/// (and wait for an in-flight tick pause) once, BEFORE the pre-exit hook
/// and outside its region, on both arms of `CRATONVM_EXIT_HOOK_GC_SAFE`;
/// the `=0` kill asks nothing.


/// Source pin: all three exit natives call the hook through the helper.
#[test]
fn gci_i11ja_every_exit_native_uses_the_helper() {
    let src = concat!(include_str!("../lang_system/mod.rs"), "\n", include_str!("../lang_system/shutdown_state.rs"), "\n", include_str!("../lang_system/runtime_version_publish.rs"), "\n", include_str!("../lang_system/note_supertype_initiation.rs")).replace("\r\n", "\n");
    for f in [
        "pub fn native_system_exit(",
        "pub fn native_runtime_exit(",
        "pub fn native_shutdown_halt0(",
    ] {
        let start = src.find(f).unwrap_or_else(|| panic!("{f}"));
        let rest = &src[start..];
        let body = &rest[..rest.find("\n}\n").expect("its end")];
        assert!(body.contains("invoke_pre_exit_hook_from(ctx, code);"), "{f}");
        assert!(!body.contains("    invoke_pre_exit_hook(code);"), "{f}");
    }
}
