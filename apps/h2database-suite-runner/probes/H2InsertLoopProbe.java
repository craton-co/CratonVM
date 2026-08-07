import java.sql.Connection;
import java.sql.DriverManager;
import java.sql.PreparedStatement;
import java.sql.Statement;

/**
 * Models org.h2.test.db.TestTempTables.testAnalyzeReuseObjectId exactly:
 * a local temporary IDENTITY table plus N autocommit `insert ... default
 * values` through one PreparedStatement on one connection.
 *
 * Phases are timed separately so VM start / H2 class load (a flat ~40 CPU-s
 * tax on this VM, see h2-update-path-throughput-20260802.md) can be
 * subtracted from the loop cost rather than folded into it.
 *
 * Usage: H2InsertLoopProbe <rows> <reps> [dbdir]
 */
public final class H2InsertLoopProbe {

    public static void main(String[] args) throws Exception {
        int rows = args.length > 0 ? Integer.parseInt(args[0]) : 10000;
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 1;
        String dir = args.length > 2 ? args[2] : "./mvperfdb";

        long t0 = System.nanoTime();
        Class.forName("org.h2.Driver");
        long tDriver = System.nanoTime();

        String url = "jdbc:h2:" + dir + "/test;LOCK_TIMEOUT=10000";
        for (int rep = 0; rep < reps; rep++) {
            long tc0 = System.nanoTime();
            Connection conn = DriverManager.getConnection(url, "sa", "");
            long tc1 = System.nanoTime();

            Statement stat = conn.createStatement();
            stat.execute("drop table if exists test");
            stat.execute("create local temporary table test(id identity)");
            long tc2 = System.nanoTime();

            PreparedStatement prep =
                    conn.prepareStatement("insert into test default values");
            long tl0 = System.nanoTime();
            for (int i = 0; i < rows; i++) {
                prep.execute();
            }
            long tl1 = System.nanoTime();
            prep.close();
            stat.close();
            conn.close();
            long tl2 = System.nanoTime();

            System.out.printf(
                    "rep=%d rows=%d connect=%.3f ddl=%.3f loop=%.3f close=%.3f us_per_row=%.1f%n",
                    rep, rows,
                    ms(tc0, tc1), ms(tc1, tc2), ms(tl0, tl1), ms(tl1, tl2),
                    (tl1 - tl0) / 1000.0 / rows);
        }
        System.out.printf("driver_load=%.3f total=%.3f%n",
                ms(t0, tDriver), ms(t0, System.nanoTime()));
    }

    private static double ms(long a, long b) {
        return (b - a) / 1e6;
    }
}
