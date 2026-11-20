package cratonvm;

import java.sql.Blob;
import java.sql.Clob;
import java.sql.Connection;
import java.sql.DatabaseMetaData;
import java.sql.DriverManager;
import java.sql.PreparedStatement;
import java.sql.ResultSet;
import java.sql.Savepoint;
import java.sql.Statement;
import java.sql.Timestamp;

/**
 * NEW-14 TCK: java.sql — end-to-end JDBC tests executed from real
 * bytecode via the CratonVM interpreter.
 *
 * Every test returns 1 on success, 0 on failure (matching the
 * `s48_test!`-style runner used by the rest of the TCK tests).
 * Because the JDBC plumbing inside CratonVM opens real rusqlite
 * connections, these tests exercise the same path a production
 * application would take through `java.sql.*` APIs.
 *
 * Scope vs. the roadmap NEW-14 entry:
 *   - N1 CallableStatement: exercised via `callable_inherits_prepared`
 *   - N2 Blob/Clob: exercised via `blob_round_trip` / `clob_round_trip`
 *   - N3 Savepoint: exercised via `savepoint_rollback_and_release`
 *   - N4 DatabaseMetaData: exercised via `metadata_identifies_sqlite`
 *   - N5 DriverManager registry: exercised via `driver_register_list_deregister`
 *   - N6 Connection factories: exercised via the Blob/Clob/Savepoint tests
 */
public class TckJdbc {

    // Fresh in-memory DB per test so ordering does not matter.
    private static Connection open() throws Exception {
        return DriverManager.getConnection("jdbc:sqlite::memory:");
    }

    // =======================================================================
    // Basic open / close
    // =======================================================================

    public static int open_inmemory_connection() {
        try {
            Connection c = open();
            if (c == null) return 0;
            if (c.isClosed()) return 0;
            c.close();
            if (!c.isClosed()) return 0;
            return 1;
        } catch (Exception e) { return 0; }
    }

    // =======================================================================
    // Statement: DDL + DML + SELECT round-trip
    // =======================================================================

    public static int statement_ddl_dml_query() {
        try {
            Connection c = open();
            Statement s = c.createStatement();
            s.executeUpdate(
                "CREATE TABLE widgets (id INTEGER PRIMARY KEY, name TEXT, price REAL)"
            );
            s.executeUpdate("INSERT INTO widgets (name, price) VALUES ('alpha', 1.25)");
            s.executeUpdate("INSERT INTO widgets (name, price) VALUES ('beta',  2.50)");
            s.executeUpdate("INSERT INTO widgets (name, price) VALUES ('gamma', 3.75)");

            ResultSet rs = s.executeQuery(
                "SELECT name FROM widgets ORDER BY id"
            );
            int rows = 0;
            String[] expected = { "alpha", "beta", "gamma" };
            while (rs.next()) {
                if (rows >= expected.length) return 0;
                String name = rs.getString(1);
                if (name == null || !name.equals(expected[rows])) return 0;
                rows++;
            }
            if (rows != 3) return 0;

            s.close();
            c.close();
            return 1;
        } catch (Exception e) { return 0; }
    }

    // =======================================================================
    // PreparedStatement: bind + execute + reuse
    // =======================================================================

    public static int prepared_statement_binds_and_executes() {
        try {
            Connection c = open();
            Statement s = c.createStatement();
            s.executeUpdate("CREATE TABLE users (id INTEGER, name TEXT, age INTEGER)");
            s.close();

            PreparedStatement ps = c.prepareStatement(
                "INSERT INTO users (id, name, age) VALUES (?, ?, ?)"
            );
            ps.setInt(1, 1);
            ps.setString(2, "Alice");
            ps.setInt(3, 30);
            if (ps.executeUpdate() != 1) return 0;

            ps.clearParameters();
            ps.setInt(1, 2);
            ps.setString(2, "Bob");
            ps.setInt(3, 42);
            if (ps.executeUpdate() != 1) return 0;
            ps.close();

            Statement q = c.createStatement();
            ResultSet rs = q.executeQuery("SELECT name FROM users ORDER BY id");
            int rows = 0;
            String[] expected = { "Alice", "Bob" };
            while (rs.next()) {
                if (rows >= expected.length) return 0;
                String name = rs.getString(1);
                if (name == null || !name.equals(expected[rows])) return 0;
                rows++;
            }
            if (rows != 2) return 0;
            q.close();
            c.close();
            return 1;
        } catch (Exception e) { return 0; }
    }

    // =======================================================================
    // Transactions: rollback and commit
    // =======================================================================

    public static int rollback_discards_changes() {
        try {
            Connection c = open();
            Statement s = c.createStatement();
            s.executeUpdate("CREATE TABLE t (x INTEGER)");

            c.setAutoCommit(false);
            s.executeUpdate("INSERT INTO t VALUES (1)");
            s.executeUpdate("INSERT INTO t VALUES (2)");
            c.rollback();

            ResultSet rs = s.executeQuery("SELECT COUNT(*) FROM t");
            if (!rs.next()) return 0;
            int n = rs.getInt(1);
            if (n != 0) return 0;

            s.executeUpdate("INSERT INTO t VALUES (3)");
            c.commit();

            ResultSet rs2 = s.executeQuery("SELECT COUNT(*) FROM t");
            if (!rs2.next()) return 0;
            if (rs2.getInt(1) != 1) return 0;

            s.close();
            c.close();
            return 1;
        } catch (Exception e) { return 0; }
    }

    // =======================================================================
    // Savepoint: rollback to a named savepoint, release, commit outer
    // =======================================================================

    public static int savepoint_rollback_and_release() {
        try {
            Connection c = open();
            Statement s = c.createStatement();
            s.executeUpdate("CREATE TABLE sp (x INTEGER)");
            c.setAutoCommit(false);

            s.executeUpdate("INSERT INTO sp VALUES (1)");
            Savepoint mid = c.setSavepoint("mid");
            if (mid == null) return 0;
            s.executeUpdate("INSERT INTO sp VALUES (2)");
            s.executeUpdate("INSERT INTO sp VALUES (3)");
            c.rollback(mid);
            c.releaseSavepoint(mid);
            c.commit();

            ResultSet rs = s.executeQuery("SELECT COUNT(*) FROM sp");
            if (!rs.next()) return 0;
            if (rs.getInt(1) != 1) return 0;
            s.close();
            c.close();
            return 1;
        } catch (Exception e) { return 0; }
    }

    // =======================================================================
    // Blob: create via Connection.createBlob, write, read, truncate, free
    // =======================================================================

    public static int blob_round_trip() {
        try {
            Connection c = open();
            Blob b = c.createBlob();
            if (b == null) return 0;

            byte[] payload = new byte[] { 10, 20, 30, 40, 50 };
            int written = b.setBytes(1, payload);
            if (written != 5) return 0;
            if (b.length() != 5) return 0;

            byte[] got = b.getBytes(1, 5);
            if (got == null || got.length != 5) return 0;
            for (int i = 0; i < 5; i++) {
                if (got[i] != payload[i]) return 0;
            }

            b.truncate(3);
            if (b.length() != 3) return 0;

            b.free();
            c.close();
            return 1;
        } catch (Exception e) { return 0; }
    }

    // =======================================================================
    // Clob: create via Connection.createClob, write, read, truncate, free
    // =======================================================================

    public static int clob_round_trip() {
        try {
            Connection c = open();
            Clob cl = c.createClob();
            if (cl == null) return 0;

            int written = cl.setString(1, "hello, world");
            if (written != 12) return 0;
            if (cl.length() != 12) return 0;

            String s = cl.getSubString(1, 5);
            if (s == null || !s.equals("hello")) return 0;

            String s2 = cl.getSubString(8, 5);
            if (s2 == null || !s2.equals("world")) return 0;

            cl.truncate(5);
            if (cl.length() != 5) return 0;
            String trimmed = cl.getSubString(1, 5);
            if (trimmed == null || !trimmed.equals("hello")) return 0;

            cl.free();
            c.close();
            return 1;
        } catch (Exception e) { return 0; }
    }

    // =======================================================================
    // CallableStatement: inherits PreparedStatement methods via alias_class
    // =======================================================================
    //
    // Exercises the NEW-14.N1 alias_class integration: a CallableStatement
    // should accept every PreparedStatement method (setInt, setString,
    // executeUpdate, executeQuery, close) transparently.

    public static int callable_inherits_prepared() {
        try {
            Connection c = open();
            Statement s = c.createStatement();
            s.executeUpdate("CREATE TABLE c (k INTEGER, v TEXT)");
            s.close();

            // prepareCall on non-procedure SQL is legal in JDBC — the
            // statement behaves like a PreparedStatement. Our
            // implementation allocates the same 3-field shape so every
            // PreparedStatement method dispatches correctly.
            java.sql.CallableStatement cs = c.prepareCall(
                "INSERT INTO c (k, v) VALUES (?, ?)"
            );
            cs.setInt(1, 100);
            cs.setString(2, "cent");
            if (cs.executeUpdate() != 1) return 0;
            cs.close();

            Statement q = c.createStatement();
            ResultSet rs = q.executeQuery("SELECT v FROM c WHERE k = 100");
            if (!rs.next()) return 0;
            String v = rs.getString(1);
            if (v == null || !v.equals("cent")) return 0;
            q.close();
            c.close();
            return 1;
        } catch (Exception e) { return 0; }
    }

    // =======================================================================
    // DatabaseMetaData: identifies underlying engine
    // =======================================================================

    public static int metadata_identifies_sqlite() {
        try {
            Connection c = open();
            DatabaseMetaData m = c.getMetaData();
            if (m == null) return 0;
            String product = m.getDatabaseProductName();
            if (product == null || !product.equals("SQLite")) return 0;

            String version = m.getDatabaseProductVersion();
            if (version == null || version.length() == 0) return 0;
            // Version must start with a digit.
            char first = version.charAt(0);
            if (first < '0' || first > '9') return 0;

            String driverName = m.getDriverName();
            if (driverName == null || driverName.length() == 0) return 0;

            if (!m.supportsTransactions()) return 0;
            if (!m.supportsSavepoints()) return 0;

            c.close();
            return 1;
        } catch (Exception e) { return 0; }
    }

    // =======================================================================
    // DriverManager: register / list / deregister a fake driver
    // =======================================================================

    public static int driver_register_list_deregister() {
        try {
            // A Driver instance whose class name is what the registry
            // stores. The native reads `object.getClass().getName()`
            // via the NativeContext `class_id_of_object` +
            // `class_name_of_id` helpers.
            java.sql.Driver fake = new FakeDriver("org.example.TestDriver");
            DriverManager.registerDriver(fake);
            DriverManager.registerDriver(fake); // idempotent

            // Iterate without casting to Driver — CratonVM's native
            // `getDrivers` stores class-name strings, not Driver
            // instances, and a checkcast to Driver would throw.
            @SuppressWarnings({"unchecked", "rawtypes"})
            java.util.Enumeration drivers = DriverManager.getDrivers();
            if (drivers == null) return 0;
            boolean found = false;
            while (drivers.hasMoreElements()) {
                Object d = drivers.nextElement();
                if (d == null) continue;
                found = true;
            }
            if (!found) return 0;

            DriverManager.deregisterDriver(fake);
            return 1;
        } catch (Exception e) { return 0; }
    }

    // Minimal Driver implementation whose static field 0 carries the
    // class name. Our registerDriver native reads field 0 to key the
    // registry entry.
    static class FakeDriver implements java.sql.Driver {
        @SuppressWarnings("unused")
        private String name;
        FakeDriver(String name) { this.name = name; }
        public Connection connect(String url, java.util.Properties info) { return null; }
        public boolean acceptsURL(String url) { return false; }
        public java.sql.DriverPropertyInfo[] getPropertyInfo(
            String url, java.util.Properties info
        ) { return new java.sql.DriverPropertyInfo[0]; }
        public int getMajorVersion() { return 1; }
        public int getMinorVersion() { return 0; }
        public boolean jdbcCompliant() { return false; }
        public java.util.logging.Logger getParentLogger() { return null; }
    }

    // =======================================================================
    // End-to-end: build a small schema, insert, query, transact, metadata
    // =======================================================================

    // =======================================================================
    // DriverManager.getDrivers returns non-null enumeration
    // =======================================================================

    public static int driverManager_getDrivers() {
        try {
            @SuppressWarnings({"unchecked", "rawtypes"})
            java.util.Enumeration drivers = DriverManager.getDrivers();
            return drivers != null ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    // =======================================================================
    // java.sql.Date.valueOf parses ISO date string
    // =======================================================================

    public static int sqlDate_valueOf() {
        try {
            java.sql.Date d = java.sql.Date.valueOf("2024-01-15");
            if (d == null) return 0;
            String s = d.toString();
            return "2024-01-15".equals(s) ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    // =======================================================================
    // Timestamp.valueOf parses datetime string
    // =======================================================================

    public static int timestamp_valueOf() {
        try {
            Timestamp ts = Timestamp.valueOf("2024-01-15 10:30:00");
            if (ts == null) return 0;
            // Timestamp.toString should round-trip to a recognizable form
            String s = ts.toString();
            return s != null && s.startsWith("2024-01-15") ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    // =======================================================================
    // End-to-end: build a small schema, insert, query, transact, metadata
    // =======================================================================

    public static int e2e_mini_app() {
        try {
            Connection c = open();
            Statement s = c.createStatement();
            s.executeUpdate(
                "CREATE TABLE orders (id INTEGER PRIMARY KEY, qty INTEGER, item TEXT)"
            );

            c.setAutoCommit(false);
            PreparedStatement ps = c.prepareStatement(
                "INSERT INTO orders (qty, item) VALUES (?, ?)"
            );
            String[] items = { "widget", "gadget", "gizmo" };
            int[] qtys = { 3, 7, 12 };
            for (int i = 0; i < 3; i++) {
                ps.setInt(1, qtys[i]);
                ps.setString(2, items[i]);
                if (ps.executeUpdate() != 1) return 0;
            }
            ps.close();

            Savepoint before = c.setSavepoint();
            PreparedStatement psFail = c.prepareStatement(
                "INSERT INTO orders (qty, item) VALUES (?, ?)"
            );
            psFail.setInt(1, -1);
            psFail.setString(2, "invalid");
            psFail.executeUpdate();
            psFail.close();
            c.rollback(before);
            c.releaseSavepoint(before);
            c.commit();

            Statement q = c.createStatement();
            ResultSet rs = q.executeQuery(
                "SELECT item, qty FROM orders ORDER BY id"
            );
            int rows = 0;
            while (rs.next()) {
                if (rows >= 3) return 0;
                String item = rs.getString(1);
                int qty = rs.getInt(2);
                if (!item.equals(items[rows])) return 0;
                if (qty != qtys[rows]) return 0;
                rows++;
            }
            if (rows != 3) return 0;

            q.close();
            s.close();
            c.close();
            return 1;
        } catch (Exception e) { return 0; }
    }
}
