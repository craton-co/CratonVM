// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! T19.8 — IronJacamar (WildFly JCA) JDBC connection pool natives.
//!
//! WildFly / Keycloak 16 routes its datasource through IronJacamar, the
//! JCA-specified connection pool shipped inside WildFly. The relevant
//! surface is:
//!
//! ```text
//!   ConnectionManager.allocateConnection(mcf, cri)
//!     -> AbstractPool.getConnection(subject, cri)
//!       -> ManagedConnectionPool.getConnection(subject, cri)
//!         -> ManagedConnectionFactory.createManagedConnection(subject, cri)
//!           -> ManagedConnection.getConnection(subject, cri)
//!             -> java.sql.Connection
//! ```
//!
//! We collapse the chain onto the same Rust pool registry that backs
//! `agroal_pool`, keyed by a dedicated ID namespace. This keeps both
//! Keycloak paths (16 = JCA, 26 = Agroal) functioning through one
//! resource-pool impl.
//!
//! ## Field layout (wired via `classloading::class_manager::synthetic_stub_fields`)
//!
//! | Class                                                                  | Slot 0       | Slot 1          | Slot 2   |
//! |------------------------------------------------------------------------|--------------|-----------------|----------|
//! | `org.jboss.jca.core.connectionmanager.pool.AbstractPool`                | config       | managedFactory  | subPools |
//! | `org.jboss.jca.core.connectionmanager.pool.ManagedConnectionPool`       | connections  | semaphore       | state    |
//!
//! ## Error handling
//!
//! * `getConnection()` on an exhausted pool throws a
//!   `javax.resource.ResourceException` with `"pool exhausted"`.
//! * Unknown/unsupported JDBC URLs throw `SQLException("Unsupported driver")`.

#![allow(clippy::needless_pass_by_value)]

use std::collections::HashMap;
use std::sync::OnceLock;

use parking_lot::Mutex;

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ClassId, ObjectRef, Value};

use crate::agroal_pool::{
    acquire, close_pool, create_pool, pool_stats, release, validate_select_1, PoolConfig,
};

// ---------------------------------------------------------------------------
// Field layout constants — mirrored in class_manager.rs synthetic_stub_fields
// ---------------------------------------------------------------------------

pub(crate) const AP_FIELD_CONFIG: usize = 0;
pub(crate) const AP_FIELD_MANAGED_FACTORY: usize = 1;
pub(crate) const AP_FIELD_SUB_POOLS: usize = 2;
pub(crate) const AP_NUM_FIELDS: usize = 3;

pub(crate) const MCP_FIELD_CONNECTIONS: usize = 0;
pub(crate) const MCP_FIELD_SEMAPHORE: usize = 1;
pub(crate) const MCP_FIELD_STATE: usize = 2;
pub(crate) const MCP_NUM_FIELDS: usize = 3;

// We overload the AbstractPool fields to also store the Rust-side pool ID
// (in slot MANAGED_FACTORY when configured via MCF) and the max size hint.
// To avoid shadowing a Java-side reference we instead side-table the
// strategy-pool -> rust-pool-id mapping.
const CLS_ABSTRACT_POOL: &str = "org/jboss/jca/core/connectionmanager/pool/AbstractPool";
const CLS_STRATEGY_POOL: &str = "org/jboss/jca/core/connectionmanager/pool/strategy/PoolBySubject";
const CLS_MCP: &str = "org/jboss/jca/core/connectionmanager/pool/ManagedConnectionPool";
const CLS_MCF: &str = "javax/resource/spi/ManagedConnectionFactory";
const CLS_MC: &str = "javax/resource/spi/ManagedConnection";
const CLS_JDBC_LOCAL_MC: &str = "org/jboss/jca/adapters/jdbc/local/LocalManagedConnection";
const CLS_H2_CONNECTION: &str = "org/h2/jdbc/JdbcConnection";

// ---------------------------------------------------------------------------
// Side-table: maps Java-side AbstractPool ObjectRef (by pointer) to an
// i32 rust pool handle allocated via agroal_pool::create_pool. This isolates
// IronJacamar state from Agroal's own DataSource pool IDs.
// ---------------------------------------------------------------------------

struct JcaMap {
    pool_ids: HashMap<usize, i32>,
}

fn jca_map() -> &'static Mutex<JcaMap> {
    static M: OnceLock<Mutex<JcaMap>> = OnceLock::new();
    M.get_or_init(|| {
        Mutex::new(JcaMap {
            pool_ids: HashMap::new(),
        })
    })
}

/// Reset the IronJacamar state for tests.
#[allow(dead_code)]
pub fn reset_for_tests() {
    let mut m = jca_map().lock();
    m.pool_ids.clear();
}

/// Install `pool_id` for the given AbstractPool Java object.
pub fn bind_pool(this: ObjectRef, pool_id: i32) {
    let mut m = jca_map().lock();
    m.pool_ids.insert(this.as_ptr() as usize, pool_id);
}

/// Look up the pool id for a given AbstractPool Java object.
pub fn pool_id_for(this: ObjectRef) -> Option<i32> {
    let m = jca_map().lock();
    m.pool_ids.get(&(this.as_ptr() as usize)).copied()
}

// ---------------------------------------------------------------------------
// Native callbacks
// ---------------------------------------------------------------------------

fn alloc_object_for(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    min_slots: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    // Fall back to a synthetic class (declaring `min_slots` fields) rather
    // than `ClassId::new(0)` when the real class can't be loaded: an object
    // allocated with `java/lang/Object`'s id but a non-zero slot count is an
    // undersized layout the GC's `get_field` bounds guard rejects.
    //
    // The fallback is the FALLIBLE spelling (JDK-only wave 2, step 3): under
    // `--jdk-only` the policy refuses to fabricate rather than recording the
    // violation and fabricating anyway, and the refusal arrives as the
    // catchable `NoClassDefFoundError` contract §5 names rather than the
    // uncatchable `MethodCallFailed::InternalError` the `?` conversion builds.
    let cid = match ctx.ensure_class_initialized(class_name) {
        Ok(cid) => cid,
        Err(_) => crate::util_concurrent_ext::refused_class(ctx, class_name, min_slots)?,
    };
    let n = ctx.class_num_total_fields(cid).max(min_slots);
    Ok(ctx.alloc_object(cid, n))
}

fn obj_arg(args: &[Value], idx: usize) -> Option<ObjectRef> {
    match args.get(idx) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

/// `AbstractPool.<init>(ManagedConnectionFactory, PoolConfiguration, ...)` —
/// allocate a Rust-backed pool seeded from the MCF's JDBC URL.
fn native_abstract_pool_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(None),
    };
    let mcf = obj_arg(args, 1);

    // Pull JDBC URL from the MCF (slot 0) if possible; otherwise default.
    let jdbc_url = mcf
        .and_then(|m| match ctx.get_field(m, 0) {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        })
        .unwrap_or_else(|| "jdbc:h2:mem:ironjacamar-default".to_string());

    let mut cfg = PoolConfig::new(jdbc_url);
    cfg.min_size = 0;
    cfg.max_size = 10;

    let pool_id = match create_pool(cfg) {
        Ok(id) => id,
        Err(e) => {
            return Err(RuntimeError::IllegalStateException {
                message: format!("SQLException: {}", e),
            }
            .into());
        }
    };

    bind_pool(this, pool_id);
    if let Some(m) = mcf {
        ctx.set_field(this, AP_FIELD_MANAGED_FACTORY, Value::Object(Some(m)));
    }
    ctx.set_field(this, AP_FIELD_SUB_POOLS, Value::Object(None));
    Ok(None)
}

/// StrategyPool.<init> — reuses AbstractPool.<init>.
fn native_strategy_pool_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_abstract_pool_init(ctx, args)
}

/// AbstractPool.getConnection(Subject, ConnectionRequestInfo)
/// — delegate through ManagedConnection to the backing H2 connection.
fn native_pool_get_connection(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    let pool_id = match pool_id_for(this) {
        Some(id) => id,
        None => {
            return Err(RuntimeError::IllegalStateException {
                message: "ResourceException: pool not initialised".to_string(),
            }
            .into());
        }
    };
    let conn_id = match acquire(pool_id) {
        Ok(id) => id,
        Err(e) if e.starts_with("SQLTransientConnectionException") || e.contains("timeout") => {
            return Err(RuntimeError::IllegalStateException {
                message: "ResourceException: pool exhausted".to_string(),
            }
            .into());
        }
        Err(e) => {
            return Err(RuntimeError::IllegalStateException {
                message: format!("ResourceException: {}", e),
            }
            .into());
        }
    };

    // Allocate Connection wrapper with back-references so release works.
    let conn = alloc_object_for(ctx, CLS_H2_CONNECTION, 4)?;
    ctx.set_field(conn, 0, Value::Long(conn_id));
    ctx.set_field(conn, 1, Value::Object(Some(this)));
    ctx.set_field(conn, 2, Value::Int(pool_id));
    ctx.set_field(conn, 3, Value::Int(0));
    Ok(Some(Value::Object(Some(conn))))
}

/// AbstractPool.returnConnection(ManagedConnection, boolean)V —
/// release a connection back to the pool.
fn native_pool_return_connection(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(None),
    };
    let conn = match obj_arg(args, 1) {
        Some(o) => o,
        None => return Ok(None),
    };
    let pool_id = match pool_id_for(this) {
        Some(id) => id,
        None => return Ok(None),
    };
    let conn_id = match ctx.get_field(conn, 0) {
        Value::Long(v) => v,
        _ => return Ok(None),
    };
    release(pool_id, conn_id);
    Ok(None)
}

/// AbstractPool.shutdown()V — closes the pool and releases all conns.
fn native_pool_shutdown(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(None),
    };
    if let Some(pool_id) = pool_id_for(this) {
        close_pool(pool_id);
    }
    ctx.set_field(this, AP_FIELD_SUB_POOLS, Value::Object(None));
    Ok(None)
}

/// ManagedConnectionFactory.createManagedConnection(Subject, ConnectionRequestInfo)
/// — build a synthetic LocalManagedConnection wrapping a backing H2 conn.
fn native_mcf_create_managed_connection(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    // MCF slot 0: JDBC URL
    let jdbc_url = match ctx.get_field(this, 0) {
        Value::Object(Some(s)) => ctx
            .read_string(s)
            .unwrap_or_else(|| "jdbc:h2:mem:mcf".to_string()),
        _ => "jdbc:h2:mem:mcf".to_string(),
    };
    // Opening a single off-pool connection. We reuse the Agroal path for
    // convenience: mint a 1-conn pool and immediately acquire.
    let mut cfg = PoolConfig::new(jdbc_url);
    cfg.min_size = 0;
    cfg.max_size = 1;
    let pool_id = match create_pool(cfg) {
        Ok(id) => id,
        Err(e) => {
            return Err(RuntimeError::IllegalStateException {
                message: format!("SQLException: {}", e),
            }
            .into());
        }
    };
    let conn_id = match acquire(pool_id) {
        Ok(id) => id,
        Err(e) => {
            return Err(RuntimeError::IllegalStateException {
                message: format!("ResourceException: {}", e),
            }
            .into());
        }
    };
    let mc = alloc_object_for(ctx, CLS_JDBC_LOCAL_MC, 4)?;
    ctx.set_field(mc, 0, Value::Long(conn_id));
    ctx.set_field(mc, 1, Value::Object(Some(this))); // back-ref to MCF
    ctx.set_field(mc, 2, Value::Int(pool_id));
    ctx.set_field(mc, 3, Value::Int(0));
    Ok(Some(Value::Object(Some(mc))))
}

/// `ManagedConnection.getConnection(Subject, ConnectionRequestInfo)` —
/// returns the backing `java.sql.Connection` wrapper.
fn native_mc_get_connection(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    let conn_id = match ctx.get_field(this, 0) {
        Value::Long(v) => v,
        _ => 0,
    };
    if validate_select_1(conn_id).is_err() {
        return Err(RuntimeError::IllegalStateException {
            message: "ResourceException: underlying connection is dead".to_string(),
        }
        .into());
    }
    let conn = alloc_object_for(ctx, CLS_H2_CONNECTION, 4)?;
    ctx.set_field(conn, 0, Value::Long(conn_id));
    ctx.set_field(conn, 1, Value::Object(Some(this)));
    Ok(Some(Value::Object(Some(conn))))
}

/// `ManagedConnection.cleanup()V` / `destroy()V` — release underlying handle.
fn native_mc_destroy(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match obj_arg(args, 0) {
        Some(o) => o,
        None => return Ok(None),
    };
    let conn_id = match ctx.get_field(this, 0) {
        Value::Long(v) => v,
        _ => return Ok(None),
    };
    let pool_id = match ctx.get_field(this, 2) {
        Value::Int(v) => v,
        _ => return Ok(None),
    };
    release(pool_id, conn_id);
    ctx.set_field(this, 3, Value::Int(1));
    Ok(None)
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Register every IronJacamar-related native. Called from
/// `register_essential_natives` in `lib.rs`.
pub fn register_ironjacamar_natives(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    // AbstractPool
    registry.register(
        CLS_ABSTRACT_POOL,
        "<init>",
        "(Ljavax/resource/spi/ManagedConnectionFactory;Lorg/jboss/jca/core/connectionmanager/pool/PoolConfiguration;)V",
        native_abstract_pool_init,
    );
    registry.register(
        CLS_ABSTRACT_POOL,
        "getConnection",
        "(Ljavax/security/auth/Subject;Ljavax/resource/spi/ConnectionRequestInfo;)Ljava/sql/Connection;",
        native_pool_get_connection,
    );
    registry.register(
        CLS_ABSTRACT_POOL,
        "returnConnection",
        "(Ljavax/resource/spi/ManagedConnection;Z)V",
        native_pool_return_connection,
    );
    registry.register(CLS_ABSTRACT_POOL, "shutdown", "()V", native_pool_shutdown);

    // StrategyPool (concrete subclass of AbstractPool)
    registry.register(
        CLS_STRATEGY_POOL,
        "<init>",
        "(Ljavax/resource/spi/ManagedConnectionFactory;Lorg/jboss/jca/core/connectionmanager/pool/PoolConfiguration;)V",
        native_strategy_pool_init,
    );
    registry.register(
        CLS_STRATEGY_POOL,
        "getConnection",
        "(Ljavax/security/auth/Subject;Ljavax/resource/spi/ConnectionRequestInfo;)Ljava/sql/Connection;",
        native_pool_get_connection,
    );

    // ManagedConnectionFactory.createManagedConnection
    registry.register(
        CLS_MCF,
        "createManagedConnection",
        "(Ljavax/security/auth/Subject;Ljavax/resource/spi/ConnectionRequestInfo;)Ljavax/resource/spi/ManagedConnection;",
        native_mcf_create_managed_connection,
    );
    registry.register(
        CLS_JDBC_LOCAL_MC,
        "<init>",
        "(Ljava/lang/String;)V",
        |ctx, args| {
            // Simple-enough `<init>`: take the jdbc url from slot 0 arg,
            // store on MCF slot 0.
            let this = match obj_arg(args, 0) {
                Some(o) => o,
                None => return Ok(None),
            };
            if let Some(Value::Object(Some(s))) = args.get(1) {
                ctx.set_field(this, 0, Value::Object(Some(*s)));
            }
            Ok(None)
        },
    );

    // ManagedConnection.getConnection / cleanup / destroy
    registry.register(
        CLS_MC,
        "getConnection",
        "(Ljavax/security/auth/Subject;Ljavax/resource/spi/ConnectionRequestInfo;)Ljava/lang/Object;",
        native_mc_get_connection,
    );
    registry.register(
        CLS_JDBC_LOCAL_MC,
        "getConnection",
        "(Ljavax/security/auth/Subject;Ljavax/resource/spi/ConnectionRequestInfo;)Ljava/lang/Object;",
        native_mc_get_connection,
    );
    registry.register(CLS_MC, "cleanup", "()V", native_mc_destroy);
    registry.register(CLS_MC, "destroy", "()V", native_mc_destroy);
    registry.register(CLS_JDBC_LOCAL_MC, "cleanup", "()V", native_mc_destroy);
    registry.register(CLS_JDBC_LOCAL_MC, "destroy", "()V", native_mc_destroy);
    registry.set_category(__prev_cat);
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
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
        let pool_id = pool_id_for(pool).expect("pool was registered");
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
        bind_pool(pool, pool_id);
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
        let pool_id = pool_id_for(pool).expect("bound");
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
}
