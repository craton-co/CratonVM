use super::{lambda_arg_cast_verdict, lambda_arg_provably_not_instance, ObjectRef, SharedVm};

const NEVER_DEFINED: &str = "Lcratonvm/test/L6NeverDefinedCastTarget;";

fn vm_with_object() -> Option<(SharedVm, ObjectRef)> {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let object = shared
        .classes
        .class_manager
        .write()
        .load_class("java/lang/Object")
        .ok()?;
    let obj = shared.mem.heap.alloc_object(object, 0);
    Some((shared, obj))
}

/// The JIT direct arm's form must never load: a target nobody has loaded
/// is undecided (the arm declines), and is still unloaded afterwards.
#[test]
fn the_safepoint_free_verdict_is_undecided_on_an_unloaded_target_and_loads_nothing() {
    let Some((shared, obj)) = vm_with_object() else {
        return; // Stripped VM (no class library on the test classpath).
    };
    assert_eq!(
        lambda_arg_cast_verdict(&shared, &|| obj, NEVER_DEFINED, false),
        None
    );
    assert!(shared
        .classes
        .class_manager
        .read()
        .get_loaded_class_id("cratonvm/test/L6NeverDefinedCastTarget")
        .is_none());
    // The loading form resolves it, fails, and fails open.
    assert_eq!(
        lambda_arg_cast_verdict(&shared, &|| obj, NEVER_DEFINED, true),
        Some(false)
    );
    assert!(!lambda_arg_provably_not_instance(
        &shared,
        &|| obj,
        NEVER_DEFINED
    ));
}

/// The decided shapes answer the same in both forms.
#[test]
fn decided_shapes_agree_across_both_forms() {
    let Some((shared, obj)) = vm_with_object() else {
        return;
    };
    for may_load in [false, true] {
        // Every reference is an `Object`.
        assert_eq!(
            lambda_arg_cast_verdict(&shared, &|| obj, "Ljava/lang/Object;", may_load),
            Some(false)
        );
        // A non-array against an array type fails open.
        assert_eq!(
            lambda_arg_cast_verdict(&shared, &|| obj, "[Ljava/lang/String;", may_load),
            Some(false)
        );
        // A non-class token fails open.
        assert_eq!(
            lambda_arg_cast_verdict(&shared, &|| obj, "I", may_load),
            Some(false)
        );
    }
}

/// Structural: the JIT arm's registers are raw addresses nothing remaps,
/// so it must ask the non-loading form, never the loading one.
#[test]
fn the_jit_site_checkcast_uses_the_non_loading_form() {
    let src = include_str!("../lambda.rs");
    let start = src
        .find("pub(crate) fn lambda_jit_site_checkcasts_pass(")
        .unwrap_or(0);
    let rest = &src[start..];
    let end = rest
        .find("pub(crate) fn lambda_jit_site_capture_args(")
        .unwrap_or(0);
    assert!(start > 0 && end > 0, "the scanned function moved");
    let body = &rest[..end];
    assert!(body.contains("lambda_arg_cast_verdict_in_host(shared, &|| obj, inst_tok, false, host_loader)"));
    assert!(!body.contains("lambda_arg_provably_not_instance("));
}
