import java.sql.Connection;
import java.sql.DriverManager;
import java.sql.PreparedStatement;
import java.sql.ResultSet;
import java.sql.SQLException;
import java.sql.Statement;

/**
 * Why `org.h2.test.db.TestTransaction.testMergeUsing` reports
 * `Expected: 100 actual: 50` on CratonVM.
 *
 * <p>The bug report it retires read the 2:1 ratio as a MERGE USING correctness
 * defect — half the rows silently not applied. It is not. `testMergeUsing`
 * runs two connections through the same 50-statement MERGE batch concurrently,
 * and `TestAll.lockTimeout` (the URL's `LOCK_TIMEOUT`) defaults to **50 ms**.
 * Whichever transaction loses the race waits for the winner's row lock, blows
 * that 50 ms wall, and H2 throws `Timeout trying to lock table "TEST"` out of
 * `executeBatch`. The test's own `catch (SQLException e) { // Ignore }` eats it,
 * so that thread contributes 0 and the total lands on exactly half. The "50" is
 * a lock-timeout casualty, not a lost row.
 *
 * <p>Three modes, each answering one question:
 * <ul>
 *   <li>{@code verify} — is MERGE USING actually correct? Drives both branches
 *       (WHEN MATCHED / WHEN NOT MATCHED) uncontended and checks the exact
 *       final row state, not just the update count.</li>
 *   <li>{@code contend <lockTimeoutMs> <iters>} — the `testMergeUsing` shape.
 *       Prints the exception the real test swallows. Sweep the budget: the
 *       verdict flips on the budget alone, with the VM held fixed.</li>
 *   <li>{@code bench <rows> <reps>} — uncontended per-operation cost for
 *       MERGE / UPDATE / SELECT / INSERT, so the MERGE path can be compared
 *       against its neighbours instead of against HotSpot alone.</li>
 * </ul>
 *
 * <p>Run the sweep against HotSpot too. Scale HotSpot's budget down by its
 * speed advantage (`-Dlt=3`) and it produces the byte-identical
 * `main=50 thread=0 total=50`. That is the control that settles it: the
 * failure follows the budget, not the VM.
 *
 * <pre>
 * javac -cp $H2/target/classes -d probe MergeLockBudgetProbe.java
 * &lt;cratonvm&gt; --java-home $JDK25 --Xmx 1g -c "probe:$H2/target/classes" \
 *     MergeLockBudgetProbe contend 50 4
 * </pre>
 */
public final class MergeLockBudgetProbe {

    /** Verbatim from TestTransaction.testMergeUsing. */
    private static final String MERGE =
            "MERGE INTO TEST T USING (SELECT ?1::INT X) S ON T.ID = S.X AND NOT T.\"VALUE\""
            + " WHEN MATCHED THEN UPDATE SET T.\"VALUE\" = TRUE"
            + " WHEN NOT MATCHED THEN INSERT VALUES (10000 + ?1, FALSE)";

    private static int failures;

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "verify";
        Class.forName("org.h2.Driver");
        switch (mode) {
            case "verify":
                verify(arg(args, 1, 50));
                break;
            case "contend":
                contend(arg(args, 1, 50), arg(args, 2, 4), arg(args, 3, 50));
                break;
            case "bench":
                bench(arg(args, 1, 500), arg(args, 2, 3));
                break;
            default:
                System.out.println("usage: MergeLockBudgetProbe verify|contend|bench [args]");
                System.exit(2);
        }
        System.out.println(failures == 0 ? "PROBE-OK" : "PROBE-FAILURES=" + failures);
        if (failures != 0) {
            System.exit(1);
        }
    }

    // ------------------------------------------------------------------
    // verify — is MERGE USING correct when nothing is contending?
    // ------------------------------------------------------------------

    private static void verify(int count) throws Exception {
        Connection conn = open("mergeverify", 50);
        conn.setAutoCommit(false);
        Statement st = conn.createStatement();
        seed(st, count);
        conn.commit();

        PreparedStatement prep = conn.prepareStatement(MERGE);

        // Pass 1: every row matches (VALUE=FALSE), so every statement takes
        // the WHEN MATCHED / UPDATE branch.
        check("pass1 updateCount", batch(prep, count), count);
        conn.commit();
        check("pass1 rows total", scalar(st, "SELECT COUNT(*) FROM TEST"), count);
        check("pass1 rows VALUE=TRUE", scalar(st, "SELECT COUNT(*) FROM TEST WHERE \"VALUE\""), count);
        check("pass1 no rows inserted", scalar(st, "SELECT COUNT(*) FROM TEST WHERE ID > 10000"), 0);

        // Pass 2: nothing matches any more (all TRUE), so every statement takes
        // the WHEN NOT MATCHED / INSERT branch.
        check("pass2 updateCount", batch(prep, count), count);
        conn.commit();
        check("pass2 rows total", scalar(st, "SELECT COUNT(*) FROM TEST"), count * 2);
        check("pass2 inserted ids exact",
                scalar(st, "SELECT COUNT(*) FROM TEST WHERE ID - 10000 BETWEEN 1 AND " + count), count);
        check("pass2 inserted all FALSE",
                scalar(st, "SELECT COUNT(*) FROM TEST WHERE ID > 10000 AND NOT \"VALUE\""), count);
        check("pass2 originals still TRUE",
                scalar(st, "SELECT COUNT(*) FROM TEST WHERE ID <= " + count + " AND \"VALUE\""), count);
        conn.close();
    }

    // ------------------------------------------------------------------
    // contend — the testMergeUsing shape, with the swallowed exception shown
    // ------------------------------------------------------------------

    private static void contend(int count, int iters, int lockTimeoutMs) throws Exception {
        System.out.println("LOCK_TIMEOUT=" + lockTimeoutMs + "ms, " + count + " merges per thread");
        int ok = 0;
        for (int it = 0; it < iters; it++) {
            final Connection conn1 = open("mergecontend", lockTimeoutMs);
            conn1.setAutoCommit(false);
            Connection conn2 = open("mergecontend", lockTimeoutMs);
            conn2.setAutoCommit(false);
            Statement stat1 = conn1.createStatement();
            seed(stat1, count);
            conn1.commit();
            stat1.executeQuery("SELECT * FROM TEST").close();
            conn2.createStatement().executeQuery("SELECT * FROM TEST").close();

            final int[] r = new int[1];
            final long[] ms = new long[2];
            final int n = count;
            Thread t = new Thread(() -> {
                long t0 = System.nanoTime();
                try {
                    r[0] = batch(conn1.prepareStatement(MERGE), n);
                    conn1.commit();
                } catch (Throwable e) {
                    System.out.println("  [thread] " + firstLine(e));
                }
                ms[0] = (System.nanoTime() - t0) / 1_000_000;
            });
            t.start();

            int sum = 0;
            long t0 = System.nanoTime();
            try {
                sum = batch(conn2.prepareStatement(MERGE), count);
                conn2.commit();
            } catch (Throwable e) {
                System.out.println("  [main]   " + firstLine(e));
            }
            ms[1] = (System.nanoTime() - t0) / 1_000_000;
            t.join();

            boolean pass = sum + r[0] == count * 2;
            if (pass) {
                ok++;
            }
            System.out.println("  iter " + it + ": main=" + sum + " (" + ms[1] + "ms)"
                    + " thread=" + r[0] + " (" + ms[0] + "ms)"
                    + " total=" + (sum + r[0]) + " expected=" + (count * 2)
                    + (pass ? "  OK" : "  FAIL"));
            conn2.close();
            conn1.close();
        }
        System.out.println("  " + ok + " of " + iters + " OK at LOCK_TIMEOUT=" + lockTimeoutMs + "ms");
        // Deliberately NOT counted as a probe failure: a FAIL here at a small
        // budget is the finding, not a defect.
    }

    // ------------------------------------------------------------------
    // bench — is the MERGE path slow relative to its neighbours, or uniformly?
    // ------------------------------------------------------------------

    private static void bench(int count, int reps) throws Exception {
        String[] names = {"MERGE", "UPDATE", "SELECT", "INSERT"};
        String[] sqls = {
            MERGE,
            "UPDATE TEST SET \"VALUE\" = TRUE WHERE ID = ?1 AND NOT \"VALUE\"",
            "SELECT ID FROM TEST WHERE ID = ?1 AND NOT \"VALUE\"",
            "INSERT INTO TEST VALUES (20000 + ?1, FALSE)",
        };
        Connection conn = open("mergebench", 50);
        conn.setAutoCommit(false);
        Statement st = conn.createStatement();
        for (int rep = 0; rep < reps; rep++) {
            for (int k = 0; k < names.length; k++) {
                st.execute("DROP TABLE IF EXISTS TEST");
                seed(st, count);
                conn.commit();
                PreparedStatement prep = conn.prepareStatement(sqls[k]);
                long t0 = System.nanoTime();
                if (k == 2) {
                    for (int i = 1; i <= count; i++) {
                        prep.setInt(1, i);
                        prep.executeQuery().close();
                    }
                } else {
                    batch(prep, count);
                }
                long dt = System.nanoTime() - t0;
                conn.commit();
                System.out.println("  rep " + rep + " " + names[k] + ": " + count + " ops in "
                        + (dt / 1_000_000.0) + " ms");
            }
        }
        conn.close();
    }

    // ------------------------------------------------------------------

    private static Connection open(String name, int lockTimeoutMs) throws SQLException {
        // MV_STORE / MAX_COMPACT_TIME / LOCK_TIMEOUT mirror what TestDb.getURL
        // builds for TestTransaction; LOCK_TIMEOUT is the one under study.
        return DriverManager.getConnection("jdbc:h2:./data/" + name + "/db"
                + ";MV_STORE=true;MAX_COMPACT_TIME=0;LOCK_TIMEOUT=" + lockTimeoutMs, "sa", "");
    }

    private static void seed(Statement st, int count) throws SQLException {
        st.execute("DROP TABLE IF EXISTS TEST");
        st.execute("CREATE TABLE TEST(ID INT PRIMARY KEY, \"VALUE\" BOOLEAN) AS "
                + "SELECT X, FALSE FROM GENERATE_SERIES(1, " + count + ")");
    }

    private static int batch(PreparedStatement prep, int count) throws SQLException {
        for (int i = 1; i <= count; i++) {
            prep.setInt(1, i);
            prep.addBatch();
        }
        int sum = 0;
        for (int v : prep.executeBatch()) {
            sum += v;
        }
        return sum;
    }

    private static int scalar(Statement st, String sql) throws SQLException {
        try (ResultSet rs = st.executeQuery(sql)) {
            rs.next();
            return rs.getInt(1);
        }
    }

    private static void check(String what, int actual, int expected) {
        boolean ok = actual == expected;
        if (!ok) {
            failures++;
        }
        System.out.println((ok ? "  ok   " : "  FAIL ") + what + ": expected=" + expected
                + " actual=" + actual);
    }

    private static String firstLine(Throwable e) {
        String s = String.valueOf(e);
        int nl = s.indexOf('\n');
        return nl < 0 ? s : s.substring(0, nl);
    }

    private static int arg(String[] args, int i, int dflt) {
        return args.length > i ? Integer.parseInt(args[i]) : dflt;
    }

    private MergeLockBudgetProbe() {
    }
}
