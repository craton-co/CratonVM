// TestTransaction.testMergeUsing's two-session MERGE with a configurable LOCK_TIMEOUT. Classpath: <h2 target/classes>.
//
// Record: docs/known-issues/h2/h2-throughput-and-budget-residuals-20260923.md
// Run: cratonvm --java-home <jdk-25> -c <dir>[:<classpath>] MergeProbe <lock-timeout-ms> <reps>
// Compare with the same command on HotSpot (java -cp ...).
import java.sql.*;

public class MergeProbe {
    public static void main(String[] a) throws Exception {
        int lockTimeout = Integer.parseInt(a[0]);
        int reps = Integer.parseInt(a[1]);
        for (int rep = 0; rep < reps; rep++) {
            String url = "jdbc:h2:./mp" + rep + ";LOCK_TIMEOUT=" + lockTimeout;
            final int count = 50;
            Connection conn1 = DriverManager.getConnection(url, "sa", "");
            conn1.setAutoCommit(false);
            Connection conn2 = DriverManager.getConnection(url, "sa", "");
            conn2.setAutoCommit(false);
            conn1.createStatement().execute("DROP TABLE IF EXISTS TEST");
            conn1.createStatement().execute("CREATE TABLE TEST(ID INT PRIMARY KEY, \"VALUE\" BOOLEAN) AS SELECT X, FALSE FROM GENERATE_SERIES(1, " + count + ")");
            conn1.commit();
            conn1.createStatement().executeQuery("SELECT * FROM TEST").close();
            conn2.createStatement().executeQuery("SELECT * FROM TEST").close();
            String sql = "MERGE INTO TEST T USING (SELECT ?1::INT X) S ON T.ID = S.X AND NOT T.\"VALUE\" WHEN MATCHED THEN UPDATE SET T.\"VALUE\" = TRUE WHEN NOT MATCHED THEN INSERT VALUES (10000 + ?1, FALSE)";
            final long[] t1 = new long[4];
            final Throwable[] e1 = new Throwable[1];
            final long base = System.nanoTime();
            Thread t = new Thread(() -> {
                try {
                    PreparedStatement prep = conn1.prepareStatement(sql);
                    for (int i = 1; i <= count; i++) { prep.setInt(1, i); prep.addBatch(); }
                    t1[0] = System.nanoTime() - base;
                    prep.executeBatch();
                    t1[1] = System.nanoTime() - base;
                    conn1.commit();
                    t1[2] = System.nanoTime() - base;
                } catch (Throwable e) { e1[0] = e; t1[3] = System.nanoTime() - base; }
            });
            t.start();
            long c0 = System.nanoTime() - base, c1 = -1, c2 = -1; String err = null;
            try {
                PreparedStatement prep = conn2.prepareStatement(sql);
                for (int i = 1; i <= count; i++) { prep.setInt(1, i); prep.addBatch(); }
                c0 = System.nanoTime() - base;
                prep.executeBatch();
                c1 = System.nanoTime() - base;
                conn2.commit();
                c2 = System.nanoTime() - base;
            } catch (SQLException e) { err = e.getErrorCode() + " " + (e.getNextException() != null ? e.getNextException().getErrorCode() : ""); c2 = System.nanoTime() - base; }
            t.join();
            System.out.printf("rep=%d T1 start=%dms batch=%dms commit=%dms err=%s | T2 start=%dms batch=%dms end=%dms err=%s%n",
                rep, t1[0] / 1000000, t1[1] / 1000000, t1[2] / 1000000, e1[0] == null ? "-" : e1[0].toString().substring(0, Math.min(80, e1[0].toString().length())),
                c0 / 1000000, c1 / 1000000, c2 / 1000000, err);
            conn2.close(); conn1.close();
        }
    }
}
