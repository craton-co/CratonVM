#[allow(unused_imports)]
use super::super::*;

/// Register core JDK natives plus the application packs selected once by the
/// VM's indexed classpath probe.
pub fn register_essential_natives_with_shims(
    registry: &mut NativeMethodRegistry,
    shim_selection: app_shims::ShimSelection,
) {
    // ModifiedClassPathClassLoader can legitimately materialize a second
    // Spring-core namespace. Spring's package-private Adapt.isIn helper is
    // only an option-name membership test, but its bytecode uses reference
    // identity and can receive option instances crossing that namespace at a
    // parent/default-interface boundary. Compare the stable enum names here so
    // CLASS_TO_STRING continues to request Class[] -> String[] adaptation.
    //
    // `SyntheticStub`, stated. This was the ONE registration in the whole boot
    // that ran on the registry's constructor default — it is the first
    // `register` call the VM makes, before any `set_category` anywhere — so it
    // got the right kind for the wrong reason, which is exactly the state
    // `no_registration_runs_on_the_ambient_default` exists to end. The kind is
    // right on its own terms too: `Adapt.isIn` is ordinary Spring bytecode that
    // this VM shadows to work around a loader-identity problem, not a boundary
    // anything must cross, so `--real-jdk` should and does refuse it.
    crate::app_shims::register_app_workarounds(registry);
    register_gc_root_providers();
    registry.register(
        "org/springframework/core/annotation/MergedAnnotation$Adapt",
        "isIn",
        "([Lorg/springframework/core/annotation/MergedAnnotation$Adapt;)Z",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(object))) => *object,
                _ => return Ok(Some(Value::Int(0))),
            };
            let this_name = match ctx.get_field_by_name(this, "name") {
                Value::Object(Some(name)) => ctx.read_string(name).unwrap_or_default(),
                _ => return Ok(Some(Value::Int(0))),
            };
            let options = match args.get(1) {
                Some(Value::Object(Some(array))) => *array,
                _ => return Ok(Some(Value::Int(0))),
            };
            for index in 0..ctx.array_length(options) {
                let Value::Object(Some(option)) = ctx.get_array_element(options, index) else {
                    continue;
                };
                if matches!(ctx.get_field_by_name(option, "name"), Value::Object(Some(name)) if ctx.read_string(name).as_deref() == Some(this_name.as_str())) {
                    return Ok(Some(Value::Int(1)));
                }
            }
            Ok(Some(Value::Int(0)))
        },
        cratonvm_native_api::NativeKind::SyntheticStub,
    );
    let before = registry.len();
    // These are the ACC_NATIVE methods with no bytecode — they ARE the real
    // behavior, so tag the whole block `Bridge` for the census/differential
    // tooling. The prior category is saved and restored at the end so a caller
    // that wrapped this in its own category is not clobbered. (Sub-clusters
    // that are really fast-paths, e.g. biginteger_intrinsics, can be promoted
    // to `Intrinsic` individually later.)
    super::register_essential_natives_with_shims_a(registry, shim_selection);
    super::register_essential_natives_with_shims_b(registry, shim_selection);
    super::register_essential_natives_with_shims_c(registry, shim_selection);
    super::register_essential_natives_with_shims_d(registry, shim_selection);
    super::register_essential_natives_with_shims_e(registry, shim_selection);
    let after = registry.len();
    tracing::info!(count = after - before, "Registered essential natives");
}

