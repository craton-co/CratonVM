/*
 * Copyright 2004-2025 H2 Group. Multiple-Licensed under the MPL 2.0,
 * and the EPL 1.0 (https://h2database.com/html/license.html).
 * Initial Developer: H2 Group
 */
package org.h2.test.synth;

import java.sql.Connection;
import java.sql.DriverManager;
import java.sql.SQLException;
import java.sql.Statement;
import org.h2.api.ErrorCode;
import org.h2.store.fs.FileUtils;
import org.h2.test.TestBase;
import org.h2.test.TestDb;
import org.h2.test.utils.FilePathUnstable;

/**
 * Test simulated disk full problems.
 *
 * OVERLAY COPY (CratonVM diagnosis) -- adds per-iteration timing and a
 * DFULL_MAX env cap so the run terminates in bounded time. The body of
 * test(int) is byte-identical to upstream.
 */
public class TestDiskFull extends TestDb {

    private FilePathUnstable fs;

    /**
     * Run just this test.
     *
     * @param a ignored
     */
    public static void main(String... a) throws Exception {
        TestBase.createCaller().init().testFromMain();
    }

    @Override
    public void test() throws Exception {
        fs = FilePathUnstable.register();
        fs.setPartialWrites(true);
        try {
            long s0 = System.nanoTime();
            test(Integer.MAX_VALUE);
            System.out.println("[dfull] warmup test(MAX_VALUE) ms=" + ((System.nanoTime() - s0) / 1000000L));
            // Since test() run seems to be non-repeatable (due to randomness in thead scheduling?)
            // this need to be limited to make testing time reasonable
            System.out.println("[dfull] raw write ops in warmup: "
                    + (Integer.MAX_VALUE - fs.getDiskFullCount()));
            int max = Math.min(1000, Integer.MAX_VALUE - fs.getDiskFullCount() + 10);
            String cap = System.getenv("DFULL_MAX");
            System.out.println("[dfull] natural write op count: " + max);
            if (cap != null && !cap.isEmpty()) {
                // absolute override so every run does the same amount of work
                max = Integer.parseInt(cap);
            }
            println("write op count: " + max);
            System.out.println("[dfull] write op count: " + max);
            long t0 = System.nanoTime();
            String startEnv = System.getenv("DFULL_START");
            int start = startEnv == null || startEnv.isEmpty() ? 0 : Integer.parseInt(startEnv);
            for (int i = start; i < start + max; i++) {
                long a = System.nanoTime();
                test(i);
                long b = System.nanoTime();
                System.out.println("[dfull] iter=" + i + " ms=" + ((b - a) / 1000000L)
                        + " elapsed_s=" + ((b - t0) / 1000000000L));
            }
        } finally {
            fs.setPartialWrites(false);
        }
    }

    private boolean test(int x) throws SQLException {
        deleteDb("memFS:", null);
        fs.setDiskFullCount(x, 0);
        String wd = System.getenv("DFULL_WRITE_DELAY");
        if (wd == null || wd.isEmpty()) {
            wd = "10";
        }
        String url = "jdbc:h2:unstable:memFS:diskFull" + x +
            ";FILE_LOCK=NO;TRACE_LEVEL_FILE=0;WRITE_DELAY=" + wd + ";" +
            "LOCK_TIMEOUT=100;CACHE_SIZE=4096;MAX_COMPACT_TIME=10";
        url = getURL(url, true);
        Connection conn = null;
        Statement stat = null;
        boolean opened = false;
        try {
            conn = DriverManager.getConnection(url);
            stat = conn.createStatement();
            opened = true;
            for (int j = 0; j < 5; j++) {
                stat.execute("create table test(id int primary key, name varchar)");
                stat.execute("insert into test values(1, 'Hello')");
                stat.execute("create index idx_name on test(name)");
                stat.execute("insert into test values(2, 'World')");
                stat.execute("update test set name='Hallo' where id=1");
                stat.execute("delete from test where id=2");
                stat.execute("checkpoint");
                stat.execute("insert into test values(3, space(10000))");
                stat.execute("update test set name='Hallo' where id=3");
                stat.execute("drop table test");
            }
            conn.close();
            conn = null;
            return fs.getDiskFullCount() > 0;
        } catch (SQLException e) {
            if (e.getErrorCode() == ErrorCode.TABLE_OR_VIEW_ALREADY_EXISTS_1) {
                throw e;
            }
            if (stat != null) {
                try {
                    fs.setDiskFullCount(0, 0);
                    stat.execute("create table if not exists test" +
                            "(id int primary key, name varchar)");
                    stat.execute("insert into test values(4, space(10000))");
                    stat.execute("update test set name='Hallo' where id=3");
                    conn.close();
                } catch (SQLException e2) {
                    if (e2.getErrorCode() != ErrorCode.IO_EXCEPTION_1
                            && e2.getErrorCode() != ErrorCode.IO_EXCEPTION_2
                            && e2.getErrorCode() != ErrorCode.DATABASE_IS_CLOSED
                            && e2.getErrorCode() != ErrorCode.OBJECT_CLOSED) {
                        throw e2;
                    }
                }
            }
        } finally {
            if (conn != null) {
                try {
                    if (stat != null) {
                        stat.execute("shutdown immediately");
                    }
                } catch (Exception e2) {
                    // ignore
                }
                try {
                    conn.close();
                } catch (Exception e2) {
                    // ignore
                }
            }
        }
        fs.setDiskFullCount(0, 0);
        try {
            conn = null;
            conn = DriverManager.getConnection(url);
        } catch (SQLException e) {
            if (!opened) {
                return false;
            }
            throw e;
        }
        stat = conn.createStatement();
        stat.execute("script to 'memFS:test.sql'");
        conn.close();

        deleteDb("memFS:", null);
        FileUtils.delete("memFS:test.sql");

        return false;
    }

}
