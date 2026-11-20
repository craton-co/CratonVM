use super::jdbc_registry;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// Verify `open_connection(":memory:")` works and yields a
/// non-zero id that the subsequent lookups recognize.
#[test]
fn new14_open_inmemory_connection() {
    let id = jdbc_registry::open_connection(":memory:").expect("open :memory: connection");
    assert!(id > 0);
    assert!(jdbc_registry::is_valid(id));
    jdbc_registry::close_connection(id);
    assert!(!jdbc_registry::is_valid(id));
}

/// Exercise the full statement → executeUpdate → executeQuery →
/// row-iteration path on a real in-memory SQLite database.
#[test]
fn new14_statement_ddl_dml_query() {
    let conn = jdbc_registry::open_connection(":memory:").unwrap();

    // CREATE TABLE via Statement.executeUpdate.
    let n = jdbc_registry::execute_update(
        conn,
        "CREATE TABLE widgets (id INTEGER PRIMARY KEY, name TEXT, price REAL)",
    )
    .expect("create table");
    assert_eq!(n, 0, "DDL returns 0 rows affected");

    // Three INSERTs.
    let n1 = jdbc_registry::execute_update(
        conn,
        "INSERT INTO widgets (name, price) VALUES ('alpha', 1.25)",
    )
    .unwrap();
    let n2 = jdbc_registry::execute_update(
        conn,
        "INSERT INTO widgets (name, price) VALUES ('beta', 2.50)",
    )
    .unwrap();
    let n3 = jdbc_registry::execute_update(
        conn,
        "INSERT INTO widgets (name, price) VALUES ('gamma', 3.75)",
    )
    .unwrap();
    assert_eq!(n1, 1);
    assert_eq!(n2, 1);
    assert_eq!(n3, 1);

    // Query all rows via executeQuery.
    let (stmt_id, row_count) =
        jdbc_registry::execute_query(conn, "SELECT id, name, price FROM widgets ORDER BY id")
            .unwrap();
    assert_eq!(row_count, 3);
    assert_eq!(jdbc_registry::get_column_count(stmt_id), 3);
    assert_eq!(jdbc_registry::get_column_name(stmt_id, 0), "id");
    assert_eq!(jdbc_registry::get_column_name(stmt_id, 1), "name");
    assert_eq!(jdbc_registry::get_column_name(stmt_id, 2), "price");

    // Read row 0.
    assert_eq!(
        jdbc_registry::get_result(stmt_id, 0, 1).as_deref(),
        Some("alpha")
    );
    assert_eq!(
        jdbc_registry::get_result(stmt_id, 1, 1).as_deref(),
        Some("beta")
    );
    assert_eq!(
        jdbc_registry::get_result(stmt_id, 2, 1).as_deref(),
        Some("gamma")
    );

    jdbc_registry::free_results(stmt_id);
    jdbc_registry::close_connection(conn);
}

/// Column type metadata is read from SQLite's DECLARED types.
///
/// Regression test for the pre-2026-07-28 behaviour: both execute paths
/// filled `column_types` with `vec!["TEXT"; n]`, so `getColumnTypeName()`
/// answered "TEXT" and `getColumnType()` answered `Types.VARCHAR` (12) for
/// every column of every result set in the VM.
#[test]
fn column_metadata_comes_from_declared_types_not_a_blanket_text() {
    let conn = jdbc_registry::open_connection(":memory:").unwrap();
    jdbc_registry::execute_update(
        conn,
        "CREATE TABLE typed (i INTEGER, b BIGINT, t TEXT, v VARCHAR(20), \
         r REAL, d DECIMAL(10,2), z BLOB, flag BOOLEAN)",
    )
    .expect("create table");
    jdbc_registry::execute_update(
        conn,
        "INSERT INTO typed VALUES (1, 2, 'three', 'four', 5.5, 6.25, x'07', 1)",
    )
    .expect("insert row");

    let (rs, n) = jdbc_registry::execute_query(conn, "SELECT * FROM typed").unwrap();
    assert_eq!(n, 1);

    // (column, expected getColumnTypeName, expected java.sql.Types code)
    let expected: &[(usize, &str, i32)] = &[
        (0, "INTEGER", 4),
        (1, "BIGINT", -5),
        (2, "TEXT", 12),
        (3, "VARCHAR", 12),
        (4, "REAL", 8),
        (5, "DECIMAL", 3),
        (6, "BLOB", 2004),
        (7, "BOOLEAN", 16),
    ];
    for &(col, name, code) in expected {
        assert_eq!(
            jdbc_registry::get_column_type_name(rs, col),
            name,
            "getColumnTypeName of column {col}"
        );
        assert_eq!(
            jdbc_registry::get_column_type_code(rs, col),
            code,
            "getColumnType of column {col}"
        );
    }

    // The declared (precision, scale) argument list is the only real
    // source for these two, and absence reports JDBC's "not applicable" 0.
    assert_eq!(jdbc_registry::get_column_precision(rs, 5), 10);
    assert_eq!(jdbc_registry::get_column_scale(rs, 5), 2);
    assert_eq!(jdbc_registry::get_column_precision(rs, 3), 20);
    assert_eq!(jdbc_registry::get_column_scale(rs, 3), 0);
    assert_eq!(jdbc_registry::get_column_precision(rs, 2), 0);
    assert_eq!(jdbc_registry::get_column_display_size(rs, 3), 20);

    assert!(jdbc_registry::is_column_signed(rs, 0), "INTEGER is signed");
    assert!(!jdbc_registry::is_column_signed(rs, 2), "TEXT is not");
    assert_eq!(
        jdbc_registry::get_column_class_name(rs, 0),
        "java.lang.Integer"
    );
    assert_eq!(
        jdbc_registry::get_column_class_name(rs, 1),
        "java.lang.Long"
    );
    assert_eq!(
        jdbc_registry::get_column_class_name(rs, 2),
        "java.lang.String"
    );
    assert_eq!(jdbc_registry::get_column_class_name(rs, 6), "[B");

    jdbc_registry::free_results(rs);
    jdbc_registry::close_connection(conn);
}

/// A column produced by an expression has no declared type at all —
/// SQLite reports `NULL` from `sqlite3_column_decltype`. The type name
/// then has to describe the value actually produced, and must not fall
/// back to a blanket TEXT. With no rows to look at, the answer is
/// SQLite's "no affinity", which is BLOB affinity.
#[test]
fn expression_columns_report_the_runtime_storage_class() {
    let conn = jdbc_registry::open_connection(":memory:").unwrap();
    jdbc_registry::execute_update(conn, "CREATE TABLE nums (a INTEGER, b INTEGER)").unwrap();
    jdbc_registry::execute_update(conn, "INSERT INTO nums VALUES (2, 3)").unwrap();

    let (rs, _) =
        jdbc_registry::execute_query(conn, "SELECT a + b, a * 1.5, 'lit', a FROM nums")
            .unwrap();
    assert_eq!(jdbc_registry::get_column_type_name(rs, 0), "INTEGER");
    assert_eq!(jdbc_registry::get_column_type_code(rs, 0), 4);
    assert_eq!(jdbc_registry::get_column_type_name(rs, 1), "REAL");
    assert_eq!(jdbc_registry::get_column_type_code(rs, 1), 8);
    assert_eq!(jdbc_registry::get_column_type_name(rs, 2), "TEXT");
    // A plain column reference still reports its DECLARED type.
    assert_eq!(jdbc_registry::get_column_type_name(rs, 3), "INTEGER");
    jdbc_registry::free_results(rs);

    // No rows → no runtime value either → SQLite's "no affinity".
    let (empty, n) =
        jdbc_registry::execute_query(conn, "SELECT a + b FROM nums WHERE a > 1000").unwrap();
    assert_eq!(n, 0);
    assert_eq!(jdbc_registry::get_column_type_name(empty, 0), "BLOB");
    assert_eq!(jdbc_registry::get_column_type_code(empty, 0), 2004);
    jdbc_registry::free_results(empty);

    // The PreparedStatement path carried the identical hardcode and has to
    // give the identical answers.
    let ps = jdbc_registry::prepare(conn, "SELECT a, a + b FROM nums WHERE a = ?");
    jdbc_registry::bind_int(ps, 1, 2);
    let (prs, rows) = jdbc_registry::execute_prepared_query(ps).unwrap();
    assert_eq!(rows, 1);
    assert_eq!(jdbc_registry::get_column_type_name(prs, 0), "INTEGER");
    assert_eq!(jdbc_registry::get_column_type_name(prs, 1), "INTEGER");
    jdbc_registry::free_results(prs);
    jdbc_registry::free_prepared(ps);

    jdbc_registry::close_connection(conn);
}

/// PreparedStatement end-to-end: bind parameters, execute, verify
/// the stored rows match the binds.
#[test]
fn new14_prepared_statement_binds_and_executes() {
    let conn = jdbc_registry::open_connection(":memory:").unwrap();
    jdbc_registry::execute_update(
        conn,
        "CREATE TABLE users (id INTEGER, name TEXT, age INTEGER)",
    )
    .unwrap();

    let ps = jdbc_registry::prepare(conn, "INSERT INTO users (id, name, age) VALUES (?, ?, ?)");
    jdbc_registry::bind_int(ps, 1, 1);
    jdbc_registry::bind_string(ps, 2, "Alice".to_string());
    jdbc_registry::bind_int(ps, 3, 30);
    let updated = jdbc_registry::execute_prepared_update(ps).unwrap();
    assert_eq!(updated, 1);

    // Re-execute the same prepared statement with different binds.
    jdbc_registry::clear_params(ps);
    jdbc_registry::bind_int(ps, 1, 2);
    jdbc_registry::bind_string(ps, 2, "Bob".to_string());
    jdbc_registry::bind_int(ps, 3, 42);
    assert_eq!(jdbc_registry::execute_prepared_update(ps).unwrap(), 1);

    // Verify both rows via a plain query.
    let (stmt_id, row_count) =
        jdbc_registry::execute_query(conn, "SELECT id, name, age FROM users ORDER BY id")
            .unwrap();
    assert_eq!(row_count, 2);
    assert_eq!(
        jdbc_registry::get_result(stmt_id, 0, 1).as_deref(),
        Some("Alice")
    );
    assert_eq!(
        jdbc_registry::get_result(stmt_id, 1, 1).as_deref(),
        Some("Bob")
    );

    jdbc_registry::free_results(stmt_id);
    jdbc_registry::free_prepared(ps);
    jdbc_registry::close_connection(conn);
}

/// Transaction isolation: set autocommit OFF, insert two rows,
/// rollback, query finds zero rows. Then repeat with a commit
/// and verify the rows persist.
#[test]
fn new14_rollback_discards_changes() {
    let conn = jdbc_registry::open_connection(":memory:").unwrap();
    jdbc_registry::execute_update(conn, "CREATE TABLE t (x INTEGER)").unwrap();

    jdbc_registry::set_auto_commit(conn, false).unwrap();
    jdbc_registry::execute_update(conn, "INSERT INTO t VALUES (1)").unwrap();
    jdbc_registry::execute_update(conn, "INSERT INTO t VALUES (2)").unwrap();
    jdbc_registry::rollback(conn).unwrap();

    let (stmt_id, count) =
        jdbc_registry::execute_query(conn, "SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(count, 1); // COUNT(*) always yields one row
    assert_eq!(
        jdbc_registry::get_result(stmt_id, 0, 0).as_deref(),
        Some("0")
    );
    jdbc_registry::free_results(stmt_id);

    // Commit path.
    jdbc_registry::execute_update(conn, "INSERT INTO t VALUES (3)").unwrap();
    jdbc_registry::commit(conn).unwrap();
    let (stmt2, _) = jdbc_registry::execute_query(conn, "SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(jdbc_registry::get_result(stmt2, 0, 0).as_deref(), Some("1"));
    jdbc_registry::free_results(stmt2);
    jdbc_registry::close_connection(conn);
}

// ---- NEW-14.N2 Blob tests ----

#[test]
fn new14_blob_round_trip() {
    let id = jdbc_registry::blob_create(vec![1, 2, 3, 4, 5]);
    assert_eq!(jdbc_registry::blob_length(id), 5);
    assert_eq!(jdbc_registry::blob_get_bytes(id, 1, 3), vec![1, 2, 3]);
    assert_eq!(jdbc_registry::blob_get_bytes(id, 4, 10), vec![4, 5]);
    // Out-of-range returns empty, not a panic.
    assert!(jdbc_registry::blob_get_bytes(id, 99, 5).is_empty());
    assert!(jdbc_registry::blob_get_bytes(id, 1, -1).is_empty());

    // setBytes extends the blob if pos+len exceeds the current length.
    let written = jdbc_registry::blob_set_bytes(id, 4, &[40, 50, 60]);
    assert_eq!(written, 3);
    assert_eq!(jdbc_registry::blob_length(id), 6);
    assert_eq!(
        jdbc_registry::blob_get_bytes(id, 1, 10),
        vec![1, 2, 3, 40, 50, 60]
    );

    jdbc_registry::blob_truncate(id, 3);
    assert_eq!(jdbc_registry::blob_length(id), 3);

    jdbc_registry::blob_free(id);
    // Post-free getters are safe — return empty / 0.
    assert_eq!(jdbc_registry::blob_length(id), 0);
    assert!(jdbc_registry::blob_get_bytes(id, 1, 3).is_empty());
}

// ---- NEW-14.N2 Clob tests ----

#[test]
fn new14_clob_round_trip() {
    let id = jdbc_registry::clob_create("hello, world".to_string());
    assert_eq!(jdbc_registry::clob_length(id), 12);
    assert_eq!(jdbc_registry::clob_get_substring(id, 1, 5), "hello");
    assert_eq!(jdbc_registry::clob_get_substring(id, 8, 5), "world");
    assert!(jdbc_registry::clob_get_substring(id, 99, 5).is_empty());

    // setString at the middle replaces chars in place.
    let written = jdbc_registry::clob_set_string(id, 8, "RUST!");
    assert_eq!(written, 5);
    assert_eq!(jdbc_registry::clob_get_substring(id, 1, 12), "hello, RUST!");

    // Extending past the end pads and appends.
    jdbc_registry::clob_set_string(id, 15, "@@@");
    assert!(jdbc_registry::clob_length(id) >= 17);

    jdbc_registry::clob_truncate(id, 5);
    assert_eq!(jdbc_registry::clob_length(id), 5);
    assert_eq!(jdbc_registry::clob_get_substring(id, 1, 5), "hello");

    jdbc_registry::clob_free(id);
    assert_eq!(jdbc_registry::clob_length(id), 0);
}

// ---- NEW-14.N3 Savepoint tests ----

#[test]
fn new14_savepoint_rollback_and_release() {
    let conn = jdbc_registry::open_connection(":memory:").unwrap();
    jdbc_registry::execute_update(conn, "CREATE TABLE t (x INTEGER)").unwrap();
    jdbc_registry::set_auto_commit(conn, false).unwrap();

    jdbc_registry::execute_update(conn, "INSERT INTO t VALUES (1)").unwrap();

    // Create a savepoint, insert more, roll back to it.
    let (sp_id, sp_name) =
        jdbc_registry::savepoint_create(conn, Some("mid".to_string())).unwrap();
    assert_eq!(sp_name, "mid");
    jdbc_registry::execute_update(conn, "INSERT INTO t VALUES (2)").unwrap();
    jdbc_registry::execute_update(conn, "INSERT INTO t VALUES (3)").unwrap();
    jdbc_registry::savepoint_rollback(sp_id).unwrap();

    // Now commit and verify only the pre-savepoint row survived.
    jdbc_registry::savepoint_release(sp_id).unwrap();
    jdbc_registry::commit(conn).unwrap();
    let (stmt_id, _) = jdbc_registry::execute_query(conn, "SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(
        jdbc_registry::get_result(stmt_id, 0, 0).as_deref(),
        Some("1")
    );
    jdbc_registry::free_results(stmt_id);
    jdbc_registry::close_connection(conn);
}

#[test]
fn new14_savepoint_rejects_injected_name() {
    let conn = jdbc_registry::open_connection(":memory:").unwrap();
    jdbc_registry::set_auto_commit(conn, false).unwrap();
    let bad = jdbc_registry::savepoint_create(conn, Some("sp; DROP TABLE t; --".to_string()));
    assert!(bad.is_err());
    let msg = bad.err().unwrap();
    assert!(msg.contains("alphanumeric"));
    jdbc_registry::close_connection(conn);
}

// ---- NEW-14.N5 Driver registry tests ----

#[test]
fn new14_driver_registry_register_list_deregister() {
    jdbc_registry::register_driver("org.example.TestDriver");
    assert!(jdbc_registry::list_drivers()
        .iter()
        .any(|d| d == "org.example.TestDriver"));
    // Idempotent.
    jdbc_registry::register_driver("org.example.TestDriver");
    let count = jdbc_registry::list_drivers()
        .iter()
        .filter(|d| *d == "org.example.TestDriver")
        .count();
    assert_eq!(count, 1);
    // Deregister.
    assert!(jdbc_registry::deregister_driver("org.example.TestDriver"));
    assert!(!jdbc_registry::list_drivers()
        .iter()
        .any(|d| d == "org.example.TestDriver"));
    // Deregistering a missing driver returns false.
    assert!(!jdbc_registry::deregister_driver("nope"));
}

// ---- NEW-14.N4 DatabaseMetaData tests ----

#[test]
fn new14_database_metadata_identifies_sqlite() {
    assert_eq!(jdbc_registry::database_product_name(), "SQLite");
    let version = jdbc_registry::database_product_version();
    // rusqlite's `version()` returns a dotted string like "3.46.0".
    assert!(
        version
            .chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false),
        "version must start with a digit: {version:?}"
    );
    assert_eq!(jdbc_registry::driver_name(), "CratonVM JDBC");
    assert_eq!(jdbc_registry::driver_version(), "1.0");
}

// ---- NEW-14.N1 CallableStatement OUT-parameter registration ----

/// `registerOutParameter` must keep the registration (it used to be a
/// no-op that dropped it), reject an unknown statement, and disappear
/// with the statement.
#[test]
fn new14_register_out_parameter_round_trips() {
    let conn = jdbc_registry::open_connection(":memory:").unwrap();
    let ps = jdbc_registry::prepare(conn, "SELECT ?");

    // (int, int) — java.sql.Types.INTEGER.
    jdbc_registry::register_out_parameter(ps, 1, 4, 0, None).unwrap();
    let p = jdbc_registry::out_parameter(ps, 1).expect("registration recorded");
    assert_eq!(p.sql_type, 4);
    assert_eq!(p.scale, 0);
    assert_eq!(p.type_name, None);

    // (int, int, int) — Types.DECIMAL with a scale.
    jdbc_registry::register_out_parameter(ps, 2, 3, 4, None).unwrap();
    assert_eq!(jdbc_registry::out_parameter(ps, 2).unwrap().scale, 4);

    // (int, int, String) — Types.STRUCT with a SQL type name.
    jdbc_registry::register_out_parameter(ps, 3, 2002, 0, Some("MY_TYPE".to_string())).unwrap();
    let p3 = jdbc_registry::out_parameter(ps, 3).expect("registration recorded");
    assert_eq!(p3.sql_type, 2002);
    assert_eq!(p3.type_name.as_deref(), Some("MY_TYPE"));

    // (String, int) — named parameter.
    jdbc_registry::register_out_parameter_named(ps, "total", 4).unwrap();
    let named = jdbc_registry::out_parameter_named(ps, "total").expect("named registration");
    assert_eq!(named.sql_type, 4);

    // Nothing was registered for index 9.
    assert!(jdbc_registry::out_parameter(ps, 9).is_none());
    // A 1-based index of 0 and an unknown statement are both errors.
    assert!(jdbc_registry::register_out_parameter(ps, 0, 4, 0, None).is_err());
    assert!(jdbc_registry::register_out_parameter(ps + 9999, 1, 4, 0, None).is_err());

    jdbc_registry::free_prepared(ps);
    assert!(jdbc_registry::out_parameter(ps, 1).is_none());
    jdbc_registry::close_connection(conn);
}
