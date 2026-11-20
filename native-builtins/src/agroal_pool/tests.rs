use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn t19_8_agroal_datasource_prefill_opens_min_size_connections() {
    let _g = test_lock();
    let cfg = PoolConfig {
        jdbc_url: "jdbc:h2:mem:prefill".to_string(),
        driver: "org.h2.Driver".to_string(),
        username: "sa".to_string(),
        password: String::new(),
        min_size: 3,
        max_size: 10,
    };
    let pool_id = create_pool(cfg).expect("pool creation succeeded");
    let (idle, in_use, opened, closed) = pool_stats(pool_id).expect("stats available");
    assert_eq!(idle, 3, "prefill should seed min_size idle connections");
    assert_eq!(in_use, 0);
    assert_eq!(opened, 3);
    assert!(!closed);
}

#[test]
fn t19_8_agroal_get_connection_blocks_until_available() {
    let _g = test_lock();
    let cfg = PoolConfig {
        jdbc_url: "jdbc:h2:mem:block".to_string(),
        driver: "org.h2.Driver".to_string(),
        username: "sa".to_string(),
        password: String::new(),
        min_size: 0,
        max_size: 1,
    };
    let pool_id = create_pool(cfg).unwrap();
    let c1 = acquire(pool_id).expect("first acquire");
    // Second acquire would block; we release after spawning a short
    // background thread.
    let pool_id_t = pool_id;
    let c1_t = c1;
    let h = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        release(pool_id_t, c1_t);
    });
    let c2 = acquire(pool_id).expect("second acquire after release");
    assert_eq!(c1, c2, "released conn should be reused");
    h.join().unwrap();
}

#[test]
fn t19_8_agroal_close_drains_pool() {
    let _g = test_lock();
    let cfg = PoolConfig {
        jdbc_url: "jdbc:h2:mem:close".to_string(),
        driver: "org.h2.Driver".to_string(),
        username: "sa".to_string(),
        password: String::new(),
        min_size: 2,
        max_size: 5,
    };
    let pool_id = create_pool(cfg).unwrap();
    let (idle_before, _, _, closed_before) = pool_stats(pool_id).unwrap();
    assert_eq!(idle_before, 2);
    assert!(!closed_before);
    close_pool(pool_id);
    let (idle_after, in_use_after, _, closed_after) = pool_stats(pool_id).unwrap();
    assert_eq!(idle_after, 0, "close must drain idle");
    assert_eq!(in_use_after, 0);
    assert!(closed_after);
}

#[test]
fn t19_8_agroal_config_reader_parses_jdbc_url() {
    let text = "\
        quarkus.datasource.jdbc.url=jdbc:h2:mem:parsed\n\
        quarkus.datasource.username=sa\n\
        quarkus.datasource.password=topsecret\n\
        quarkus.datasource.jdbc.min-size=2\n\
        quarkus.datasource.jdbc.max-size=10\n\
        # comment line\n\
    ";
    let cfg = parse_properties(text).expect("parse succeeds");
    assert_eq!(cfg.jdbc_url, "jdbc:h2:mem:parsed");
    assert_eq!(cfg.username, "sa");
    assert_eq!(cfg.password, "topsecret");
    assert_eq!(cfg.min_size, 2);
    assert_eq!(cfg.max_size, 10);
    // Redacted must not leak the password.
    assert!(!cfg.redacted().contains("topsecret"));
    assert!(cfg.redacted().contains("<redacted>"));
}

#[test]
fn t19_8_agroal_config_reader_rejects_bad_url_with_sqlexception() {
    let text = "quarkus.datasource.jdbc.url=jdbc:postgresql://localhost/db\n";
    let err = parse_properties(text).expect_err("postgres URL must be rejected");
    assert!(err.contains("Unsupported driver"), "got: {}", err);

    let text2 = "quarkus.datasource.jdbc.url=ftp://wrong\n";
    let err2 = parse_properties(text2).expect_err("malformed URL rejected");
    assert!(err2.contains("malformed"), "got: {}", err2);

    let text3 = "# missing jdbc.url\n";
    let err3 = parse_properties(text3).expect_err("missing URL rejected");
    assert!(err3.contains("missing"), "got: {}", err3);
}

#[test]
fn t19_8_agroal_connection_validate_h2_roundtrip() {
    let _g = test_lock();
    let cfg = PoolConfig {
        jdbc_url: "jdbc:h2:mem:validate".to_string(),
        driver: "org.h2.Driver".to_string(),
        username: "sa".to_string(),
        password: String::new(),
        min_size: 1,
        max_size: 2,
    };
    let pool_id = create_pool(cfg).unwrap();
    let conn_id = acquire(pool_id).unwrap();
    let one = validate_select_1(conn_id).expect("SELECT 1 should return 1");
    assert_eq!(one, 1);
    release(pool_id, conn_id);
}

// Bonus: registration smoke test.
#[test]
fn t19_8_agroal_natives_registered() {
    let mut r = NativeMethodRegistry::new();
    register_agroal_natives(&mut r);
    assert!(r
        .find(CLS_DS, "getConnection", "()Ljava/sql/Connection;")
        .is_some());
    assert!(r.find(CLS_DS, "close", "()V").is_some());
    assert!(r.find(
        CLS_DS,
        "from",
        "(Lio/agroal/api/configuration/AgroalDataSourceConfiguration;)Lio/agroal/api/AgroalDataSource;",
    ).is_some());
    assert!(r
        .find(
            CLS_PROPERTIES_READER,
            "readProperties",
            "(Ljava/lang/String;)Lio/agroal/api/configuration/AgroalDataSourceConfiguration;",
        )
        .is_some());
}

// Bonus: the native ds.from() path wiring through a config object.
#[test]
fn t19_8_agroal_ds_from_config_object_stores_pool_handle() {
    let _g = test_lock();
    let mut ctx = mock_ctx();
    // Build a configuration object with a valid H2 URL.
    let cfg_cid = ctx.ensure_class_initialized(CLS_CONFIG).unwrap();
    let cfg = ctx.alloc_object(cfg_cid, CFG_NUM_FIELDS);
    let url = ctx.create_string("jdbc:h2:mem:fromcfg");
    ctx.set_field(cfg, CFG_FIELD_JDBC_URL, Value::Object(Some(url)));
    ctx.set_field(cfg, CFG_FIELD_MIN_SIZE, Value::Int(1));
    ctx.set_field(cfg, CFG_FIELD_MAX_SIZE, Value::Int(3));
    let ret = native_ds_from(&mut ctx, &[Value::Object(Some(cfg))]).unwrap();
    let ds = match ret {
        Some(Value::Object(Some(o))) => o,
        other => panic!("expected DS object, got {:?}", other),
    };
    let pool_val = ctx.get_field(ds, DS_FIELD_POOL);
    let pool_id = match pool_val {
        Value::Int(v) => v,
        other => panic!("expected pool id, got {:?}", other),
    };
    let (idle, _, _, _) = pool_stats(pool_id).expect("pool exists");
    assert_eq!(idle, 1, "min_size=1 should have prefilled 1 conn");
}
