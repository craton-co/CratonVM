import java.math.BigDecimal;
import java.sql.Connection;
import java.sql.DriverManager;
import java.sql.PreparedStatement;

/**
 * Smallest single-threaded reduction of the `VersionedValueUncommitted cannot
 * be cast to org.h2.result.SearchRow` failure: just the seed loop that
 * H2ConcurrentUpdateLoop (and TestMultiThread.testConcurrentUpdate) runs before
 * any worker thread starts.
 *
 * Usage: H2MergeSeed [rows]
 */
public class H2MergeSeed {
    public static void main(String[] args) throws Exception {
        int rows = args.length > 0 ? Integer.parseInt(args[0]) : 10000;
        String dir = System.getProperty("probe.dir", "./h2mergeseed");
        String url = "jdbc:h2:" + dir + "/seed;LOCK_TIMEOUT=10000";
        Connection conn = DriverManager.getConnection(url, "sa", "");
        conn.createStatement().execute("DROP TABLE IF EXISTS ACCOUNT");
        conn.createStatement().execute(
                "CREATE TABLE IF NOT EXISTS ACCOUNT"
                + "(ID NUMBER(18,0) not null PRIMARY KEY, BALANCE NUMBER null)");
        PreparedStatement merge = conn.prepareStatement(
                "MERGE INTO Account(id, balance) key (id) VALUES (?, ?)");
        for (int i = 0; i < rows; i++) {
            merge.setLong(1, i);
            merge.setBigDecimal(2, BigDecimal.ZERO);
            merge.execute();
            if (i % 1000 == 0) {
                System.out.println("seeded " + i);
                System.out.flush();
            }
        }
        System.out.println("SEEDED OK rows=" + rows);
        conn.close();
    }
}
