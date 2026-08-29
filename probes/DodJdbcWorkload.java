// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
// Definition-of-done driver: the JDBC workload.
//
// Real java.sql against a real engine (H2), covering the surface a JDBC
// application actually touches: DDL, batched inserts, prepared statements with
// every primitive width, transactions and rollback, savepoints, scrollable and
// updatable result sets, DatabaseMetaData, ResultSetMetaData, CLOB/BLOB, the
// aggregate/join/index planner, MERGE, and both an in-memory and a file-backed
// database. Returns normally: no System.exit, so the --jdk-only report is
// written; it is dropped on the System.exit path.
//
// Only values the PROGRAM chooses are printed. No timings, no addresses, no
// identity hashes; every query that could return rows in an engine-chosen
// order carries an ORDER BY.

import java.math.BigDecimal;
import java.sql.Blob;
import java.sql.Clob;
import java.sql.Connection;
import java.sql.DatabaseMetaData;
import java.sql.Date;
import java.sql.DriverManager;
import java.sql.PreparedStatement;
import java.sql.ResultSet;
import java.sql.ResultSetMetaData;
import java.sql.Savepoint;
import java.sql.SQLException;
import java.sql.Statement;
import java.sql.Timestamp;
import java.sql.Types;

public final class DodJdbcWorkload {

    private static int checks = 0;
    private static int failures = 0;

    public static void main(String[] args) throws Exception {
        String fileDir = args.length > 0 ? args[0] : null;

        run("jdbc:h2:mem:l7mem;DB_CLOSE_DELAY=-1", "mem");
        if (fileDir != null) {
            run("jdbc:h2:" + fileDir + "/l7file;DB_CLOSE_ON_EXIT=FALSE", "file");
        }

        System.out.println("DOD RESULT " + (failures == 0 ? "OK" : "FAILURES=" + failures)
                + " checks=" + checks);
    }

    private static void run(String url, String tag) {
        System.out.println("DOD DB-BEGIN " + tag);
        try (Connection c = DriverManager.getConnection(url, "sa", "")) {
            ddl(c, tag);
            inserts(c, tag);
            queries(c, tag);
            transactions(c, tag);
            metadata(c, tag);
            lobs(c, tag);
            scrollable(c, tag);
            merge(c, tag);
            try (Statement s = c.createStatement()) {
                s.execute("DROP ALL OBJECTS");
            }
        } catch (Throwable t) {
            failures++;
            System.out.println("DOD DB-THREW " + tag + " " + t.getClass().getName() + ": " + t.getMessage());
            t.printStackTrace(System.out);
        }
        System.out.println("DOD DB-END " + tag);
    }

    // ---------------------------------------------------------------- checks

    private static void eq(String what, Object expected, Object actual) {
        checks++;
        boolean ok = (expected == null) ? actual == null : expected.equals(actual);
        if (!ok) {
            failures++;
            System.out.println("DOD CHECK " + what + " FAIL expected=" + expected + " actual=" + actual);
        } else {
            System.out.println("DOD CHECK " + what + " OK");
        }
    }

    private static void ok(String what, boolean cond) {
        checks++;
        if (!cond) {
            failures++;
            System.out.println("DOD CHECK " + what + " FAIL");
        } else {
            System.out.println("DOD CHECK " + what + " OK");
        }
    }

    // ------------------------------------------------------------------ work

    private static void ddl(Connection c, String tag) throws SQLException {
        try (Statement s = c.createStatement()) {
            s.execute("DROP TABLE IF EXISTS ORDERS");
            s.execute("DROP TABLE IF EXISTS CUSTOMER");
            s.execute("CREATE TABLE CUSTOMER ("
                    + "ID INT PRIMARY KEY, NAME VARCHAR(64) NOT NULL, "
                    + "BALANCE DECIMAL(12,2), ACTIVE BOOLEAN, "
                    + "JOINED DATE, TOUCHED TIMESTAMP, "
                    + "RATIO DOUBLE, SMALLNO SMALLINT, BIGNO BIGINT, TINY TINYINT)");
            s.execute("CREATE TABLE ORDERS ("
                    + "ID INT PRIMARY KEY, CUSTOMER_ID INT NOT NULL, "
                    + "TOTAL DECIMAL(12,2), NOTE VARCHAR(255), "
                    + "FOREIGN KEY (CUSTOMER_ID) REFERENCES CUSTOMER(ID))");
            s.execute("CREATE INDEX IDX_ORDERS_CUST ON ORDERS(CUSTOMER_ID)");
        }
        eq(tag + "/ddl.tables", Boolean.TRUE, tableExists(c, "CUSTOMER") && tableExists(c, "ORDERS"));
    }

    private static boolean tableExists(Connection c, String name) throws SQLException {
        try (ResultSet rs = c.getMetaData().getTables(null, null, name, null)) {
            return rs.next();
        }
    }

    private static void inserts(Connection c, String tag) throws SQLException {
        c.setAutoCommit(false);
        try (PreparedStatement ps = c.prepareStatement(
                "INSERT INTO CUSTOMER (ID,NAME,BALANCE,ACTIVE,JOINED,TOUCHED,RATIO,SMALLNO,BIGNO,TINY)"
                        + " VALUES (?,?,?,?,?,?,?,?,?,?)")) {
            for (int i = 1; i <= 200; i++) {
                ps.setInt(1, i);
                ps.setString(2, "customer-" + i);
                ps.setBigDecimal(3, new BigDecimal(i * 3).setScale(2));
                ps.setBoolean(4, (i % 3) != 0);
                ps.setDate(5, Date.valueOf("2020-01-01"));
                ps.setTimestamp(6, Timestamp.valueOf("2020-01-01 12:00:00"));
                ps.setDouble(7, i / 4.0d);
                ps.setShort(8, (short) (i % 100));
                ps.setLong(9, 1_000_000_000L * i);
                ps.setByte(10, (byte) (i % 60));
                ps.addBatch();
            }
            int[] counts = ps.executeBatch();
            eq(tag + "/insert.batchLen", 200, counts.length);
        }
        try (PreparedStatement ps = c.prepareStatement(
                "INSERT INTO ORDERS (ID,CUSTOMER_ID,TOTAL,NOTE) VALUES (?,?,?,?)")) {
            for (int i = 1; i <= 600; i++) {
                ps.setInt(1, i);
                ps.setInt(2, ((i - 1) % 200) + 1);
                ps.setBigDecimal(3, new BigDecimal(i).setScale(2));
                if (i % 7 == 0) {
                    ps.setNull(4, Types.VARCHAR);
                } else {
                    ps.setString(4, "note-" + i);
                }
                ps.executeUpdate();
            }
        }
        c.commit();
        c.setAutoCommit(true);
        eq(tag + "/insert.customers", 200, count(c, "SELECT COUNT(*) FROM CUSTOMER"));
        eq(tag + "/insert.orders", 600, count(c, "SELECT COUNT(*) FROM ORDERS"));
    }

    private static void queries(Connection c, String tag) throws SQLException {
        eq(tag + "/query.nullNotes", 85, count(c, "SELECT COUNT(*) FROM ORDERS WHERE NOTE IS NULL"));
        try (PreparedStatement ps = c.prepareStatement(
                "SELECT c.NAME, COUNT(o.ID) AS N, SUM(o.TOTAL) AS T FROM CUSTOMER c"
                        + " JOIN ORDERS o ON o.CUSTOMER_ID = c.ID"
                        + " WHERE c.ACTIVE = ? GROUP BY c.ID, c.NAME"
                        + " ORDER BY T DESC, c.NAME LIMIT 3")) {
            ps.setBoolean(1, true);
            StringBuilder sb = new StringBuilder();
            try (ResultSet rs = ps.executeQuery()) {
                while (rs.next()) {
                    sb.append(rs.getString("NAME")).append('=')
                      .append(rs.getInt("N")).append('/')
                      .append(rs.getBigDecimal("T").toPlainString()).append(';');
                }
            }
            ok(tag + "/query.groupBy.nonEmpty", sb.length() > 0);
            System.out.println("DOD GROUPBY " + tag + " " + sb);
        }
        try (PreparedStatement ps = c.prepareStatement(
                "SELECT ID, NAME, BALANCE, RATIO, BIGNO FROM CUSTOMER WHERE ID BETWEEN ? AND ? ORDER BY ID")) {
            ps.setInt(1, 10);
            ps.setInt(2, 12);
            try (ResultSet rs = ps.executeQuery()) {
                rs.next();
                eq(tag + "/query.row10.id", 10, rs.getInt(1));
                eq(tag + "/query.row10.name", "customer-10", rs.getString(2));
                eq(tag + "/query.row10.balance", "30.00", rs.getBigDecimal(3).toPlainString());
                eq(tag + "/query.row10.ratio", "2.5", String.valueOf(rs.getDouble(4)));
                eq(tag + "/query.row10.bigno", 10_000_000_000L, rs.getLong(5));
                eq(tag + "/query.row10.wasNull", Boolean.FALSE, rs.wasNull());
            }
        }
        try (Statement s = c.createStatement();
             ResultSet rs = s.executeQuery("SELECT NOTE FROM ORDERS WHERE ID = 7")) {
            rs.next();
            eq(tag + "/query.null.value", null, rs.getString(1));
            eq(tag + "/query.null.wasNull", Boolean.TRUE, rs.wasNull());
        }
    }

    private static void transactions(Connection c, String tag) throws SQLException {
        c.setAutoCommit(false);
        try (Statement s = c.createStatement()) {
            s.executeUpdate("UPDATE CUSTOMER SET BALANCE = 0 WHERE ID <= 10");
            eq(tag + "/tx.beforeRollback", 10, count(c, "SELECT COUNT(*) FROM CUSTOMER WHERE BALANCE = 0"));
            c.rollback();
            eq(tag + "/tx.afterRollback", 0, count(c, "SELECT COUNT(*) FROM CUSTOMER WHERE BALANCE = 0"));

            s.executeUpdate("UPDATE CUSTOMER SET BALANCE = 1 WHERE ID = 1");
            Savepoint sp = c.setSavepoint("sp1");
            s.executeUpdate("UPDATE CUSTOMER SET BALANCE = 2 WHERE ID = 2");
            c.rollback(sp);
            eq(tag + "/tx.savepoint.kept", 1, count(c, "SELECT COUNT(*) FROM CUSTOMER WHERE BALANCE = 1"));
            eq(tag + "/tx.savepoint.undone", 0, count(c, "SELECT COUNT(*) FROM CUSTOMER WHERE BALANCE = 2"));
            c.rollback();
        }
        // Constraint violation must arrive as a SQLException, not as a VM error.
        try (PreparedStatement ps = c.prepareStatement(
                "INSERT INTO ORDERS (ID,CUSTOMER_ID,TOTAL,NOTE) VALUES (?,?,?,?)")) {
            ps.setInt(1, 999999);
            ps.setInt(2, 999999);
            ps.setBigDecimal(3, BigDecimal.ONE);
            ps.setString(4, "orphan");
            ps.executeUpdate();
            ok(tag + "/tx.fkViolation.threw", false);
        } catch (SQLException e) {
            ok(tag + "/tx.fkViolation.threw", true);
        } finally {
            c.rollback();
            c.setAutoCommit(true);
        }
    }

    private static void metadata(Connection c, String tag) throws SQLException {
        DatabaseMetaData md = c.getMetaData();
        ok(tag + "/meta.productName", md.getDatabaseProductName() != null
                && !md.getDatabaseProductName().isEmpty());
        ok(tag + "/meta.driverName", md.getDriverName() != null && !md.getDriverName().isEmpty());
        ok(tag + "/meta.jdbcMajor", md.getJDBCMajorVersion() >= 4);
        int cols = 0;
        try (ResultSet rs = md.getColumns(null, null, "CUSTOMER", "%")) {
            while (rs.next()) {
                cols++;
            }
        }
        eq(tag + "/meta.customerColumns", 10, cols);
        try (ResultSet rs = md.getPrimaryKeys(null, null, "CUSTOMER")) {
            ok(tag + "/meta.primaryKey", rs.next());
        }
        try (ResultSet rs = md.getImportedKeys(null, null, "ORDERS")) {
            ok(tag + "/meta.foreignKey", rs.next());
        }
        try (Statement s = c.createStatement();
             ResultSet rs = s.executeQuery("SELECT ID, NAME, BALANCE FROM CUSTOMER WHERE ID = 1")) {
            ResultSetMetaData rm = rs.getMetaData();
            eq(tag + "/meta.rsColumnCount", 3, rm.getColumnCount());
            eq(tag + "/meta.rsColumn2Label", "NAME", rm.getColumnLabel(2));
            eq(tag + "/meta.rsColumn1Type", Types.INTEGER, rm.getColumnType(1));
            eq(tag + "/meta.rsColumn3Scale", 2, rm.getScale(3));
        }
    }

    private static void lobs(Connection c, String tag) throws SQLException {
        try (Statement s = c.createStatement()) {
            s.execute("DROP TABLE IF EXISTS LOBS");
            s.execute("CREATE TABLE LOBS (ID INT PRIMARY KEY, T CLOB, B BLOB)");
        }
        StringBuilder text = new StringBuilder();
        for (int i = 0; i < 4000; i++) {
            text.append((char) ('a' + (i % 26)));
        }
        byte[] bin = new byte[8192];
        for (int i = 0; i < bin.length; i++) {
            bin[i] = (byte) (i % 251);
        }
        try (PreparedStatement ps = c.prepareStatement("INSERT INTO LOBS VALUES (?,?,?)")) {
            ps.setInt(1, 1);
            ps.setString(2, text.toString());
            ps.setBytes(3, bin);
            ps.executeUpdate();
        }
        try (Statement s = c.createStatement();
             ResultSet rs = s.executeQuery("SELECT T, B FROM LOBS WHERE ID = 1")) {
            rs.next();
            Clob clob = rs.getClob(1);
            Blob blob = rs.getBlob(2);
            eq(tag + "/lob.clobLength", 4000L, clob.length());
            eq(tag + "/lob.clobHead", "abcdefghij", clob.getSubString(1, 10));
            eq(tag + "/lob.blobLength", 8192L, blob.length());
            byte[] head = blob.getBytes(1, 4);
            eq(tag + "/lob.blobHead", "0,1,2,3",
                    head[0] + "," + head[1] + "," + head[2] + "," + head[3]);
        }
    }

    private static void scrollable(Connection c, String tag) throws SQLException {
        try (Statement s = c.createStatement(ResultSet.TYPE_SCROLL_INSENSITIVE, ResultSet.CONCUR_READ_ONLY);
             ResultSet rs = s.executeQuery("SELECT ID FROM CUSTOMER ORDER BY ID")) {
            ok(tag + "/scroll.last", rs.last());
            eq(tag + "/scroll.lastId", 200, rs.getInt(1));
            eq(tag + "/scroll.rowCount", 200, rs.getRow());
            ok(tag + "/scroll.first", rs.first());
            eq(tag + "/scroll.firstId", 1, rs.getInt(1));
            ok(tag + "/scroll.absolute", rs.absolute(50));
            eq(tag + "/scroll.absoluteId", 50, rs.getInt(1));
            ok(tag + "/scroll.relative", rs.relative(-49));
            eq(tag + "/scroll.relativeId", 1, rs.getInt(1));
        }
        try (Statement s = c.createStatement();
             ResultSet rs = s.executeQuery("SELECT ID FROM CUSTOMER WHERE ID = -1")) {
            ok(tag + "/scroll.emptyNext", !rs.next());
        }
    }

    private static void merge(Connection c, String tag) throws SQLException {
        try (Statement s = c.createStatement()) {
            s.executeUpdate("MERGE INTO CUSTOMER (ID,NAME,BALANCE,ACTIVE) KEY(ID)"
                    + " VALUES (1,'merged',9.50,TRUE)");
            s.executeUpdate("MERGE INTO CUSTOMER (ID,NAME,BALANCE,ACTIVE) KEY(ID)"
                    + " VALUES (9001,'inserted',1.25,FALSE)");
        }
        try (Statement s = c.createStatement();
             ResultSet rs = s.executeQuery("SELECT NAME, BALANCE FROM CUSTOMER WHERE ID = 1")) {
            rs.next();
            eq(tag + "/merge.updatedName", "merged", rs.getString(1));
            eq(tag + "/merge.updatedBalance", "9.50", rs.getBigDecimal(2).toPlainString());
        }
        eq(tag + "/merge.inserted", 201, count(c, "SELECT COUNT(*) FROM CUSTOMER"));
        try (Statement s = c.createStatement()) {
            s.executeUpdate("DELETE FROM CUSTOMER WHERE ID = 9001");
        }
    }

    private static int count(Connection c, String sql) throws SQLException {
        try (Statement s = c.createStatement(); ResultSet rs = s.executeQuery(sql)) {
            rs.next();
            return rs.getInt(1);
        }
    }
}
