use super::*;
use crate::agroal_pool::test_lock;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn make_mcf(ctx: &mut crate::test_utils::MockNativeContext, url: &str) -> ObjectRef {
    let cid = ctx.ensure_class_initialized(CLS_MCF).unwrap();
    let mcf = ctx.alloc_object(cid, 4);
    let url_s = ctx.create_string(url);
    ctx.set_field(mcf, 0, Value::Object(Some(url_s)));
    mcf
}

#[test]
fn t19_8_ironjacamar_strategy_pool_get_connection() {
    let _g = test_lock();
    reset_for_tests();
    let mut ctx = mock_ctx();
    let mcf = make_mcf(&mut ctx, "jdbc:h2:mem:sp-get");
    let cid = ctx.ensure_class_initialized(CLS_STRATEGY_POOL).unwrap();
    let pool = ctx.alloc_object(cid, AP_NUM_FIELDS);
    native_strategy_pool_init(
        &mut ctx,
        &[
            Value::Object(Some(pool)),
            Value::Object(Some(mcf)),
            Value::Object(None),
        ],
    )
    .unwrap();
    let ret = native_pool_get_connection(
        &mut ctx,
        &[
            Value::Object(Some(pool)),
            Value::Object(None),
            Value::Object(None),
        ],
    )
    .unwrap();
    let conn = match ret {
        Some(Value::Object(Some(c))) => c,
        other => panic!("expected Connection, got {:?}", other),
    };
    let conn_id = match ctx.get_field(conn, 0) {
        Value::Long(v) => v,
        other => panic!("expected Long conn id, got {:?}", other),
    };
    assert!(conn_id > 0);
    // The pool_id mapped to this AbstractPool is recorded.
    let pool_id = pool_id_for(&ctx, pool).expect("pool was registered");
    let (idle, in_use, _, _) = pool_stats(pool_id).unwrap();
    assert_eq!(in_use, 1);
    assert_eq!(idle, 0);
}

#[test]
fn t19_8_ironjacamar_managed_connection_factory_h2() {
    let _g = test_lock();
    reset_for_tests();
    let mut ctx = mock_ctx();
    let mcf = make_mcf(&mut ctx, "jdbc:h2:mem:mcf-real");
    let ret = native_mcf_create_managed_connection(
        &mut ctx,
        &[
            Value::Object(Some(mcf)),
            Value::Object(None),
            Value::Object(None),
        ],
    )
    .unwrap();
    let mc = match ret {
        Some(Value::Object(Some(o))) => o,
        other => panic!("expected MC, got {:?}", other),
    };
    let conn_id = match ctx.get_field(mc, 0) {
        Value::Long(v) => v,
        other => panic!("expected conn id Long, got {:?}", other),
    };
    // Round-trip SELECT 1 on the underlying H2 conn.
    let one = validate_select_1(conn_id).expect("H2 round-trip works");
    assert_eq!(one, 1);
}

/// A max-size-1 pool with its one connection out must REFUSE a second
/// acquire, and `native_pool_get_connection` must wrap the refusal in the
/// JCA `ResourceException` shape.
///
/// The body used to re-write `native_pool_get_connection`'s error
/// classification over a hard-coded string literal and then assert its own
/// copy's output. The native was never invoked on any error path, so
/// deleting the classification from production left this green — while
/// WildFly/Keycloak would have seen a raw `SQLTransientConnectionException`
/// escape where the JCA contract requires a `ResourceException`.
///
/// Mutations this now catches:
///  * remove the `max_size` gate in `agroal_pool::acquire` (the pool would
///    over-allocate and the waiter below would finish immediately);
///  * delete the error mapping in `native_pool_get_connection` and
///    propagate the raw acquire error instead — the message stops being
///    `ResourceException: …`;
///  * change the non-timeout arm's wrapping text or drop the cause from it.
///
/// Residual, and it is NOT fixable from a test: the `"pool exhausted"` arm
/// itself — the one that fires on `e.contains("timeout")` — is only
/// reachable by letting `acquire` run out its `DEFAULT_ACQUIRE_TIMEOUT`,
/// which is a hard-coded 30 s constant in `agroal_pool.rs` with no
/// per-pool override. Closing that needs a configurable acquire timeout in
/// `PoolConfig`, which is a production change.
#[test]
fn t19_8_ironjacamar_pool_exhausted_throws_resource_exception() {
    let _g = test_lock();
    reset_for_tests();
    let mut ctx = mock_ctx();
    let _mcf = make_mcf(&mut ctx, "jdbc:h2:mem:exhaust");
    let cid = ctx.ensure_class_initialized(CLS_STRATEGY_POOL).unwrap();
    let pool = ctx.alloc_object(cid, AP_NUM_FIELDS);
    // Manually create a max-size-1 pool so exhaustion is easy to hit.
    let mut cfg = PoolConfig::new("jdbc:h2:mem:exhaust".to_string());
    cfg.min_size = 0;
    cfg.max_size = 1;
    let pool_id = create_pool(cfg).unwrap();
    bind_pool(&ctx, pool, pool_id);
    let _ = native_pool_get_connection(
        &mut ctx,
        &[
            Value::Object(Some(pool)),
            Value::Object(None),
            Value::Object(None),
        ],
    )
    .expect("first acquire");

    // The premise, measured rather than assumed: the pool really is at
    // capacity. `pool_stats` is (idle, in_use, opened_total, closed).
    let (idle, in_use, _, _) = pool_stats(pool_id).expect("pool is registered");
    assert_eq!(
        (idle, in_use),
        (0, 1),
        "a max_size=1 pool holding its only connection has nothing idle"
    );

    // A second acquire must WAIT for a slot instead of over-allocating.
    // `acquire` takes only the pool id, so it can run on another thread
    // without the (non-`Send`) mock context.
    let waiter = std::thread::spawn(move || acquire(pool_id));
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(
        !waiter.is_finished(),
        "a max_size=1 pool must not hand out a second connection while the \
         first is still in use"
    );

    // Unblock the waiter deterministically rather than paying the 30 s
    // acquire timeout; `close_pool` makes the next loop iteration fail.
    close_pool(pool_id);
    let second = waiter.join().expect("acquire waiter panicked");
    assert!(
        second.is_err(),
        "the blocked acquire must fail, not invent a connection; got {second:?}"
    );

    // Now the classification itself, through the production native: an
    // acquire failure must reach Java as a `ResourceException` carrying the
    // cause, not as the bare Rust error string.
    let err = native_pool_get_connection(
        &mut ctx,
        &[
            Value::Object(Some(pool)),
            Value::Object(None),
            Value::Object(None),
        ],
    )
    .expect_err("a closed pool must refuse to hand out a connection");
    let text = format!("{err:?}");
    assert!(
        text.contains("ResourceException: pool closed"),
        "getConnection must raise the JCA `ResourceException: <cause>` \
         shape that IronJacamar callers catch; got {text}"
    );
}

#[test]
fn t19_8_ironjacamar_close_releases_all_connections() {
    let _g = test_lock();
    reset_for_tests();
    let mut ctx = mock_ctx();
    let mcf = make_mcf(&mut ctx, "jdbc:h2:mem:close-all");
    let cid = ctx.ensure_class_initialized(CLS_STRATEGY_POOL).unwrap();
    let pool = ctx.alloc_object(cid, AP_NUM_FIELDS);
    native_strategy_pool_init(
        &mut ctx,
        &[
            Value::Object(Some(pool)),
            Value::Object(Some(mcf)),
            Value::Object(None),
        ],
    )
    .unwrap();
    // Acquire two connections.
    let _c1 = native_pool_get_connection(
        &mut ctx,
        &[
            Value::Object(Some(pool)),
            Value::Object(None),
            Value::Object(None),
        ],
    )
    .unwrap();
    let _c2 = native_pool_get_connection(
        &mut ctx,
        &[
            Value::Object(Some(pool)),
            Value::Object(None),
            Value::Object(None),
        ],
    )
    .unwrap();
    let pool_id = pool_id_for(&ctx, pool).expect("bound");
    let (_, in_use_before, _, _) = pool_stats(pool_id).unwrap();
    assert_eq!(in_use_before, 2);
    native_pool_shutdown(&mut ctx, &[Value::Object(Some(pool))]).unwrap();
    let (idle_after, in_use_after, _, closed) = pool_stats(pool_id).unwrap();
    assert!(closed);
    assert_eq!(idle_after, 0);
    assert_eq!(in_use_after, 0);
}

// Bonus: registration smoke test.
#[test]
fn t19_8_ironjacamar_natives_registered() {
    let mut r = NativeMethodRegistry::new();
    register_ironjacamar_natives(&mut r);
    assert!(r.find(
        CLS_ABSTRACT_POOL,
        "getConnection",
        "(Ljavax/security/auth/Subject;Ljavax/resource/spi/ConnectionRequestInfo;)Ljava/sql/Connection;",
    ).is_some());
    assert!(r.find(
        CLS_STRATEGY_POOL,
        "getConnection",
        "(Ljavax/security/auth/Subject;Ljavax/resource/spi/ConnectionRequestInfo;)Ljava/sql/Connection;",
    ).is_some());
    assert!(r.find(
        CLS_MCF,
        "createManagedConnection",
        "(Ljavax/security/auth/Subject;Ljavax/resource/spi/ConnectionRequestInfo;)Ljavax/resource/spi/ManagedConnection;",
    ).is_some());
    assert!(r.find(CLS_ABSTRACT_POOL, "shutdown", "()V").is_some());
}

/// gch h1/ak: the pool-id row follows a pool a collection MOVED (it used to
/// stay under the vacated address, so the next `getConnection` threw "pool
/// not initialised"), and a dead pool's row is not handed to whatever
/// object is later placed at its address (it used to be: the key was the
/// address itself).
#[test]
fn gch_h1ak_jca_pool_row_follows_a_moved_pool_and_dies_with_it() {
    const VM: usize = 0x1A0C_0001;
    let _g = test_lock();
    reset_for_tests();
    let ctx = mock_ctx();
    ctx.set_vm_identity(VM);
    // SAFETY: keys only; nothing on these paths dereferences them.
    let obj = |addr: usize| unsafe { ObjectRef::from_raw(addr as *mut u8) };
    let (old, new) = (0x1_1A0C_0010usize, 0x3_1A0C_0010usize);
    bind_pool(&ctx, obj(old), 41);
    assert_eq!(pool_id_for(&ctx, obj(old)), Some(41));

    let mut moved = cratonvm_types::PointerMap::default();
    moved.insert(old, new);
    crate::gc_sweep_and_remap_lock_keys(VM, &|a| a == old, &moved);
    assert_eq!(
        pool_id_for(&ctx, obj(new)),
        Some(41),
        "the moved pool lost its id"
    );
    assert_eq!(
        pool_id_for(&ctx, obj(old)),
        None,
        "the vacated address kept the row"
    );

    // The pool dies: its key is freed, and a newcomer at its address is
    // a stranger (no row), whether or not the lib.rs hook ran.
    crate::gc_sweep_lock_keys(VM, &|a| a != new);
    assert_eq!(
        pool_id_for(&ctx, obj(new)),
        None,
        "a dead pool's row answered a newcomer"
    );
    crate::forget_vm_lock_keys(VM);
}

/// gch h1/ak: `forget_jca_pool_keys` drops exactly the rows filed under the
/// freed keys.
#[test]
fn gch_h1ak_forget_jca_pool_keys_drops_only_the_freed_rows() {
    const VM: usize = 0x1A0C_0002;
    let _g = test_lock();
    reset_for_tests();
    let ctx = mock_ctx();
    ctx.set_vm_identity(VM);
    // SAFETY: keys only; nothing on these paths dereferences them.
    let obj = |addr: usize| unsafe { ObjectRef::from_raw(addr as *mut u8) };
    let (a, b) = (0x1_1A0D_0010usize, 0x1_1A0E_0010usize);
    bind_pool(&ctx, obj(a), 7);
    bind_pool(&ctx, obj(b), 8);
    let ka = crate::existing_weak_lock_key(&ctx, obj(a)).expect("a was keyed");
    assert_eq!(forget_jca_pool_keys(&[]), 0);
    assert_eq!(forget_jca_pool_keys(&[ka]), 1);
    assert_eq!(pool_id_for(&ctx, obj(a)), None);
    assert_eq!(pool_id_for(&ctx, obj(b)), Some(8));
    crate::forget_vm_lock_keys(VM);
}
