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
import java.util.concurrent.TimeUnit;

/**
 * `org.h2.test.db.TestMultiThread.testConcurrentUpdate`, standalone.
 *
 * Same shape as the H2 test the MVStore-writer object-identity write-up
 * reproduces on: one 10000-row ACCOUNT table, 25 connections each running 1000
 * committed UPDATEs, with H2's MVStore background writer committing underneath.
 * Extracted so one attempt costs a couple of minutes instead of the >900s the
 * whole class needs to reach this method.
 */
public class MvidRepro {
    public static void main(String[] args) throws Exception {
        int objectCount = Integer.getInteger("objects", 10000);
        int threadCount = Integer.getInteger("threads", 25);
        int updates = Integer.getInteger("updates", 1000);
        int rounds = Integer.getInteger("rounds", 1);

        for (int round = 1; round <= rounds; round++) {
            String url = "jdbc:h2:./data/mvidrepro" + round + ";LOCK_TIMEOUT=10000";
            ExecutorService executor = Executors.newFixedThreadPool(threadCount);
            Connection conn = DriverManager.getConnection(url);
            long t0 = System.currentTimeMillis();
            try {
                conn.createStatement().execute("DROP TABLE IF EXISTS ACCOUNT");
                conn.createStatement().execute("CREATE TABLE IF NOT EXISTS ACCOUNT"
                        + "(ID NUMBER(18,0) not null PRIMARY KEY, BALANCE NUMBER null)");
                PreparedStatement mergeAcctStmt = conn
                        .prepareStatement("MERGE INTO Account(id, balance) key (id) VALUES (?, ?)");
                for (int i = 0; i < objectCount; i++) {
                    mergeAcctStmt.setLong(1, i);
                    mergeAcctStmt.setBigDecimal(2, BigDecimal.ZERO);
                    mergeAcctStmt.execute();
                }
                List<Callable<Void>> callables = new ArrayList<>();
                for (int i = 0; i < threadCount; i++) {
                    callables.add(() -> {
                        try (Connection taskConn = DriverManager.getConnection(url)) {
                            taskConn.setAutoCommit(false);
                            PreparedStatement updateAcctStmt = taskConn
                                    .prepareStatement("UPDATE account SET balance = ? WHERE id = ?");
                            for (int j = 0; j < updates; j++) {
                                updateAcctStmt.setDouble(1, Math.random());
                                updateAcctStmt.setLong(2, (int) (Math.random() * objectCount));
                                updateAcctStmt.execute();
                                taskConn.commit();
                            }
                        }
                        return null;
                    });
                }
                List<Future<Void>> jobs = new ArrayList<>();
                for (Callable<Void> c : callables) {
                    jobs.add(executor.submit(c));
                }
                for (Future<Void> job : jobs) {
                    job.get(5, TimeUnit.MINUTES);
                }
            } finally {
                try {
                    conn.close();
                } catch (Exception ignored) {
                    // closing the control connection cannot change the verdict
                }
                executor.shutdown();
                executor.awaitTermination(20, TimeUnit.SECONDS);
            }
            System.out.println("round " + round + " OK in " + (System.currentTimeMillis() - t0) + "ms");
        }
        System.out.println("PASS");
    }
}
