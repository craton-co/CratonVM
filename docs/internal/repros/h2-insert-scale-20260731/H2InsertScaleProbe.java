import java.sql.Connection;
import java.sql.DriverManager;
import java.sql.PreparedStatement;
import java.sql.Statement;
import java.util.ArrayList;
import java.util.List;

/**
 * Strips org.h2.test.db.TestMultiThread.testConcurrentInsert down to its
 * measurable core: N threads, each opening its own connection and doing
 * `rows` INSERT+commit pairs against one file-backed H2 database.
 *
 * Run with threads=1 and threads=25 on both VMs to tell a raw per-statement
 * throughput gap apart from a concurrency-scaling wall.
 *
 * Usage: H2InsertScaleProbe <dbdir> <threads> <rows>
 */
public class H2InsertScaleProbe {

    public static void main(String[] args) throws Exception {
        String dir = args.length > 0 ? args[0] : ".";
        final int threads = args.length > 1 ? Integer.parseInt(args[1]) : 25;
        final int rows = args.length > 2 ? Integer.parseInt(args[2]) : 1000;
        final String url = "jdbc:h2:" + dir + "/scaleprobe;LOCK_TIMEOUT=10000";

        Class.forName("org.h2.Driver");
        Connection setup = DriverManager.getConnection(url, "sa", "");
        Statement st = setup.createStatement();
        st.execute("DROP TABLE IF EXISTS TRAN");
        st.execute("CREATE TABLE TRAN (ID NUMBER(18,0) not null PRIMARY KEY)");

        final long[] elapsed = new long[threads];
        final Throwable[] errors = new Throwable[threads];
        List<Thread> ts = new ArrayList<Thread>();
        for (int t = 0; t < threads; t++) {
            final int idx = t;
            Thread th = new Thread(new Runnable() {
                public void run() {
                    long t0 = System.nanoTime();
                    try {
                        Connection c = DriverManager.getConnection(url, "sa", "");
                        try {
                            c.setAutoCommit(false);
                            PreparedStatement ins =
                                    c.prepareStatement("INSERT INTO tran (id) VALUES(?)");
                            long id = idx * 1000000L;
                            for (int j = 0; j < rows; j++) {
                                ins.setLong(1, id++);
                                ins.execute();
                                c.commit();
                            }
                        } finally {
                            c.close();
                        }
                    } catch (Throwable e) {
                        errors[idx] = e;
                    }
                    elapsed[idx] = (System.nanoTime() - t0) / 1000000L;
                }
            });
            ts.add(th);
        }
        long start = System.nanoTime();
        for (Thread th : ts) {
            th.start();
        }
        for (Thread th : ts) {
            th.join();
        }
        long wall = (System.nanoTime() - start) / 1000000L;

        long slowest = 0;
        long total = 0;
        int failed = 0;
        for (int t = 0; t < threads; t++) {
            if (errors[t] != null) {
                failed++;
                if (failed == 1) {
                    System.out.println("first error: " + errors[t]);
                    errors[t].printStackTrace(System.out);
                }
            }
            total += elapsed[t];
            if (elapsed[t] > slowest) {
                slowest = elapsed[t];
            }
        }
        System.out.println("threads=" + threads + " rows=" + rows
                + " wall_ms=" + wall
                + " slowest_thread_ms=" + slowest
                + " mean_thread_ms=" + (total / threads)
                + " failed=" + failed);
        setup.createStatement().execute("SHUTDOWN");
        setup.close();
    }
}
