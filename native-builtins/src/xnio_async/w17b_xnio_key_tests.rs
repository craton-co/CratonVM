use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn obj(addr: usize) -> ObjectRef {
    // SAFETY: a key only; nothing on these paths dereferences it.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn ctx_for(vm: usize) -> crate::test_utils::MockNativeContext {
    let c = crate::test_utils::mock_ctx();
    c.set_vm_identity(vm);
    c
}

fn one_entry_map(n: i32) -> Arc<OptionMapInner> {
    let mut entries = HashMap::new();
    entries.insert(
        OptionKey {
            declaring_class: "org/xnio/Options".to_string(),
            name: "WORKER_IO_THREADS".to_string(),
        },
        OptionValue::Int(n),
    );
    Arc::new(OptionMapInner {
        entries: Mutex::new(entries),
    })
}

/// The object-key row of `o`, as `map_handle_for` reads it once the
/// handle slot is absent.
fn map_row(ctx: &dyn NativeContext, o: ObjectRef) -> Option<i64> {
    let k = xnio_obj_key_for(ctx, o);
    registries().map_handles_by_obj.lock().get(&k).copied()
}

fn builder_row(ctx: &dyn NativeContext, o: ObjectRef) -> Option<i64> {
    let k = xnio_obj_key_for(ctx, o);
    registries().builder_handles_by_obj.lock().get(&k).copied()
}

fn teardown(vm: usize) {
    crate::forget_vm_lock_keys(vm);
    forget_vm_xnio_roots(vm);
}

/// Two VMs' OptionMaps with one identity hash keep distinct rows, and one
/// VM's teardown takes only its own row and store.
#[test]
fn two_vms_option_maps_with_one_identity_hash_keep_distinct_rows() {
    const VM_A: usize = 0xB171_0001;
    const VM_B: usize = 0xB171_0002;
    let a = ctx_for(VM_A);
    let b = ctx_for(VM_B);
    let (ma, mb) = (obj(0x1_B17A_0010), obj(0x2_B17A_0010));
    assert_eq!(a.identity_hash_code(ma), b.identity_hash_code(mb));
    let ha = register_map(VM_A, one_entry_map(3));
    remember_map_handle(&a, ma, ha);
    assert_eq!(map_row(&b, mb), None, "B read A's OptionMap handle");
    let hb = register_map(VM_B, one_entry_map(5));
    remember_map_handle(&b, mb, hb);
    assert_ne!(xnio_obj_key_for(&a, ma), xnio_obj_key_for(&b, mb));
    assert_eq!(map_row(&a, ma), Some(ha));
    assert_eq!(map_row(&b, mb), Some(hb));

    teardown(VM_A);
    assert_eq!(map_row(&b, mb), Some(hb));
    assert!(lookup_map(hb).is_some());
    assert!(lookup_map(ha).is_none());
    teardown(VM_A);
    teardown(VM_B);
    assert!(lookup_map(hb).is_none());
}

/// A dead OptionMap's and a dead Builder's rows go with their keys, and so
/// do the stores they name (whose `OptionValue::Obj` refs the
/// `xnio-futures` root row would otherwise keep alive until teardown). A
/// later object with the dead one's identity hash inherits nothing (the
/// old registry rebound the dead slot to it).
#[test]
fn a_dead_option_maps_rows_and_store_go_with_its_key() {
    const VM: usize = 0xB171_0003;
    let c = ctx_for(VM);
    let (dead_map, dead_builder) = (obj(0x1_B17B_0010), obj(0x1_B17B_0020));
    let hm = register_map(VM, one_entry_map(7));
    remember_map_handle(&c, dead_map, hm);
    let hb = register_builder(VM, Arc::new(BuilderInner::default()));
    remember_builder_handle(&c, dead_builder, hb);
    let km = xnio_obj_key_for(&c, dead_map);
    let kb = xnio_obj_key_for(&c, dead_builder);
    let dead = [dead_map.as_ptr() as usize, dead_builder.as_ptr() as usize];
    assert!(crate::gc_sweep_lock_keys(VM, &|a| !dead.contains(&a)) >= 2);
    assert!(!registries().map_handles_by_obj.lock().contains_key(&km));
    assert!(!registries().builder_handles_by_obj.lock().contains_key(&kb));
    assert!(lookup_map(hm).is_none(), "the dead map's store outlived it");
    assert!(
        lookup_builder(hb).is_none(),
        "the dead builder's store outlived it"
    );

    let reborn = obj(0x2_B17B_0010);
    assert_ne!(xnio_obj_key_for(&c, reborn), km);
    assert_eq!(map_row(&c, reborn), None);
    assert_eq!(builder_row(&c, reborn), None);
    teardown(VM);
}

/// A live OptionMap and Builder that a collection moves keep their rows
/// and stores.
#[test]
fn a_moved_option_map_keeps_its_handle() {
    const VM: usize = 0xB171_0004;
    let c = ctx_for(VM);
    let (old_m, new_m) = (0x1_B17C_0010usize, 0x3_B17C_0010usize);
    let (old_b, new_b) = (0x1_B17C_0020usize, 0x3_B17C_0020usize);
    let hm = register_map(VM, one_entry_map(9));
    remember_map_handle(&c, obj(old_m), hm);
    let hb = register_builder(VM, Arc::new(BuilderInner::default()));
    remember_builder_handle(&c, obj(old_b), hb);
    let mut moved = cratonvm_types::PointerMap::default();
    moved.insert(old_m, new_m);
    moved.insert(old_b, new_b);
    crate::gc_sweep_and_remap_lock_keys(VM, &|a| a == old_m || a == old_b, &moved);
    assert_eq!(map_row(&c, obj(new_m)), Some(hm));
    assert_eq!(builder_row(&c, obj(new_b)), Some(hb));
    crate::gc_sweep_lock_keys(VM, &|a| a == new_m || a == new_b);
    assert_eq!(map_row(&c, obj(new_m)), Some(hm));
    assert!(lookup_map(hm).is_some());
    assert!(lookup_builder(hb).is_some());
    teardown(VM);
    assert!(lookup_map(hm).is_none());
    assert!(lookup_builder(hb).is_none());
}

/// Discovery (w17-b): a dead IoFuture's and FutureResult's `futures` rows
/// go with them. They share one `Arc`, which goes with the last, so the
/// `live_futures` `Weak` stops upgrading and the `xnio-futures` root row
/// stops rooting the future's refs. The handle map used to keep every
/// future ever created.
#[test]
fn a_dead_futures_handle_rows_go_with_their_objects() {
    const VM: usize = 0xB171_0006;
    let mut c = ctx_for(VM);
    let fr = try_alloc_concurrent_synthetic(&mut c, "org/xnio/FutureResult", 1).unwrap();
    native_future_result_init(&mut c, &[Value::Object(Some(fr))]).unwrap();
    let fut = match native_future_result_get_future(&mut c, &[Value::Object(Some(fr))])
        .unwrap()
        .unwrap()
    {
        Value::Object(Some(o)) => o,
        other => panic!("expected IoFuture, got {other:?}"),
    };
    let h_fr = c.get_field(fr, FR_FUTURE_HANDLE).as_long().unwrap();
    let h_fut = c.get_field(fut, IOF_RESULT_SLOT).as_long().unwrap();
    assert_ne!(h_fr, h_fut);
    let weak = Arc::downgrade(&lookup_future(h_fr).expect("FutureResult row"));

    // The IoFuture dies first: its row goes; the FutureResult's, and the
    // shared `Arc`, stay.
    let fut_addr = fut.as_ptr() as usize;
    crate::gc_sweep_lock_keys(VM, &|a| a != fut_addr);
    assert!(
        lookup_future(h_fut).is_none(),
        "the dead IoFuture's row outlived it"
    );
    assert!(lookup_future(h_fr).is_some());
    assert!(weak.upgrade().is_some());

    // Then the FutureResult: the last handle, so the `Arc` goes too.
    let fr_addr = fr.as_ptr() as usize;
    crate::gc_sweep_lock_keys(VM, &|a| a != fr_addr);
    assert!(
        lookup_future(h_fr).is_none(),
        "the dead FutureResult's row outlived it"
    );
    assert!(
        weak.upgrade().is_none(),
        "the future outlived both its objects"
    );
    teardown(VM);
}

/// Teardown drops a row that names one of the VM's stores even when the
/// lock-key teardown did not report its key freed (a key some other
/// caller minted held).
#[test]
fn teardown_drops_rows_naming_the_vms_stores() {
    const VM: usize = 0xB171_0005;
    const HELD_KEY: usize = 0xB171_0005_0000;
    let hm = register_map(VM, one_entry_map(1));
    registries().map_handles_by_obj.lock().insert(HELD_KEY, hm);
    let hb = register_builder(VM, Arc::new(BuilderInner::default()));
    registries()
        .builder_handles_by_obj
        .lock()
        .insert(HELD_KEY, hb);
    forget_vm_xnio_roots(VM);
    assert!(!registries()
        .map_handles_by_obj
        .lock()
        .contains_key(&HELD_KEY));
    assert!(!registries()
        .builder_handles_by_obj
        .lock()
        .contains_key(&HELD_KEY));
    assert!(lookup_map(hm).is_none());
}
