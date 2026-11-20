use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

fn at(addr: usize) -> ObjectRef {
    // SAFETY: never dereferenced; the mock hashes the address and the
    // lock-key registry compares it.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

/// Forgets exactly the test's own VM's rows and keys, even on a failure.
struct Teardown(usize);
impl Drop for Teardown {
    fn drop(&mut self) {
        crate::forget_vm_lock_keys(self.0);
        forget_vm_type_parameter_keys(self.0, &[]);
    }
}

fn tv_of(ctx: &mut dyn NativeContext, decl: ObjectRef, name: &str) -> Option<ObjectRef> {
    match cached_building_type_parameter(ctx, decl, name) {
        Some(Value::Object(Some(tv))) => Some(tv),
        _ => None,
    }
}

/// What the `type-variables` root source is handed for `vm`, sorted.
fn scanned(vm: usize) -> Vec<(Option<u32>, usize)> {
    let mut out = Vec::new();
    gc_scan_type_parameter_roots(vm, &mut |owner, tv| out.push((owner, tv.as_ptr() as usize)));
    out.sort_unstable();
    out
}

fn rows_of(vm: usize) -> usize {
    type_parameter_build_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&vm)
        .map_or(0, |rows| rows.len())
}

/// Two live declarations of ONE VM with one identity hash keep their own
/// `T`; the placeholder mark of one is not the other's.
#[test]
fn same_hash_declarations_keep_their_own_type_variables() {
    const VM: usize = 0x29E0_1501;
    let _t = Teardown(VM);
    let mut ctx = crate::test_utils::mock_ctx();
    ctx.set_vm_identity(VM);
    let (decl_a, decl_b) = (at(0x1_29E0_1500), at(0x2_29E0_1500));
    assert_eq!(
        ctx.identity_hash_code(decl_a),
        ctx.identity_hash_code(decl_b)
    );
    let (tv_a, tv_b) = (ctx.fresh_object_ref(), ctx.fresh_object_ref());

    cache_type_parameter_row(&mut ctx, decl_a, "T", tv_a, None);
    mark_placeholder_type_parameter(&mut ctx, decl_a, "T");
    assert_eq!(tv_of(&mut ctx, decl_b, "T"), None, "B has no T yet");
    assert!(!is_placeholder_type_parameter(&mut ctx, decl_b, "T"));

    cache_type_parameter_row(&mut ctx, decl_b, "T", tv_b, None);
    assert_eq!(tv_of(&mut ctx, decl_a, "T"), Some(tv_a));
    assert_eq!(tv_of(&mut ctx, decl_b, "T"), Some(tv_b));
    assert!(is_placeholder_type_parameter(&mut ctx, decl_a, "T"));
    assert!(!is_placeholder_type_parameter(&mut ctx, decl_b, "T"));

    // Re-caching the same `TypeVariable` (the placeholder patch) replaces
    // the row in place and takes no JNI global root (gc-common w30-b).
    let roots = ctx.global_root_count();
    cache_type_parameter_row(&mut ctx, decl_a, "T", tv_a, None);
    clear_placeholder_type_parameter(&mut ctx, decl_a, "T");
    assert_eq!(ctx.global_root_count(), roots);
    assert_eq!(rows_of(VM), 2);
    assert!(!is_placeholder_type_parameter(&mut ctx, decl_a, "T"));
    assert_eq!(tv_of(&mut ctx, decl_a, "T"), Some(tv_a));
}

/// The lock-key sweep frees a dead declaration's key: its rows and marks
/// go, and its `TypeVariable` is no longer reported to the collector. The
/// live collider keeps its row.
#[test]
fn a_dead_declarations_rows_go_and_its_root_is_released() {
    const VM: usize = 0x29E0_1502;
    let _t = Teardown(VM);
    let mut ctx = crate::test_utils::mock_ctx();
    ctx.set_vm_identity(VM);
    let (dead, live) = (at(0x1_29E0_1508), at(0x2_29E0_1508));
    let (tv_dead, tv_live) = (ctx.fresh_object_ref(), ctx.fresh_object_ref());
    cache_type_parameter_row(&mut ctx, dead, "E", tv_dead, Some(0x30B0));
    mark_placeholder_type_parameter(&mut ctx, dead, "E");
    cache_type_parameter_row(&mut ctx, live, "E", tv_live, Some(0x30B0));
    assert_eq!(scanned(VM).len(), 2);

    let live_addr = live.as_ptr() as usize;
    crate::gc_sweep_lock_keys(VM, &|x| x == live_addr);
    assert_eq!(tv_of(&mut ctx, dead, "E"), None);
    assert!(!is_placeholder_type_parameter(&mut ctx, dead, "E"));
    assert_eq!(tv_of(&mut ctx, live, "E"), Some(tv_live));
    assert_eq!(
        scanned(VM),
        vec![(Some(0x30B0), tv_live.as_ptr() as usize)],
        "the dead declaration's TypeVariable is no longer a root"
    );
}

/// gc-common w30-b: a row whose owner class unloaded (its loader died) is
/// dropped by the unload hint with its placeholder mark, and is no longer
/// reported; a row of another class, and another VM's row of the same
/// class id, stay.
#[test]
fn a_dead_loaders_rows_go_with_the_unload_hint() {
    const VM: usize = 0x30B0_0001;
    const OTHER_VM: usize = 0x30B0_0002;
    let _t = Teardown(VM);
    let _o = Teardown(OTHER_VM);
    let mut ctx = crate::test_utils::mock_ctx();
    ctx.set_vm_identity(VM);
    let (gone_decl, kept_decl) = (at(0x30B0_0010), at(0x30B0_0018));
    let (tv_gone, tv_kept) = (ctx.fresh_object_ref(), ctx.fresh_object_ref());
    cache_type_parameter_row(&mut ctx, gone_decl, "K", tv_gone, Some(0x30B1));
    mark_placeholder_type_parameter(&mut ctx, gone_decl, "K");
    cache_type_parameter_row(&mut ctx, kept_decl, "K", tv_kept, Some(0x30B2));
    let mut other = crate::test_utils::mock_ctx();
    other.set_vm_identity(OTHER_VM);
    let (other_decl, tv_other) = (at(0x30B0_0020), other.fresh_object_ref());
    cache_type_parameter_row(&mut other, other_decl, "K", tv_other, Some(0x30B1));

    assert_eq!(forget_unloaded_type_parameter_rows(VM, &[0x30B1]), 1);
    assert_eq!(tv_of(&mut ctx, gone_decl, "K"), None);
    assert!(!is_placeholder_type_parameter(&mut ctx, gone_decl, "K"));
    assert_eq!(tv_of(&mut ctx, kept_decl, "K"), Some(tv_kept));
    assert_eq!(scanned(VM), vec![(Some(0x30B2), tv_kept.as_ptr() as usize)]);
    assert_eq!(
        scanned(OTHER_VM),
        vec![(Some(0x30B1), tv_other.as_ptr() as usize)]
    );
    assert_eq!(forget_unloaded_type_parameter_rows(VM, &[]), 0);
}

/// gc-common w30-b: a live row keeps its value across a relocation -- the
/// remap re-addresses the `TypeVariable`, the lookup answers the new
/// address and the scan reports it. Another VM's row is not touched by
/// this VM's pointer map.
#[test]
fn a_live_row_keeps_its_value_across_a_relocation() {
    const VM: usize = 0x30B0_0003;
    const OTHER_VM: usize = 0x30B0_0004;
    let _t = Teardown(VM);
    let _o = Teardown(OTHER_VM);
    let mut ctx = crate::test_utils::mock_ctx();
    ctx.set_vm_identity(VM);
    let decl = at(0x30B0_0030);
    let tv = at(0x30B0_0038);
    cache_type_parameter_row(&mut ctx, decl, "V", tv, None);
    let mut other = crate::test_utils::mock_ctx();
    other.set_vm_identity(OTHER_VM);
    cache_type_parameter_row(&mut other, at(0x30B0_0040), "V", tv, None);

    let moved_to = 0x1_30B0_0038usize;
    let mut map = cratonvm_types::PointerMap::new();
    map.insert(tv.as_ptr() as usize, moved_to);
    gc_update_type_parameter_refs(VM, &map);
    assert_eq!(tv_of(&mut ctx, decl, "V"), Some(at(moved_to)));
    assert_eq!(scanned(VM), vec![(None, moved_to)]);
    assert_eq!(scanned(OTHER_VM), vec![(None, tv.as_ptr() as usize)]);
    // An empty map is a no-op.
    gc_update_type_parameter_refs(VM, &cratonvm_types::PointerMap::new());
    assert_eq!(scanned(VM), vec![(None, moved_to)]);
}

/// gc-common w30-b: the owner of a `Class` declaration is its class, of a
/// `Method` its declaring class (`clazz`); an object that is neither a
/// resolvable mirror nor an executable has none, so its rows are rooted
/// outright (as bootstrap-loader rows are, by `defer_or_root`).
#[test]
fn the_owner_is_the_declarations_class() {
    const VM: usize = 0x30B0_0005;
    let _t = Teardown(VM);
    let mut ctx = crate::test_utils::mock_ctx();
    ctx.set_vm_identity(VM);
    let mirror = ctx.get_class_mirror(ClassId::new(0x30B5));
    assert_eq!(type_parameter_owner(&ctx, mirror), Some(0x30B5));

    let method = match ctx.new_object("java/lang/reflect/Method") {
        Ok(Some(Value::Object(Some(m)))) => m,
        other => panic!("mock new_object: {other:?}"),
    };
    ctx.set_field(method, 0, Value::Object(Some(mirror)));
    assert_eq!(type_parameter_owner(&ctx, method), Some(0x30B5));

    let clazzless = match ctx.new_object("java/lang/reflect/Constructor") {
        Ok(Some(Value::Object(Some(c)))) => c,
        other => panic!("mock new_object: {other:?}"),
    };
    assert_eq!(type_parameter_owner(&ctx, clazzless), None);

    // The public write path records that owner.
    let tv = ctx.fresh_object_ref();
    cache_building_type_parameter(&mut ctx, method, "M", tv);
    assert_eq!(scanned(VM), vec![(Some(0x30B5), tv.as_ptr() as usize)]);
}
