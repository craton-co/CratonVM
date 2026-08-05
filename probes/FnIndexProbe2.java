import java.sql.Connection;
import java.sql.DriverManager;
import java.sql.ResultSet;
import java.sql.Statement;
import java.sql.Types;

import org.h2.tools.SimpleResultSet;

/**
 * FnIndexProbe with warmup: the same query run N times, so the
 * `org.h2.command.query.Select` chain that must appear in the ALIAS function's
 * `Thread.currentThread().getStackTrace()` gets JIT-compiled first.
 *
 * Reports the iteration at which a `Select` frame stops being visible, which is
 * the whole of `TestIndex.testFunctionIndex`'s assertion.
 */
public final class FnIndexProbe2 {

    static boolean sawSelectThisCall = false;
    static int deepestDepth = 0;
    static int shallowestDepth = Integer.MAX_VALUE;

    /** Called from the database. */
    public static ResultSet fn() {
        StackTraceElement[] st = Thread.currentThread().getStackTrace();
        boolean hit = false;
        for (StackTraceElement e : st) {
            if (e.getClassName().startsWith("org.h2.command.query.Select")) {
                hit = true;
                break;
            }
        }
        sawSelectThisCall |= hit;
        if (hit) {
            deepestDepth = Math.max(deepestDepth, st.length);
            shallowestDepth = Math.min(shallowestDepth, st.length);
        }
        SimpleResultSet rs = new SimpleResultSet();
        rs.addColumn("ID", Types.INTEGER, 10, 0);
        rs.addColumn("VALUE", Types.INTEGER, 10, 0);
        rs.addRow(1, 10);
        rs.addRow(2, 20);
        rs.addRow(3, 30);
        return rs;
    }

    static void dumpOnce() {
        StackTraceElement[] st = Thread.currentThread().getStackTrace();
        System.out.println("[stk] depth=" + st.length);
        for (StackTraceElement e : st) {
            System.out.println("[stk]   " + e.getClassName() + "." + e.getMethodName()
                    + ":" + e.getLineNumber());
        }
    }

    public static void main(String[] args) throws Exception {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 20000;
        int firstMiss = -1;
        int misses = 0;
        try (Connection c = DriverManager.getConnection("jdbc:h2:mem:fnidx2")) {
            Statement s = c.createStatement();
            s.execute("CREATE ALIAS TEST_INDEX FOR '" + FnIndexProbe2.class.getName() + ".fn'");
            for (int i = 0; i < n; i++) {
                sawSelectThisCall = false;
                try (ResultSet rs = s.executeQuery(
                        "SELECT * FROM TEST_INDEX() WHERE ID = 1 OR ID = 3")) {
                    while (rs.next()) {
                        rs.getInt(1);
                        rs.getInt(2);
                    }
                }
                if (!sawSelectThisCall) {
                    misses++;
                    if (firstMiss < 0) {
                        firstMiss = i;
                        System.out.println("[stk] FIRST MISS at iteration " + i
                                + " — dumping the NEXT call's stack");
                    }
                }
            }
            s.execute("DROP ALIAS TEST_INDEX");
        }
        System.out.println("RESULT iterations=" + n + " misses=" + misses
                + " firstMiss=" + firstMiss
                + " selectFrameDepthRange=[" + shallowestDepth + "," + deepestDepth + "]"
                + " (expect misses=0)");
        if (misses > 0) {
            dumpOnce();
        }
    }
}
