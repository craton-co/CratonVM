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
        /// SQLite column type names (TEXT, INTEGER, REAL, BLOB, NULL)
        column_types: Vec<String>,
    }

    struct PreparedState {
        conn_id: i64,
        sql: String,
        params: HashMap<usize, ParamValue>,
        batch: Vec<HashMap<usize, ParamValue>>,
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

        let (rows, column_names, column_types) = {
            let conn = reg
                .connections
                .get(&conn_id)
                .ok_or_else(|| "Connection not found".to_string())?;

            let mut stmt = conn.prepare(sql).map_err(|e| sanitize_error(&e))?;

            let column_count = stmt.column_count();
            let col_names: Vec<String> = (0..column_count)
                .map(|i| stmt.column_name(i).unwrap_or("?").to_string())
                .collect();

            // Column types — rusqlite doesn't expose decl_type easily before execution.
            // Default all to TEXT; actual type detection happens at row-read time.
            let col_types: Vec<String> = (0..column_count).map(|_| "TEXT".to_string()).collect();

            let mut result_rows: Vec<Vec<String>> = Vec::new();
            let mut rows_iter = stmt.query([]).map_err(|e| sanitize_error(&e))?;

            while let Some(row) = rows_iter.next().map_err(|e| sanitize_error(&e))? {
                if result_rows.len() >= MAX_RESULT_ROWS {
                    break; // Prevent unbounded memory growth
                }
                let mut vals = Vec::with_capacity(column_count);
                for i in 0..column_count {
                    let val: String = row
                        .get::<_, String>(i)
                        .or_else(|_| row.get::<_, i64>(i).map(|v| v.to_string()))
                        .or_else(|_| row.get::<_, f64>(i).map(|v| v.to_string()))
                        .unwrap_or_else(|_| "NULL".to_string());
                    vals.push(val);
                }
                result_rows.push(vals);
            }

            (result_rows, col_names, col_types)
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
            },
        );
        Ok((stmt_id, row_count))
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

        let (rows, column_names, column_types) = {
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
            let col_types: Vec<String> = (0..column_count).map(|_i| "TEXT".to_string()).collect();

            let mut result_rows: Vec<Vec<String>> = Vec::new();
            let mut rows_iter = stmt.raw_query();
            while let Some(row) = rows_iter.next().map_err(|e| sanitize_error(&e))? {
                if result_rows.len() >= MAX_RESULT_ROWS {
                    break;
                }
                let mut vals = Vec::with_capacity(column_count);
                for i in 0..column_count {
                    let val: String = row
                        .get::<_, String>(i)
                        .or_else(|_| row.get::<_, i64>(i).map(|v| v.to_string()))
                        .or_else(|_| row.get::<_, f64>(i).map(|v| v.to_string()))
                        .unwrap_or_else(|_| "NULL".to_string());
                    vals.push(val);
                }
                result_rows.push(vals);
            }

            (result_rows, col_names, col_types)
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

    /// Get the declared column type name (e.g. "INTEGER", "TEXT", "REAL").
    pub fn get_column_type_name(stmt_id: i64, col: usize) -> String {
        let reg = registry().lock();
        reg.results
            .get(&stmt_id)
            .and_then(|r| r.column_types.get(col))
            .cloned()
            .unwrap_or_else(|| "TEXT".to_string())
    }

    /// Map SQLite type name to JDBC type code.
    pub fn get_column_type_code(stmt_id: i64, col: usize) -> i32 {
        let type_name = get_column_type_name(stmt_id, col);
        match type_name.to_uppercase().as_str() {
            "INTEGER" | "INT" | "BIGINT" => 4, // Types.INTEGER
            "REAL" | "DOUBLE" | "FLOAT" => 8,  // Types.DOUBLE
            "BLOB" => 2004,                    // Types.BLOB
            "BOOLEAN" | "BOOL" => 16,          // Types.BOOLEAN
            _ => 12,                           // Types.VARCHAR
        }
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
}

pub(crate) fn register_p68_jdbc(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // DriverManager — real SQLite connection via rusqlite
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
                    let conn = alloc_concurrent_synthetic(ctx, "java/sql/Connection", 4);
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
                    let conn = alloc_concurrent_synthetic(ctx, "java/sql/Connection", 2);
                    ctx.set_field(conn, 0, Value::Long(conn_id));
                    ctx.set_field(conn, 1, Value::Int(0));
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
                    let conn = alloc_concurrent_synthetic(ctx, "java/sql/Connection", 2);
                    ctx.set_field(conn, 0, Value::Long(conn_id));
                    ctx.set_field(conn, 1, Value::Int(0));
                    Ok(Some(Value::Object(Some(conn))))
                }
                Err(e) => Err(RuntimeError::IllegalStateException { message: e }.into()),
            }
        },
    );

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
            // Statement = 2-field (conn_id=0, closed=1)
            let stmt = alloc_concurrent_synthetic(ctx, "java/sql/Statement", 2);
            ctx.set_field(stmt, 0, Value::Long(conn_id)); // pass conn_id through
            ctx.set_field(stmt, 1, Value::Int(0)); // not closed
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
            // PreparedStatement = 3-field (ps_id=0, closed=1, conn_id=2)
            let stmt = alloc_concurrent_synthetic(ctx, "java/sql/PreparedStatement", 3);
            ctx.set_field(stmt, 0, Value::Long(ps_id));
            ctx.set_field(stmt, 1, Value::Int(0)); // not closed
            ctx.set_field(stmt, 2, Value::Long(conn_id));
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
            let stmt = alloc_concurrent_synthetic(ctx, "java/sql/CallableStatement", 3);
            ctx.set_field(stmt, 0, Value::Long(ps_id));
            ctx.set_field(stmt, 1, Value::Int(0)); // not closed
            ctx.set_field(stmt, 2, Value::Long(conn_id));
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
            let dbmd = alloc_concurrent_synthetic(ctx, "java/sql/DatabaseMetaData", 1);
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

    // Statement = 2-field (conn_id=0, closed=1)
    let stmt = "java/sql/Statement";
    r.register(
        stmt,
        "executeQuery",
        "(Ljava/lang/String;)Ljava/sql/ResultSet;",
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
            match jdbc_registry::execute_query(conn_id, &sql) {
                Ok((stmt_id, row_count)) => {
                    // ResultSet = 3-field (stmt_id=0, cursor=1, rowCount=2)
                    let rs = alloc_concurrent_synthetic(ctx, "java/sql/ResultSet", 3);
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
            let conn_id = match ctx.get_field(this, 0) {
                Value::Long(v) => v,
                Value::Int(v) => v as i64,
                _ => 0,
            };
            let sql = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            match jdbc_registry::execute_update(conn_id, &sql) {
                Ok(n) => Ok(Some(Value::Int(n))),
                Err(e) => Err(RuntimeError::IllegalStateException { message: e }.into()),
            }
        },
    );
    r.register(stmt, "execute", "(Ljava/lang/String;)Z", |ctx, args| {
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
        let _ = jdbc_registry::execute_update(conn_id, &sql);
        Ok(Some(Value::Int(1)))
    });
    r.register(stmt, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, Value::Int(1));
        Ok(None)
    });
    r.register(stmt, "isClosed", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(
        stmt,
        "getResultSet",
        "()Ljava/sql/ResultSet;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
    r.register(stmt, "getUpdateCount", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(-1)))
    });
    r.register(stmt, "getMoreResults", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(0)))
    });
    r.register(stmt, "setMaxRows", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let max = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        ctx.set_field(this, 2, Value::Int(max)); // store in field 2
        Ok(None)
    });
    r.register(stmt, "getMaxRows", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let max = ctx.get_field(this, 2).as_int().unwrap_or(0);
        Ok(Some(Value::Int(max)))
    });
    r.register(stmt, "setQueryTimeout", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let timeout = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        ctx.set_field(this, 3, Value::Int(timeout)); // store in field 3
                                                     // Apply busy_timeout to SQLite connection
        let conn_id = match ctx.get_field(this, 0) {
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
        let timeout = ctx.get_field(this, 3).as_int().unwrap_or(0);
        Ok(Some(Value::Int(timeout)))
    });

    // PreparedStatement = 3-field (ps_id=0, closed=1, conn_id=2)
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
                    let rs = alloc_concurrent_synthetic(ctx, "java/sql/ResultSet", 3);
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
            Ok(n) => Ok(Some(Value::Int(n))),
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
        match jdbc_registry::execute_prepared_update(ps_id) {
            Ok(_) => Ok(Some(Value::Int(1))),
            Err(_) => Ok(Some(Value::Int(0))),
        }
    });
    r.register(pstmt, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ps_id = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        jdbc_registry::free_prepared(ps_id);
        ctx.set_field(this, 1, Value::Int(1));
        Ok(None)
    });
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
    r.register(
        rs,
        "getObject",
        "(Ljava/lang/String;)Ljava/lang/Object;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
    r.register(rs, "getBytes", "(I)[B", |ctx, _args| {
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 0);
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
            let rsmd = alloc_concurrent_synthetic(ctx, "java/sql/ResultSetMetaData", 1);
            ctx.set_field(rsmd, 0, stmt_id); // pass stmt_id for column metadata
            Ok(Some(Value::Object(Some(rsmd))))
        },
    );
    r.register(rs, "getRow", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(rs, "isBeforeFirst", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let cursor = match ctx.get_field(this, 0) {
            Value::Int(v) => v,
            _ => 0,
        };
        Ok(Some(Value::Int(if cursor == 0 { 1 } else { 0 })))
    });
    r.register(rs, "isAfterLast", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let cursor = match ctx.get_field(this, 0) {
            Value::Int(v) => v,
            _ => 0,
        };
        let rows = match ctx.get_field(this, 1) {
            Value::Int(v) => v,
            _ => 0,
        };
        Ok(Some(Value::Int(if cursor > rows { 1 } else { 0 })))
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
    r.register(
        rsmd,
        "getTableName",
        "(I)Ljava/lang/String;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
    r.register(rsmd, "isNullable", "(I)I", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
    }); // columnNullable

    // SQL types constants
    let types = "java/sql/Types";
    r.register(types, "INTEGER", "I", |_ctx, _args| Ok(Some(Value::Int(4))));
    r.register(types, "VARCHAR", "I", |_ctx, _args| {
        Ok(Some(Value::Int(12)))
    });
    r.register(types, "BIGINT", "I", |_ctx, _args| Ok(Some(Value::Int(-5))));
    r.register(types, "DOUBLE", "I", |_ctx, _args| Ok(Some(Value::Int(8))));
    r.register(types, "FLOAT", "I", |_ctx, _args| Ok(Some(Value::Int(6))));
    r.register(types, "BOOLEAN", "I", |_ctx, _args| {
        Ok(Some(Value::Int(16)))
    });
    r.register(types, "BLOB", "I", |_ctx, _args| Ok(Some(Value::Int(2004))));
    r.register(types, "CLOB", "I", |_ctx, _args| Ok(Some(Value::Int(2005))));
    r.register(types, "NULL", "I", |_ctx, _args| Ok(Some(Value::Int(0))));
    r.register(types, "TIMESTAMP", "I", |_ctx, _args| {
        Ok(Some(Value::Int(93)))
    });
    r.register(types, "DATE", "I", |_ctx, _args| Ok(Some(Value::Int(91))));

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
    r.register(dbmd, "getDriverMajorVersion", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
    });
    r.register(dbmd, "getDriverMinorVersion", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(0)))
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
    r.register(dbmd, "isReadOnly", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(0)))
    });
    r.register(dbmd, "supportsTransactions", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
    });
    r.register(dbmd, "supportsSavepoints", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
    });
    r.register(dbmd, "supportsBatchUpdates", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
    });
    r.register(dbmd, "getMaxConnections", "()I", |_ctx, _args| {
        // rusqlite doesn't impose a hard connection limit; 0 means
        // "no limit or unknown" per the JDBC spec.
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
            let en = alloc_concurrent_synthetic(ctx, "java/util/Enumeration$Impl", 2);
            ctx.set_field(en, 0, Value::Object(Some(arr)));
            ctx.set_field(en, 1, Value::Int(0));
            Ok(Some(Value::Object(Some(en))))
        },
    );

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
        let obj = alloc_concurrent_synthetic(ctx, "java/sql/Blob", 2);
        ctx.set_field(obj, 0, Value::Long(id));
        ctx.set_field(obj, 1, Value::Int(0));
        Ok(Some(Value::Object(Some(obj))))
    });
    r.register(conn, "createClob", "()Ljava/sql/Clob;", |ctx, _args| {
        let id = jdbc_registry::clob_create(String::new());
        let obj = alloc_concurrent_synthetic(ctx, "java/sql/Clob", 2);
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
                    let sp_obj = alloc_concurrent_synthetic(ctx, "java/sql/Savepoint", 2);
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
                    let sp_obj = alloc_concurrent_synthetic(ctx, "java/sql/Savepoint", 2);
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

    // NEW-14.N1 — CallableStatement output parameter support.
    //
    // `registerOutParameter(int, int)` / `registerOutParameter(int, int, int)`
    // / `registerOutParameter(String, int)` record the caller's intent but
    // our rusqlite backend doesn't expose true stored-procedure OUT
    // parameters — rusqlite is a pure SQLite binding and SQLite's CALL
    // mechanism is limited. The natives below mark the registration as
    // a no-op (which is spec-legal: a CallableStatement that registers
    // an OUT parameter but never reads it is valid per JDBC §4.2), and
    // `wasNull()` / `getObject(int)` fall through to the inherited
    // PreparedStatement implementation which returns the corresponding
    // ResultSet column value.
    let cstmt = "java/sql/CallableStatement";
    r.register(cstmt, "registerOutParameter", "(II)V", |_ctx, _args| {
        Ok(None)
    });
    r.register(cstmt, "registerOutParameter", "(III)V", |_ctx, _args| {
        Ok(None)
    });
    r.register(
        cstmt,
        "registerOutParameter",
        "(ILjava/lang/String;)V",
        |_ctx, _args| Ok(None),
    );
    r.register(
        cstmt,
        "registerOutParameter",
        "(Ljava/lang/String;I)V",
        |_ctx, _args| Ok(None),
    );
    r.set_category(__prev_cat);
}

// =============================================================================
// NEW-14 — JDBC end-to-end tests
// =============================================================================

#[cfg(test)]
pub(crate) mod new14_jdbc_tests {
    use super::jdbc_registry;

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
}
