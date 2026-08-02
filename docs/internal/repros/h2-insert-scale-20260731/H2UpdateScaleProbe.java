import java.math.BigDecimal;
import java.sql.Connection;
import java.sql.DriverManager;
import java.sql.PreparedStatement;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.Callable;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;

/**
 * Standalone model of org.h2.test.db.TestMultiThread.testConcurrentUpdate.
 *
 * Same schema (NUMBER(18,0) PK + NUMBER balance), same MERGE seed loop, same
 * per-thread "UPDATE account SET balance=? WHERE id=?" + commit inner loop,
 * same LOCK_TIMEOUT. Row/thread counts are parameters so the shape can be
 * measured at 1 thread (constant-factor) and at 25 (contention) without
 * running the 13-minute class.
 *
 * Usage: H2UpdateScaleProbe [threads] [updatesPerThread] [objectCount]
 */
public class H2UpdateScaleProbe {

    public static void main(String[] args) throws Exception {
        int threadCount = args.length > 0 ? Integer.parseInt(args[0]) : 25;
        int updates = args.length > 1 ? Integer.parseInt(args[1]) : 1000;
        int objectCount = args.length > 2 ? Integer.parseInt(args[2]) : 10000;

        String dir = System.getProperty("probe.dir", "./h2updb");
        String url = "jdbc:h2:" + dir + "/lockMode;LOCK_TIMEOUT=10000";

        long tSetup = System.currentTimeMillis();
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
        long setupMs = System.currentTimeMillis() - tSetup;
        System.out.println("setup_ms=" + setupMs + " rows=" + objectCount);

        ExecutorService executor = Executors.newFixedThreadPool(threadCount);
        final long[] threadMs = new long[threadCount];
        List<Callable<Void>> callables = new ArrayList<>();
        for (int i = 0; i < threadCount; i++) {
            final int idx = i;
            final int oc = objectCount;
            callables.add(() -> {
                long t0 = System.currentTimeMillis();
                try (Connection taskConn = DriverManager.getConnection(url, "sa", "")) {
                    taskConn.setAutoCommit(false);
                    PreparedStatement upd = taskConn.prepareStatement(
                            "UPDATE account SET balance = ? WHERE id = ?");
                    for (int j = 0; j < updates; j++) {
                        upd.setDouble(1, Math.random());
                        upd.setLong(2, (int) (Math.random() * oc));
                        upd.execute();
                        taskConn.commit();
                    }
                }
                threadMs[idx] = System.currentTimeMillis() - t0;
                return null;
            });
        }

        long t0 = System.currentTimeMillis();
        List<Future<Void>> jobs = new ArrayList<>();
        for (int i = 0; i < threadCount; i++) {
            jobs.add(executor.submit(callables.get(i)));
        }
        int failed = 0;
        for (Future<Void> job : jobs) {
            try {
                job.get();
            } catch (Exception e) {
                failed++;
                if (failed == 1) {
                    System.out.println("first error: " + e);
                    e.printStackTrace(System.out);
                }
            }
        }
        long wall = System.currentTimeMillis() - t0;
        executor.shutdown();
        long slowest = 0, sum = 0;
        for (long m : threadMs) {
            slowest = Math.max(slowest, m);
            sum += m;
        }
        System.out.println("threads=" + threadCount + " updates=" + updates
                + " wall_ms=" + wall
                + " slowest_thread_ms=" + slowest
                + " mean_thread_ms=" + (sum / threadCount)
                + " failed=" + failed);
        conn.close();
        System.out.println("DONE");
    }
}
