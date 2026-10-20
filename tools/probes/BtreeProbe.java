// TestBtreeIndex.testAddDelete in isolation. Classpath: <h2 target/classes>.
//
// Record: docs/known-issues/h2/h2-throughput-and-budget-residuals-20260923.md
// Run: cratonvm --java-home <jdk-25> -c <dir>[:<classpath>] BtreeProbe <reps> [file|mem]
// Compare with the same command on HotSpot (java -cp ...).
import java.sql.*;

public class BtreeProbe {
    public static void main(String[] a) throws Exception {
        int reps = Integer.parseInt(a[0]);
        String mode = a.length > 1 ? a[1] : "file";
        for (int r = 0; r < reps; r++) {
            long t0 = System.nanoTime();
            String url = mode.equals("mem") ? "jdbc:h2:mem:bt" + r : "jdbc:h2:./bt" + r;
            Connection conn = DriverManager.getConnection(url, "sa", "");
            Statement stat = conn.createStatement();
            stat.execute("CREATE TABLE TEST(ID bigint primary key)");
            int count = 1000;
            stat.execute("insert into test select x from system_range(1, " + count + ")");
            if (!mode.equals("mem")) { conn.close(); conn = DriverManager.getConnection(url, "sa", ""); stat = conn.createStatement(); }
            long t1 = System.nanoTime();
            long rows = 0;
            for (int i = 1; i < count; i++) {
                ResultSet rs = stat.executeQuery("select * from test order by id");
                while (rs.next()) { rs.getInt(1); rows++; }
                stat.execute("delete from test where id =" + i);
            }
            long t2 = System.nanoTime();
            stat.execute("drop all objects delete files");
            conn.close();
            System.out.printf("rep=%d setup=%dms loop=%dms rows=%d%n", r, (t1 - t0) / 1000000, (t2 - t1) / 1000000, rows);
        }
    }
}
