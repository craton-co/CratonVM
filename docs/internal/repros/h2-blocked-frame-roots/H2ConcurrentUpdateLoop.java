import java.math.BigDecimal;
import java.sql.Connection;
import java.sql.DriverManager;
import java.sql.PreparedStatement;
import java.util.ArrayList;
import java.util.concurrent.Callable;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.TimeUnit;

/**
 * Looping standalone model of org.h2.test.db.TestMultiThread.testConcurrentUpdate,
 * for the ClassId(0) blocked-frame family
 * (fixed-suite-bugs/h2-suite-bugs/bug-h2-classid0-stale-address-family-FIXED.md).
 *
 * The real class costs ~450 s per attempt and runs ten other methods first.
 * This keeps everything the family's evidence points at and drops the rest:
 *
 *   * the SAME enhanced-for over the futures list with a TIMED
 *     `job.get(5, MINUTES)` — that synthetic `Iterator` local, held only by a
 *     frame of a thread parked inside `get()`, is the reported victim;
 *   * the SAME 25 threads x 1000 `UPDATE ... ; commit` inner loop, which is
 *     what keeps 25 peers allocating hard while main is parked;
 *   * the SAME long-lived outer `Connection` (so the database is not closed
 *     between rounds) and the SAME `LOCK_TIMEOUT=10000`.
 *
 * Every round re-enters the parked-main state, so exposure per second is far
 * higher than the class gives. Errors are reported and counted per round
 * instead of ending the run, because the interesting outcome is a rate.
 *
 * Usage: H2ConcurrentUpdateLoop [rounds] [threads] [updatesPerThread] [rows]
 */
public class H2ConcurrentUpdateLoop {

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 20;
        int threadCount = args.length > 1 ? Integer.parseInt(args[1]) : 25;
        int updates = args.length > 2 ? Integer.parseInt(args[2]) : 1000;
        int objectCount = args.length > 3 ? Integer.parseInt(args[3]) : 10000;

        String dir = System.getProperty("probe.dir", "./h2culdb");
        final String url = "jdbc:h2:" + dir + "/lockMode;LOCK_TIMEOUT=10000";

        Connection conn = DriverManager.getConnection(url, "sa", "");
        conn.createStatement().execute("DROP TABLE IF EXISTS ACCOUNT");
        conn.createStatement().execute(
                "CREATE TABLE IF NOT EXISTS ACCOUNT"
                + "(ID NUMBER(18,0) not null PRIMARY KEY, BALANCE NUMBER null)");
        PreparedStatement merge = conn.prepareStatement(
                "MERGE INTO Account(id, balance) key (id) VALUES (?, ?)");
        for (int i = 0; i < objectCount; i++) {
            merge.setLong(1, i);
            merge.setBigDecimal(2, BigDecimal.ZERO);
            merge.execute();
        }
        System.out.println("seeded rows=" + objectCount);

        ExecutorService executor = Executors.newFixedThreadPool(threadCount);
        int totalFailed = 0;
        for (int round = 1; round <= rounds; round++) {
            long t0 = System.currentTimeMillis();
            final int oc = objectCount;
            final int upd = updates;
            final ArrayList<Callable<Void>> callables = new ArrayList<>();
            for (int i = 0; i < threadCount; i++) {
                callables.add(() -> {
                    try (Connection taskConn = DriverManager.getConnection(url, "sa", "")) {
                        taskConn.setAutoCommit(false);
                        PreparedStatement u = taskConn.prepareStatement(
                                "UPDATE account SET balance = ? WHERE id = ?");
                        for (int j = 0; j < upd; j++) {
                            u.setDouble(1, Math.random());
                            u.setLong(2, (int) (Math.random() * oc));
                            u.execute();
                            taskConn.commit();
                        }
                    }
                    return null;
                });
            }
            final ArrayList<Future<Void>> jobs = new ArrayList<>();
            for (int i = 0; i < threadCount; i++) {
                jobs.add(executor.submit(callables.get(i)));
            }
            int failed = 0;
            // The shape under test: main parks inside get() while 25 peers
            // allocate, and the iterator lives only in this frame's locals.
            for (Future<Void> job : jobs) {
                try {
                    job.get(5, TimeUnit.MINUTES);
                } catch (Throwable e) {
                    failed++;
                    System.out.println("ROUND " + round + " FAILURE: " + e);
                    e.printStackTrace(System.out);
                }
            }
            totalFailed += failed;
            System.out.println("round=" + round
                    + " ms=" + (System.currentTimeMillis() - t0)
                    + " failed=" + failed
                    + " totalFailed=" + totalFailed);
            System.out.flush();
        }
        executor.shutdown();
        executor.awaitTermination(20, TimeUnit.SECONDS);
        conn.close();
        System.out.println("DONE totalFailed=" + totalFailed);
    }
}
