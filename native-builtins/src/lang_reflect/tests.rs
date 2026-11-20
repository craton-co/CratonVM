use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// Interpreter round i6 wave 116, lane L3: a hidden class keeps its
/// `/0x<hex>` tail in `Method.toString`, as `Class.getName()` spells it.
#[test]
fn i116_l3_type_names_keep_a_hidden_class_tail() {
    assert_eq!(
        class_name_to_type_name("p/Host$$Lambda/0x0000020001001400"),
        "p.Host$$Lambda/0x0000020001001400"
    );
    assert_eq!(
        class_name_to_type_name("[[Lp/H/0x1f;"),
        "p.H/0x1f[][]"
    );
    assert_eq!(class_name_to_type_name("java/lang/String"), "java.lang.String");
    assert_eq!(class_name_to_type_name("p/0xnot/Hex"), "p.0xnot.Hex");
    assert_eq!(class_name_to_type_name("int"), "int");
}

/// Smoke: registry exposes the WP2.1 surface without panicking and
/// without duplicate-method warnings.
#[test]
fn register_wp2_1_natives_smoke() {
    let mut registry = NativeMethodRegistry::new();
    register_wp2_1_natives(&mut registry);
    // Registry should have at least all the new methods we added (~20).
    assert!(
        registry.len() >= 20,
        "expected at least 20 WP2.1 natives, got {}",
        registry.len(),
    );
}

/// Interpreter round i3 wave 79, lane L3: the nested `Method.invoke`
/// guard measures the stack left, not a count of 100.
#[test]
fn i79_reflective_guard_trips_only_near_the_stack_end() {
    let (low, high) = (0x1000_0000usize, 0x1000_0000 + (8 << 20));
    let floor = reflective_stack_floor(low, high);
    assert_eq!(floor, low + (1 << 20), "an 8 MiB stack keeps 1 MiB");
    assert!(!reflective_stack_exhausted_at(high - 4096, low, high));
    assert!(!reflective_stack_exhausted_at(floor + 1, low, high));
    assert!(reflective_stack_exhausted_at(floor, low, high));
    assert!(reflective_stack_exhausted_at(low + 16, low, high));
    // A small stack keeps a quarter of itself.
    let small_high = low + (2 << 20);
    assert_eq!(reflective_stack_floor(low, small_high), low + (512 << 10));
    // Unknown bounds, or a frame outside the cached band, never trip.
    assert!(!reflective_stack_exhausted_at(low + 16, 0, 0));
    assert!(!reflective_stack_exhausted_at(low - 4096, low, high));
    assert!(!reflective_stack_exhausted_at(high + 4096, low, high));
    // The last-resort count is far above the old 100.
    assert!(REFLECTIVE_INVOKE_DEPTH_CEILING > 1000);
    // The live probe on this test thread is not near its end.
    assert!(!reflective_native_stack_exhausted());
}

/// JIT round 21 wave 8, lane indy (page
/// `r21-hidden-class-own-private-ctor-access-refused-20261019`): JDK 25's
/// `StringConcatFactory.InlineHiddenClassStrategy` spins
/// `java/lang/String$$StringConcat` as a hidden class from `String`'s
/// lookup and asks `findConstructor` for its PACKAGE-PRIVATE `<init>`. The
/// hidden class is in `java/lang` (JEP 371), so the access holds; the old
/// last-`/` split put it in `java/lang/String$$StringConcat` and refused.
#[test]
fn r21_hidden_class_package_private_member_reachable_from_its_lookup_package() {
    let mut ctx = crate::test_utils::mock_ctx();
    let caller = ctx
        .ensure_class_initialized("java/lang/String")
        .expect("caller class");
    let declaring = ctx
        .ensure_class_initialized("java/lang/String$$StringConcat/0x0000000000000000")
        .expect("hidden class");
    ctx.set_class_hidden(declaring);
    // Package-private (no ACC_PUBLIC / PRIVATE / PROTECTED).
    assert!(caller_may_access_member(&mut ctx, caller, declaring, 0, None));
    // Private stays nest-scoped: not a NESTMATE-defined hidden class here.
    assert!(!caller_may_access_member(
        &mut ctx, caller, declaring, 0x0002, None
    ));
    // The falsifier: a NON-hidden class of that spelling really is in the
    // package `java/lang/String$$StringConcat`, and stays refused.
    let plain = ctx
        .ensure_class_initialized("java/lang/String$$StringConcat/0x1")
        .expect("plain class");
    assert!(!caller_may_access_member(&mut ctx, caller, plain, 0, None));
}

#[test]
fn r21_hidden_class_names_split_and_spell_like_hotspot() {
    assert_eq!(
        hidden_class_file_name("java/lang/String$$StringConcat/0x0000000000000000"),
        "java/lang/String$$StringConcat"
    );
    assert_eq!(hidden_class_file_name("p/Host$$Lambda/0x1f"), "p/Host$$Lambda");
    assert_eq!(hidden_class_file_name("p/Host/0xzz"), "p/Host/0xzz");
    assert_eq!(hidden_class_file_name("p/Host"), "p/Host");
    assert_eq!(
        external_name_of_maybe_hidden("java/lang/String$$StringConcat/0x0", true),
        "java.lang.String$$StringConcat/0x0"
    );
    assert_eq!(external_name_of_maybe_hidden("p/Host/0x0", false), "p.Host.0x0");
}

/// JIT round 21 wave 13 (review rev-native item 5): the hidden-package
/// switch and the `Lookup.in` relation switch are read once per VM (the
/// door caches), never from the flag table per call, and the runtime
/// package asks the name's shape, then the hidden bit, then the switch.
#[test]
fn r21_w13_relation_switches_are_read_once_per_vm() {
    let reflect = include_str!("../lang_reflect.rs").replace("\r\n", "\n");
    let body = &reflect[reflect
        .find("\npub fn runtime_package_of(")
        .expect("runtime_package_of")..];
    let body = &body[..body.find("\n}\n").expect("its end")];
    let shape = body.find("hidden_class_file_name(&name)").expect("the shape");
    let hidden = body.find("ctx.is_class_hidden(class_id)").expect("the hidden bit");
    let switch = body.find("hidden_runtime_package_enabled(ctx)").expect("the switch");
    assert!(shape < hidden && hidden < switch);
    let reader = &reflect[reflect
        .find("\nfn hidden_runtime_package_enabled(")
        .expect("the reader")..];
    let reader = &reader[..reader.find("\n}\n").expect("its end")];
    assert!(reader.contains("reflect_hidden_runtime_package_per_vm(ctx)"));
    let loader = concat!(include_str!("../classloader/mod.rs"), "\n", include_str!("../classloader/closed_url_classloader_ids.rs"), "\n", include_str!("../classloader/load_class_override_memo_key.rs"), "\n", include_str!("../classloader/adopt_add_class_orphan_by_id.rs"), "\n", include_str!("../classloader/builtin_classloader_find_resource_as.rs"), "\n", include_str!("../classloader/ucl_try_define_local_class.rs")).replace("\r\n", "\n");
    let lk = &loader[loader
        .find("\npub fn lk_in_package_member_enabled(")
        .expect("the Lookup.in reader")..];
    let lk = &lk[..lk.find("\n}\n").expect("its end")];
    assert!(lk.contains("lookup_in_package_member_per_vm(ctx)"));
    assert!(!lk.contains("runtime_flag_default_on"));
    // The mock context has no cache slot: the flag table answers (default on).
    let ctx = crate::test_utils::mock_ctx();
    assert!(crate::lang_class::reflect_hidden_runtime_package_per_vm(&ctx));
}
