use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

/// Drops the test's VM rows even when an assertion fails.
struct Teardown(&'static [usize]);
impl Drop for Teardown {
    fn drop(&mut self) {
        for &vm in self.0 {
            forget_vm_undertow_rows(vm);
        }
    }
}

fn mock_on(vm: usize) -> MockNativeContext {
    let ctx = crate::test_utils::mock_ctx();
    ctx.set_vm_identity(vm);
    ctx
}

/// An initialised `HeaderMap` with one header, and its id.
fn header_map(ctx: &mut MockNativeContext) -> (ObjectRef, u64) {
    let hm = try_alloc_concurrent_synthetic(ctx, CLS_HEADER_MAP, HM_NUM_SLOTS).unwrap();
    native_header_map_init(ctx, &[Value::Object(Some(hm))]).unwrap();
    let id = header_map_id_of(&*ctx, hm);
    header_map_put(id, "X-W20g", "yes").unwrap();
    (hm, id)
}

fn has_obj_row(ctx: &MockNativeContext, hm: ObjectRef) -> bool {
    header_map_obj_registry()
        .lock()
        .unwrap()
        .contains_key(&header_map_obj_key(ctx, hm))
}

/// A dead map's object row and header row go together at the sweep; a
/// moved map is re-addressed and kept until it dies at its new address.
#[test]
fn w20g_undertow_dead_header_map_rows_are_swept() {
    const VM: usize = 0x20D0_0A01;
    const MOVED: usize = 0x7720_D0A0;
    let _teardown = Teardown(&[VM]);
    let mut ctx = mock_on(VM);
    let (hm, id) = header_map(&mut ctx);
    assert_eq!(header_map_get(id, "x-w20g").as_deref(), Some("yes"));

    let mut moved = cratonvm_types::PointerMap::default();
    moved.insert(hm.as_ptr() as usize, MOVED);
    assert_eq!(
        gc_sweep_undertow_rows(VM, &moved, &|_| false),
        0,
        "a moved map is re-addressed, never judged"
    );
    assert!(has_obj_row(&ctx, hm));
    let nothing_moved = cratonvm_types::PointerMap::default();
    assert_eq!(
        gc_sweep_undertow_rows(VM, &nothing_moved, &|a| a != MOVED),
        1
    );
    assert!(!has_obj_row(&ctx, hm), "the object row went with its map");
    assert_eq!(
        header_map_get(id, "x-w20g"),
        None,
        "and so did its header entries"
    );
}

/// A live map keeps both rows; another VM's sweep never judges them.
#[test]
fn w20g_undertow_live_and_other_vm_rows_survive_the_sweep() {
    const VM: usize = 0x20D0_0A02;
    const OTHER: usize = 0x20D0_0B02;
    let _teardown = Teardown(&[VM, OTHER]);
    let mut ctx = mock_on(VM);
    let (hm, id) = header_map(&mut ctx);
    let nothing_moved = cratonvm_types::PointerMap::default();
    let hm_addr = hm.as_ptr() as usize;
    assert_eq!(
        gc_sweep_undertow_rows(VM, &nothing_moved, &|a| a == hm_addr),
        0
    );
    assert_eq!(gc_sweep_undertow_rows(OTHER, &nothing_moved, &|_| false), 0);
    assert!(has_obj_row(&ctx, hm));
    assert_eq!(header_map_get(id, "x-w20g").as_deref(), Some("yes"));
}

/// `forget_vm_undertow_rows` drops only its VM's rows.
#[test]
fn w20g_undertow_forget_vm_drops_only_its_rows() {
    const VM_A: usize = 0x20D0_0A03;
    const VM_B: usize = 0x20D0_0B03;
    let _teardown = Teardown(&[VM_A, VM_B]);
    let mut a = mock_on(VM_A);
    let mut b = mock_on(VM_B);
    let (hm_a, id_a) = header_map(&mut a);
    let (hm_b, id_b) = header_map(&mut b);
    let builder_b = match native_undertow_builder(&mut b, &[]).unwrap() {
        Some(Value::Object(Some(o))) => o,
        other => panic!("expected a builder, got {other:?}"),
    };

    forget_vm_undertow_rows(VM_A);
    assert!(!has_obj_row(&a, hm_a));
    assert_eq!(header_map_get(id_a, "x-w20g"), None);
    assert!(has_obj_row(&b, hm_b));
    assert_eq!(header_map_get(id_b, "x-w20g").as_deref(), Some("yes"));
    assert!(undertow_builder_configs()
        .lock()
        .unwrap()
        .contains_key(&undertow_obj_key(&b, builder_b)));
}

/// A dead builder's configuration row goes at the sweep.
#[test]
fn w20g_undertow_dead_builder_config_is_swept() {
    const VM: usize = 0x20D0_0A04;
    let _teardown = Teardown(&[VM]);
    let mut ctx = mock_on(VM);
    let builder = match native_undertow_builder(&mut ctx, &[]).unwrap() {
        Some(Value::Object(Some(o))) => o,
        other => panic!("expected a builder, got {other:?}"),
    };
    let key = undertow_obj_key(&ctx, builder);
    let nothing_moved = cratonvm_types::PointerMap::default();
    assert_eq!(gc_sweep_undertow_rows(VM, &nothing_moved, &|_| false), 1);
    assert!(!undertow_builder_configs()
        .lock()
        .unwrap()
        .contains_key(&key));
}

/// Re-filing a map's identity row frees the id it replaced.
#[test]
fn w20g_undertow_replaced_object_row_frees_its_old_id() {
    const VM: usize = 0x20D0_0A05;
    let _teardown = Teardown(&[VM]);
    let mut ctx = mock_on(VM);
    let (hm, old_id) = header_map(&mut ctx);
    let new_id = alloc_header_map_id(VM);
    remember_header_map_obj(&ctx, hm, new_id);
    assert_eq!(header_map_id_of(&ctx, hm), new_id);
    assert_eq!(header_map_get(old_id, "x-w20g"), None, "old id freed");
}

/// gc-common w29-d (`common-w28b-remaining-identity-hash-keyed-side-tables`,
/// rank 10): two LIVE `HeaderMap`s with one identity hash (the mock's hash
/// is the address truncated to `i32`) keep their own rows. The second
/// map's `<init>` used to replace the first's row and FREE its headers.
#[test]
fn w29d_same_hash_header_maps_keep_their_own_headers() {
    const VM: usize = 0x29D0_0A06;
    let _teardown = Teardown(&[VM]);
    let ctx = mock_on(VM);
    // SAFETY: keys only, 8-byte aligned, never dereferenced here.
    let a = unsafe { ObjectRef::from_raw(0x1_29D0_0A00usize as *mut u8) };
    let b = unsafe { ObjectRef::from_raw(0x2_29D0_0A00usize as *mut u8) };
    assert_eq!(
        ctx.identity_hash_code(a),
        ctx.identity_hash_code(b),
        "premise: the two maps share an identity hash"
    );
    let (id_a, id_b) = (alloc_header_map_id(VM), alloc_header_map_id(VM));
    header_map_put(id_a, "Authorization", "a-secret").unwrap();
    remember_header_map_obj(&ctx, a, id_a);
    remember_header_map_obj(&ctx, b, id_b);
    assert_eq!(header_map_id_of(&ctx, a), id_a);
    assert_eq!(header_map_id_of(&ctx, b), id_b);
    assert_eq!(
        header_map_get(id_a, "authorization").as_deref(),
        Some("a-secret"),
        "B's init must not free A's headers"
    );
    assert_eq!(
        header_map_get(id_b, "authorization"),
        None,
        "B never sees A's"
    );
    crate::forget_vm_lock_keys(VM);
}

/// The instance-id rows (no owner address, so no sweep) go with their
/// objects' lock keys.
#[test]
fn w29d_undertow_instance_rows_go_with_their_lock_keys() {
    const VM: usize = 0x29D0_0A07;
    let _teardown = Teardown(&[VM]);
    let ctx = mock_on(VM);
    let (a_addr, b_addr) = (0x1_29D0_0B00usize, 0x2_29D0_0B00usize);
    // SAFETY: keys only, 8-byte aligned, never dereferenced here.
    let a = unsafe { ObjectRef::from_raw(a_addr as *mut u8) };
    let b = unsafe { ObjectRef::from_raw(b_addr as *mut u8) };
    remember_undertow_instance(&ctx, a, 11);
    remember_undertow_instance(&ctx, b, 22);
    let ka = existing_undertow_obj_key(&ctx, a).expect("a was keyed");
    let kb = existing_undertow_obj_key(&ctx, b).expect("b was keyed");
    assert_ne!(ka, kb);
    let row = |key: UndertowObjKey| {
        undertow_obj_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .copied()
    };
    assert_eq!((row(ka), row(kb)), (Some(11), Some(22)));
    assert_eq!(undertow_instance_id_of(&ctx, a), Some(11));
    assert_eq!(undertow_instance_id_of(&ctx, b), Some(22));
    crate::gc_sweep_lock_keys(VM, &|addr| addr == b_addr);
    assert_eq!(row(ka), None, "a dead server's row goes with its key");
    assert_eq!(row(kb), Some(22));
    crate::forget_vm_lock_keys(VM);
    assert_eq!(row(kb), None, "and the rest with the VM's keys");
}
