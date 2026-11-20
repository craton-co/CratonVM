/// zt t10/x7: `defineClass0`'s duplicate arm runs the deferred `<clinit>`
/// (`init_after_define`) on a class it adopts from a failed `addClass`
/// upcall, after the adoption, as its success arm runs it after the
/// upcall. Before, a retry of a non-hidden `defineClass0` with `initialize`
/// (`Lookup.defineClass` itself passes `false`) answered the orphan
/// uninitialized: its own define stopped at the upcall, before the
/// initialization. A text pin: the unit-test context
/// cannot run a failing `ArrayList.add` inside a define.
#[test]
fn zt10x7_an_adopted_define0_class_is_initialized_after_the_adoption() {
    let src = concat!(include_str!("../lang_system/mod.rs"), "\n", include_str!("../lang_system/shutdown_state.rs"), "\n", include_str!("../lang_system/runtime_version_publish.rs"), "\n", include_str!("../lang_system/note_supertype_initiation.rs"));
    let start = src
        .find("pub fn native_classloader_define_class0(")
        .expect("defineClass0");
    let end = start
        + src[start..]
            .find("pub fn native_perf_attach(")
            .expect("the next native");
    let define0 = &src[start..end];
    let arm = define0
        .find("DuplicateDefine::SameLoaderObject =>")
        .expect("the duplicate arm");
    let adopt = define0[arm..]
        .find("adopt_add_class_orphan(")
        .expect("the adoption")
        + arm;
    let gate = define0[adopt..]
        .find("if !init_after_define")
        .expect("the deferred-init gate")
        + adopt;
    let init = define0[adopt..]
        .find("ctx.initialize_class(adopted_id)?")
        .expect("the deferred init")
        + adopt;
    let refused = define0[adopt..]
        .find("duplicate_define_error(")
        .expect("the LinkageError")
        + adopt;
    assert!(adopt < gate && gate < init, "adopt, then the gate, then <clinit>");
    assert!(init < refused, "inside the adoption, before the LinkageError arm");
}
