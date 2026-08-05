import java.math.BigDecimal;
import java.math.RoundingMode;
import java.sql.Connection;
import java.sql.DriverManager;
import java.sql.PreparedStatement;
import java.sql.ResultSet;
import java.sql.Statement;
import java.util.HashMap;
import java.util.Map;

/**
 * Reproduce BatchTest's JDBC shape without Hibernate: real H2 bytecode, real
 * PreparedStatement.addBatch/executeBatch, the same statements Hibernate's
 * SingleStatementBatchImpl issues, the same unique index on (xval,yval).
 *
 * BatchTest fails 100% under JIT with
 *
 *   Unique index or primary key violation: PUBLIC.XY_INDEX_9 ON PUBLIC.DATAPOINT(XVAL, YVAL)
 *   ... update DataPoint set description=?,xval=?,yval=? where id=?
 *
 * and 0% under --nojit. Two mechanisms fit: a batch executed twice (a duplicated
 * row), or a batch entry whose bound xval/yval were not fully overwritten by a
 * later addBatch (one row silently carrying an earlier row's values). This probe
 * distinguishes them: it reads every row back after each phase and reports both
 * the row count and any duplicate (x,y) pair, so a duplicated INSERT shows up as
 * count > nEntities while a mis-bound parameter shows up as a duplicate pair at
 * the same count.
 *
 * Usage: H2BatchBindProbe [nEntities] [nBeforeFlush] [reps]
 * Exits 1 on the first divergence, 0 when every rep round-trips faithfully.
 */
public class H2BatchBindProbe {

    static final int SCALE = 19;

    static BigDecimal x(int i) {
        return new BigDecimal(i * 0.1d).setScale(SCALE, RoundingMode.DOWN);
    }

    static BigDecimal y(BigDecimal x) {
        return BigDecimal.valueOf(Math.cos(x.doubleValue())).setScale(SCALE, RoundingMode.DOWN);
    }

    /** Read the table back and report (count, duplicate-pair-or-null). */
    static String verify(Connection c, int expectedRows, String phase, int rep) throws Exception {
        Map<String, Long> seen = new HashMap<>();
        int rows = 0;
        try (Statement st = c.createStatement();
             ResultSet rs = st.executeQuery("select id, xval, yval from DataPoint order by xval asc")) {
            while (rs.next()) {
                rows++;
                long id = rs.getLong(1);
                String key = rs.getBigDecimal(2).toPlainString() + "|" + rs.getBigDecimal(3).toPlainString();
                Long prev = seen.put(key, id);
                if (prev != null) {
                    return "DUPLICATE-PAIR rep=" + rep + " phase=" + phase
                            + " id=" + id + " and id=" + prev + " share " + key;
                }
            }
        }
        if (rows != expectedRows) {
            return "ROW-COUNT rep=" + rep + " phase=" + phase
                    + " got=" + rows + " want=" + expectedRows;
        }
        return null;
    }

    public static void main(String[] args) throws Exception {
        final int nEntities = args.length > 0 ? Integer.parseInt(args[0]) : 50;
        final int nBeforeFlush = args.length > 1 ? Integer.parseInt(args[1]) : 20;
        final int reps = args.length > 2 ? Integer.parseInt(args[2]) : 500;

        Connection c = DriverManager.getConnection("jdbc:h2:mem:probe;DB_CLOSE_DELAY=-1", "sa", "");
        c.setAutoCommit(false);
        try (Statement st = c.createStatement()) {
            st.execute("create table DataPoint (id bigint not null primary key, "
                    + "description varchar(255), xval numeric(38,19), yval numeric(38,19))");
            st.execute("alter table DataPoint add constraint XY_INDEX_9 unique (xval, yval)");
        }
        c.commit();

        long failures = 0;
        for (int rep = 0; rep < reps; rep++) {
            // --- insert phase: batches of nBeforeFlush, exactly like the flush loop
            try (PreparedStatement ps = c.prepareStatement(
                    "insert into DataPoint (description, xval, yval, id) values (?,?,?,?)")) {
                for (int i = 0; i < nEntities; i++) {
                    BigDecimal xv = x(i);
                    BigDecimal yv = y(xv);
                    ps.setString(1, null);
                    ps.setBigDecimal(2, xv);
                    ps.setBigDecimal(3, yv);
                    ps.setLong(4, i + 1);
                    ps.addBatch();
                    if ((i + 1) % nBeforeFlush == 0) {
                        ps.executeBatch();
                    }
                }
                ps.executeBatch();
            }
            c.commit();
            String bad = verify(c, nEntities, "insert", rep);
            if (bad != null) {
                System.out.println(bad);
                failures++;
                if (failures > 5) {
                    System.exit(1);
                }
            }

            // --- update phase: the statement that actually threw in BatchTest.
            // Hibernate's unconditional dirty check rewrites xval/yval with the
            // row's UNCHANGED values, so this must be a no-op for the index.
            // Materialize first: holding the cursor open while updating the same
            // table on the same connection makes H2 time out on its own lock.
            long[] ids = new long[nEntities];
            BigDecimal[] xs = new BigDecimal[nEntities];
            BigDecimal[] ys = new BigDecimal[nEntities];
            int nRead = 0;
            try (Statement st = c.createStatement();
                 ResultSet rs = st.executeQuery("select id, xval, yval from DataPoint order by xval asc")) {
                while (rs.next()) {
                    ids[nRead] = rs.getLong(1);
                    xs[nRead] = rs.getBigDecimal(2);
                    ys[nRead] = rs.getBigDecimal(3);
                    nRead++;
                }
            }
            try (PreparedStatement ps = c.prepareStatement(
                    "update DataPoint set description=?,xval=?,yval=? where id=?")) {
                for (int k = 0; k < nRead; k++) {
                    ps.setString(1, "done!");
                    ps.setBigDecimal(2, xs[k]);
                    ps.setBigDecimal(3, ys[k]);
                    ps.setLong(4, ids[k]);
                    ps.addBatch();
                    if ((k + 1) % nBeforeFlush == 0) {
                        ps.executeBatch();
                    }
                }
                ps.executeBatch();
            }
            c.commit();
            bad = verify(c, nEntities, "update", rep);
            if (bad != null) {
                System.out.println(bad);
                failures++;
                if (failures > 5) {
                    System.exit(1);
                }
            }

            // --- delete phase, so the next rep starts on an empty table
            try (PreparedStatement ps = c.prepareStatement("delete from DataPoint where id=?")) {
                int n = 0;
                for (int i = 0; i < nEntities; i++) {
                    ps.setLong(1, i + 1);
                    ps.addBatch();
                    if (++n % nBeforeFlush == 0) {
                        ps.executeBatch();
                    }
                }
                ps.executeBatch();
            }
            c.commit();
            bad = verify(c, 0, "delete", rep);
            if (bad != null) {
                System.out.println(bad);
                failures++;
                if (failures > 5) {
                    System.exit(1);
                }
            }
        }
        System.out.println("H2BatchBindProbe nEntities=" + nEntities
                + " nBeforeFlush=" + nBeforeFlush + " reps=" + reps + " failures=" + failures);
        c.close();
        if (failures != 0) {
            System.exit(1);
        }
    }
}
