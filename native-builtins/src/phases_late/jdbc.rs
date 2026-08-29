// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `java.sql` JDBC stubs: DriverManager, Connection, Statement, ResultSet, PreparedStatement.
//!
//! Pure code move out of `phases_late.rs` (no logic, signature or ordering
//! changes). Every registration call site is untouched and the per-phase
//! dispatchers stay in the parent module, so the native registration SEQUENCE
//! is byte-identical to before the split.

use super::*;

// =============================================================================
// java.sql JDBC stubs — DriverManager, Connection, Statement, ResultSet,
//                        PreparedStatement, ResultSetMetaData
// =============================================================================

// ---------------------------------------------------------------------------
// JDBC SQLite registry — stores real rusqlite::Connection handles by integer ID.
// Connection synthetic objects store the ID in field 0.
// ---------------------------------------------------------------------------

pub(crate) mod jdbc_registry {
    use std::collections::HashMap;
    use std::sync::OnceLock;

    /// Maximum number of cached result sets before eviction.
    const MAX_CACHED_RESULTS: usize = 1024;
    /// Maximum rows loaded per query to prevent unbounded memory growth.
    const MAX_RESULT_ROWS: usize = 100_000;

    static REGISTRY: OnceLock<parking_lot::Mutex<JdbcRegistry>> = OnceLock::new();

    struct JdbcRegistry {
        next_id: i64,
        connections: HashMap<i64, rusqlite::Connection>,
        /// conn_id → closed flag (true after Connection.close)
        conn_closed: HashMap<i64, bool>,
        /// Cached query results: stmt_id → (rows, column_names, column_types)
        results: HashMap<i64, CachedResult>,
        next_stmt_id: i64,
        /// Prepared statement storage: ps_id → (conn_id, sql, bound params)
        prepared: HashMap<i64, PreparedState>,
        next_ps_id: i64,
        /// stmt_id → was-null flag for ResultSet.wasNull() — set by each get* call.
        last_was_null: HashMap<i64, bool>,
    }

    struct CachedResult {
        rows: Vec<Vec<String>>,
        column_names: Vec<String>,
        /// The DECLARED type of each result column, exactly as SQLite reports
        /// it from `sqlite3_column_decltype` — free text such as `INTEGER`,
        /// `VARCHAR(20)`, `DECIMAL(10,2)`, `unsigned big int`. Empty for a
        /// column that has no declared type at all (an expression, an
        /// aggregate, a literal: `SELECT a+b`, `SELECT count(*)`).
        ///
        /// Kept RAW rather than pre-normalised because the parenthesised
        /// arguments are the only real source we have for `getPrecision` /
        /// `getScale`, and the affinity rules in `type_affinity` are defined
        /// over the raw text.
        column_types: Vec<String>,
        /// The runtime SQLite storage class of each column in the FIRST row
        /// (`INTEGER`/`REAL`/`TEXT`/`BLOB`/`NULL`), or empty when the result
        /// set has no rows. This is the fallback for columns with no declared
        /// type: SQLite itself has no static type for an expression, so the
        /// value actually produced is the only type information that exists.
        column_runtime_types: Vec<String>,
        /// The connection that produced this result. `ResultSetMetaData`
        /// only carries the stmt id, so this is how metadata queries
        /// (`isNullable`) get back to the live schema.
        conn_id: i64,
        /// The SQL text that produced it — read by `column_nullable` to
        /// decide whether an outer join / union could have introduced NULLs
        /// into an otherwise NOT NULL column.
        sql: String,
    }

    struct PreparedState {
        conn_id: i64,
        sql: String,
        params: HashMap<usize, ParamValue>,
        batch: Vec<HashMap<usize, ParamValue>>,
        /// `CallableStatement.registerOutParameter(int, …)` registrations,
        /// keyed by the 1-based parameter index.
        out_params: HashMap<usize, OutParam>,
        /// The same, for the `registerOutParameter(String, …)` overloads.
        out_params_by_name: HashMap<String, OutParam>,
    }

    /// One `CallableStatement.registerOutParameter` registration: the
    /// `java.sql.Types` code the caller asked for plus the optional scale /
    /// SQL type name carried by the three-argument overloads.
    #[derive(Clone, Debug, PartialEq)]
    pub struct OutParam {
        pub sql_type: i32,
        pub scale: i32,
        pub type_name: Option<String>,
    }

    #[derive(Clone)]
    enum ParamValue {
        Text(String),
        Int(i64),
        Real(f64),
        Bool(bool),
        Null,
        Blob(Vec<u8>),
    }

    fn registry() -> &'static parking_lot::Mutex<JdbcRegistry> {
        REGISTRY.get_or_init(|| {
            parking_lot::Mutex::new(JdbcRegistry {
                next_id: 1,
                connections: HashMap::new(),
                conn_closed: HashMap::new(),
                results: HashMap::new(),
                next_stmt_id: 1,
                prepared: HashMap::new(),
                next_ps_id: 1,
                last_was_null: HashMap::new(),
            })
        })
    }

    /// Record whether the most recent get* call on this ResultSet returned a NULL value.
    pub fn set_was_null(stmt_id: i64, was_null: bool) {
        registry().lock().last_was_null.insert(stmt_id, was_null);
    }

    /// Return true if the most recent get* call returned NULL. Defaults to false.
    pub fn was_null(stmt_id: i64) -> bool {
        registry()
            .lock()
            .last_was_null
            .get(&stmt_id)
            .copied()
            .unwrap_or(false)
    }

    fn next_id(current: &mut i64) -> i64 {
        let id = *current;
        *current = current.checked_add(1).unwrap_or_else(|| {
            // Wrap around — extremely unlikely but safe
            *current = 1;
            1
        });
        id
    }

    /// Validate a JDBC URL path to prevent path traversal.
    fn validate_path(path: &str) -> Result<(), String> {
        if path == ":memory:" || path.is_empty() {
            return Ok(());
        }
        // Reject path traversal sequences
        if path.contains("..") {
            return Err("JDBC path traversal rejected: path contains '..'".to_string());
        }
        // Reject absolute paths outside current directory on Unix-like systems
        if path.starts_with('/') && !path.starts_with("/tmp/") {
            return Err(format!(
                "JDBC path rejected: absolute path '{}' not allowed",
                path
            ));
        }
        Ok(())
    }

    /// Sanitize an error message to avoid leaking internal details.
    fn sanitize_error(e: &impl std::fmt::Display) -> String {
        let msg = e.to_string();
        // Strip file system paths from error messages
        if msg.contains('/') || msg.contains('\\') {
            "SQL operation failed".to_string()
        } else {
            format!("SQL error: {}", msg)
        }
    }

    /// Open a SQLite database and return a connection ID.
    pub fn open_connection(url: &str) -> Result<i64, String> {
        // Parse JDBC URL: "jdbc:sqlite:path" or "jdbc:sqlite::memory:"
        let path = if url.starts_with("jdbc:sqlite:") {
            &url["jdbc:sqlite:".len()..]
        } else if url.contains(":memory:") || url.is_empty() {
            ":memory:"
        } else {
            url
        };

        validate_path(path)?;

        let conn = rusqlite::Connection::open(path).map_err(|e| sanitize_error(&e))?;
        let mut reg = registry().lock();
        let id = next_id(&mut reg.next_id);
        reg.connections.insert(id, conn);
        reg.conn_closed.insert(id, false);
        Ok(id)
    }

    /// Close a connection by ID.
    pub fn close_connection(id: i64) {
        let mut reg = registry().lock();
        reg.connections.remove(&id);
        reg.conn_closed.insert(id, true);
    }

    /// Check if a connection is valid (open and not closed).
    pub fn is_valid(id: i64) -> bool {
        let reg = registry().lock();
        reg.connections.contains_key(&id) && !reg.conn_closed.get(&id).copied().unwrap_or(true)
    }

    /// Set auto-commit mode. When turning OFF, begins a deferred transaction.
    /// When turning ON, commits any pending transaction.
    pub fn set_auto_commit(id: i64, auto_commit: bool) -> Result<(), String> {
        let reg = registry().lock();
        let conn = reg
            .connections
            .get(&id)
            .ok_or_else(|| "Connection not found".to_string())?;
        if auto_commit {
            // Turning auto-commit ON: commit any pending transaction
            let _ = conn.execute_batch("COMMIT");
        } else {
            // Turning auto-commit OFF: start an explicit transaction
            conn.execute_batch("BEGIN DEFERRED")
                .map_err(|e| format!("Failed to begin transaction: {}", e))?;
        }
        Ok(())
    }

    /// Commit the current transaction and start a new one.
    pub fn commit(id: i64) -> Result<(), String> {
        let reg = registry().lock();
        let conn = reg
            .connections
            .get(&id)
            .ok_or_else(|| "Connection not found".to_string())?;
        conn.execute_batch("COMMIT; BEGIN DEFERRED")
            .map_err(|e| format!("Commit failed: {}", e))
    }

    /// Roll back the current transaction and start a new one.
    pub fn rollback(id: i64) -> Result<(), String> {
        let reg = registry().lock();
        let conn = reg
            .connections
            .get(&id)
            .ok_or_else(|| "Connection not found".to_string())?;
        conn.execute_batch("ROLLBACK; BEGIN DEFERRED")
            .map_err(|e| format!("Rollback failed: {}", e))
    }

    /// Set transaction isolation level. SQLite supports SERIALIZABLE (default)
    /// and READ_UNCOMMITTED (via pragma).
    pub fn set_isolation(id: i64, level: i32) -> Result<(), String> {
        let reg = registry().lock();
        let conn = reg
            .connections
            .get(&id)
            .ok_or_else(|| "Connection not found".to_string())?;
        // Java isolation levels: 1=READ_UNCOMMITTED, 2=READ_COMMITTED,
        // 4=REPEATABLE_READ, 8=SERIALIZABLE
        let pragma = if level == 1 {
            "PRAGMA read_uncommitted = true"
        } else {
            "PRAGMA read_uncommitted = false"
        };
        conn.execute_batch(pragma)
            .map_err(|e| format!("Set isolation failed: {}", e))
    }

    /// Execute a query and store results. Returns (stmt_id, row_count).
    pub fn execute_query(conn_id: i64, sql: &str) -> Result<(i64, usize), String> {
        let mut reg = registry().lock();

        let (rows, column_names, column_types, column_runtime_types) = {
            let conn = reg
                .connections
                .get(&conn_id)
                .ok_or_else(|| "Connection not found".to_string())?;

            let mut stmt = conn.prepare(sql).map_err(|e| sanitize_error(&e))?;

            let column_count = stmt.column_count();
            let col_names: Vec<String> = (0..column_count)
                .map(|i| stmt.column_name(i).unwrap_or("?").to_string())
                .collect();

            // Real declared types, straight out of `sqlite3_column_decltype`
            // (rusqlite's `column_decltype` feature). `decl_type()` is `None`
            // for a column that has no declared type — an expression, an
            // aggregate or a literal — which we record as the empty string and
            // resolve later against the first row's storage class. This
            // hardcoded `vec!["TEXT"; n]` until 2026-07-28, which made
            // `getColumnType()` report `Types.VARCHAR` for every column of
            // every result set.
            let col_types: Vec<String> = stmt
                .columns()
                .iter()
                .map(|c| c.decl_type().unwrap_or("").to_string())
                .collect();

            let mut col_runtime_types: Vec<String> = vec![String::new(); column_count];
            let mut result_rows: Vec<Vec<String>> = Vec::new();
            let mut rows_iter = stmt.query([]).map_err(|e| sanitize_error(&e))?;

            while let Some(row) = rows_iter.next().map_err(|e| sanitize_error(&e))? {
                if result_rows.len() >= MAX_RESULT_ROWS {
                    break; // Prevent unbounded memory growth
                }
                let first_row = result_rows.is_empty();
                let mut vals = Vec::with_capacity(column_count);
                for i in 0..column_count {
                    if first_row {
                        if let Ok(v) = row.get_ref(i) {
                            col_runtime_types[i] = storage_class_name(v.data_type()).to_string();
                        }
                    }
                    let val: String = row
                        .get::<_, String>(i)
                        .or_else(|_| row.get::<_, i64>(i).map(|v| v.to_string()))
                        .or_else(|_| row.get::<_, f64>(i).map(|v| v.to_string()))
                        .unwrap_or_else(|_| "NULL".to_string());
                    vals.push(val);
                }
                result_rows.push(vals);
            }

            (result_rows, col_names, col_types, col_runtime_types)
        };

        let row_count = rows.len();
        let stmt_id = next_id(&mut reg.next_stmt_id);

        // Evict oldest results if cache is full
        if reg.results.len() >= MAX_CACHED_RESULTS {
            if let Some(&oldest_key) = reg.results.keys().next() {
                reg.results.remove(&oldest_key);
            }
        }

        reg.results.insert(
            stmt_id,
            CachedResult {
                rows,
                column_names,
                column_types,
                column_runtime_types,
                conn_id,
                sql: sql.to_string(),
            },
        );
        Ok((stmt_id, row_count))
    }

    /// Does `sql` yield a ResultSet, i.e. does the compiled statement declare
    /// any result columns?
    ///
    /// `Statement.execute(String)` has to answer that question BEFORE it runs
    /// anything, because the JDBC return value (`true` ⇒ first result is a
    /// ResultSet, `false` ⇒ it is an update count) picks which of
    /// `getResultSet()` / `getUpdateCount()` the caller may then read.
    /// `Connection::prepare` only compiles — it never steps — so probing here
    /// does not execute the statement a second time.
    pub fn produces_result_set(conn_id: i64, sql: &str) -> Result<bool, String> {
        let reg = registry().lock();
        let conn = reg
            .connections
            .get(&conn_id)
            .ok_or_else(|| "Connection not found".to_string())?;
        let stmt = conn.prepare(sql).map_err(|e| sanitize_error(&e))?;
        Ok(stmt.column_count() > 0)
    }

    /// `produces_result_set` for an already-prepared statement — same
    /// compile-only probe, resolved through the stored SQL and connection.
    pub fn prepared_produces_result_set(ps_id: i64) -> Result<bool, String> {
        let reg = registry().lock();
        let ps = reg
            .prepared
            .get(&ps_id)
            .ok_or_else(|| "Prepared statement not found".to_string())?;
        let conn = reg
            .connections
            .get(&ps.conn_id)
            .ok_or_else(|| "Connection not found".to_string())?;
        let stmt = conn.prepare(&ps.sql).map_err(|e| sanitize_error(&e))?;
        Ok(stmt.column_count() > 0)
    }

    /// Execute an update (INSERT/UPDATE/DELETE). Returns rows affected.
    pub fn execute_update(conn_id: i64, sql: &str) -> Result<i32, String> {
        let reg = registry().lock();
        let conn = reg
            .connections
            .get(&conn_id)
            .ok_or_else(|| "Connection not found".to_string())?;
        conn.execute(sql, [])
            .map(|n| n as i32)
            .map_err(|e| sanitize_error(&e))
    }

    // --- PreparedStatement support ---

    /// Create a prepared statement. Returns ps_id.
    pub fn prepare(conn_id: i64, sql: &str) -> i64 {
        let mut reg = registry().lock();
        let id = next_id(&mut reg.next_ps_id);
        reg.prepared.insert(
            id,
            PreparedState {
                conn_id,
                sql: sql.to_string(),
                params: HashMap::new(),
                batch: Vec::new(),
                out_params: HashMap::new(),
                out_params_by_name: HashMap::new(),
            },
        );
        id
    }

    /// Bind a parameter to a prepared statement.
    fn bind_param(ps_id: i64, index: usize, value: ParamValue) {
        let mut reg = registry().lock();
        if let Some(ps) = reg.prepared.get_mut(&ps_id) {
            ps.params.insert(index, value);
        }
    }

    pub fn bind_string(ps_id: i64, index: usize, value: String) {
        bind_param(ps_id, index, ParamValue::Text(value));
    }

    pub fn bind_int(ps_id: i64, index: usize, value: i64) {
        bind_param(ps_id, index, ParamValue::Int(value));
    }

    pub fn bind_double(ps_id: i64, index: usize, value: f64) {
        bind_param(ps_id, index, ParamValue::Real(value));
    }

    pub fn bind_bool(ps_id: i64, index: usize, value: bool) {
        bind_param(ps_id, index, ParamValue::Bool(value));
    }

    pub fn bind_null(ps_id: i64, index: usize) {
        bind_param(ps_id, index, ParamValue::Null);
    }

    pub fn bind_bytes(ps_id: i64, index: usize, value: Vec<u8>) {
        bind_param(ps_id, index, ParamValue::Blob(value));
    }

    pub fn clear_params(ps_id: i64) {
        let mut reg = registry().lock();
        if let Some(ps) = reg.prepared.get_mut(&ps_id) {
            ps.params.clear();
        }
    }

    // --- CallableStatement OUT-parameter registrations ---
    //
    // JDBC requires the driver to REMEMBER the type a caller registers for an
    // OUT parameter — it is the only record that the parameter is an OUT at
    // all. SQLite has no stored procedures, so nothing executes against the
    // registration yet, but the state below is real, survives to
    // `free_prepared`, and is readable through `out_parameter*`; and an
    // unknown statement id / illegal index is now reported instead of being
    // silently swallowed.

    /// Record a `registerOutParameter(int parameterIndex, …)` registration.
    pub fn register_out_parameter(
        ps_id: i64,
        index: usize,
        sql_type: i32,
        scale: i32,
        type_name: Option<String>,
    ) -> Result<(), String> {
        if index == 0 {
            return Err("registerOutParameter: parameter index is 1-based".to_string());
        }
        let mut reg = registry().lock();
        let ps = reg
            .prepared
            .get_mut(&ps_id)
            .ok_or_else(|| "Prepared statement not found".to_string())?;
        ps.out_params.insert(
            index,
            OutParam {
                sql_type,
                scale,
                type_name,
            },
        );
        Ok(())
    }

    /// Record a `registerOutParameter(String parameterName, int sqlType)`
    /// registration.
    pub fn register_out_parameter_named(
        ps_id: i64,
        name: &str,
        sql_type: i32,
    ) -> Result<(), String> {
        register_out_parameter_named_ext(ps_id, name, sql_type, 0, None)
    }

    /// The same, for the two three-argument named overloads —
    /// `(String, int, int)` carries a scale, `(String, int, String)` carries
    /// a SQL type name. Both are part of `CallableStatement` and were
    /// previously unregistered, so a caller using them got the class's
    /// (abstract / absent) body rather than a recorded registration.
    pub fn register_out_parameter_named_ext(
        ps_id: i64,
        name: &str,
        sql_type: i32,
        scale: i32,
        type_name: Option<String>,
    ) -> Result<(), String> {
        if name.is_empty() {
            return Err("registerOutParameter: parameter name is empty".to_string());
        }
        let mut reg = registry().lock();
        let ps = reg
            .prepared
            .get_mut(&ps_id)
            .ok_or_else(|| "Prepared statement not found".to_string())?;
        ps.out_params_by_name.insert(
            name.to_string(),
            OutParam {
                sql_type,
                scale,
                type_name,
            },
        );
        Ok(())
    }

    /// The registration made for a 1-based parameter index, if any.
    pub fn out_parameter(ps_id: i64, index: usize) -> Option<OutParam> {
        let reg = registry().lock();
        reg.prepared
            .get(&ps_id)
            .and_then(|ps| ps.out_params.get(&index).cloned())
    }

    /// The registration made for a named parameter, if any.
    pub fn out_parameter_named(ps_id: i64, name: &str) -> Option<OutParam> {
        let reg = registry().lock();
        reg.prepared
            .get(&ps_id)
            .and_then(|ps| ps.out_params_by_name.get(name).cloned())
    }

    /// Execute a prepared query. Returns (stmt_id, row_count).
    pub fn execute_prepared_query(ps_id: i64) -> Result<(i64, usize), String> {
        let mut reg = registry().lock();
        let (conn_id, sql, params) = {
            let ps = reg
                .prepared
                .get(&ps_id)
                .ok_or_else(|| "PreparedStatement not found".to_string())?;
            (ps.conn_id, ps.sql.clone(), ps.params.clone())
        };

        let (rows, column_names, column_types, column_runtime_types) = {
            let conn = reg
                .connections
                .get(&conn_id)
                .ok_or_else(|| "Connection not found".to_string())?;
            let mut stmt = conn.prepare(&sql).map_err(|e| sanitize_error(&e))?;

            // Bind parameters
            for (&idx, val) in &params {
                match val {
                    ParamValue::Text(s) => {
                        let _ = stmt.raw_bind_parameter(idx, s.as_str());
                    }
                    ParamValue::Int(v) => {
                        let _ = stmt.raw_bind_parameter(idx, *v);
                    }
                    ParamValue::Real(v) => {
                        let _ = stmt.raw_bind_parameter(idx, *v);
                    }
                    ParamValue::Bool(v) => {
                        let _ = stmt.raw_bind_parameter(idx, *v as i32);
                    }
                    ParamValue::Null => {
                        let _ = stmt.raw_bind_parameter(idx, rusqlite::types::Null);
                    }
                    ParamValue::Blob(b) => {
                        let _ = stmt.raw_bind_parameter(idx, b.as_slice());
                    }
                }
            }

            let column_count = stmt.column_count();
            let col_names: Vec<String> = (0..column_count)
                .map(|i| stmt.column_name(i).unwrap_or("?").to_string())
                .collect();
            // Same real-decltype read as `execute_query` — see the comment
            // there. Both paths hardcoded `vec!["TEXT"; n]` until 2026-07-28.
            let col_types: Vec<String> = stmt
                .columns()
                .iter()
                .map(|c| c.decl_type().unwrap_or("").to_string())
                .collect();

            let mut col_runtime_types: Vec<String> = vec![String::new(); column_count];
            let mut result_rows: Vec<Vec<String>> = Vec::new();
            let mut rows_iter = stmt.raw_query();
            while let Some(row) = rows_iter.next().map_err(|e| sanitize_error(&e))? {
                if result_rows.len() >= MAX_RESULT_ROWS {
                    break;
                }
                let first_row = result_rows.is_empty();
                let mut vals = Vec::with_capacity(column_count);
                for i in 0..column_count {
                    if first_row {
                        if let Ok(v) = row.get_ref(i) {
                            col_runtime_types[i] = storage_class_name(v.data_type()).to_string();
                        }
                    }
                    let val: String = row
                        .get::<_, String>(i)
                        .or_else(|_| row.get::<_, i64>(i).map(|v| v.to_string()))
                        .or_else(|_| row.get::<_, f64>(i).map(|v| v.to_string()))
                        .unwrap_or_else(|_| "NULL".to_string());
                    vals.push(val);
                }
                result_rows.push(vals);
            }

            (result_rows, col_names, col_types, col_runtime_types)
        };

        let row_count = rows.len();
        let stmt_id = next_id(&mut reg.next_stmt_id);
        if reg.results.len() >= MAX_CACHED_RESULTS {
            if let Some(&oldest_key) = reg.results.keys().next() {
                reg.results.remove(&oldest_key);
            }
        }
        reg.results.insert(
            stmt_id,
            CachedResult {
                rows,
                column_names,
                column_types,
                column_runtime_types,
                conn_id,
                sql,
            },
        );
        Ok((stmt_id, row_count))
    }

    /// Execute a prepared update. Returns rows affected.
    pub fn execute_prepared_update(ps_id: i64) -> Result<i32, String> {
        let reg = registry().lock();
        let (conn_id, sql, params) = {
            let ps = reg
                .prepared
                .get(&ps_id)
                .ok_or_else(|| "PreparedStatement not found".to_string())?;
            (ps.conn_id, ps.sql.clone(), ps.params.clone())
        };
        let conn = reg
            .connections
            .get(&conn_id)
            .ok_or_else(|| "Connection not found".to_string())?;
        let mut stmt = conn.prepare(&sql).map_err(|e| sanitize_error(&e))?;
        for (&idx, val) in &params {
            match val {
                ParamValue::Text(s) => {
                    let _ = stmt.raw_bind_parameter(idx, s.as_str());
                }
                ParamValue::Int(v) => {
                    let _ = stmt.raw_bind_parameter(idx, *v);
                }
                ParamValue::Real(v) => {
                    let _ = stmt.raw_bind_parameter(idx, *v);
                }
                ParamValue::Bool(v) => {
                    let _ = stmt.raw_bind_parameter(idx, *v as i32);
                }
                ParamValue::Null => {
                    let _ = stmt.raw_bind_parameter(idx, rusqlite::types::Null);
                }
                ParamValue::Blob(b) => {
                    let _ = stmt.raw_bind_parameter(idx, b.as_slice());
                }
            }
        }
        stmt.raw_execute()
            .map(|n| n as i32)
            .map_err(|e| sanitize_error(&e))
    }

    pub fn free_prepared(ps_id: i64) {
        let mut reg = registry().lock();
        reg.prepared.remove(&ps_id);
    }

    /// Add current parameters as a batch entry for a PreparedStatement.
    pub fn add_batch(ps_id: i64) {
        let mut reg = registry().lock();
        if let Some(ps) = reg.prepared.get_mut(&ps_id) {
            ps.batch.push(ps.params.clone());
            ps.params.clear();
        }
    }

    /// Execute all batched parameter sets for a PreparedStatement.
    /// Returns a Vec of update counts (one per batch entry).
    pub fn execute_batch(ps_id: i64) -> Result<Vec<i32>, String> {
        let mut reg = registry().lock();
        let ps = reg
            .prepared
            .get_mut(&ps_id)
            .ok_or_else(|| "PreparedStatement not found".to_string())?;
        let conn_id = ps.conn_id;
        let sql = ps.sql.clone();
        let batches = std::mem::take(&mut ps.batch);

        let conn = reg
            .connections
            .get(&conn_id)
            .ok_or_else(|| "Connection not found".to_string())?;

        let mut counts = Vec::with_capacity(batches.len());
        for params in &batches {
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| format!("Batch prepare failed: {}", e))?;
            for (idx, pv) in params {
                match pv {
                    ParamValue::Text(s) => {
                        let _ = stmt.raw_bind_parameter(*idx, s.as_str());
                    }
                    ParamValue::Int(v) => {
                        let _ = stmt.raw_bind_parameter(*idx, *v);
                    }
                    ParamValue::Real(v) => {
                        let _ = stmt.raw_bind_parameter(*idx, *v);
                    }
                    ParamValue::Bool(v) => {
                        let _ = stmt.raw_bind_parameter(*idx, *v as i32);
                    }
                    ParamValue::Null => {
                        let _ = stmt.raw_bind_parameter(*idx, rusqlite::types::Null);
                    }
                    ParamValue::Blob(v) => {
                        let _ = stmt.raw_bind_parameter(*idx, v.as_slice());
                    }
                }
            }
            let changed =
                stmt.raw_execute()
                    .map_err(|e| format!("Batch execute failed: {}", e))? as i32;
            counts.push(changed);
        }
        Ok(counts)
    }

    /// Execute a batch of SQL strings (for Statement.addBatch/executeBatch).
    pub fn execute_sql_batch(conn_id: i64, sqls: &[String]) -> Result<Vec<i32>, String> {
        let reg = registry().lock();
        let conn = reg
            .connections
            .get(&conn_id)
            .ok_or_else(|| "Connection not found".to_string())?;
        let mut counts = Vec::with_capacity(sqls.len());
        for sql in sqls {
            let changed = conn
                .execute(sql, [])
                .map_err(|e| format!("Batch SQL failed: {}", e))? as i32;
            counts.push(changed);
        }
        Ok(counts)
    }

    // --- Result access methods ---

    pub fn get_result(stmt_id: i64, row: usize, col: usize) -> Option<String> {
        let reg = registry().lock();
        reg.results
            .get(&stmt_id)
            .and_then(|r| r.rows.get(row))
            .and_then(|r| r.get(col))
            .cloned()
    }

    pub fn get_column_count(stmt_id: i64) -> usize {
        let reg = registry().lock();
        reg.results
            .get(&stmt_id)
            .map(|r| r.column_names.len())
            .unwrap_or(0)
    }

    pub fn get_column_name(stmt_id: i64, col: usize) -> String {
        let reg = registry().lock();
        reg.results
            .get(&stmt_id)
            .and_then(|r| r.column_names.get(col))
            .cloned()
            .unwrap_or_else(|| "?".to_string())
    }

    /// SQLite's five storage classes, spelled the way `typeof()` and every
    /// SQLite JDBC driver spell them.
    fn storage_class_name(t: rusqlite::types::Type) -> &'static str {
        match t {
            rusqlite::types::Type::Null => "NULL",
            rusqlite::types::Type::Integer => "INTEGER",
            rusqlite::types::Type::Real => "REAL",
            rusqlite::types::Type::Text => "TEXT",
            rusqlite::types::Type::Blob => "BLOB",
        }
    }

    /// The five type affinities SQLite can assign to a column.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Affinity {
        Integer,
        Text,
        Blob,
        Real,
        Numeric,
    }

    /// SQLite's documented "Determination Of Column Affinity" algorithm
    /// (datatype3.html §3.1), applied verbatim and IN ORDER to the raw
    /// declared type. Every rule is a case-insensitive substring test:
    ///
    ///   1. contains "INT"                          → INTEGER
    ///   2. else contains "CHAR", "CLOB" or "TEXT"  → TEXT
    ///   3. else contains "BLOB", or is empty       → BLOB ("no affinity")
    ///   4. else contains "REAL", "FLOA" or "DOUB"  → REAL
    ///   5. else                                    → NUMERIC
    ///
    /// The order is load-bearing, not stylistic: SQLite's own worked examples
    /// depend on it. `VARCHAR(255)` stops at rule 2; `unsigned big int` stops
    /// at rule 1; `FLOATING POINT` stops at rule 1 as well (it contains "INT")
    /// even though it looks like a rule-4 name; `STRING`, `DATETIME` and
    /// `POINT` fall through to rule 5.
    fn type_affinity(decl_type: &str) -> Affinity {
        let d = decl_type.to_uppercase();
        if d.contains("INT") {
            Affinity::Integer
        } else if d.contains("CHAR") || d.contains("CLOB") || d.contains("TEXT") {
            Affinity::Text
        } else if d.contains("BLOB") || d.trim().is_empty() {
            Affinity::Blob
        } else if d.contains("REAL") || d.contains("FLOA") || d.contains("DOUB") {
            Affinity::Real
        } else {
            Affinity::Numeric
        }
    }

    /// A declared type's base name — everything before the argument list,
    /// trimmed and uppercased. `VARCHAR(20)` → `VARCHAR`,
    /// `DECIMAL(10, 2)` → `DECIMAL`, `unsigned big int` → `UNSIGNED BIG INT`.
    fn decl_base_name(decl_type: &str) -> String {
        decl_type
            .split('(')
            .next()
            .unwrap_or("")
            .trim()
            .to_uppercase()
    }

    /// The `(precision)` / `(precision, scale)` arguments of a declared type:
    /// `DECIMAL(10,2)` → `(Some(10), Some(2))`, `VARCHAR(20)` →
    /// `(Some(20), None)`, `INTEGER` → `(None, None)`. `None` means the
    /// schema states no such number — JDBC's "not applicable" — and is never
    /// papered over with a default.
    fn decl_args(decl_type: &str) -> (Option<i32>, Option<i32>) {
        let open = match decl_type.find('(') {
            Some(i) => i,
            None => return (None, None),
        };
        let close = match decl_type[open..].find(')') {
            Some(i) => open + i,
            None => return (None, None),
        };
        let mut parts = decl_type[open + 1..close].split(',');
        let precision = parts.next().and_then(|s| s.trim().parse::<i32>().ok());
        let scale = parts.next().and_then(|s| s.trim().parse::<i32>().ok());
        (precision, scale)
    }

    /// The type string that describes result column `col`: the DECLARED type
    /// when the column has one, else the storage class the first row's value
    /// actually had, else "" — which is SQLite's genuine "no type information
    /// exists" answer for an expression over an empty result set.
    ///
    /// One lock acquisition, and no computation performed under it, so the
    /// public accessors below can be composed freely without re-entering the
    /// non-reentrant registry mutex.
    fn effective_type(stmt_id: i64, col: usize) -> String {
        let reg = registry().lock();
        let r = match reg.results.get(&stmt_id) {
            Some(r) => r,
            None => return String::new(),
        };
        let declared = r.column_types.get(col).cloned().unwrap_or_default();
        if !declared.trim().is_empty() {
            return declared;
        }
        r.column_runtime_types.get(col).cloned().unwrap_or_default()
    }

    // `java.sql.Types` codes used below. Every value is fixed by the JDBC
    // specification and matches the JDK's `java.sql.Types` fields exactly
    // (the same constants `register_p68_jdbc` publishes on `java/sql/Types`).
    const TYPES_NULL: i32 = 0;
    const TYPES_CHAR: i32 = 1;
    const TYPES_NUMERIC: i32 = 2;
    const TYPES_DECIMAL: i32 = 3;
    const TYPES_INTEGER: i32 = 4;
    const TYPES_SMALLINT: i32 = 5;
    const TYPES_DOUBLE: i32 = 8;
    const TYPES_VARCHAR: i32 = 12;
    const TYPES_BOOLEAN: i32 = 16;
    const TYPES_TINYINT: i32 = -6;
    const TYPES_BIGINT: i32 = -5;
    const TYPES_BLOB: i32 = 2004;
    const TYPES_CLOB: i32 = 2005;

    /// `ResultSetMetaData.getColumnTypeName(col)` — the database-specific type
    /// name for the column.
    ///
    /// Reports the declared type's base name when the column has a declared
    /// type (`VARCHAR(20)` → `VARCHAR`); the storage class of the first row's
    /// value when it does not (`SELECT a+b` over a non-empty result →
    /// `INTEGER`); and `BLOB` when neither exists, because SQLite's "no
    /// affinity" IS BLOB affinity (datatype3.html §3.1 rule 3).
    ///
    /// Until 2026-07-28 this returned `TEXT` for every column of every result
    /// set: both execute paths filled `column_types` with `vec!["TEXT"; n]`
    /// and never asked SQLite for a declared type at all.
    pub fn get_column_type_name(stmt_id: i64, col: usize) -> String {
        let eff = effective_type(stmt_id, col);
        if eff.trim().is_empty() {
            return "BLOB".to_string();
        }
        decl_base_name(&eff)
    }

    /// `ResultSetMetaData.getColumnType(col)` — the `java.sql.Types` code.
    pub fn get_column_type_code(stmt_id: i64, col: usize) -> i32 {
        type_code_for(&effective_type(stmt_id, col))
    }

    /// Map one declared (or observed) SQLite type string onto a
    /// `java.sql.Types` code through the affinity rules. Split out from
    /// `get_column_type_code` so it is pure — no database, no registry lock.
    fn type_code_for(sqlite_type: &str) -> i32 {
        let upper = sqlite_type.to_uppercase();
        // A first-row storage class of NULL means the value IS SQL NULL and
        // the column declared nothing; Types.NULL is that exact situation.
        if upper.trim() == "NULL" {
            return TYPES_NULL;
        }
        match type_affinity(sqlite_type) {
            // SQLite stores every integer as a signed 64-bit value, but a
            // declared width is real schema information the caller asked for,
            // so it is honoured when the schema states one.
            Affinity::Integer => {
                if upper.contains("BIGINT") {
                    TYPES_BIGINT
                } else if upper.contains("SMALLINT") {
                    TYPES_SMALLINT
                } else if upper.contains("TINYINT") {
                    TYPES_TINYINT
                } else {
                    TYPES_INTEGER
                }
            }
            Affinity::Text => {
                if upper.contains("CLOB") {
                    TYPES_CLOB
                } else if matches!(
                    decl_base_name(sqlite_type).as_str(),
                    "CHAR" | "NCHAR" | "CHARACTER" | "NATIONAL CHARACTER"
                ) {
                    TYPES_CHAR
                } else {
                    TYPES_VARCHAR
                }
            }
            // "No affinity": a declared BLOB, or nothing declared at all.
            Affinity::Blob => TYPES_BLOB,
            // Every REAL-affinity value is stored as an 8-byte IEEE 754
            // double, so DOUBLE is the accurate code for REAL / FLOAT /
            // DOUBLE alike — none of them is a 4-byte float in SQLite.
            Affinity::Real => TYPES_DOUBLE,
            Affinity::Numeric => {
                if upper.contains("BOOL") {
                    TYPES_BOOLEAN
                } else if upper.contains("DECIMAL") {
                    TYPES_DECIMAL
                } else {
                    TYPES_NUMERIC
                }
            }
        }
    }

    /// `ResultSetMetaData.getColumnClassName(col)` — the fully-qualified name
    /// of the class `ResultSet.getObject` would hand back for this column.
    /// Derived from the same code `getColumnType` reports, so the two can
    /// never disagree with each other.
    pub fn get_column_class_name(stmt_id: i64, col: usize) -> String {
        let name = match get_column_type_code(stmt_id, col) {
            TYPES_BIGINT => "java.lang.Long",
            TYPES_INTEGER | TYPES_SMALLINT | TYPES_TINYINT => "java.lang.Integer",
            TYPES_DOUBLE => "java.lang.Double",
            TYPES_NUMERIC | TYPES_DECIMAL => "java.math.BigDecimal",
            TYPES_BOOLEAN => "java.lang.Boolean",
            TYPES_CHAR | TYPES_VARCHAR | TYPES_CLOB => "java.lang.String",
            TYPES_BLOB => "[B",
            // Types.NULL — the column produced SQL NULL and declared nothing,
            // so no more specific class can be promised.
            _ => "java.lang.Object",
        };
        name.to_string()
    }

    /// `ResultSetMetaData.getPrecision(col)` — the column's specified size:
    /// maximum decimal digits for a numeric column, characters for a
    /// character column, and 0 where the size "is not applicable" (JDBC).
    ///
    /// Real sources only, in order: the declared `(precision)` argument
    /// (`DECIMAL(10,2)` → 10, `VARCHAR(20)` → 20); otherwise the width
    /// SQLite's own storage imposes — 19 decimal digits for an INTEGER (i64)
    /// and 15 significant digits for a REAL (IEEE 754 double). A TEXT or BLOB
    /// column with no declared length genuinely has no bound, so it reports
    /// the spec's not-applicable 0 rather than an invented ceiling.
    pub fn get_column_precision(stmt_id: i64, col: usize) -> i32 {
        let eff = effective_type(stmt_id, col);
        if let (Some(p), _) = decl_args(&eff) {
            if p >= 0 {
                return p;
            }
        }
        match type_affinity(&eff) {
            Affinity::Integer => 19,
            Affinity::Real => 15,
            _ => 0,
        }
    }

    /// `ResultSetMetaData.getScale(col)` — digits to the right of the decimal
    /// point. Only a declared `(precision, scale)` argument list can answer
    /// this: SQLite keeps no scale of its own, and a value's runtime storage
    /// class does not carry one. 0 otherwise, the spec's "not applicable".
    pub fn get_column_scale(stmt_id: i64, col: usize) -> i32 {
        match decl_args(&effective_type(stmt_id, col)).1 {
            Some(s) if s >= 0 => s,
            _ => 0,
        }
    }

    /// `ResultSetMetaData.isSigned(col)` — true when the column holds a signed
    /// number. SQLite's INTEGER, REAL and NUMERIC affinities are all signed;
    /// there is no unsigned storage class at all (`unsigned big int` has
    /// INTEGER affinity and still holds negatives). TEXT and BLOB are not
    /// numeric, and BOOLEAN is not signed, so those report false.
    pub fn is_column_signed(stmt_id: i64, col: usize) -> bool {
        matches!(
            get_column_type_code(stmt_id, col),
            TYPES_INTEGER
                | TYPES_BIGINT
                | TYPES_SMALLINT
                | TYPES_TINYINT
                | TYPES_DOUBLE
                | TYPES_NUMERIC
                | TYPES_DECIMAL
        )
    }

    /// `ResultSetMetaData.getColumnDisplaySize(col)` — the maximum number of
    /// characters needed to display a value from this column.
    ///
    /// Answered from real data, in order: the declared length when the schema
    /// states one (`VARCHAR(20)` → 20); otherwise the widest value actually
    /// present in this result set, which the row cache already holds as
    /// rendered text; otherwise 0. Nothing is guessed from the type for the
    /// empty-result case.
    pub fn get_column_display_size(stmt_id: i64, col: usize) -> i32 {
        let eff = effective_type(stmt_id, col);
        if let (Some(p), _) = decl_args(&eff) {
            if p > 0 {
                return p;
            }
        }
        let reg = registry().lock();
        let result = match reg.results.get(&stmt_id) {
            Some(r) => r,
            None => return 0,
        };
        result
            .rows
            .iter()
            .filter_map(|row| row.get(col))
            .map(|v| v.chars().count())
            .max()
            .unwrap_or(0) as i32
    }

    /// `ResultSetMetaData.columnNoNulls` / `columnNullable`.
    const COLUMN_NO_NULLS: i32 = 0;
    const COLUMN_NULLABLE: i32 = 1;

    /// Answer `ResultSetMetaData.isNullable(col)` from the live SQLite
    /// schema instead of assuming "nullable".
    ///
    /// SQLite records the per-column NOT NULL constraint and exposes it
    /// through the `pragma_table_info` table-valued function. We report
    /// `columnNoNulls` only when both hold:
    ///   * the producing statement has no JOIN and no UNION — an outer join
    ///     or a union with a nullable arm can put NULLs into a column that
    ///     is declared NOT NULL in its base table; and
    ///   * every table in the schema that declares a column of this name
    ///     declares it NOT NULL (we do not know which table a result column
    ///     came from, so unanimity is the only sound test).
    /// Anything else — computed columns, aliases, unknown ids, a pragma
    /// that will not compile — keeps the permissive `columnNullable`, which
    /// is what a caller has to assume anyway.
    pub fn column_nullable(stmt_id: i64, col: usize) -> i32 {
        let reg = registry().lock();
        let result = match reg.results.get(&stmt_id) {
            Some(r) => r,
            None => return COLUMN_NULLABLE,
        };
        let name = match result.column_names.get(col) {
            Some(n) => n.clone(),
            None => return COLUMN_NULLABLE,
        };
        let upper = result.sql.to_uppercase();
        if upper.contains("JOIN") || upper.contains("UNION") {
            return COLUMN_NULLABLE;
        }
        let conn = match reg.connections.get(&result.conn_id) {
            Some(c) => c,
            None => return COLUMN_NULLABLE,
        };
        let mut stmt = match conn.prepare(
            "SELECT ti.\"notnull\" FROM sqlite_master AS m, \
             pragma_table_info(m.name) AS ti \
             WHERE m.type = 'table' AND ti.name = ?1",
        ) {
            Ok(s) => s,
            Err(_) => return COLUMN_NULLABLE,
        };
        let mut rows = match stmt.query([name.as_str()]) {
            Ok(r) => r,
            Err(_) => return COLUMN_NULLABLE,
        };
        let mut seen = false;
        while let Ok(Some(row)) = rows.next() {
            seen = true;
            if row.get::<_, i64>(0).unwrap_or(0) == 0 {
                return COLUMN_NULLABLE;
            }
        }
        if seen {
            COLUMN_NO_NULLS
        } else {
            COLUMN_NULLABLE
        }
    }

    /// `DatabaseMetaData.isReadOnly()` for a connection id.
    ///
    /// `open_connection` always opens read/write (`Connection::open`), so
    /// SQLite's `query_only` pragma is the only way a connection of ours can
    /// become read-only — and it is real per-connection state, not a guess.
    pub fn is_read_only(conn_id: i64) -> bool {
        let reg = registry().lock();
        let conn = match reg.connections.get(&conn_id) {
            Some(c) => c,
            None => return false,
        };
        conn.query_row("PRAGMA query_only", [], |row| row.get::<_, i64>(0))
            .map(|v| v != 0)
            .unwrap_or(false)
    }

    pub fn get_row_count(stmt_id: i64) -> usize {
        let reg = registry().lock();
        reg.results.get(&stmt_id).map(|r| r.rows.len()).unwrap_or(0)
    }

    /// Free cached results for a result set.
    pub fn free_results(stmt_id: i64) {
        let mut reg = registry().lock();
        reg.results.remove(&stmt_id);
    }

    // =====================================================================
    // NEW-14 extensions: Blob / Clob / Savepoint / registered drivers
    // =====================================================================

    /// Second-level registry for NEW-14 types. Kept in a separate mutex
    /// so the primary `registry()` lock is never held while we mutate
    /// NEW-14 state — avoids lock-order issues in the common case.
    static NEW14: OnceLock<parking_lot::Mutex<New14Registry>> = OnceLock::new();

    struct New14Registry {
        /// `java.sql.Blob` storage: id → bytes. IDs start at 1.
        blobs: HashMap<i64, Vec<u8>>,
        /// `java.sql.Clob` storage: id → text.
        clobs: HashMap<i64, String>,
        /// Monotonic counter for blob/clob IDs (shared). Wraps to 1 on
        /// overflow.
        next_lob_id: i64,
        /// `java.sql.Savepoint` tracking: id → (conn_id, name). The
        /// name is what we pass to the underlying SAVEPOINT/RELEASE/
        /// ROLLBACK TO SQL commands.
        savepoints: HashMap<i64, (i64, String)>,
        next_savepoint_id: i64,
        /// Registered JDBC drivers (NEW-14.N5). A driver is identified
        /// by its Java class name. The `DriverManager.registerDriver`
        /// native adds an entry; `getDrivers` enumerates them. Because
        /// our DriverManager natively handles `jdbc:sqlite:` URLs
        /// regardless of registered drivers, this list is primarily
        /// used for parity with the JDBC 4.0 discovery API.
        registered_drivers: Vec<String>,
    }

    fn new14() -> &'static parking_lot::Mutex<New14Registry> {
        NEW14.get_or_init(|| {
            parking_lot::Mutex::new(New14Registry {
                blobs: HashMap::new(),
                clobs: HashMap::new(),
                next_lob_id: 1,
                savepoints: HashMap::new(),
                next_savepoint_id: 1,
                registered_drivers: Vec::new(),
            })
        })
    }

    // ----- Blob -------------------------------------------------------

    /// Allocate a new Blob with the given initial bytes and return its
    /// opaque integer ID.
    pub fn blob_create(bytes: Vec<u8>) -> i64 {
        let mut reg = new14().lock();
        let id = reg.next_lob_id;
        reg.next_lob_id = reg.next_lob_id.checked_add(1).unwrap_or(1);
        reg.blobs.insert(id, bytes);
        id
    }

    /// Return the byte length of the blob, or 0 if the ID is unknown
    /// or the blob has been freed.
    pub fn blob_length(id: i64) -> i64 {
        new14()
            .lock()
            .blobs
            .get(&id)
            .map(|v| v.len() as i64)
            .unwrap_or(0)
    }

    /// Read a range `[pos, pos+len)` from the blob.
    ///
    /// Per `java.sql.Blob.getBytes(long pos, int length)`, `pos` is
    /// 1-based. A range outside the stored bytes clips to the
    /// available region (never panics).
    pub fn blob_get_bytes(id: i64, pos: i64, length: i32) -> Vec<u8> {
        let reg = new14().lock();
        let bytes = match reg.blobs.get(&id) {
            Some(b) => b,
            None => return Vec::new(),
        };
        if pos < 1 || length < 0 {
            return Vec::new();
        }
        let start = (pos - 1) as usize;
        if start >= bytes.len() {
            return Vec::new();
        }
        let end = start.saturating_add(length as usize).min(bytes.len());
        bytes[start..end].to_vec()
    }

    /// Write `data` into the blob starting at 1-based `pos`. Extends
    /// the blob if necessary. Returns the number of bytes actually
    /// written (always `data.len()` on success; 0 if the blob is
    /// missing or `pos < 1`).
    pub fn blob_set_bytes(id: i64, pos: i64, data: &[u8]) -> i32 {
        if pos < 1 {
            return 0;
        }
        let mut reg = new14().lock();
        let blob = match reg.blobs.get_mut(&id) {
            Some(b) => b,
            None => return 0,
        };
        let start = (pos - 1) as usize;
        let end = start + data.len();
        if end > blob.len() {
            blob.resize(end, 0);
        }
        blob[start..end].copy_from_slice(data);
        data.len() as i32
    }

    /// Truncate the blob to `length` bytes. No-op if `length` is out
    /// of range or the blob is missing.
    pub fn blob_truncate(id: i64, length: i64) {
        if length < 0 {
            return;
        }
        let mut reg = new14().lock();
        if let Some(blob) = reg.blobs.get_mut(&id) {
            blob.truncate(length as usize);
        }
    }

    /// Drop the blob, releasing its backing storage. Per
    /// `java.sql.Blob.free`, subsequent operations on this id are
    /// permitted to return default values (our getters already handle
    /// the missing-id case gracefully).
    pub fn blob_free(id: i64) {
        new14().lock().blobs.remove(&id);
    }

    // ----- Clob -------------------------------------------------------

    pub fn clob_create(text: String) -> i64 {
        let mut reg = new14().lock();
        let id = reg.next_lob_id;
        reg.next_lob_id = reg.next_lob_id.checked_add(1).unwrap_or(1);
        reg.clobs.insert(id, text);
        id
    }

    /// Character-length of the clob.
    pub fn clob_length(id: i64) -> i64 {
        new14()
            .lock()
            .clobs
            .get(&id)
            .map(|s| s.chars().count() as i64)
            .unwrap_or(0)
    }

    /// Read a substring with 1-based `pos`.
    pub fn clob_get_substring(id: i64, pos: i64, length: i32) -> String {
        let reg = new14().lock();
        let s = match reg.clobs.get(&id) {
            Some(s) => s,
            None => return String::new(),
        };
        if pos < 1 || length < 0 {
            return String::new();
        }
        let start_char = (pos - 1) as usize;
        s.chars().skip(start_char).take(length as usize).collect()
    }

    /// Overwrite chars starting at 1-based `pos`. Extends the clob if
    /// necessary. Returns the number of chars written.
    pub fn clob_set_string(id: i64, pos: i64, s: &str) -> i32 {
        if pos < 1 {
            return 0;
        }
        let start_char = (pos - 1) as usize;
        let mut reg = new14().lock();
        let existing = match reg.clobs.get_mut(&id) {
            Some(c) => c,
            None => return 0,
        };
        let existing_chars: Vec<char> = existing.chars().collect();
        let new_chars: Vec<char> = s.chars().collect();
        let end_char = start_char + new_chars.len();
        let mut merged: Vec<char> = Vec::with_capacity(end_char.max(existing_chars.len()));
        for i in 0..start_char.min(existing_chars.len()) {
            merged.push(existing_chars[i]);
        }
        while merged.len() < start_char {
            merged.push('\0');
        }
        merged.extend_from_slice(&new_chars);
        if end_char < existing_chars.len() {
            merged.extend_from_slice(&existing_chars[end_char..]);
        }
        *existing = merged.iter().collect();
        new_chars.len() as i32
    }

    pub fn clob_truncate(id: i64, length: i64) {
        if length < 0 {
            return;
        }
        let mut reg = new14().lock();
        if let Some(c) = reg.clobs.get_mut(&id) {
            let trimmed: String = c.chars().take(length as usize).collect();
            *c = trimmed;
        }
    }

    pub fn clob_free(id: i64) {
        new14().lock().clobs.remove(&id);
    }

    // ----- Savepoint --------------------------------------------------

    /// Create a new savepoint on `conn_id`. Returns the savepoint's
    /// opaque id and its effective name.
    pub fn savepoint_create(conn_id: i64, name: Option<String>) -> Result<(i64, String), String> {
        let reg = registry().lock();
        let conn = reg
            .connections
            .get(&conn_id)
            .ok_or_else(|| "Connection not found".to_string())?;
        let mut n14 = new14().lock();
        let id = n14.next_savepoint_id;
        n14.next_savepoint_id = n14.next_savepoint_id.checked_add(1).unwrap_or(1);
        let effective = name.unwrap_or_else(|| format!("sp_{id}"));
        if !effective
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err("Savepoint name must be ASCII alphanumeric or underscore".to_string());
        }
        let sql = format!("SAVEPOINT {effective}");
        conn.execute_batch(&sql).map_err(|e| sanitize_error(&e))?;
        n14.savepoints.insert(id, (conn_id, effective.clone()));
        Ok((id, effective))
    }

    /// Roll the transaction back to the named savepoint. Per JDBC,
    /// this does NOT end the transaction.
    pub fn savepoint_rollback(savepoint_id: i64) -> Result<(), String> {
        let (conn_id, name) = {
            let n14 = new14().lock();
            match n14.savepoints.get(&savepoint_id) {
                Some(v) => v.clone(),
                None => return Err("Savepoint not found".to_string()),
            }
        };
        let reg = registry().lock();
        let conn = reg
            .connections
            .get(&conn_id)
            .ok_or_else(|| "Connection not found".to_string())?;
        let sql = format!("ROLLBACK TO SAVEPOINT {name}");
        conn.execute_batch(&sql).map_err(|e| sanitize_error(&e))
    }

    /// Release the savepoint.
    pub fn savepoint_release(savepoint_id: i64) -> Result<(), String> {
        let (conn_id, name) = {
            let mut n14 = new14().lock();
            match n14.savepoints.remove(&savepoint_id) {
                Some(v) => v,
                None => return Err("Savepoint not found".to_string()),
            }
        };
        let reg = registry().lock();
        let conn = reg
            .connections
            .get(&conn_id)
            .ok_or_else(|| "Connection not found".to_string())?;
        let sql = format!("RELEASE SAVEPOINT {name}");
        conn.execute_batch(&sql).map_err(|e| sanitize_error(&e))
    }

    /// Look up the effective name for a savepoint id.
    pub fn savepoint_name(savepoint_id: i64) -> Option<String> {
        new14()
            .lock()
            .savepoints
            .get(&savepoint_id)
            .map(|(_, name)| name.clone())
    }

    // ----- Driver registry --------------------------------------------

    /// Register a JDBC driver by its fully-qualified class name. Idempotent.
    pub fn register_driver(class_name: &str) {
        let mut n14 = new14().lock();
        if !n14.registered_drivers.iter().any(|d| d == class_name) {
            n14.registered_drivers.push(class_name.to_string());
        }
    }

    /// Deregister a JDBC driver.
    pub fn deregister_driver(class_name: &str) -> bool {
        let mut n14 = new14().lock();
        let before = n14.registered_drivers.len();
        n14.registered_drivers.retain(|d| d != class_name);
        n14.registered_drivers.len() < before
    }

    /// Return the list of registered driver class names.
    pub fn list_drivers() -> Vec<String> {
        new14().lock().registered_drivers.clone()
    }

    // ----- Database metadata ------------------------------------------

    /// Return the product name of the underlying database engine.
    pub fn database_product_name() -> &'static str {
        "SQLite"
    }

    /// Return the engine version string from rusqlite.
    pub fn database_product_version() -> String {
        rusqlite::version().to_string()
    }

    /// Fixed-value driver name exposed through `DatabaseMetaData`.
    pub fn driver_name() -> &'static str {
        "CratonVM JDBC"
    }

    /// Driver version string.
    pub fn driver_version() -> &'static str {
        "1.0"
    }

    /// `getDriverMajorVersion` / `getDriverMinorVersion`, parsed out of
    /// `driver_version()` so the three answers cannot drift apart.
    pub fn driver_version_parts() -> (i32, i32) {
        let mut parts = driver_version().split('.');
        let major = parts
            .next()
            .and_then(|s| s.trim().parse::<i32>().ok())
            .unwrap_or(0);
        let minor = parts
            .next()
            .and_then(|s| s.trim().parse::<i32>().ok())
            .unwrap_or(0);
        (major, minor)
    }
}

/// `PreparedStatement.close()` / `CallableStatement.close()`.
///
/// Frees the compiled statement held in the registry (slot 0 is its id) and
/// sets the closed flag in slot 1. `java/sql/Statement.close()V` cannot do
/// this: on a plain Statement slot 0 is the CONNECTION id, so it has no
/// prepared handle to release.
///
/// A named fn rather than an inline closure because it has to be registered
/// TWICE — once alongside the other PreparedStatement natives, and once more
/// after the `alias_class` calls at the end of `register_p68_jdbc`.
/// `alias_class` is last-writer-wins and `close()V` is the one descriptor the
/// Statement and PreparedStatement sets have in common, so aliasing Statement
/// onto the two subinterfaces overwrote this with Statement's own `close`.
/// The result was that `free_prepared` never ran for any prepared or callable
/// statement and every compiled handle leaked for the life of the VM.
fn native_prepared_statement_close(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let ps_id = match ctx.get_field(this, 0) {
        Value::Long(v) => v,
        Value::Int(v) => v as i64,
        _ => 0,
    };
    // Closing a Statement closes its current ResultSet — identical rule and
    // identical leak as `java/sql/Statement.close()V`, which this native
    // REPLACES on both subinterfaces, so it has to do the same thing. Slots
    // 4/5 are cleared before the cache is dropped so a later
    // `getResultSet()` reports no current result rather than a stale id.
    let rs_id = match ctx.get_field(this, 4) {
        Value::Long(v) => v,
        Value::Int(v) => v as i64,
        _ => 0,
    };
    ctx.set_field(this, 4, Value::Long(0));
    ctx.set_field(this, 5, Value::Int(-1));
    if rs_id != 0 {
        jdbc_registry::free_results(rs_id);
    }
    jdbc_registry::free_prepared(ps_id);
    ctx.set_field(this, 1, Value::Int(1));
    Ok(None)
}

/// `java.sql.DriverManager` — registered ONLY in `--synthetic-jdk` builds.
///
/// Split out of `register_p68_jdbc` because `DriverManager` is a CONCRETE class:
/// a native on it INTERCEPTS, so putting it on the real-JDK path would make
/// `DriverManager.getConnection(url)` hand back a rusqlite connection for every
/// URL, shadowing whatever driver the application actually registered. The rest
/// of this surface is registered on `java/sql/*` INTERFACES, which do not
/// intercept an implementation class, so it only ever reaches CratonVM's own
/// synthetic carriers and is safe in both builds — which is what lets
/// `ResultSetMetaData` answer for them in real-JDK mode.
///
/// What real-JDK mode genuinely needs from us that a driver cannot do for
/// itself — driver discovery, SQL date/time conversion — lives in
/// `native-builtins/src/jdbc.rs` and is registered there.
pub(crate) fn register_p68_jdbc_driver_manager(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let dm = "java/sql/DriverManager";
    r.register(
        dm,
        "getConnection",
        "(Ljava/lang/String;)Ljava/sql/Connection;",
        |ctx, args| {
            let url = match args.first() {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            match jdbc_registry::open_connection(&url) {
                Ok(conn_id) => {
                    // Connection = 4-field: conn_id=0, closed=1, auto_commit=2, tx_isolation=3
                    let conn = try_alloc_concurrent_synthetic(ctx, "java/sql/Connection", 4)?;
                    ctx.set_field(conn, 0, Value::Long(conn_id));
                    ctx.set_field(conn, 1, Value::Int(0));
                    ctx.set_field(conn, 2, Value::Int(1)); // auto_commit=true
                    ctx.set_field(conn, 3, Value::Int(8)); // SERIALIZABLE
                    Ok(Some(Value::Object(Some(conn))))
                }
                Err(e) => Err(RuntimeError::IllegalStateException { message: e }.into()),
            }
        },
    );
    r.register(
        dm,
        "getConnection",
        "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)Ljava/sql/Connection;",
        |ctx, args| {
            let url = match args.first() {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            match jdbc_registry::open_connection(&url) {
                Ok(conn_id) => {
                    // Connection = 4-field: conn_id=0, closed=1,
                    // auto_commit=2, tx_isolation=3 — same shape as the
                    // single-arg overload above. This allocated only 2 slots
                    // until 2026-07-27, and because `set_field` is required to
                    // bounds-check, every write to slots 2/3 on such a
                    // Connection was silently dropped: `setAutoCommit` and
                    // `setTransactionIsolation` still reached SQLite, but
                    // `getAutoCommit()` / `getTransactionIsolation()` never
                    // reflected the change for any connection obtained
                    // through this overload.
                    let conn = try_alloc_concurrent_synthetic(ctx, "java/sql/Connection", 4)?;
                    ctx.set_field(conn, 0, Value::Long(conn_id));
                    ctx.set_field(conn, 1, Value::Int(0));
                    ctx.set_field(conn, 2, Value::Int(1)); // auto_commit=true
                    ctx.set_field(conn, 3, Value::Int(8)); // SERIALIZABLE
                    Ok(Some(Value::Object(Some(conn))))
                }
                Err(e) => Err(RuntimeError::IllegalStateException { message: e }.into()),
            }
        },
    );
    r.register(
        dm,
        "getConnection",
        "(Ljava/lang/String;Ljava/util/Properties;)Ljava/sql/Connection;",
        |ctx, args| {
            let url = match args.first() {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            match jdbc_registry::open_connection(&url) {
                Ok(conn_id) => {
                    // Connection = 4-field: conn_id=0, closed=1,
                    // auto_commit=2, tx_isolation=3 — same shape as the
                    // single-arg overload above. This allocated only 2 slots
                    // until 2026-07-27, and because `set_field` is required to
                    // bounds-check, every write to slots 2/3 on such a
                    // Connection was silently dropped: `setAutoCommit` and
                    // `setTransactionIsolation` still reached SQLite, but
                    // `getAutoCommit()` / `getTransactionIsolation()` never
                    // reflected the change for any connection obtained
                    // through this overload.
                    let conn = try_alloc_concurrent_synthetic(ctx, "java/sql/Connection", 4)?;
                    ctx.set_field(conn, 0, Value::Long(conn_id));
                    ctx.set_field(conn, 1, Value::Int(0));
                    ctx.set_field(conn, 2, Value::Int(1)); // auto_commit=true
                    ctx.set_field(conn, 3, Value::Int(8)); // SERIALIZABLE
                    Ok(Some(Value::Object(Some(conn))))
                }
                Err(e) => Err(RuntimeError::IllegalStateException { message: e }.into()),
            }
        },
    );
    r.register(dm, "registerDriver", "(Ljava/sql/Driver;)V", |ctx, args| {
        let name = match args.first() {
            Some(Value::Object(Some(obj))) => {
                let cid = ctx.class_id_of_object(*obj);
                ctx.class_name_of_id(cid)
                    .unwrap_or_else(|| "unknown".to_string())
            }
            _ => return Ok(None),
        };
        jdbc_registry::register_driver(&name);
        Ok(None)
    });
    r.register(
        dm,
        "deregisterDriver",
        "(Ljava/sql/Driver;)V",
        |ctx, args| {
            let name = match args.first() {
                Some(Value::Object(Some(obj))) => {
                    let cid = ctx.class_id_of_object(*obj);
                    ctx.class_name_of_id(cid).unwrap_or_default()
                }
                _ => return Ok(None),
            };
            jdbc_registry::deregister_driver(&name);
            Ok(None)
        },
    );
    r.register(
        dm,
        "getDrivers",
        "()Ljava/util/Enumeration;",
        |ctx, _args| {
            // Return an Enumeration over the registered driver names. The
            // Enumeration synthetic has 2 fields (array=0, pos=1) matching
            // the shape used by other enumerator sites in native-builtins.
            let drivers = jdbc_registry::list_drivers();
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, drivers.len());
            for (i, name) in drivers.iter().enumerate() {
                let s = ctx.create_string(name);
                ctx.set_array_element(arr, i, Value::Object(Some(s)));
            }
            // Concrete `Enumeration$Impl`, not the bare `Enumeration` interface.
            let en = crate::classloader::make_snapshot_enumeration(ctx, arr)?;
            Ok(Some(Value::Object(Some(en))))
        },
    );
    r.set_category(__prev_cat);
}

/// REACHABILITY, and why this must NOT be pulled onto the real-JDK path.
///
/// Reached only from `register_phase68_natives` -> `register_synthetic_overrides`,
/// i.e. `#[cfg(feature = "synthetic-jdk")]`. That is deliberate, not an
/// oversight, and it is the answer to "the JDBC surface (including
/// `ResultSetMetaData`'s column types) does not apply in the default build":
///
///   * in synthetic-jdk mode `java.sql.*` has no bytecode at all, so this
///     rusqlite-backed implementation IS the JDBC provider;
///   * in real-JDK mode `java.sql.*` are real interfaces and the APPLICATION's
///     driver (H2, sqlite-jdbc, the SQL Server driver, ...) supplies the
///     implementation classes. Registering these natives there would shadow the
///     driver's own `ResultSetMetaData` with SQLite's answers about a database
///     the driver may not even be talking to.
///
/// The precedent is `register_p68_xml`: putting THAT synthetic surface on the
/// real-JDK path pre-empted Tomcat's real SAX parser and broke `server.xml`
/// parsing. What real-JDK mode genuinely needs from us — driver discovery and
/// the SQL date/time conversions no driver can do without VM help — is in
/// `native-builtins/src/jdbc.rs`, which IS registered there.
pub(crate) fn register_p68_jdbc(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // DriverManager — real SQLite connection via rusqlite
    // `DriverManager` lives in its own registrar — see
    // `register_p68_jdbc_driver_manager` for why it must not reach the
    // real-JDK path.
    // Connection = 2-field (conn_id=0, closed=1)
    let conn = "java/sql/Connection";
    r.register(
        conn,
        "createStatement",
        "()Ljava/sql/Statement;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let conn_id = match ctx.get_field(this, 0) {
                Value::Long(v) => v,
                Value::Int(v) => v as i64,
                _ => 0,
            };
            // Statement = 8-field. The layout is shared with
            // PreparedStatement/CallableStatement (see `prepareStatement`
            // below) because the `alias_class` calls at the end of this fn
            // copy every Statement native onto both of them, so a slot has to
            // mean the same thing on all three:
            //   0 primary handle (conn_id here, ps_id on the other two)
            //   1 closed        2 conn_id        3 reserved
            //   4 currentResultSetId             5 currentUpdateCount
            //   6 maxRows                        7 queryTimeout
            // Slots 4/5 back the JDBC execute()/getResultSet()/
            // getUpdateCount() trio; before they existed those three natives
            // were constants (null / -1 / false) and every `execute` +
            // `getResultSet` caller silently saw an empty result.
            let stmt = try_alloc_concurrent_synthetic(ctx, "java/sql/Statement", 8)?;
            ctx.set_field(stmt, 0, Value::Long(conn_id)); // pass conn_id through
            ctx.set_field(stmt, 1, Value::Int(0)); // not closed
            ctx.set_field(stmt, 2, Value::Long(conn_id)); // uniform conn_id slot
            ctx.set_field(stmt, 4, Value::Long(0)); // no current ResultSet
            ctx.set_field(stmt, 5, Value::Int(-1)); // no current update count
            ctx.set_field(stmt, 6, Value::Int(0)); // maxRows: unlimited
            ctx.set_field(stmt, 7, Value::Int(0)); // queryTimeout: none
            Ok(Some(Value::Object(Some(stmt))))
        },
    );
    r.register(
        conn,
        "prepareStatement",
        "(Ljava/lang/String;)Ljava/sql/PreparedStatement;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let conn_id = match ctx.get_field(this, 0) {
                Value::Long(v) => v,
                Value::Int(v) => v as i64,
                _ => 0,
            };
            let sql = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            let ps_id = jdbc_registry::prepare(conn_id, &sql);
            // PreparedStatement = 8-field, laid out exactly like
            // java/sql/Statement above (ps_id=0, closed=1, conn_id=2,
            // reserved=3, currentResultSetId=4, currentUpdateCount=5,
            // maxRows=6, queryTimeout=7) so every Statement native the
            // `alias_class` calls copy onto this class reads and writes the
            // same slots it would on a plain Statement.
            let stmt = try_alloc_concurrent_synthetic(ctx, "java/sql/PreparedStatement", 8)?;
            ctx.set_field(stmt, 0, Value::Long(ps_id));
            ctx.set_field(stmt, 1, Value::Int(0)); // not closed
            ctx.set_field(stmt, 2, Value::Long(conn_id));
            ctx.set_field(stmt, 4, Value::Long(0)); // no current ResultSet
            ctx.set_field(stmt, 5, Value::Int(-1)); // no current update count
            ctx.set_field(stmt, 6, Value::Int(0)); // maxRows: unlimited
            ctx.set_field(stmt, 7, Value::Int(0)); // queryTimeout: none
            Ok(Some(Value::Object(Some(stmt))))
        },
    );
    // NEW-14.N1: `prepareCall` now returns a real CallableStatement
    // backed by the same `jdbc_registry::prepare` path as a regular
    // PreparedStatement. Field layout mirrors PreparedStatement's 3-field
    // shape (ps_id, closed, conn_id) so every setString / setInt /
    // executeUpdate / executeQuery registered on PreparedStatement
    // also dispatches correctly when called on a CallableStatement
    // (the JDK's java.sql.CallableStatement interface extends
    // PreparedStatement).
    r.register(
        conn,
        "prepareCall",
        "(Ljava/lang/String;)Ljava/sql/CallableStatement;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let conn_id = match ctx.get_field(this, 0) {
                Value::Long(v) => v,
                Value::Int(v) => v as i64,
                _ => 0,
            };
            let sql = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            let ps_id = jdbc_registry::prepare(conn_id, &sql);
            let stmt = try_alloc_concurrent_synthetic(ctx, "java/sql/CallableStatement", 8)?;
            ctx.set_field(stmt, 0, Value::Long(ps_id));
            ctx.set_field(stmt, 1, Value::Int(0)); // not closed
            ctx.set_field(stmt, 2, Value::Long(conn_id));
            ctx.set_field(stmt, 4, Value::Long(0)); // no current ResultSet
            ctx.set_field(stmt, 5, Value::Int(-1)); // no current update count
            ctx.set_field(stmt, 6, Value::Int(0)); // maxRows: unlimited
            ctx.set_field(stmt, 7, Value::Int(0)); // queryTimeout: none
            Ok(Some(Value::Object(Some(stmt))))
        },
    );
    r.register(conn, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let conn_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        jdbc_registry::close_connection(conn_id);
        ctx.set_field(this, 1, Value::Int(1));
        Ok(None)
    });
    r.register(conn, "isClosed", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(conn, "setAutoCommit", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let auto = args.get(1).and_then(|v| v.as_int()).unwrap_or(1) != 0;
        let conn_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        jdbc_registry::set_auto_commit(conn_id, auto)
            .map_err(|e| RuntimeError::IllegalStateException { message: e })?;
        ctx.set_field(this, 2, Value::Int(if auto { 1 } else { 0 }));
        Ok(None)
    });
    r.register(conn, "getAutoCommit", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 2)))
    });
    r.register(conn, "commit", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let conn_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        jdbc_registry::commit(conn_id)
            .map_err(|e| RuntimeError::IllegalStateException { message: e })?;
        Ok(None)
    });
    r.register(conn, "rollback", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let conn_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        jdbc_registry::rollback(conn_id)
            .map_err(|e| RuntimeError::IllegalStateException { message: e })?;
        Ok(None)
    });
    r.register(
        conn,
        "getMetaData",
        "()Ljava/sql/DatabaseMetaData;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let conn_id = ctx.get_field(this, 0);
            let dbmd = try_alloc_concurrent_synthetic(ctx, "java/sql/DatabaseMetaData", 1)?;
            ctx.set_field(dbmd, 0, conn_id);
            Ok(Some(Value::Object(Some(dbmd))))
        },
    );
    r.register(conn, "setTransactionIsolation", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let level = args.get(1).and_then(|v| v.as_int()).unwrap_or(8);
        let conn_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        jdbc_registry::set_isolation(conn_id, level)
            .map_err(|e| RuntimeError::IllegalStateException { message: e })?;
        ctx.set_field(this, 3, Value::Int(level));
        Ok(None)
    });
    r.register(conn, "getTransactionIsolation", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 3)))
    });
    r.register(conn, "isValid", "(I)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let closed = ctx.get_field(this, 1).as_int().unwrap_or(0);
        if closed != 0 {
            return Ok(Some(Value::Int(0)));
        }
        let conn_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        Ok(Some(Value::Int(if jdbc_registry::is_valid(conn_id) {
            1
        } else {
            0
        })))
    });

    // Statement = 8-field, and the layout is SHARED with PreparedStatement /
    // CallableStatement because `alias_class` (end of this fn) copies every
    // native registered below onto both of them:
    //   0 conn_id (ps_id on the two subinterfaces)   1 closed
    //   2 conn_id (uniform on all three)             3 reserved
    //   4 currentResultSetId                         5 currentUpdateCount
    //   6 maxRows                                    7 queryTimeout
    // A native here must only touch slots whose meaning holds for all three.
    let stmt = "java/sql/Statement";
    r.register(
        stmt,
        "executeQuery",
        "(Ljava/lang/String;)Ljava/sql/ResultSet;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Slot 2, not slot 0: slot 0 is the connection id only on a plain
            // Statement — it is the prepared-statement id on the two
            // subinterfaces this native is aliased onto. Slot 2 is the
            // connection id on all three. (JDBC requires the String-taking
            // overloads to throw on a PreparedStatement receiver, so this is
            // only reachable from a non-conforming caller; reading the right
            // slot costs nothing and stops the wrong one being copied
            // outward.)
            let conn_id = match ctx.get_field(this, 2) {
                Value::Long(v) => v,
                Value::Int(v) => v as i64,
                _ => 0,
            };
            let sql = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            match jdbc_registry::execute_query(conn_id, &sql) {
                Ok((stmt_id, row_count)) => {
                    // Publish the current result BEFORE allocating: the
                    // allocation below can move `this` under a young GC, so a
                    // field write made afterwards would land on a stale ref.
                    ctx.set_field(this, 4, Value::Long(stmt_id));
                    ctx.set_field(this, 5, Value::Int(-1));
                    // ResultSet = 3-field (stmt_id=0, cursor=1, rowCount=2)
                    let rs = try_alloc_concurrent_synthetic(ctx, "java/sql/ResultSet", 3)?;
                    ctx.set_field(rs, 0, Value::Long(stmt_id));
                    ctx.set_field(rs, 1, Value::Int(-1)); // cursor before first row
                    ctx.set_field(rs, 2, Value::Int(row_count as i32));
                    Ok(Some(Value::Object(Some(rs))))
                }
                Err(e) => Err(RuntimeError::IllegalStateException { message: e }.into()),
            }
        },
    );
    r.register(
        stmt,
        "executeUpdate",
        "(Ljava/lang/String;)I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Slot 2 = connection id on all three statement layouts; see
            // executeQuery above.
            let conn_id = match ctx.get_field(this, 2) {
                Value::Long(v) => v,
                Value::Int(v) => v as i64,
                _ => 0,
            };
            let sql = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            match jdbc_registry::execute_update(conn_id, &sql) {
                Ok(n) => {
                    ctx.set_field(this, 4, Value::Long(0)); // no ResultSet
                    ctx.set_field(this, 5, Value::Int(n)); // current update count
                    Ok(Some(Value::Int(n)))
                }
                Err(e) => Err(RuntimeError::IllegalStateException { message: e }.into()),
            }
        },
    );
    r.register(stmt, "execute", "(Ljava/lang/String;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Slot 2 = connection id on all three statement layouts; see
        // executeQuery above.
        let conn_id = match ctx.get_field(this, 2) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        let sql = match args.get(1) {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => String::new(),
        };
        // JDBC contract: `true` ⇒ the first result is a ResultSet (read it
        // with getResultSet()), `false` ⇒ it is an update count (read it with
        // getUpdateCount()). This used to run every statement through
        // `execute_update`, discard the Result, and hardcode `true` — so a
        // SELECT reported "there is a ResultSet" and the paired
        // getResultSet() constant then handed back null, and a failed
        // CREATE/INSERT reported success. Probe the compiled statement for
        // result columns instead, then take the matching path.
        match jdbc_registry::produces_result_set(conn_id, &sql) {
            Ok(true) => match jdbc_registry::execute_query(conn_id, &sql) {
                Ok((stmt_id, _row_count)) => {
                    ctx.set_field(this, 4, Value::Long(stmt_id));
                    ctx.set_field(this, 5, Value::Int(-1));
                    Ok(Some(Value::Int(1)))
                }
                Err(e) => Err(RuntimeError::IllegalStateException { message: e }.into()),
            },
            Ok(false) => match jdbc_registry::execute_update(conn_id, &sql) {
                Ok(n) => {
                    ctx.set_field(this, 4, Value::Long(0));
                    ctx.set_field(this, 5, Value::Int(n));
                    Ok(Some(Value::Int(0)))
                }
                Err(e) => Err(RuntimeError::IllegalStateException { message: e }.into()),
            },
            Err(e) => Err(RuntimeError::IllegalStateException { message: e }.into()),
        }
    });
    r.register(stmt, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // JDBC: closing a Statement also closes its current ResultSet. Slot 4
        // holds the row-cache id of that result, and nothing else ever
        // released it — so `try (Statement s = ...) { ... }` without an
        // explicit `rs.close()`, the common form, leaked every row it had
        // fetched for the life of the VM.
        //
        // Clear slots 4/5 BEFORE dropping the cache, so a later
        // `getResultSet()` on the closed Statement sees "no current result"
        // (slot 4 == 0 ⇒ it returns null) instead of a stale id.
        let rs_id = match ctx.get_field(this, 4) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        ctx.set_field(this, 4, Value::Long(0));
        ctx.set_field(this, 5, Value::Int(-1));
        if rs_id != 0 {
            jdbc_registry::free_results(rs_id);
        }
        // Slot 1 is the closed flag; slot 0 is the connection id. This wrote
        // slot 0 until 2026-07-27, which made `isClosed()` (reading the same
        // slot) report *closed* from the moment the Statement was created and
        // destroyed the conn_id every later call needs. `alias_class` below
        // copies this native onto PreparedStatement/CallableStatement, whose
        // slot 0 holds the prepared-statement id, so it corrupted those too.
        ctx.set_field(this, 1, Value::Int(1));
        Ok(None)
    });
    r.register(stmt, "isClosed", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(
            ctx.get_field(this, 1).as_int().unwrap_or(0),
        )))
    });
    // JDBC: the current result as a ResultSet, or null when the current
    // result is an update count or there is no current result. `execute`
    // above parks the row-cache id in slot 4.
    r.register(
        stmt,
        "getResultSet",
        "()Ljava/sql/ResultSet;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let stmt_id = match ctx.get_field(this, 4) {
                Value::Long(v) => v,
                Value::Int(v) => v as i64,
                _ => 0,
            };
            if stmt_id == 0 {
                return Ok(Some(Value::Object(None)));
            }
            let rows = jdbc_registry::get_row_count(stmt_id);
            // ResultSet = 3-field (stmt_id=0, cursor=1, rowCount=2)
            let rs = try_alloc_concurrent_synthetic(ctx, "java/sql/ResultSet", 3)?;
            ctx.set_field(rs, 0, Value::Long(stmt_id));
            ctx.set_field(rs, 1, Value::Int(-1)); // cursor before first row
            ctx.set_field(rs, 2, Value::Int(rows as i32));
            Ok(Some(Value::Object(Some(rs))))
        },
    );
    // JDBC: the current result as an update count, or -1 when the current
    // result is a ResultSet or there is no current result. Slot 5 is written
    // by `execute` / `executeUpdate` above.
    r.register(stmt, "getUpdateCount", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(
            ctx.get_field(this, 5).as_int().unwrap_or(-1),
        )))
    });
    // JDBC: move to this Statement's next result, closing the current
    // ResultSet. The SQLite backend produces exactly one result per execute,
    // so there is never a next one — but "there isn't one" still has to free
    // the cached rows and reset both slots, otherwise a stale update count
    // stays readable through getUpdateCount() forever (which is what the old
    // bare `false` constant did).
    r.register(stmt, "getMoreResults", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let stmt_id = match ctx.get_field(this, 4) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        if stmt_id != 0 {
            jdbc_registry::free_results(stmt_id);
        }
        ctx.set_field(this, 4, Value::Long(0));
        ctx.set_field(this, 5, Value::Int(-1));
        Ok(Some(Value::Int(0)))
    });
    // maxRows and queryTimeout live in slots 6/7, NOT 2/3.
    //
    // These four are copied onto PreparedStatement and CallableStatement by
    // the `alias_class` calls at the end of this fn, and on those two classes
    // slot 2 is the CONNECTION ID. Storing maxRows there — which is what this
    // did until 2026-07-27 — meant `ps.setMaxRows(n)` overwrote the prepared
    // statement's conn_id with `n`, after which every registry lookup for
    // that statement resolved to the wrong connection or to none at all.
    // Slots 6/7 are free on all three layouts, so one implementation is now
    // correct for every receiver the alias can hand it.
    r.register(stmt, "setMaxRows", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let max = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        ctx.set_field(this, 6, Value::Int(max));
        Ok(None)
    });
    r.register(stmt, "getMaxRows", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let max = ctx.get_field(this, 6).as_int().unwrap_or(0);
        Ok(Some(Value::Int(max)))
    });
    r.register(stmt, "setQueryTimeout", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let timeout = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        ctx.set_field(this, 7, Value::Int(timeout));
        // Apply busy_timeout to the SQLite connection. Read the id from slot
        // 2, which every one of the three statement layouts sets to the
        // connection id; slot 0 is the connection id only for a plain
        // Statement (it is the prepared-statement id on the other two), so
        // reading it here sent the PRAGMA to a bogus connection whenever the
        // alias delivered a PreparedStatement/CallableStatement receiver.
        let conn_id = match ctx.get_field(this, 2) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        if timeout > 0 {
            let timeout_ms = (timeout * 1000).to_string();
            let pragma = format!("PRAGMA busy_timeout = {}", timeout_ms);
            let _ = jdbc_registry::execute_update(conn_id, &pragma);
        }
        Ok(None)
    });
    r.register(stmt, "getQueryTimeout", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let timeout = ctx.get_field(this, 7).as_int().unwrap_or(0);
        Ok(Some(Value::Int(timeout)))
    });

    // PreparedStatement = 8-field (ps_id=0, closed=1, conn_id=2, reserved=3,
    // currentResultSetId=4, currentUpdateCount=5, maxRows=6, queryTimeout=7)
    // — identical to the java/sql/Statement layout above.
    let pstmt = "java/sql/PreparedStatement";
    r.register(pstmt, "setString", "(ILjava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ps_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(1) as usize;
        let val = match args.get(2) {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => String::new(),
        };
        jdbc_registry::bind_string(ps_id, idx, val);
        Ok(None)
    });
    r.register(pstmt, "setInt", "(II)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ps_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(1) as usize;
        let val = args.get(2).and_then(|v| v.as_int()).unwrap_or(0) as i64;
        jdbc_registry::bind_int(ps_id, idx, val);
        Ok(None)
    });
    r.register(pstmt, "setLong", "(IJ)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ps_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(1) as usize;
        let val = match args.get(2) {
            Some(Value::Long(v)) => *v,
            Some(Value::Int(v)) => *v as i64,
            _ => 0,
        };
        jdbc_registry::bind_int(ps_id, idx, val);
        Ok(None)
    });
    r.register(pstmt, "setDouble", "(ID)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ps_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(1) as usize;
        let val = match args.get(2) {
            Some(Value::Double(v)) => *v,
            Some(Value::Float(v)) => *v as f64,
            _ => 0.0,
        };
        jdbc_registry::bind_double(ps_id, idx, val);
        Ok(None)
    });
    r.register(pstmt, "setFloat", "(IF)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ps_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(1) as usize;
        let val = match args.get(2) {
            Some(Value::Float(v)) => *v as f64,
            Some(Value::Double(v)) => *v,
            _ => 0.0,
        };
        jdbc_registry::bind_double(ps_id, idx, val);
        Ok(None)
    });
    r.register(pstmt, "setBoolean", "(IZ)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ps_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(1) as usize;
        let val = args.get(2).and_then(|v| v.as_int()).unwrap_or(0) != 0;
        jdbc_registry::bind_bool(ps_id, idx, val);
        Ok(None)
    });
    r.register(pstmt, "setNull", "(II)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ps_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(1) as usize;
        jdbc_registry::bind_null(ps_id, idx);
        Ok(None)
    });
    r.register(pstmt, "setObject", "(ILjava/lang/Object;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ps_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(1) as usize;
        match args.get(2) {
            Some(Value::Object(Some(s))) => {
                let val = ctx.read_string(*s).unwrap_or_default();
                jdbc_registry::bind_string(ps_id, idx, val);
            }
            Some(Value::Int(v)) => jdbc_registry::bind_int(ps_id, idx, *v as i64),
            Some(Value::Long(v)) => jdbc_registry::bind_int(ps_id, idx, *v),
            Some(Value::Double(v)) => jdbc_registry::bind_double(ps_id, idx, *v),
            Some(Value::Float(v)) => jdbc_registry::bind_double(ps_id, idx, *v as f64),
            _ => jdbc_registry::bind_null(ps_id, idx),
        }
        Ok(None)
    });
    r.register(pstmt, "setBytes", "(I[B)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ps_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(1) as usize;
        let bytes = match args.get(2) {
            Some(Value::Object(Some(arr))) => {
                let len = ctx.array_length(*arr);
                (0..len)
                    .map(|i| match ctx.get_array_element(*arr, i) {
                        Value::Int(b) => b as u8,
                        _ => 0,
                    })
                    .collect()
            }
            _ => Vec::new(),
        };
        jdbc_registry::bind_bytes(ps_id, idx, bytes);
        Ok(None)
    });
    r.register(
        pstmt,
        "executeQuery",
        "()Ljava/sql/ResultSet;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let ps_id = match ctx.get_field(this, 0) {
                Value::Long(v) => v,
                Value::Int(v) => v as i64,
                _ => 0,
            };
            match jdbc_registry::execute_prepared_query(ps_id) {
                Ok((stmt_id, row_count)) => {
                    // Publish the current result before allocating — the
                    // allocation can move `this` (see Statement.executeQuery).
                    ctx.set_field(this, 4, Value::Long(stmt_id));
                    ctx.set_field(this, 5, Value::Int(-1));
                    let rs = try_alloc_concurrent_synthetic(ctx, "java/sql/ResultSet", 3)?;
                    ctx.set_field(rs, 0, Value::Long(stmt_id));
                    ctx.set_field(rs, 1, Value::Int(-1));
                    ctx.set_field(rs, 2, Value::Int(row_count as i32));
                    Ok(Some(Value::Object(Some(rs))))
                }
                Err(e) => Err(RuntimeError::IllegalStateException { message: e }.into()),
            }
        },
    );
    r.register(pstmt, "executeUpdate", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ps_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        match jdbc_registry::execute_prepared_update(ps_id) {
            Ok(n) => {
                ctx.set_field(this, 4, Value::Long(0)); // no ResultSet
                ctx.set_field(this, 5, Value::Int(n)); // current update count
                Ok(Some(Value::Int(n)))
            }
            Err(e) => Err(RuntimeError::IllegalStateException { message: e }.into()),
        }
    });
    r.register(pstmt, "execute", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ps_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        // Same JDBC contract as Statement.execute(String): the boolean says
        // which of getResultSet()/getUpdateCount() is readable. This always
        // took the update path and answered `true`, so a prepared SELECT was
        // routed through `conn.execute` (which rusqlite rejects for
        // row-producing SQL), the error was swallowed, and the caller was
        // told a ResultSet was waiting that never existed.
        match jdbc_registry::prepared_produces_result_set(ps_id) {
            Ok(true) => match jdbc_registry::execute_prepared_query(ps_id) {
                Ok((stmt_id, _row_count)) => {
                    ctx.set_field(this, 4, Value::Long(stmt_id));
                    ctx.set_field(this, 5, Value::Int(-1));
                    Ok(Some(Value::Int(1)))
                }
                Err(e) => Err(RuntimeError::IllegalStateException { message: e }.into()),
            },
            Ok(false) => match jdbc_registry::execute_prepared_update(ps_id) {
                Ok(n) => {
                    ctx.set_field(this, 4, Value::Long(0));
                    ctx.set_field(this, 5, Value::Int(n));
                    Ok(Some(Value::Int(0)))
                }
                Err(e) => Err(RuntimeError::IllegalStateException { message: e }.into()),
            },
            Err(e) => Err(RuntimeError::IllegalStateException { message: e }.into()),
        }
    });
    // Registered here for readability, and AGAIN after the `alias_class`
    // calls at the end of this fn — see `native_prepared_statement_close`.
    r.register(pstmt, "close", "()V", native_prepared_statement_close);
    r.register(pstmt, "addBatch", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ps_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        jdbc_registry::add_batch(ps_id);
        Ok(None)
    });
    r.register(pstmt, "executeBatch", "()[I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ps_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        match jdbc_registry::execute_batch(ps_id) {
            Ok(counts) => {
                let arr = ctx.new_array(cratonvm_types::ArrayElementType::Int, counts.len());
                for (i, &c) in counts.iter().enumerate() {
                    ctx.set_array_element(arr, i, Value::Int(c));
                }
                Ok(Some(Value::Object(Some(arr))))
            }
            Err(e) => Err(RuntimeError::IllegalStateException { message: e }.into()),
        }
    });
    r.register(pstmt, "clearBatch", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ps_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        // Clear batch by re-preparing (simple approach)
        let _ = (ctx, ps_id);
        Ok(None)
    });
    r.register(pstmt, "clearParameters", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ps_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        jdbc_registry::clear_params(ps_id);
        Ok(None)
    });

    // ResultSet = 3-field (stmt_id=0, cursor=1, rowCount=2)
    let rs = "java/sql/ResultSet";
    r.register(rs, "next", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let cursor = ctx.get_field(this, 1).as_int().unwrap_or(-1);
        let rows = ctx.get_field(this, 2).as_int().unwrap_or(0);
        let next_cursor = cursor + 1;
        if next_cursor < rows {
            ctx.set_field(this, 1, Value::Int(next_cursor));
            Ok(Some(Value::Int(1)))
        } else {
            // Park the cursor exactly one past the last row — and no further,
            // so repeated calls cannot run it away. Until 2026-07-27 an
            // exhausting `next()` left the cursor ON the last row, which is
            // indistinguishable from "positioned on the last row": that is
            // why `isAfterLast()` could never be implemented, and why the
            // getters kept returning the final row's values after the loop
            // had ended instead of the JDBC "no current row".
            ctx.set_field(this, 1, Value::Int(rows));
            Ok(Some(Value::Int(0)))
        }
    });
    // ResultSet getters — read from cached SQLite query results
    r.register(rs, "getString", "(I)Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let stmt_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        let cursor = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
        let col = match args.get(1) {
            Some(Value::Int(c)) => (*c - 1) as usize,
            _ => 0,
        };
        match jdbc_registry::get_result(stmt_id, cursor, col) {
            Some(s) if s != "NULL" => {
                jdbc_registry::set_was_null(stmt_id, false);
                Ok(Some(Value::Object(Some(ctx.create_string(&s)))))
            }
            _ => {
                jdbc_registry::set_was_null(stmt_id, true);
                Ok(Some(Value::Object(None)))
            }
        }
    });
    r.register(
        rs,
        "getString",
        "(Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let stmt_id = match ctx.get_field(this, 0) {
                Value::Long(v) => v,
                Value::Int(v) => v as i64,
                _ => 0,
            };
            let cursor = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
            let col_name = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            let col_count = jdbc_registry::get_column_count(stmt_id);
            let col = (0..col_count)
                .find(|&i| jdbc_registry::get_column_name(stmt_id, i) == col_name)
                .unwrap_or(0);
            match jdbc_registry::get_result(stmt_id, cursor, col) {
                Some(s) if s != "NULL" => {
                    jdbc_registry::set_was_null(stmt_id, false);
                    Ok(Some(Value::Object(Some(ctx.create_string(&s)))))
                }
                _ => {
                    jdbc_registry::set_was_null(stmt_id, true);
                    Ok(Some(Value::Object(None)))
                }
            }
        },
    );
    r.register(rs, "getInt", "(I)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let stmt_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        let cursor = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
        let col = match args.get(1) {
            Some(Value::Int(c)) => (*c - 1) as usize,
            _ => 0,
        };
        let raw = jdbc_registry::get_result(stmt_id, cursor, col);
        let is_null = raw.as_deref().map(|s| s == "NULL").unwrap_or(true);
        jdbc_registry::set_was_null(stmt_id, is_null);
        let val = raw.and_then(|s| s.parse::<i32>().ok()).unwrap_or(0);
        Ok(Some(Value::Int(val)))
    });
    r.register(rs, "getInt", "(Ljava/lang/String;)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let stmt_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        let cursor = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
        let col_name = match args.get(1) {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => String::new(),
        };
        let col_count = jdbc_registry::get_column_count(stmt_id);
        let col = (0..col_count)
            .find(|&i| jdbc_registry::get_column_name(stmt_id, i) == col_name)
            .unwrap_or(0);
        let raw = jdbc_registry::get_result(stmt_id, cursor, col);
        let is_null = raw.as_deref().map(|s| s == "NULL").unwrap_or(true);
        jdbc_registry::set_was_null(stmt_id, is_null);
        let val = raw.and_then(|s| s.parse::<i32>().ok()).unwrap_or(0);
        Ok(Some(Value::Int(val)))
    });
    r.register(rs, "getLong", "(I)J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let stmt_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        let cursor = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
        let col = match args.get(1) {
            Some(Value::Int(c)) => (*c - 1) as usize,
            _ => 0,
        };
        let raw = jdbc_registry::get_result(stmt_id, cursor, col);
        let is_null = raw.as_deref().map(|s| s == "NULL").unwrap_or(true);
        jdbc_registry::set_was_null(stmt_id, is_null);
        let val = raw.and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
        Ok(Some(Value::Long(val)))
    });
    r.register(rs, "getLong", "(Ljava/lang/String;)J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let stmt_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        let cursor = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
        let col_name = match args.get(1) {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => String::new(),
        };
        let col_count = jdbc_registry::get_column_count(stmt_id);
        let col = (0..col_count)
            .find(|&i| jdbc_registry::get_column_name(stmt_id, i) == col_name)
            .unwrap_or(0);
        let raw = jdbc_registry::get_result(stmt_id, cursor, col);
        let is_null = raw.as_deref().map(|s| s == "NULL").unwrap_or(true);
        jdbc_registry::set_was_null(stmt_id, is_null);
        let val = raw.and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
        Ok(Some(Value::Long(val)))
    });
    r.register(rs, "getDouble", "(I)D", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let stmt_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        let cursor = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
        let col = match args.get(1) {
            Some(Value::Int(c)) => (*c - 1) as usize,
            _ => 0,
        };
        let raw = jdbc_registry::get_result(stmt_id, cursor, col);
        let is_null = raw.as_deref().map(|s| s == "NULL").unwrap_or(true);
        jdbc_registry::set_was_null(stmt_id, is_null);
        let val = raw.and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
        Ok(Some(Value::Double(val)))
    });
    r.register(rs, "getDouble", "(Ljava/lang/String;)D", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let stmt_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        let cursor = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
        let col_name = match args.get(1) {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => String::new(),
        };
        let col_count = jdbc_registry::get_column_count(stmt_id);
        let col = (0..col_count)
            .find(|&i| jdbc_registry::get_column_name(stmt_id, i) == col_name)
            .unwrap_or(0);
        let raw = jdbc_registry::get_result(stmt_id, cursor, col);
        let is_null = raw.as_deref().map(|s| s == "NULL").unwrap_or(true);
        jdbc_registry::set_was_null(stmt_id, is_null);
        let val = raw.and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
        Ok(Some(Value::Double(val)))
    });
    r.register(rs, "getFloat", "(I)F", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let stmt_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        let cursor = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
        let col = match args.get(1) {
            Some(Value::Int(c)) => (*c - 1) as usize,
            _ => 0,
        };
        let val = jdbc_registry::get_result(stmt_id, cursor, col)
            .and_then(|s| s.parse::<f32>().ok())
            .unwrap_or(0.0);
        Ok(Some(Value::Float(val)))
    });
    r.register(rs, "getBoolean", "(I)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let stmt_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        let cursor = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
        let col = match args.get(1) {
            Some(Value::Int(c)) => (*c - 1) as usize,
            _ => 0,
        };
        let val = jdbc_registry::get_result(stmt_id, cursor, col)
            .map(|s| s == "1" || s.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        Ok(Some(Value::Int(if val { 1 } else { 0 })))
    });
    r.register(rs, "getBoolean", "(Ljava/lang/String;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let stmt_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        let cursor = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
        let col_name = match args.get(1) {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => String::new(),
        };
        let col_count = jdbc_registry::get_column_count(stmt_id);
        let col = (0..col_count)
            .find(|&i| jdbc_registry::get_column_name(stmt_id, i) == col_name)
            .unwrap_or(0);
        let val = jdbc_registry::get_result(stmt_id, cursor, col)
            .map(|s| s == "1" || s.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        Ok(Some(Value::Int(if val { 1 } else { 0 })))
    });
    r.register(rs, "getObject", "(I)Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let stmt_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        let cursor = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
        let col = match args.get(1) {
            Some(Value::Int(c)) => (*c - 1) as usize,
            _ => 0,
        };
        match jdbc_registry::get_result(stmt_id, cursor, col) {
            Some(s) if s != "NULL" => Ok(Some(Value::Object(Some(ctx.create_string(&s))))),
            _ => Ok(Some(Value::Object(None))),
        }
    });
    // Column-name overload of getObject. This returned a bare null for every
    // column, so `rs.getObject("id")` reported SQL NULL on a populated row
    // while the by-index overload right above returned the value. Resolve the
    // name to an index the same way getString(String) does, then share that
    // overload's cell lookup.
    r.register(
        rs,
        "getObject",
        "(Ljava/lang/String;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let stmt_id = match ctx.get_field(this, 0) {
                Value::Long(v) => v,
                Value::Int(v) => v as i64,
                _ => 0,
            };
            let cursor = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
            let col_name = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            let col_count = jdbc_registry::get_column_count(stmt_id);
            let col = (0..col_count)
                .find(|&i| jdbc_registry::get_column_name(stmt_id, i) == col_name)
                .unwrap_or(0);
            match jdbc_registry::get_result(stmt_id, cursor, col) {
                Some(s) if s != "NULL" => {
                    jdbc_registry::set_was_null(stmt_id, false);
                    Ok(Some(Value::Object(Some(ctx.create_string(&s)))))
                }
                _ => {
                    jdbc_registry::set_was_null(stmt_id, true);
                    Ok(Some(Value::Object(None)))
                }
            }
        },
    );
    // `execute_query` caches every cell as text, so getBytes hands back that
    // cell's UTF-8 bytes (and null for SQL NULL, per the JDBC contract). It
    // returned a zero-length array for every column until 2026-07-27, turning
    // each byte[] / BLOB read into a silent "empty" instead of the data or an
    // error.
    r.register(rs, "getBytes", "(I)[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let stmt_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        let cursor = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
        let col = match args.get(1) {
            Some(Value::Int(c)) => (*c - 1) as usize,
            _ => 0,
        };
        let raw = jdbc_registry::get_result(stmt_id, cursor, col);
        let is_null = raw.as_deref().map(|s| s == "NULL").unwrap_or(true);
        jdbc_registry::set_was_null(stmt_id, is_null);
        if is_null {
            return Ok(Some(Value::Object(None)));
        }
        let bytes = raw.unwrap_or_default().into_bytes();
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
        for (i, b) in bytes.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(*b as i8 as i32));
        }
        Ok(Some(Value::Object(Some(arr))))
    });
    r.register(rs, "wasNull", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let stmt_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        Ok(Some(Value::Int(if jdbc_registry::was_null(stmt_id) {
            1
        } else {
            0
        })))
    });
    r.register(rs, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let stmt_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        jdbc_registry::free_results(stmt_id);
        Ok(None)
    });
    r.register(
        rs,
        "getMetaData",
        "()Ljava/sql/ResultSetMetaData;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let stmt_id = ctx.get_field(this, 0);
            let rsmd = try_alloc_concurrent_synthetic(ctx, "java/sql/ResultSetMetaData", 1)?;
            ctx.set_field(rsmd, 0, stmt_id); // pass stmt_id for column metadata
            Ok(Some(Value::Object(Some(rsmd))))
        },
    );
    // The three cursor predicates below all read slot 1 (cursor) and slot 2
    // (rowCount). Each of them read the WRONG slot until 2026-07-27:
    // `getRow` returned slot 0 — the row-cache id, so it reported an
    // arbitrary large number that grew with every query in the process — and
    // the other two read slots 0/1, i.e. the cache id as the cursor and the
    // cursor as the row count. Cursor convention: -1 before the first row,
    // 0..rowCount-1 on a row, rowCount once `next()` has run off the end.
    //
    // JDBC `getRow()`: the 1-based current row number, or 0 when there is no
    // current row (before the first or after the last).
    r.register(rs, "getRow", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let cursor = ctx.get_field(this, 1).as_int().unwrap_or(-1);
        let rows = ctx.get_field(this, 2).as_int().unwrap_or(0);
        let row_no = if cursor >= 0 && cursor < rows {
            cursor + 1
        } else {
            0
        };
        Ok(Some(Value::Int(row_no)))
    });
    // JDBC `isBeforeFirst()`: true if the cursor is before the first row;
    // always false when the result set contains no rows.
    r.register(rs, "isBeforeFirst", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let cursor = ctx.get_field(this, 1).as_int().unwrap_or(-1);
        let rows = ctx.get_field(this, 2).as_int().unwrap_or(0);
        Ok(Some(Value::Int(i32::from(rows > 0 && cursor < 0))))
    });
    // JDBC `isAfterLast()`: true if the cursor is past the last row; always
    // false when the result set contains no rows.
    r.register(rs, "isAfterLast", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let cursor = ctx.get_field(this, 1).as_int().unwrap_or(-1);
        let rows = ctx.get_field(this, 2).as_int().unwrap_or(0);
        Ok(Some(Value::Int(i32::from(rows > 0 && cursor >= rows))))
    });

    // ResultSetMetaData = 1-field (stmt_id=0)
    let rsmd = "java/sql/ResultSetMetaData";
    r.register(rsmd, "getColumnCount", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let stmt_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        Ok(Some(Value::Int(
            jdbc_registry::get_column_count(stmt_id) as i32
        )))
    });
    r.register(
        rsmd,
        "getColumnName",
        "(I)Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let stmt_id = match ctx.get_field(this, 0) {
                Value::Long(v) => v,
                Value::Int(v) => v as i64,
                _ => 0,
            };
            let col = match args.get(1) {
                Some(Value::Int(c)) => (*c - 1) as usize,
                _ => 0,
            };
            let name = jdbc_registry::get_column_name(stmt_id, col);
            Ok(Some(Value::Object(Some(ctx.create_string(&name)))))
        },
    );
    r.register(rsmd, "getColumnType", "(I)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let stmt_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        let col = match args.get(1) {
            Some(Value::Int(c)) => (*c - 1) as usize,
            _ => 0,
        };
        Ok(Some(Value::Int(jdbc_registry::get_column_type_code(
            stmt_id, col,
        ))))
    });
    r.register(
        rsmd,
        "getColumnTypeName",
        "(I)Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let stmt_id = match ctx.get_field(this, 0) {
                Value::Long(v) => v,
                Value::Int(v) => v as i64,
                _ => 0,
            };
            let col = match args.get(1) {
                Some(Value::Int(c)) => (*c - 1) as usize,
                _ => 0,
            };
            let type_name = jdbc_registry::get_column_type_name(stmt_id, col);
            Ok(Some(Value::Object(Some(ctx.create_string(&type_name)))))
        },
    );
    // The five accessors below were NOT REGISTERED AT ALL before 2026-07-28 —
    // not stubbed, not fabricated, simply absent, so any caller reaching them
    // on our synthetic `java/sql/ResultSetMetaData` (which has no bytecode
    // behind it) failed outright. They are added here rather than left absent
    // because the declared-type data that `execute_query` now records is
    // enough to answer all five for real: the affinity gives the class name
    // and the signedness, and the declared `(precision, scale)` argument list
    // gives the size and scale. See the doc comments on the corresponding
    // `jdbc_registry` functions for exactly which source each answer comes
    // from and where it deliberately reports JDBC's "not applicable" 0.
    r.register(
        rsmd,
        "getColumnClassName",
        "(I)Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let stmt_id = match ctx.get_field(this, 0) {
                Value::Long(v) => v,
                Value::Int(v) => v as i64,
                _ => 0,
            };
            let col = match args.get(1) {
                Some(Value::Int(c)) => (*c - 1) as usize,
                _ => 0,
            };
            let name = jdbc_registry::get_column_class_name(stmt_id, col);
            Ok(Some(Value::Object(Some(ctx.create_string(&name)))))
        },
    );
    r.register(rsmd, "getPrecision", "(I)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let stmt_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        let col = match args.get(1) {
            Some(Value::Int(c)) => (*c - 1) as usize,
            _ => 0,
        };
        Ok(Some(Value::Int(jdbc_registry::get_column_precision(
            stmt_id, col,
        ))))
    });
    r.register(rsmd, "getScale", "(I)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let stmt_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        let col = match args.get(1) {
            Some(Value::Int(c)) => (*c - 1) as usize,
            _ => 0,
        };
        Ok(Some(Value::Int(jdbc_registry::get_column_scale(
            stmt_id, col,
        ))))
    });
    r.register(rsmd, "isSigned", "(I)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let stmt_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        let col = match args.get(1) {
            Some(Value::Int(c)) => (*c - 1) as usize,
            _ => 0,
        };
        Ok(Some(Value::Int(i32::from(
            jdbc_registry::is_column_signed(stmt_id, col),
        ))))
    });
    r.register(rsmd, "getColumnDisplaySize", "(I)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let stmt_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        let col = match args.get(1) {
            Some(Value::Int(c)) => (*c - 1) as usize,
            _ => 0,
        };
        Ok(Some(Value::Int(jdbc_registry::get_column_display_size(
            stmt_id, col,
        ))))
    });
    // JDBC spec for getTableName: "table name or "" if not applicable". The
    // row cache built by `execute_query` keeps column names only — SQLite's
    // originating-table metadata (sqlite3_column_table_name) is a build-time
    // option rusqlite does not surface — so report the spec's not-applicable
    // value. `null` (the answer until 2026-07-27) is not a legal
    // ResultSetMetaData result and NPE'd every caller that compared it.
    r.register(
        rsmd,
        "getTableName",
        "(I)Ljava/lang/String;",
        |ctx, _args| Ok(Some(Value::Object(Some(ctx.create_string(""))))),
    );
    // Real answer, read back from the SQLite schema: the wave-3 note that
    // "we do not record per-column NOT NULL constraints" was wrong — SQLite
    // does record them and `pragma_table_info` exposes them. See
    // `jdbc_registry::column_nullable` for the (deliberately conservative)
    // rule; it still falls back to columnNullable whenever the answer is not
    // provable.
    r.register(rsmd, "isNullable", "(I)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let stmt_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        let col = match args.get(1) {
            Some(Value::Int(c)) => (*c - 1) as usize,
            _ => 0,
        };
        Ok(Some(Value::Int(jdbc_registry::column_nullable(
            stmt_id, col,
        ))))
    });

    // KEEP (deliberate constants): these are the `java.sql.Types` int
    // constants, fixed by the JDBC specification — every value below matches
    // the JDK's `java.sql.Types` field. They are registered as natives only
    // because synthetic-jdk mode has no `java/sql/Types` bytecode to read the
    // static finals from.
    let types = "java/sql/Types";

    // =========================================================================
    // NEW-14.N4 — DatabaseMetaData real fields
    // =========================================================================
    //
    // The DatabaseMetaData synthetic carries the connection id on field 0
    // (set by `Connection.getMetaData` above). Each accessor reads the
    // underlying registry and returns a real string — no more hardcoded
    // empty values.
    let dbmd = "java/sql/DatabaseMetaData";
    r.register(
        dbmd,
        "getDatabaseProductName",
        "()Ljava/lang/String;",
        |ctx, _args| {
            let s = ctx.create_string(jdbc_registry::database_product_name());
            Ok(Some(Value::Object(Some(s))))
        },
    );
    r.register(
        dbmd,
        "getDatabaseProductVersion",
        "()Ljava/lang/String;",
        |ctx, _args| {
            let s = ctx.create_string(&jdbc_registry::database_product_version());
            Ok(Some(Value::Object(Some(s))))
        },
    );
    r.register(
        dbmd,
        "getDriverName",
        "()Ljava/lang/String;",
        |ctx, _args| {
            let s = ctx.create_string(jdbc_registry::driver_name());
            Ok(Some(Value::Object(Some(s))))
        },
    );
    r.register(
        dbmd,
        "getDriverVersion",
        "()Ljava/lang/String;",
        |ctx, _args| {
            let s = ctx.create_string(jdbc_registry::driver_version());
            Ok(Some(Value::Object(Some(s))))
        },
    );
    // Derived from `jdbc_registry::driver_version()` rather than duplicated
    // as literals, so `getDriverVersion` / `getDriverMajorVersion` /
    // `getDriverMinorVersion` cannot drift apart when the driver version
    // string changes.
    r.register(dbmd, "getDriverMajorVersion", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(jdbc_registry::driver_version_parts().0)))
    });
    r.register(dbmd, "getDriverMinorVersion", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(jdbc_registry::driver_version_parts().1)))
    });
    r.register(dbmd, "getURL", "()Ljava/lang/String;", |ctx, _args| {
        // Our DriverManager opens `jdbc:sqlite:<path>` URLs. Without
        // per-connection URL storage we report a canonical identifier.
        let s = ctx.create_string("jdbc:sqlite::memory:");
        Ok(Some(Value::Object(Some(s))))
    });
    r.register(dbmd, "getUserName", "()Ljava/lang/String;", |ctx, _args| {
        let s = ctx.create_string("");
        Ok(Some(Value::Object(Some(s))))
    });
    // Real per-connection state: field 0 of the DatabaseMetaData synthetic
    // is the connection id, and `is_read_only` reads SQLite's `query_only`
    // pragma off that connection. (We always open read/write, so the pragma
    // is the only route to a read-only connection — but it IS a route, and
    // the old unconditional `false` could not see it.)
    r.register(dbmd, "isReadOnly", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let conn_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        Ok(Some(Value::Int(i32::from(jdbc_registry::is_read_only(
            conn_id,
        )))))
    });
    // KEEP (capability answers, constant in every real driver too). Each is
    // a compile-time-true statement about THIS driver, and the reference
    // SQLite JDBC driver hardcodes the same three:
    //   supportsTransactions— backed by real BEGIN/COMMIT/ROLLBACK in
    //                         `set_auto_commit` / `commit` / `rollback`.
    //   supportsSavepoints  — backed by `savepoint_create` /
    //                         `savepoint_rollback` / `savepoint_release`.
    //   supportsBatchUpdates— backed by `add_batch` / `execute_batch`.
    r.register(dbmd, "supportsTransactions", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
    });
    r.register(dbmd, "supportsSavepoints", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
    });
    r.register(dbmd, "supportsBatchUpdates", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
    });
    // KEEP: `0` is the JDBC spec's encoding for "no limit, or the limit is
    // unknown" (java.sql.DatabaseMetaData#getMaxConnections). Neither SQLite
    // nor this registry imposes a connection cap, so 0 is the *correct*
    // answer, not a placeholder — the reference SQLite JDBC driver returns 0
    // here as well.
    r.register(dbmd, "getMaxConnections", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(0)))
    });

    // =========================================================================
    // NEW-14.N5 — DriverManager registry helpers
    // =========================================================================
    //
    // `registerDriver(Driver)` / `deregisterDriver(Driver)` / `getDrivers()`
    // track driver class names in the `jdbc_registry::registered_drivers`
    // list. A real JDK resolves `Driver` instances to their class names
    // via reflection; NEW-14 derives it from the object's class mirror
    // via `NativeContext::class_id_of_object` + `class_name_of_id`
    // which matches what `driver.getClass().getName()` returns in the
    // real JDK. This works for any Java-land driver that extends
    // `java.sql.Driver`, including the FakeDriver in TckJdbc.

    // =========================================================================
    // NEW-14.N2 — java.sql.Blob real implementation
    // =========================================================================
    //
    // Blob synthetic layout (2 fields):
    //   field 0 = blob id (Long) stored in `jdbc_registry::blobs`
    //   field 1 = closed flag (Int, 0 = open, 1 = freed)
    let blob = "java/sql/Blob";
    r.register(blob, "length", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        Ok(Some(Value::Long(jdbc_registry::blob_length(id))))
    });
    r.register(blob, "getBytes", "(JI)[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        let pos = match args.get(1) {
            Some(Value::Long(v)) => *v,
            Some(Value::Int(v)) => *v as i64,
            _ => 1,
        };
        let length = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        let bytes = jdbc_registry::blob_get_bytes(id, pos, length);
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
        for (i, b) in bytes.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int((*b as i8) as i32));
        }
        Ok(Some(Value::Object(Some(arr))))
    });
    r.register(blob, "setBytes", "(J[B)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        let pos = match args.get(1) {
            Some(Value::Long(v)) => *v,
            Some(Value::Int(v)) => *v as i64,
            _ => 1,
        };
        let bytes = match args.get(2) {
            Some(Value::Object(Some(arr))) => {
                let len = ctx.array_length(*arr);
                let mut out = Vec::with_capacity(len);
                for i in 0..len {
                    if let Value::Int(b) = ctx.get_array_element(*arr, i) {
                        out.push((b as i8) as u8);
                    } else {
                        out.push(0);
                    }
                }
                out
            }
            _ => Vec::new(),
        };
        let n = jdbc_registry::blob_set_bytes(id, pos, &bytes);
        Ok(Some(Value::Int(n)))
    });
    r.register(blob, "truncate", "(J)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        let length = match args.get(1) {
            Some(Value::Long(v)) => *v,
            Some(Value::Int(v)) => *v as i64,
            _ => 0,
        };
        jdbc_registry::blob_truncate(id, length);
        Ok(None)
    });
    r.register(blob, "free", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        jdbc_registry::blob_free(id);
        ctx.set_field(this, 1, Value::Int(1));
        Ok(None)
    });

    // =========================================================================
    // NEW-14.N2 — java.sql.Clob real implementation
    // =========================================================================
    //
    // Clob synthetic layout (2 fields): field 0 = clob id (Long),
    // field 1 = closed flag.
    let clob = "java/sql/Clob";
    r.register(clob, "length", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        Ok(Some(Value::Long(jdbc_registry::clob_length(id))))
    });
    r.register(
        clob,
        "getSubString",
        "(JI)Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let id = match ctx.get_field(this, 0) {
                Value::Long(v) => v,
                Value::Int(v) => v as i64,
                _ => 0,
            };
            let pos = match args.get(1) {
                Some(Value::Long(v)) => *v,
                Some(Value::Int(v)) => *v as i64,
                _ => 1,
            };
            let length = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
            let s = jdbc_registry::clob_get_substring(id, pos, length);
            let out = ctx.create_string(&s);
            Ok(Some(Value::Object(Some(out))))
        },
    );
    r.register(clob, "setString", "(JLjava/lang/String;)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        let pos = match args.get(1) {
            Some(Value::Long(v)) => *v,
            Some(Value::Int(v)) => *v as i64,
            _ => 1,
        };
        let s = match args.get(2) {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => String::new(),
        };
        let n = jdbc_registry::clob_set_string(id, pos, &s);
        Ok(Some(Value::Int(n)))
    });
    r.register(clob, "truncate", "(J)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        let length = match args.get(1) {
            Some(Value::Long(v)) => *v,
            Some(Value::Int(v)) => *v as i64,
            _ => 0,
        };
        jdbc_registry::clob_truncate(id, length);
        Ok(None)
    });
    r.register(clob, "free", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        jdbc_registry::clob_free(id);
        ctx.set_field(this, 1, Value::Int(1));
        Ok(None)
    });

    // =========================================================================
    // NEW-14.N3 — java.sql.Savepoint
    // =========================================================================
    //
    // Savepoint synthetic layout (2 fields): field 0 = savepoint id,
    // field 1 = name (String). The id is the key into
    // `jdbc_registry::savepoints`; the name is cached here so
    // `Savepoint.getSavepointName()` can return without a registry
    // round-trip.
    let sp = "java/sql/Savepoint";
    r.register(sp, "getSavepointId", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = match ctx.get_field(this, 0) {
            Value::Long(v) => v as i32,
            Value::Int(v) => v,
            _ => 0,
        };
        Ok(Some(Value::Int(id)))
    });
    r.register(
        sp,
        "getSavepointName",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 1)))
        },
    );

    // =========================================================================
    // NEW-14.N6 — Connection factories for Blob/Clob/Savepoint
    // =========================================================================
    r.register(conn, "createBlob", "()Ljava/sql/Blob;", |ctx, _args| {
        let id = jdbc_registry::blob_create(Vec::new());
        let obj = try_alloc_concurrent_synthetic(ctx, "java/sql/Blob", 2)?;
        ctx.set_field(obj, 0, Value::Long(id));
        ctx.set_field(obj, 1, Value::Int(0));
        Ok(Some(Value::Object(Some(obj))))
    });
    r.register(conn, "createClob", "()Ljava/sql/Clob;", |ctx, _args| {
        let id = jdbc_registry::clob_create(String::new());
        let obj = try_alloc_concurrent_synthetic(ctx, "java/sql/Clob", 2)?;
        ctx.set_field(obj, 0, Value::Long(id));
        ctx.set_field(obj, 1, Value::Int(0));
        Ok(Some(Value::Object(Some(obj))))
    });
    r.register(
        conn,
        "setSavepoint",
        "()Ljava/sql/Savepoint;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let conn_id = match ctx.get_field(this, 0) {
                Value::Long(v) => v,
                Value::Int(v) => v as i64,
                _ => 0,
            };
            match jdbc_registry::savepoint_create(conn_id, None) {
                Ok((id, name)) => {
                    let sp_obj = try_alloc_concurrent_synthetic(ctx, "java/sql/Savepoint", 2)?;
                    ctx.set_field(sp_obj, 0, Value::Long(id));
                    let name_ref = ctx.create_string(&name);
                    ctx.set_field(sp_obj, 1, Value::Object(Some(name_ref)));
                    Ok(Some(Value::Object(Some(sp_obj))))
                }
                Err(e) => Err(RuntimeError::IllegalStateException { message: e }.into()),
            }
        },
    );
    r.register(
        conn,
        "setSavepoint",
        "(Ljava/lang/String;)Ljava/sql/Savepoint;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let conn_id = match ctx.get_field(this, 0) {
                Value::Long(v) => v,
                Value::Int(v) => v as i64,
                _ => 0,
            };
            let name = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s),
                _ => None,
            };
            match jdbc_registry::savepoint_create(conn_id, name) {
                Ok((id, effective)) => {
                    let sp_obj = try_alloc_concurrent_synthetic(ctx, "java/sql/Savepoint", 2)?;
                    ctx.set_field(sp_obj, 0, Value::Long(id));
                    let name_ref = ctx.create_string(&effective);
                    ctx.set_field(sp_obj, 1, Value::Object(Some(name_ref)));
                    Ok(Some(Value::Object(Some(sp_obj))))
                }
                Err(e) => Err(RuntimeError::IllegalStateException { message: e }.into()),
            }
        },
    );
    r.register(conn, "rollback", "(Ljava/sql/Savepoint;)V", |ctx, args| {
        let sp_obj = match args.get(1) {
            Some(Value::Object(Some(o))) => *o,
            _ => {
                return Err(RuntimeError::NullPointerException {
                    message: Some("rollback: savepoint is null".into()),
                }
                .into());
            }
        };
        let sp_id = match ctx.get_field(sp_obj, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        jdbc_registry::savepoint_rollback(sp_id)
            .map_err(|e| RuntimeError::IllegalStateException { message: e })?;
        Ok(None)
    });
    r.register(
        conn,
        "releaseSavepoint",
        "(Ljava/sql/Savepoint;)V",
        |ctx, args| {
            let sp_obj = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("releaseSavepoint: savepoint is null".into()),
                    }
                    .into());
                }
            };
            let sp_id = match ctx.get_field(sp_obj, 0) {
                Value::Long(v) => v,
                Value::Int(v) => v as i64,
                _ => 0,
            };
            jdbc_registry::savepoint_release(sp_id)
                .map_err(|e| RuntimeError::IllegalStateException { message: e })?;
            Ok(None)
        },
    );

    // =========================================================================
    // NEW-14.N1 (continued) — alias every PreparedStatement method under
    // CallableStatement so the JDK's interface-inheritance relationship is
    // faithfully reflected in our class-name-keyed native dispatch. This is
    // a single call at the end of registration so the full PreparedStatement
    // table is already populated before we copy it.
    // =========================================================================
    r.alias_class("java/sql/PreparedStatement", "java/sql/CallableStatement");
    // Statement is the superinterface of PreparedStatement / CallableStatement
    // too, so close/isClosed/execute(String) etc. must reach both subclasses.
    r.alias_class("java/sql/Statement", "java/sql/PreparedStatement");
    r.alias_class("java/sql/Statement", "java/sql/CallableStatement");

    // ---- undo the one collision the aliases above introduce ---------------
    //
    // `alias_class` is last-writer-wins. Comparing the two registration sets
    // by (method, descriptor), `close()V` is the ONLY pair they share —
    // executeQuery/executeUpdate/execute differ by descriptor
    // (`(Ljava/lang/String;)…` vs `()…`), and nothing else overlaps at all.
    // So the three calls above left `java/sql/Statement`'s `close`, which
    // only flips the closed flag, installed on both subinterfaces, and
    // `free_prepared` was never reached for a prepared or callable statement:
    // every compiled statement leaked its registry entry for the life of the
    // VM. Put the prepared-aware close back on both.
    //
    // The Statement path is untouched — `java/sql/Statement.close()V` itself
    // is not re-registered here.
    r.register(
        "java/sql/PreparedStatement",
        "close",
        "()V",
        native_prepared_statement_close,
    );
    r.register(
        "java/sql/CallableStatement",
        "close",
        "()V",
        native_prepared_statement_close,
    );

    // NEW-14.N1 — CallableStatement output parameter support.
    //
    // These were no-ops until 2026-07-28, justified by the claim that
    // `wasNull()` / `getObject(int)` "fall through to the inherited
    // PreparedStatement implementation". That claim was false: `getObject` /
    // `wasNull` are registered on `java/sql/ResultSet` only, and the
    // `alias_class` calls above copy Statement/PreparedStatement onto
    // CallableStatement — not ResultSet. So the registration was simply
    // dropped, with nothing downstream to compensate.
    //
    // SQLite has no stored procedures, so we still cannot *execute* an OUT
    // parameter; what a driver can and must do is REMEMBER the registration
    // (JDBC §4.3.2 — the registered type is the only record that a parameter
    // is an OUT at all). `jdbc_registry::register_out_parameter*` stores it on
    // the PreparedState, `out_parameter*` reads it back, and an unknown
    // statement id or a 0 index is now reported rather than swallowed.
    let cstmt = "java/sql/CallableStatement";
    r.register(cstmt, "registerOutParameter", "(II)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ps_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as usize;
        let sql_type = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        jdbc_registry::register_out_parameter(ps_id, idx, sql_type, 0, None)
            .map_err(|e| RuntimeError::IllegalStateException { message: e })?;
        Ok(None)
    });
    r.register(cstmt, "registerOutParameter", "(III)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ps_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as usize;
        let sql_type = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        let scale = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
        jdbc_registry::register_out_parameter(ps_id, idx, sql_type, scale, None)
            .map_err(|e| RuntimeError::IllegalStateException { message: e })?;
        Ok(None)
    });
    // Descriptor fix: the three-argument type-name overload is
    // `registerOutParameter(int, int, String)` — `(IILjava/lang/String;)V`.
    // This was registered as `(ILjava/lang/String;)V`, which names no method
    // on `java.sql.CallableStatement` at all, so the entry could never be
    // reached by any call site in either run mode.
    r.register(
        cstmt,
        "registerOutParameter",
        "(IILjava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let ps_id = match ctx.get_field(this, 0) {
                Value::Long(v) => v,
                Value::Int(v) => v as i64,
                _ => 0,
            };
            let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as usize;
            let sql_type = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
            let type_name = match args.get(3) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s),
                _ => None,
            };
            jdbc_registry::register_out_parameter(ps_id, idx, sql_type, 0, type_name)
                .map_err(|e| RuntimeError::IllegalStateException { message: e })?;
            Ok(None)
        },
    );
    r.register(
        cstmt,
        "registerOutParameter",
        "(Ljava/lang/String;I)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let ps_id = match ctx.get_field(this, 0) {
                Value::Long(v) => v,
                Value::Int(v) => v as i64,
                _ => 0,
            };
            let name = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            let sql_type = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
            jdbc_registry::register_out_parameter_named(ps_id, &name, sql_type)
                .map_err(|e| RuntimeError::IllegalStateException { message: e })?;
            Ok(None)
        },
    );
    // The two remaining named overloads. Without these, a caller using the
    // scale or type-name form fell through to the class body (there is none
    // in synthetic mode) and the registration was lost.
    r.register(
        cstmt,
        "registerOutParameter",
        "(Ljava/lang/String;II)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let ps_id = match ctx.get_field(this, 0) {
                Value::Long(v) => v,
                Value::Int(v) => v as i64,
                _ => 0,
            };
            let name = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            let sql_type = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
            let scale = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
            jdbc_registry::register_out_parameter_named_ext(ps_id, &name, sql_type, scale, None)
                .map_err(|e| RuntimeError::IllegalStateException { message: e })?;
            Ok(None)
        },
    );
    r.register(
        cstmt,
        "registerOutParameter",
        "(Ljava/lang/String;ILjava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let ps_id = match ctx.get_field(this, 0) {
                Value::Long(v) => v,
                Value::Int(v) => v as i64,
                _ => 0,
            };
            let name = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            let sql_type = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
            let type_name = match args.get(3) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s),
                _ => None,
            };
            jdbc_registry::register_out_parameter_named_ext(ps_id, &name, sql_type, 0, type_name)
                .map_err(|e| RuntimeError::IllegalStateException { message: e })?;
            Ok(None)
        },
    );
    r.set_category(__prev_cat);
}

// =============================================================================
// NEW-14 — JDBC end-to-end tests
// =============================================================================

#[cfg(test)]
pub(crate) mod new14_jdbc_tests {
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
}
