use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn at(addr: usize) -> ObjectRef {
    // SAFETY: a key only -- hashed, compared and rooted by the mock, never
    // dereferenced.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

/// Forgets exactly this test's VM (lesson pp).
struct W29aForget(usize);
impl Drop for W29aForget {
    fn drop(&mut self) {
        crate::forget_vm_lock_keys(self.0);
        forget_vm_thread_local_side_tables(self.0);
    }
}

fn get(ctx: &mut MockNativeContext, tl: ObjectRef) -> Option<Value> {
    native_tl_get(ctx, &[Value::Object(Some(tl))]).expect("get must not fail")
}

fn set(ctx: &mut MockNativeContext, tl: ObjectRef, value: Value) {
    native_tl_set(ctx, &[Value::Object(Some(tl)), value]).expect("set must not fail");
}

fn rows_in_bucket(hash: i32) -> usize {
    TL_MAP.with(|m| m.borrow().get(&hash).map_or(0, Vec::len))
}

/// File a row for `tl` holding `value` whose owner handle is a STRONG
/// mock root standing in for the weak global the VM would mint: removing
/// it is how a test says "this ThreadLocal died" (the mock has no weak
/// globals, so the natives' own rows carry none). Returns that handle.
fn put_row_with_owner(ctx: &mut MockNativeContext, tl: ObjectRef, value: ObjectRef) -> usize {
    let owner = ctx.add_global_root(tl);
    let row = TlRow {
        owner,
        owner_fallback: tl,
        value: tl_value_from_java(ctx, Value::Object(Some(value))),
    };
    let hash = ctx.identity_hash_code(tl);
    TL_MAP.with(|m| m.borrow_mut().entry(hash).or_default().push(row));
    owner
}

#[test]
fn colliding_thread_locals_keep_separate_values() {
    std::thread::spawn(|| {
        let mut ctx = MockNativeContext::new();
        ctx.set_vm_identity(0x29a0_7101);
        let (tl_a, tl_b) = (at(0x1_29a0_0a08), at(0x2_29a0_0a08));
        assert_eq!(
            ctx.identity_hash_code(tl_a),
            ctx.identity_hash_code(tl_b),
            "premise: the two ThreadLocals share one identity hash"
        );
        let before = ctx.global_root_count();
        let (va, vb, vb2) = (
            ctx.fresh_object_ref(),
            ctx.fresh_object_ref(),
            ctx.fresh_object_ref(),
        );
        set(&mut ctx, tl_a, Value::Object(Some(va)));
        set(&mut ctx, tl_b, Value::Object(Some(vb)));
        assert_eq!(
            rows_in_bucket(ctx.identity_hash_code(tl_a)),
            2,
            "one row each"
        );
        assert_eq!(get(&mut ctx, tl_a), Some(Value::Object(Some(va))));
        assert_eq!(
            get(&mut ctx, tl_b),
            Some(Value::Object(Some(vb))),
            "B.get() answered A's value"
        );
        set(&mut ctx, tl_b, Value::Object(Some(vb2)));
        assert_eq!(
            get(&mut ctx, tl_a),
            Some(Value::Object(Some(va))),
            "B.set() overwrote A's value"
        );
        assert_eq!(
            ctx.global_root_count(),
            before + 2,
            "B's replaced value is released"
        );
        native_tl_remove(&mut ctx, &[Value::Object(Some(tl_a))]).expect("remove");
        let mut dead = Vec::new();
        assert!(tl_lookup(&ctx, tl_a, &mut dead).is_none());
        assert!(dead.is_empty());
        assert_eq!(
            get(&mut ctx, tl_b),
            Some(Value::Object(Some(vb2))),
            "A.remove() removed B's value"
        );
        assert_eq!(release_current_thread_locals(&mut ctx), 1);
        assert_eq!(ctx.global_root_count(), before);
    })
    .join()
    .expect("test thread must not panic");
}

#[test]
fn a_dead_thread_locals_row_is_expunged_and_never_inherited() {
    std::thread::spawn(|| {
        let mut ctx = MockNativeContext::new();
        ctx.set_vm_identity(0x29a0_7102);
        let (tl_a, tl_b) = (at(0x1_29a0_3a08), at(0x2_29a0_3a08));
        let hash = ctx.identity_hash_code(tl_a);
        assert_eq!(
            hash,
            ctx.identity_hash_code(tl_b),
            "premise: one identity hash"
        );
        let (va, vb) = (ctx.fresh_object_ref(), ctx.fresh_object_ref());
        let before = ctx.global_root_count();
        let owner_a = put_row_with_owner(&mut ctx, tl_a, va);
        put_row_with_owner(&mut ctx, tl_b, vb);
        assert_eq!(
            ctx.global_root_count(),
            before + 4,
            "two owners, two values"
        );

        // A dies: its weak handle is cleared.
        assert!(ctx.remove_global_root(owner_a));

        // A NEW ThreadLocal with A's hash -- here even at A's old address
        // -- does not inherit A's value, and the lookup expunges A's row.
        let mut dead = Vec::new();
        assert!(
            tl_lookup(&ctx, tl_a, &mut dead).is_none(),
            "a new ThreadLocal inherited a dead one's value"
        );
        assert_eq!(dead.len(), 1, "A's row is expunged");
        tl_release_rows(&mut ctx, dead);
        assert_eq!(rows_in_bucket(hash), 1);
        assert_eq!(
            ctx.global_root_count(),
            before + 2,
            "a dead ThreadLocal's value must stop being rooted"
        );
        assert_eq!(
            get(&mut ctx, tl_b),
            Some(Value::Object(Some(vb))),
            "the survivor keeps its own value"
        );
        assert_eq!(release_current_thread_locals(&mut ctx), 1);
        assert_eq!(ctx.global_root_count(), before);
    })
    .join()
    .expect("test thread must not panic");
}

#[test]
fn an_insert_past_the_threshold_expunges_every_dead_row() {
    std::thread::spawn(|| {
        let mut ctx = MockNativeContext::new();
        ctx.set_vm_identity(0x29a0_7103);
        let before = ctx.global_root_count();
        let tls: Vec<ObjectRef> = (0..3).map(|_| ctx.fresh_object_ref()).collect();
        let mut owners = Vec::new();
        for &tl in &tls {
            let value = ctx.fresh_object_ref();
            owners.push(put_row_with_owner(&mut ctx, tl, value));
        }
        // Two of the three die; nothing touches their buckets again.
        assert!(ctx.remove_global_root(owners[0]));
        assert!(ctx.remove_global_root(owners[1]));
        // The next NEW row finds the map at the threshold.
        TL_EXPUNGE_AT.with(|at| at.set(1));
        let fresh = ctx.fresh_object_ref();
        set(&mut ctx, fresh, Value::Int(9));
        let rows: usize = TL_MAP.with(|m| m.borrow().values().map(Vec::len).sum());
        assert_eq!(rows, 2, "the survivor and the new row");
        assert_eq!(
            TL_EXPUNGE_AT.with(|at| at.get()),
            TL_EXPUNGE_FLOOR,
            "re-armed at twice the live size, floored"
        );
        // Survivor: its owner and value; the new row: a primitive.
        assert_eq!(ctx.global_root_count(), before + 2);
        assert_eq!(release_current_thread_locals(&mut ctx), 2);
        assert_eq!(ctx.global_root_count(), before);
    })
    .join()
    .expect("test thread must not panic");
}

#[test]
fn with_initial_rows_belong_to_their_thread_local_and_go_with_its_key() {
    const VM: usize = 0x29a0_7104;
    let _forget = W29aForget(VM);
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM);
    let (tl_a, tl_b) = (at(0x1_29a0_1a08), at(0x2_29a0_1a08));
    assert_eq!(
        ctx.identity_hash_code(tl_a),
        ctx.identity_hash_code(tl_b),
        "premise: one identity hash"
    );
    let supplier = ctx.fresh_object_ref();
    let before = ctx.global_root_count();
    // File A's supplier the way `withInitial` does (it allocates the
    // ThreadLocal itself, at an address the test cannot choose).
    let key = crate::gc_stable_weak_lock_key(&ctx, tl_a).expect("key");
    let root = ctx.add_global_root(supplier);
    tl_side_tables().lock().suppliers.insert(
        key,
        TlSupplierRow {
            vm: VM,
            root,
            fallback: supplier,
        },
    );
    ANY_TL_SUPPLIER.store(true, std::sync::atomic::Ordering::Release);
    assert_eq!(tl_with_initial_supplier(&ctx, tl_a), Some(supplier));
    assert_eq!(
        tl_with_initial_supplier(&ctx, tl_b),
        None,
        "a collider must not run A's supplier"
    );
    assert!(
        crate::existing_weak_lock_key(&ctx, tl_b).is_none(),
        "a read path never mints a key"
    );

    // A dies: the lock-key sweep frees its key, and the row goes with it;
    // its supplier root is released by the VM's next ThreadLocal native.
    let a_addr = tl_a.as_ptr() as usize;
    assert_eq!(
        crate::gc_sweep_lock_keys(VM, &|addr: usize| addr != a_addr),
        1
    );
    assert!(!tl_side_tables().lock().suppliers.contains_key(&key));
    tl_release_orphaned_roots(&mut ctx);
    assert_eq!(
        ctx.global_root_count(),
        before,
        "a dead ThreadLocal's supplier must stop being rooted"
    );
    assert_eq!(
        tl_with_initial_supplier(&ctx, tl_a),
        None,
        "a new ThreadLocal at A's address is another object"
    );
}

#[test]
fn a_colliding_plain_thread_local_is_not_inherited() {
    std::thread::spawn(|| {
        const VM: usize = 0x29a0_7105;
        let _forget = W29aForget(VM);
        let mut ctx = MockNativeContext::new();
        ctx.set_vm_identity(VM);
        let (itl, plain) = (at(0x1_29a0_2a08), at(0x2_29a0_2a08));
        assert_eq!(
            ctx.identity_hash_code(itl),
            ctx.identity_hash_code(plain),
            "premise: one identity hash"
        );
        native_itl_init(&mut ctx, &[Value::Object(Some(itl))]).expect("ITL <init>");
        set(&mut ctx, itl, Value::Int(1));
        set(&mut ctx, plain, Value::Int(2));
        // No scripted `childValue`: the mock answers `Ok(None)`, the base
        // implementation's parent value.
        let snap = snapshot_inheritable_tl_entries(&mut ctx).expect("the ITL's entry");
        assert_eq!(
            snap.len(),
            1,
            "only the InheritableThreadLocal is inherited"
        );
        let (_, row) = snap[0];
        assert_eq!(row.owner_fallback, itl);
        assert_eq!(tl_value_to_java(&ctx, row.value), Value::Int(1));
        for (_, row) in snap {
            tl_release_row(&mut ctx, row);
        }
        assert_eq!(release_current_thread_locals(&mut ctx), 2);
    })
    .join()
    .expect("test thread must not panic");
}
